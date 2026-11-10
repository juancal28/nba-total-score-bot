pub mod analytics;
pub mod backtest;
pub mod config;
pub mod error;
pub mod executor;
pub mod feed;
pub mod paper;
pub mod polymarket;
pub mod risk;
pub mod status;
pub mod strategy;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use secrecy::ExposeSecret;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::executor::Executor;
use crate::feed::sportradar::SportRadarPoller;
use crate::polymarket::client::PolymarketClient;
use crate::polymarket::marketdata::MarketDataClient;
use crate::strategy::StrategyEngine;
use crate::strategy::sizing::SizingParams;

pub struct RunArgs {
    pub game_id: String,
    pub over_token_id: String,
    pub under_token_id: String,
    pub line: Decimal,
    pub dry_run: bool,
}

/// Run the bot against a single live game.
pub async fn run(args: RunArgs) -> Result<()> {
    let cfg = config::Config::load()?;
    info!(?cfg.strategy, ?cfg.execution, ?cfg.sizing, ?cfg.exit, ?cfg.risk, "config loaded");

    let pk = cfg
        .wallet
        .private_key
        .as_ref()
        .ok_or_else(|| anyhow!("NPB_WALLET__PRIVATE_KEY is not set"))?;
    let signer = PolymarketClient::signer_from_hex(pk.expose_secret())
        .context("invalid wallet private key")?;

    let poly = Arc::new(
        PolymarketClient::new(
            &cfg.polymarket,
            signer,
            args.over_token_id.clone(),
            args.under_token_id.clone(),
        )
        .await
        .context("authenticate against Polymarket CLOB")?,
    );

    let market_data = MarketDataClient::new();
    let book_rx = market_data.subscribe(&args.over_token_id)?;

    let (feed_tx, feed_rx) = mpsc::channel::<feed::GameEvent>(1024);
    let sr_key = cfg
        .sportradar
        .api_key
        .as_ref()
        .ok_or_else(|| anyhow!("NPB_SPORTRADAR__API_KEY is not set"))?
        .clone();
    let poller = SportRadarPoller::new(
        cfg.sportradar.base_url.clone(),
        sr_key,
        args.game_id.clone(),
        cfg.sportradar.poll_interval_ms,
    )?;
    tokio::spawn(async move {
        if let Err(e) = poller.run(feed_tx).await {
            warn!(error = ?e, "sportradar poller exited with error");
        }
    });

    let sizing = SizingParams {
        bankroll_usdc: cfg.sizing.bankroll_usdc.to_f64().unwrap_or(0.0),
        kelly_fraction: cfg.sizing.kelly_fraction,
        ci_ref_width: cfg.sizing.ci_ref_width,
        min_order_usdc: cfg.sizing.min_order_usdc.to_f64().unwrap_or(0.0),
        max_order_usdc: cfg.risk.max_position_usdc.to_f64().unwrap_or(0.0),
    };
    let strategy = StrategyEngine::new(cfg.strategy.clone(), sizing, args.line);
    let intent = strategy.run(feed_rx, book_rx.clone()).await;

    let Some(intent) = intent else {
        info!("no edge found; exiting");
        return Ok(());
    };

    info!(?intent, "trade intent produced");

    if args.dry_run {
        info!("DRY RUN: would submit the above intent");
        return Ok(());
    }

    let executor = Executor::open(&cfg.storage.db_path, poly.clone())?;
    let client_order_id = uuid::Uuid::new_v4().to_string();
    let state = executor.submit(&client_order_id, &intent).await?;
    info!(?state, %client_order_id, "executor::submit complete");

    let trigger = executor::exit::watch_for_exit(&cfg.exit, book_rx).await;
    info!(?trigger, "exit trigger fired");

    Ok(())
}

/// One-time setup: submit on-chain approvals and derive Polymarket L2 API
/// credentials. Both steps are idempotent — safe to re-run.
pub async fn setup(rpc_override: Option<&str>, skip_approvals: bool) -> Result<()> {
    let cfg = config::Config::load()?;
    let pk = cfg
        .wallet
        .private_key
        .as_ref()
        .ok_or_else(|| anyhow!("NPB_WALLET__PRIVATE_KEY is not set"))?;
    let signer = PolymarketClient::signer_from_hex(pk.expose_secret())?;

    if !skip_approvals {
        let rpc = rpc_override.unwrap_or(polymarket::approvals::DEFAULT_POLYGON_RPC);
        let sent = polymarket::approvals::run(&cfg.polymarket, signer.clone(), rpc)
            .await
            .context("on-chain approvals")?;
        info!(new_txs = sent, "on-chain approvals complete");
    } else {
        warn!("skipping on-chain approvals (--skip-approvals); CLOB will reject fills if not already approved");
    }

    // Trigger SDK's L1->L2 derivation by constructing an authenticated client.
    // The Polymarket server persists the API key/secret/passphrase keyed to
    // the wallet address, so subsequent runs fetch the same credentials.
    let _client = PolymarketClient::new(
        &cfg.polymarket,
        signer,
        "0".to_string(),
        "0".to_string(),
    )
    .await
    .context("authenticate to derive L2 credentials")?;

    info!("setup complete: approvals on-chain, L2 credentials derived server-side");
    Ok(())
}
