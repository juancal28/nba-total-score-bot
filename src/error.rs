use thiserror::Error;

#[derive(Error, Debug)]
pub enum BotError {
    #[error("config error: {0}")]
    Config(String),

    #[error("feed error: {0}")]
    Feed(String),

    #[error("polymarket error: {0}")]
    Polymarket(String),

    #[error("insufficient sample coverage: {actual:.3} < {required:.3}")]
    InsufficientCoverage { actual: f64, required: f64 },

    #[error("scoring rates inconsistent: lr p-value {p:.4} < {threshold:.4}")]
    InconsistentScoring { p: f64, threshold: f64 },

    #[error("order book stale: {age_ms} ms > {max_ms} ms")]
    StaleBook { age_ms: u64, max_ms: u64 },

    #[error("kill switch active: {0}")]
    KillSwitch(String),

    #[error("blowout filter: q1 margin {margin} > {max}")]
    Blowout { margin: i32, max: i32 },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T, E = BotError> = std::result::Result<T, E>;
