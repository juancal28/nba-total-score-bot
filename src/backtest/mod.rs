//! Backtesting: replay cached past games through the real `StrategyEngine`
//! and compare sizing variants. See
//! `docs/superpowers/specs/2026-10-28-backtester-design.md`.

pub mod fetch;
pub mod run;
