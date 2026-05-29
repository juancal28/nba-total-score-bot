//! Layered config loading: defaults from `config/default.toml`, overrides from
//! `config/local.toml`, and finally env vars prefixed with `NPB_`.

use std::path::PathBuf;

use figment::{Figment, providers::{Env, Format, Toml}};
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub wallet: WalletConfig,
    pub polymarket: PolymarketConfig,
    pub sportradar: SportRadarConfig,
    pub strategy: StrategyConfig,
    pub execution: ExecutionConfig,
    pub exit: ExitConfig,
    pub risk: RiskConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalletConfig {
    pub chain_id: u64,
    #[serde(default)]
    pub private_key: Option<SecretString>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PolymarketConfig {
    pub clob_rest: String,
    pub exchange_address: String,
    pub collateral_address: String,
    pub ctf_address: String,
    pub neg_risk_exchange_address: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SportRadarConfig {
    pub base_url: String,
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub api_key: Option<SecretString>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StrategyConfig {
    pub q2_window_minutes: u32,
    pub lr_p_threshold: f64,
    pub bootstrap_iters: usize,
    pub q1_scaling_factor: f64,
    pub min_coverage_ratio: f64,
    pub min_edge: f64,
    pub max_q1_margin: i32,
    pub max_book_age_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecutionConfig {
    #[serde(with = "rust_decimal::serde::str")]
    pub order_size_usdc: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_slippage: Decimal,
    pub sign_max_book_age_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExitConfig {
    pub take_profit_prob: f64,
    pub cut_loss_prob: f64,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RiskConfig {
    #[serde(with = "rust_decimal::serde::str")]
    pub max_position_usdc: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_daily_loss_usdc: Decimal,
    pub max_consecutive_feed_gaps: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub db_path: PathBuf,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let cfg: Config = Figment::new()
            .merge(Toml::file("config/default.toml"))
            .merge(Toml::file("config/local.toml"))
            .merge(Env::prefixed("NPB_").split("__"))
            .extract()?;
        Ok(cfg)
    }
}
