pub mod edge;

use std::sync::Arc;

use rust_decimal::Decimal;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::analytics::poisson::{PoissonFit, bootstrap_projected_total, lr_test_two_sample};
use crate::analytics::projection::{GAME_MINUTES, Projection};
use crate::config::StrategyConfig;
use crate::feed::{GameEvent, Team};
use crate::feed::coverage::CoverageTracker;
use crate::polymarket::book::OrderBookSnapshot;
use crate::strategy::edge::{EdgeDecision, evaluate_edge};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    PreGame,
    Q1Collecting,
    Q1End,
    Q2Validating,
    EdgeEvaluation,
    Holding,
    Done,
}

#[derive(Debug, Clone)]
pub struct TradeIntent {
    pub buy_side: BuyOutcome,
    pub size_usdc: Decimal,
    pub max_vwap: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub enum BuyOutcome {
    Over,
    Under,
}

pub struct StrategyEngine {
    pub cfg: StrategyConfig,
    pub line: Decimal,
    pub phase: Phase,
    pub q1_per_minute: Vec<u32>,
    pub q2_per_minute: Vec<u32>,
    pub coverage: CoverageTracker,
    pub q1_home: u32,
    pub q1_away: u32,
}

impl StrategyEngine {
    pub fn new(cfg: StrategyConfig, line: Decimal) -> Self {
        let q2_window = cfg.q2_window_minutes as usize;
        Self {
            cfg,
            line,
            phase: Phase::PreGame,
            q1_per_minute: vec![0; 12],
            q2_per_minute: vec![0; q2_window],
            coverage: CoverageTracker::new(12),
            q1_home: 0,
            q1_away: 0,
        }
    }

    /// Main loop. Reads events + book snapshots and produces a `TradeIntent`
    /// when the model says there is edge. Returns when the trade decision is
    /// made (`Some`), the game enters a state where no trade is possible
    /// (`None`), or the channels are closed.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<GameEvent>,
        mut book: watch::Receiver<Arc<OrderBookSnapshot>>,
    ) -> Option<TradeIntent> {
        self.phase = Phase::Q1Collecting;
        info!(phase = ?self.phase, "strategy started");

        while let Some(ev) = events.recv().await {
            self.handle_event(&ev);

            if self.phase == Phase::EdgeEvaluation {
                let snapshot = book.borrow_and_update().clone();
                match self.evaluate(snapshot) {
                    Some(intent) => {
                        info!(?intent, "edge found; emitting trade intent");
                        return Some(intent);
                    }
                    None => {
                        info!("no edge; transitioning to Done");
                        self.phase = Phase::Done;
                        return None;
                    }
                }
            }

            if self.phase == Phase::Done {
                return None;
            }
        }
        None
    }

    fn handle_event(&mut self, ev: &GameEvent) {
        match ev {
            GameEvent::Score { period, game_clock_seconds, team, points, home_score, away_score, .. } => {
                let minute = minute_index(*period, *game_clock_seconds);
                match (*period, minute) {
                    (1, Some(m)) if m < self.q1_per_minute.len() => {
                        self.q1_per_minute[m] += *points as u32;
                        if matches!(team, Team::Home) { self.q1_home += *points as u32; }
                        else { self.q1_away += *points as u32; }
                    }
                    (2, Some(m)) if m < self.q2_per_minute.len() => {
                        self.q2_per_minute[m] += *points as u32;
                    }
                    _ => {}
                }
                let _ = (home_score, away_score);
            }
            GameEvent::PeriodEnd { period, .. } => {
                if *period == 1 {
                    self.phase = Phase::Q1End;
                    info!(phase = ?self.phase, q1 = ?self.q1_per_minute, "Q1 closed");
                    self.phase = Phase::Q2Validating;
                }
            }
            GameEvent::ClockUpdate { period, game_clock_seconds, .. } => {
                if let Some(m) = minute_index(*period, *game_clock_seconds) {
                    if *period == 1 && m < 12 {
                        self.coverage.observe(m, 60);
                    }
                    if *period == 2 && self.phase == Phase::Q2Validating {
                        if m >= self.cfg.q2_window_minutes as usize {
                            self.phase = Phase::EdgeEvaluation;
                        }
                    }
                }
            }
            GameEvent::FeedGap { duration_ms, .. } => {
                warn!(duration_ms, "feed gap");
                // Pessimistically reduce coverage. We don't know what minute we
                // lost, so charge it against the most-recent observed minute.
                self.coverage.gap(0, (*duration_ms / 1000) as u32);
            }
            GameEvent::RosterChange { team, reason, .. } => {
                warn!(?team, reason, "roster change during sample window — invalidating");
                self.phase = Phase::Done;
            }
            GameEvent::Tipoff { .. } => {}
        }
    }

    fn evaluate(&self, book: Arc<OrderBookSnapshot>) -> Option<TradeIntent> {
        // Coverage gate.
        let coverage = self.coverage.coverage_ratio();
        if coverage < self.cfg.min_coverage_ratio {
            warn!(coverage, required = self.cfg.min_coverage_ratio, "coverage gate failed");
            return None;
        }

        // Blowout gate.
        let margin = (self.q1_home as i32 - self.q1_away as i32).abs();
        if margin > self.cfg.max_q1_margin {
            warn!(margin, max = self.cfg.max_q1_margin, "blowout gate failed");
            return None;
        }

        // Poisson fit and LR consistency test.
        let q1_fit = PoissonFit::fit(&self.q1_per_minute);
        let q2_fit = PoissonFit::fit(&self.q2_per_minute);
        let p = lr_test_two_sample(&q1_fit, &q2_fit);
        if p < self.cfg.lr_p_threshold {
            warn!(p, threshold = self.cfg.lr_p_threshold, "consistency test failed");
            return None;
        }

        // Bootstrap projection.
        let current_score: u32 = self.q1_per_minute.iter().sum::<u32>()
            + self.q2_per_minute.iter().sum::<u32>();
        let consumed = 12.0 + self.cfg.q2_window_minutes as f64;
        let remaining = GAME_MINUTES - consumed;
        let (lo, _point, hi) = bootstrap_projected_total(
            &self.q1_per_minute,
            current_score,
            remaining,
            self.cfg.q1_scaling_factor,
            self.cfg.bootstrap_iters,
            0.05,
        );
        let projection = Projection::build(
            &q1_fit,
            current_score,
            remaining,
            self.cfg.q1_scaling_factor,
            (lo, hi),
        );
        info!(?projection, line = %self.line, "projection built");

        // Edge eval against book.
        let line_f = self.line.to_string().parse::<f64>().unwrap_or(0.0);
        let p_over = projection.p_over(line_f);

        match evaluate_edge(&book, p_over, self.cfg.min_edge, self.cfg.max_book_age_ms) {
            EdgeDecision::Trade(intent) => Some(intent),
            EdgeDecision::Skip(reason) => {
                warn!(reason, "skipping trade");
                None
            }
        }
    }
}

/// Convert (period, game_clock_seconds) into a 0-indexed elapsed minute within
/// that period. Returns None for invalid periods.
fn minute_index(period: u8, game_clock_seconds: u32) -> Option<usize> {
    if !(1..=4).contains(&period) { return None; }
    let elapsed = 12u32 * 60 - game_clock_seconds.min(12 * 60);
    Some((elapsed / 60) as usize)
}
