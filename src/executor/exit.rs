//! Holds-to-resolution exit logic with two early-close triggers:
//!   - Take-profit if implied resolution probability >= take_profit_prob
//!   - Cut-loss   if implied resolution probability <= cut_loss_prob
//! Implied probability is read directly from the order book mid.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::sleep;
use tracing::info;

use crate::config::ExitConfig;
use crate::polymarket::book::OrderBookSnapshot;

#[derive(Debug, Clone, Copy)]
pub enum ExitTrigger {
    TakeProfit,
    CutLoss,
    HoldToResolution,
}

/// Block until the position should be exited (TP/SL) or the receiver closes
/// (interpreted as "hold to resolution").
pub async fn watch_for_exit(
    cfg: &ExitConfig,
    mut book: watch::Receiver<Arc<OrderBookSnapshot>>,
) -> ExitTrigger {
    loop {
        let snapshot = book.borrow_and_update().clone();
        if let Some(mid) = snapshot.mid() {
            let prob: f64 = mid.to_string().parse().unwrap_or(0.5);
            if prob >= cfg.take_profit_prob {
                info!(prob, "take profit triggered");
                return ExitTrigger::TakeProfit;
            }
            if prob <= cfg.cut_loss_prob {
                info!(prob, "cut loss triggered");
                return ExitTrigger::CutLoss;
            }
        }
        sleep(Duration::from_millis(cfg.poll_interval_ms)).await;
        if book.changed().await.is_err() {
            return ExitTrigger::HoldToResolution;
        }
    }
}
