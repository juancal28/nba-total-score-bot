//! Risk / kill-switch state. Tracks daily P&L, position size, and feed-gap
//! streaks. Strategy + executor consult `should_block_new_trades()` before
//! every order.

use std::sync::atomic::{AtomicU32, Ordering};

use rust_decimal::Decimal;
use thiserror::Error;

use crate::config::RiskConfig;

#[derive(Debug, Error, Clone)]
pub enum KillReason {
    #[error("position cap reached: {position} >= {cap}")]
    PositionCap { position: Decimal, cap: Decimal },
    #[error("daily loss limit reached: {loss} >= {cap}")]
    DailyLoss { loss: Decimal, cap: Decimal },
    #[error("feed-gap streak {streak} >= {cap}")]
    FeedGapStreak { streak: u32, cap: u32 },
}

pub struct RiskController {
    cfg: RiskConfig,
    position_usdc: parking_lot_compat::Mutex<Decimal>,
    daily_loss_usdc: parking_lot_compat::Mutex<Decimal>,
    feed_gap_streak: AtomicU32,
}

impl RiskController {
    pub fn new(cfg: RiskConfig) -> Self {
        Self {
            cfg,
            position_usdc: parking_lot_compat::Mutex::new(Decimal::ZERO),
            daily_loss_usdc: parking_lot_compat::Mutex::new(Decimal::ZERO),
            feed_gap_streak: AtomicU32::new(0),
        }
    }

    pub fn record_position_delta(&self, delta: Decimal) {
        let mut p = self.position_usdc.lock();
        *p += delta;
    }

    pub fn record_realized_loss(&self, loss: Decimal) {
        if loss > Decimal::ZERO {
            let mut l = self.daily_loss_usdc.lock();
            *l += loss;
        }
    }

    pub fn record_feed_gap(&self) {
        self.feed_gap_streak.fetch_add(1, Ordering::Relaxed);
    }

    pub fn clear_feed_gap_streak(&self) {
        self.feed_gap_streak.store(0, Ordering::Relaxed);
    }

    pub fn check(&self) -> Result<(), KillReason> {
        let position = *self.position_usdc.lock();
        if position >= self.cfg.max_position_usdc {
            return Err(KillReason::PositionCap {
                position,
                cap: self.cfg.max_position_usdc,
            });
        }
        let loss = *self.daily_loss_usdc.lock();
        if loss >= self.cfg.max_daily_loss_usdc {
            return Err(KillReason::DailyLoss {
                loss,
                cap: self.cfg.max_daily_loss_usdc,
            });
        }
        let streak = self.feed_gap_streak.load(Ordering::Relaxed);
        if streak >= self.cfg.max_consecutive_feed_gaps {
            return Err(KillReason::FeedGapStreak {
                streak,
                cap: self.cfg.max_consecutive_feed_gaps,
            });
        }
        Ok(())
    }
}

/// Thin std::sync::Mutex wrapper to avoid pulling in `parking_lot` for one
/// type. Local module — name kept to indicate API parity intent.
mod parking_lot_compat {
    use std::sync::{Mutex as StdMutex, MutexGuard};
    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self { Self(StdMutex::new(v)) }
        pub fn lock(&self) -> MutexGuard<'_, T> { self.0.lock().expect("mutex poisoned") }
    }
}
