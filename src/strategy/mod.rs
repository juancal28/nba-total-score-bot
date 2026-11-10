pub mod edge;
pub mod sizing;

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
use crate::strategy::sizing::SizingParams;

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

/// Outcome of evaluating a game at the decision point. The model outputs are
/// filled in even when a gate blocks the trade, so skipped games can still be
/// scored against what actually happened.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub intent: Option<TradeIntent>,
    /// Why no trade was made; `None` when `intent` is `Some`.
    pub skip_reason: Option<String>,
    pub projection: Projection,
    pub p_over: f64,
    pub coverage: f64,
    pub q1_margin: i32,
    /// p-value of the Q1 vs Q2 rate consistency test.
    pub lr_p: f64,
}

pub struct StrategyEngine {
    pub cfg: StrategyConfig,
    pub sizing: SizingParams,
    pub line: Decimal,
    pub phase: Phase,
    pub q1_per_minute: Vec<u32>,
    pub q2_per_minute: Vec<u32>,
    pub coverage: CoverageTracker,
    pub q1_home: u32,
    pub q1_away: u32,
}

impl StrategyEngine {
    pub fn new(cfg: StrategyConfig, sizing: SizingParams, line: Decimal) -> Self {
        let q2_window = cfg.q2_window_minutes as usize;
        Self {
            cfg,
            sizing,
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
                match self.evaluate(snapshot).intent {
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

    pub fn handle_event(&mut self, ev: &GameEvent) {
        match ev {
            GameEvent::Score { period, game_clock_seconds, team, points, home_score, away_score, .. } => {
                let minute = minute_index(*period, *game_clock_seconds);
                match (*period, minute) {
                    (1, Some(m)) if m < self.q1_per_minute.len() => {
                        // A scoring play is proof the feed covered this minute.
                        self.coverage.observe(m, 60);
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

    /// Run the gates, the model, and the edge check against `book`. Gates are
    /// checked in order (coverage, blowout, consistency, edge) and the first
    /// failure is the skip reason.
    pub fn evaluate(&self, book: Arc<OrderBookSnapshot>) -> Evaluation {
        let mut skip_reason: Option<String> = None;

        // Coverage gate.
        let coverage = self.coverage.coverage_ratio();
        if coverage < self.cfg.min_coverage_ratio {
            warn!(coverage, required = self.cfg.min_coverage_ratio, "coverage gate failed");
            skip_reason.get_or_insert_with(|| "coverage gate failed".into());
        }

        // Blowout gate.
        let q1_margin = (self.q1_home as i32 - self.q1_away as i32).abs();
        if q1_margin > self.cfg.max_q1_margin {
            warn!(margin = q1_margin, max = self.cfg.max_q1_margin, "blowout gate failed");
            skip_reason.get_or_insert_with(|| "blowout gate failed".into());
        }

        // Poisson fit and LR consistency test.
        let q1_fit = PoissonFit::fit(&self.q1_per_minute);
        let q2_fit = PoissonFit::fit(&self.q2_per_minute);
        let lr_p = lr_test_two_sample(&q1_fit, &q2_fit);
        if lr_p < self.cfg.lr_p_threshold {
            warn!(p = lr_p, threshold = self.cfg.lr_p_threshold, "consistency test failed");
            skip_reason.get_or_insert_with(|| "consistency test failed".into());
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

        let mut intent = None;
        if skip_reason.is_none() {
            match evaluate_edge(
                &book,
                p_over,
                projection.ci_high - projection.ci_low,
                self.cfg.min_edge,
                self.cfg.max_book_age_ms,
                &self.sizing,
            ) {
                EdgeDecision::Trade(t) => intent = Some(t),
                EdgeDecision::Skip(reason) => {
                    warn!(reason, "skipping trade");
                    skip_reason = Some(reason.into());
                }
            }
        }

        Evaluation { intent, skip_reason, projection, p_over, coverage, q1_margin, lr_p }
    }
}

/// Convert (period, game_clock_seconds) into a 0-indexed elapsed minute within
/// that period. Returns None for invalid periods.
fn minute_index(period: u8, game_clock_seconds: u32) -> Option<usize> {
    if !(1..=4).contains(&period) { return None; }
    let elapsed = 12u32 * 60 - game_clock_seconds.min(12 * 60);
    Some((elapsed / 60) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::sportradar::{PbpCursor, parse_pbp};
    use jiff::Timestamp;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sportradar_pbp_okc_hou.json");

    fn engine() -> StrategyEngine {
        let cfg = StrategyConfig {
            q2_window_minutes: 2,
            lr_p_threshold: 0.05,
            bootstrap_iters: 200,
            q1_scaling_factor: 0.97,
            min_coverage_ratio: 0.95,
            min_edge: 0.05,
            max_q1_margin: 12,
            max_book_age_ms: 500,
        };
        let sizing = SizingParams {
            bankroll_usdc: 500.0,
            kelly_fraction: 0.25,
            ci_ref_width: 60.0,
            min_order_usdc: 1.0,
            max_order_usdc: 100.0,
        };
        StrategyEngine::new(cfg, sizing, Decimal::new(2245, 1))
    }

    fn score(period: u8, clock: u32, team: Team, points: u8) -> GameEvent {
        GameEvent::Score {
            at: Timestamp::UNIX_EPOCH,
            period,
            game_clock_seconds: clock,
            team,
            points,
            home_score: 0,
            away_score: 0,
        }
    }

    /// Feed events until the engine is ready to evaluate.
    fn drive(engine: &mut StrategyEngine, events: &[GameEvent]) {
        for ev in events {
            engine.handle_event(ev);
            if engine.phase == Phase::EdgeEvaluation {
                break;
            }
        }
    }

    #[test]
    fn real_game_reaches_evaluation_with_correct_q1_state() {
        let events = parse_pbp(FIXTURE, &mut PbpCursor::default()).unwrap();
        let mut e = engine();
        drive(&mut e, &events);
        assert_eq!(e.phase, Phase::EdgeEvaluation);
        assert_eq!((e.q1_home, e.q1_away), (27, 30));
        assert_eq!(e.q1_per_minute, [5, 4, 5, 5, 6, 7, 8, 6, 1, 2, 8, 0]);
        assert_eq!(e.coverage.coverage_ratio(), 1.0);
    }

    #[test]
    fn minute_with_only_scoring_events_counts_as_observed() {
        let mut e = engine();
        for m in 0..12u32 {
            e.handle_event(&score(1, 720 - m * 60 - 30, Team::Home, 2));
        }
        assert_eq!(e.coverage.coverage_ratio(), 1.0);
    }

    #[test]
    fn blowout_is_skipped_but_still_projected() {
        let mut e = engine();
        let mut events: Vec<GameEvent> =
            (0..12u32).map(|m| score(1, 720 - m * 60 - 30, Team::Home, 3)).collect();
        events.push(GameEvent::PeriodEnd { at: Timestamp::UNIX_EPOCH, period: 1 });
        events.push(GameEvent::ClockUpdate { at: Timestamp::UNIX_EPOCH, period: 2, game_clock_seconds: 590 });
        drive(&mut e, &events);
        assert_eq!(e.phase, Phase::EdgeEvaluation);

        let eval = e.evaluate(Arc::new(OrderBookSnapshot::empty("over".into())));
        assert!(eval.intent.is_none());
        assert!(eval.skip_reason.unwrap().contains("blowout"));
        assert_eq!(eval.q1_margin, 36);
        assert!(eval.projection.projected_total > 36.0);
    }
}
