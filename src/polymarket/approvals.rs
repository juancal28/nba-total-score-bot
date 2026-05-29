//! One-time on-chain approvals that the Polymarket CLOB needs before it will
//! match orders for a fresh EOA:
//!
//!   1. `collateral.approve(exchange, MAX)` — lets the Exchange pull USDC
//!      collateral when filling buys.
//!   2. `collateral.approve(neg_risk_exchange, MAX)` — same for neg-risk
//!      markets, which route through a separate Exchange contract.
//!   3. `ctf.setApprovalForAll(exchange, true)` — lets the Exchange transfer
//!      conditional-token positions when filling sells.
//!   4. `ctf.setApprovalForAll(neg_risk_exchange, true)` — same for neg-risk.
//!
//! All four are idempotent: we read current allowance / approval state and
//! skip if already set. Each tx costs a few cents of MATIC on Polygon.

use std::str::FromStr;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::{Context, Result, anyhow};
use tracing::{info, warn};

use crate::config::PolymarketConfig;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICTF {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address owner, address operator) external view returns (bool);
    }
}

/// Default public Polygon RPC. Free, rate-limited; override via `--rpc` if you
/// have a private provider (Alchemy / QuickNode) for higher throughput.
pub const DEFAULT_POLYGON_RPC: &str = "https://polygon-rpc.com";

/// Run all four approvals if not already set. Returns the number of new txs
/// sent (0 if everything was already approved — a useful idempotency signal).
pub async fn run(
    cfg: &PolymarketConfig,
    signer: PrivateKeySigner,
    rpc_url: &str,
) -> Result<usize> {
    let owner = signer.address();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse().context("invalid RPC URL")?);

    // MATIC balance sanity check — approvals cost gas.
    let matic = provider.get_balance(owner).await.context("RPC get_balance")?;
    if matic < U256::from(10_000_000_000_000_000u128) {
        warn!(
            wei = %matic,
            "wallet MATIC balance is below 0.01 — approvals may fail with out-of-gas",
        );
    }

    let collateral = Address::from_str(&cfg.collateral_address)?;
    let ctf = Address::from_str(&cfg.ctf_address)?;
    let exchange = Address::from_str(&cfg.exchange_address)?;
    let neg_risk = Address::from_str(&cfg.neg_risk_exchange_address)?;

    let usdc = IERC20::new(collateral, provider.clone());
    let ctf_contract = ICTF::new(ctf, provider.clone());
    let max = U256::MAX;
    // Anything above this we treat as already-approved.
    let approval_floor = U256::MAX / U256::from(2);

    let mut sent = 0usize;

    for spender in [("Exchange", exchange), ("NegRiskExchange", neg_risk)] {
        let current = usdc
            .allowance(owner, spender.1)
            .call()
            .await
            .with_context(|| format!("USDC.allowance({}, {})", owner, spender.1))?;
        if current >= approval_floor {
            info!(spender = spender.0, "USDC already approved");
            continue;
        }
        info!(spender = spender.0, "approving USDC -> {}", spender.1);
        let pending = usdc.approve(spender.1, max).send().await?;
        let receipt = pending.get_receipt().await?;
        if !receipt.status() {
            return Err(anyhow!(
                "USDC.approve(spender={}) tx reverted: {:?}",
                spender.1,
                receipt.transaction_hash
            ));
        }
        info!(spender = spender.0, tx = %receipt.transaction_hash, "USDC approved");
        sent += 1;
    }

    for spender in [("Exchange", exchange), ("NegRiskExchange", neg_risk)] {
        let approved = ctf_contract
            .isApprovedForAll(owner, spender.1)
            .call()
            .await
            .with_context(|| format!("CTF.isApprovedForAll({}, {})", owner, spender.1))?;
        if approved {
            info!(spender = spender.0, "CTF already approved");
            continue;
        }
        info!(spender = spender.0, "CTF.setApprovalForAll -> {}", spender.1);
        let pending = ctf_contract.setApprovalForAll(spender.1, true).send().await?;
        let receipt = pending.get_receipt().await?;
        if !receipt.status() {
            return Err(anyhow!(
                "CTF.setApprovalForAll(operator={}) tx reverted: {:?}",
                spender.1,
                receipt.transaction_hash
            ));
        }
        info!(spender = spender.0, tx = %receipt.transaction_hash, "CTF approved");
        sent += 1;
    }

    Ok(sent)
}
