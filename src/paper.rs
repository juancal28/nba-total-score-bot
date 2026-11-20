//! Paper trading: watch a day's NBA games and log the trade the bot would
//! make in each, without a wallet and without submitting anything.
//!
//! Per game the play-by-play is polled sparingly (the SportRadar trial allows
//! 1,000 requests per 30 days), the events are fed to the same
//! `StrategyEngine` the live bot uses, and at the decision point the order is
//! sized against the real Polymarket book fetched over public REST. One JSON
//! line per game is appended to `data/paper/decisions.jsonl`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use jiff::{SignedDuration, Timestamp};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{info, warn};

use crate::config::Config;
use crate::feed::GameEvent;
use crate::feed::sportradar::{PbpCursor, parse_pbp};
use crate::polymarket::book::OrderBookSnapshot;
use crate::polymarket::public::{TotalsMarket, event_slug, fetch_book, fetch_main_totals_market};
use crate::strategy::sizing::SizingParams;
use crate::strategy::{BuyOutcome, Evaluation, Phase, StrategyEngine, TradeIntent};

pub const DECISIONS_PATH: &str = "data/paper/decisions.jsonl";
/// Minimum spacing between SportRadar requests (trial limit is 1 per second).
const SR_SPACING: Duration = Duration::from_millis(1100);
/// First poll this long after the scheduled start; tipoff runs late.
const FIRST_POLL_AFTER: SignedDuration = SignedDuration::from_mins(12);
/// Stop watching a game that has not reached the decision point by then.
const GIVE_UP_AFTER: SignedDuration = SignedDuration::from_hours(3);
const MIN_POLL_DELAY: Duration = Duration::from_secs(45);
const MAX_POLL_DELAY: Duration = Duration::from_secs(600);

pub struct PaperArgs {
    /// Game date (US Eastern), `YYYY-MM-DD`. Defaults to today.
    pub date: Option<String>,
    /// Restrict to these SportRadar game ids. Empty = every game that day.
    pub games: Vec<String>,
    /// Hard cap on SportRadar requests this run (the trial budget is scarce).
    pub max_requests: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledGame {
    pub id: String,
    pub status: String,
    pub scheduled: Timestamp,
    pub home: TeamRef,
    pub away: TeamRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamRef {
    pub alias: String,
}

#[derive(Debug, Deserialize)]
struct DailySchedule {
    #[serde(default)]
    games: Vec<ScheduledGame>,
}

pub fn parse_daily_schedule(body: &[u8]) -> Result<Vec<ScheduledGame>> {
    Ok(serde_json::from_slice::<DailySchedule>(body)?.games)
}

/// One line of `decisions.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionRecord {
    pub game_id: String,
    pub away: String,
    pub home: String,
    pub scheduled: Timestamp,
    pub decided_at: Timestamp,
    pub slug: String,
    pub question: Option<String>,
    pub line: Option<String>,
    pub over_token_id: Option<String>,
    pub under_token_id: Option<String>,
    pub taker_base_fee: Option<f64>,
    pub fees_enabled: Option<bool>,
    pub traded: bool,
    pub skip_reason: Option<String>,
    pub side: Option<String>,
    pub size_usdc: Option<String>,
    pub vwap: Option<String>,
    pub best_bid: Option<String>,
    pub best_ask: Option<String>,
    pub projected_total: Option<f64>,
    pub ci_low: Option<f64>,
    pub ci_high: Option<f64>,
    pub p_over: Option<f64>,
    pub coverage: Option<f64>,
    pub q1_margin: Option<i32>,
    pub lr_p: Option<f64>,
    pub q1_per_minute: Vec<u32>,
    pub q2_per_minute: Vec<u32>,
    pub sportradar_requests: u32,
    /// Which projection made the decision above: `"blended"`, or `None` for
    /// the pure pace extrapolation (every record before the blend existed).
    #[serde(default)]
    pub model: Option<String>,
    /// What the pure pace model would have done with the same game and the
    /// same book. Present only when the blended model made the decision.
    #[serde(default)]
    pub pace_only: Option<ModelDecision>,
    /// The same decision sized flat (the backtest's `flat_10` variant), so
    /// paper results compare flat and Kelly on identical games and books.
    #[serde(default)]
    pub flat_traded: bool,
    #[serde(default)]
    pub flat_side: Option<String>,
    #[serde(default)]
    pub flat_size_usdc: Option<String>,
    #[serde(default)]
    pub flat_vwap: Option<String>,
    #[serde(default)]
    pub flat_skip_reason: Option<String>,
    /// Real (wall-clock) time of the play that reached the decision point.
    /// The book is fetched at `decided_at`; a large gap means the price
    /// already reflects later scoring and the decision is not a fair test.
    #[serde(default)]
    pub decision_point_at: Option<Timestamp>,
    #[serde(default)]
    pub decision_lag_secs: Option<i64>,
}

/// A second model's decision on a game, recorded for comparison.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelDecision {
    pub projected_total: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub p_over: f64,
    pub traded: bool,
    pub skip_reason: Option<String>,
    pub side: Option<String>,
    pub size_usdc: Option<String>,
    pub vwap: Option<String>,
    /// The same model's decision sized flat $10.
    #[serde(default)]
    pub flat_traded: bool,
    #[serde(default)]
    pub flat_side: Option<String>,
    #[serde(default)]
    pub flat_size_usdc: Option<String>,
    #[serde(default)]
    pub flat_vwap: Option<String>,
    #[serde(default)]
    pub flat_skip_reason: Option<String>,
}

/// Side, stake and price limit of a trade as the record stores them.
fn trade_fields(intent: &Option<TradeIntent>) -> (Option<String>, Option<String>, Option<String>) {
    match intent {
        Some(t) => (
            Some(
                match t.buy_side {
                    BuyOutcome::Over => "over",
                    BuyOutcome::Under => "under",
                }
                .to_string(),
            ),
            Some(t.size_usdc.to_string()),
            Some(t.max_vwap.to_string()),
        ),
        None => (None, None, None),
    }
}

impl DecisionRecord {
    fn new(game: &ScheduledGame, slug: &str) -> Self {
        Self {
            game_id: game.id.clone(),
            away: game.away.alias.clone(),
            home: game.home.alias.clone(),
            scheduled: game.scheduled,
            decided_at: Timestamp::now(),
            slug: slug.to_string(),
            question: None,
            line: None,
            over_token_id: None,
            under_token_id: None,
            taker_base_fee: None,
            fees_enabled: None,
            traded: false,
            skip_reason: None,
            side: None,
            size_usdc: None,
            vwap: None,
            best_bid: None,
            best_ask: None,
            projected_total: None,
            ci_low: None,
            ci_high: None,
            p_over: None,
            coverage: None,
            q1_margin: None,
            lr_p: None,
            q1_per_minute: vec![],
            q2_per_minute: vec![],
            sportradar_requests: 0,
            model: None,
            pace_only: None,
            flat_traded: false,
            flat_side: None,
            flat_size_usdc: None,
            flat_vwap: None,
            flat_skip_reason: None,
            decision_point_at: None,
            decision_lag_secs: None,
        }
    }

    fn skipped(mut self, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        // Skipped before any sizing ran, so flat skips for the same reason.
        if !self.flat_traded {
            self.flat_skip_reason = Some(reason.clone());
        }
        self.skip_reason = Some(reason);
        self
    }

    fn with_market(mut self, m: &TotalsMarket) -> Self {
        self.question = Some(m.question.clone());
        self.line = Some(m.line.to_string());
        self.over_token_id = Some(m.over_token_id.clone());
        self.under_token_id = Some(m.under_token_id.clone());
        self.taker_base_fee = m.taker_base_fee;
        self.fees_enabled = Some(m.fees_enabled);
        self
    }

    /// Whether the book was fetched too long after the decision point for
    /// the trade to be a fair test (see `LATE_DECISION_SECS`).
    pub fn is_late(&self) -> bool {
        self.decision_lag_secs.is_some_and(|lag| lag > LATE_DECISION_SECS)
    }

    /// Stamp when the decision point was reached and, if the book came too
    /// late after it (bot started mid-game, or the machine slept), void every
    /// trade. Model outputs and prices are kept: the projection is still a
    /// valid data point for model accuracy, only the P&L would be unfair.
    fn at_decision_point(mut self, at: Option<Timestamp>) -> Self {
        self.decision_point_at = at;
        self.decision_lag_secs = at.map(|t| self.decided_at.duration_since(t).as_secs());
        if !self.is_late() {
            return self;
        }
        let reason = format!(
            "late: book fetched {} s after the decision point (limit {LATE_DECISION_SECS} s)",
            self.decision_lag_secs.unwrap_or_default()
        );
        (self.traded, self.side, self.size_usdc, self.vwap) = (false, None, None, None);
        self.skip_reason = Some(reason.clone());
        (self.flat_traded, self.flat_side, self.flat_size_usdc, self.flat_vwap) = (false, None, None, None);
        self.flat_skip_reason = Some(reason.clone());
        if let Some(p) = self.pace_only.as_mut() {
            (p.traded, p.side, p.size_usdc, p.vwap) = (false, None, None, None);
            p.skip_reason = Some(reason.clone());
            (p.flat_traded, p.flat_side, p.flat_size_usdc, p.flat_vwap) = (false, None, None, None);
            p.flat_skip_reason = Some(reason);
        }
        self
    }

    fn with_engine(mut self, engine: &StrategyEngine) -> Self {
        self.q1_per_minute = engine.q1_per_minute.clone();
        self.q2_per_minute = engine.q2_per_minute.clone();
        self
    }
}

/// Evaluate `engine` (already at the decision point) against the market's
/// OVER book and describe the outcome.
pub fn decide(
    record: DecisionRecord,
    engine: &mut StrategyEngine,
    market: &TotalsMarket,
    book: OrderBookSnapshot,
) -> DecisionRecord {
    engine.line = market.line;
    let mut record = record.with_market(market).with_engine(engine);
    // The record was created when the game started being watched.
    record.decided_at = Timestamp::now();
    record.best_bid = book.best_bid().map(|l| l.price.to_string());
    record.best_ask = book.best_ask().map(|l| l.price.to_string());

    let book = Arc::new(book);
    // Every evaluation below must see the same bootstrap CI, so fix the seed
    // for this decision if the engine has none.
    if engine.bootstrap_seed.is_none() {
        engine.bootstrap_seed = Some(rand::random());
    }
    // With the blend on, also ask the pure pace model about the same game
    // and book. It needs no extra data, so the comparison costs no requests.
    if let Some(blend) = engine.cfg.blend.take() {
        let pace = engine.evaluate(book.clone());
        let pace_flat = evaluate_flat(engine, book.clone());
        engine.cfg.blend = Some(blend);
        let (side, size_usdc, vwap) = trade_fields(&pace.intent);
        let (flat_side, flat_size_usdc, flat_vwap) = trade_fields(&pace_flat.intent);
        record.model = Some("blended".to_string());
        record.pace_only = Some(ModelDecision {
            projected_total: pace.projection.projected_total,
            ci_low: pace.projection.ci_low,
            ci_high: pace.projection.ci_high,
            p_over: pace.p_over,
            traded: pace.intent.is_some(),
            skip_reason: pace.skip_reason,
            side,
            size_usdc,
            vwap,
            flat_traded: pace_flat.intent.is_some(),
            flat_side,
            flat_size_usdc,
            flat_vwap,
            flat_skip_reason: pace_flat.skip_reason,
        });
    }

    let flat = evaluate_flat(engine, book.clone());
    record.flat_traded = flat.intent.is_some();
    (record.flat_side, record.flat_size_usdc, record.flat_vwap) = trade_fields(&flat.intent);
    record.flat_skip_reason = flat.skip_reason;

    let Evaluation { intent, skip_reason, projection, p_over, coverage, q1_margin, lr_p } =
        engine.evaluate(book);
    record.projected_total = Some(projection.projected_total);
    record.ci_low = Some(projection.ci_low);
    record.ci_high = Some(projection.ci_high);
    record.p_over = Some(p_over);
    record.coverage = Some(coverage);
    record.q1_margin = Some(q1_margin);
    record.lr_p = Some(lr_p);
    record.skip_reason = skip_reason;
    record.traded = intent.is_some();
    (record.side, record.size_usdc, record.vwap) = trade_fields(&intent);
    record
}

/// Flat stake used for the paper comparison (matches the backtest's `flat_10`).
pub const FLAT_STAKE_USDC: f64 = 10.0;

/// Evaluate with flat sizing: an effectively infinite bankroll makes Kelly
/// exceed the cap whenever there is any edge, so the stake is always
/// `FLAT_STAKE_USDC` (same trick as the backtest). Restores the sizing.
fn evaluate_flat(engine: &mut StrategyEngine, book: Arc<OrderBookSnapshot>) -> Evaluation {
    let kelly = engine.sizing.clone();
    engine.sizing = SizingParams {
        bankroll_usdc: 1e12,
        kelly_fraction: 1.0,
        ci_ref_width: f64::MAX,
        min_order_usdc: kelly.min_order_usdc,
        max_order_usdc: FLAT_STAKE_USDC,
    };
    let eval = engine.evaluate(book);
    engine.sizing = kelly;
    eval
}

/// How long to wait before the next play-by-play poll, given the game clock
/// at the last one. Aims at the end of the Q2 observation window, assuming
/// real time runs at least 1.5x game time, so polls bunch up near the
/// decision point instead of being spread evenly.
pub fn next_poll_delay(period: u8, game_clock_seconds: u32, q2_window_minutes: u32) -> Duration {
    let window = q2_window_minutes * 60;
    let game_seconds_left = match period {
        0 | 1 => game_clock_seconds + window,
        2 => window.saturating_sub(720u32.saturating_sub(game_clock_seconds)),
        _ => 0,
    };
    Duration::from_secs(game_seconds_left as u64 * 3 / 2).clamp(MIN_POLL_DELAY, MAX_POLL_DELAY)
}

/// Rate-limited SportRadar client that counts its requests.
pub(crate) struct SportRadar {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
    next_slot: Mutex<Instant>,
    requests: AtomicU32,
    max_requests: Option<u32>,
}

impl SportRadar {
    pub(crate) fn new(http: reqwest::Client, base_url: String, api_key: SecretString) -> Self {
        Self {
            http,
            base_url,
            api_key,
            next_slot: Mutex::new(Instant::now()),
            requests: AtomicU32::new(0),
            max_requests: None,
        }
    }

    /// Refuse to send more than `max` requests.
    pub(crate) fn with_limit(mut self, max: Option<u32>) -> Self {
        self.max_requests = max;
        self
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.max_requests.is_some_and(|max| self.requests() >= max)
    }

    pub(crate) fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Vec<u8>> {
        {
            let mut slot = self.next_slot.lock().await;
            sleep_until(*slot).await;
            *slot = Instant::now() + SR_SPACING;
            // Checked under the lock so concurrent games cannot overshoot.
            if self.exhausted() {
                return Err(anyhow!("sportradar request budget exhausted"));
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
        }
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .header("x-api-key", self.api_key.expose_secret())
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }
}

pub async fn run(args: PaperArgs) -> Result<()> {
    let cfg = Config::load()?;
    let api_key = cfg
        .sportradar
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("NPB_SPORTRADAR__API_KEY is not set"))?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let sr = Arc::new(
        SportRadar::new(http.clone(), cfg.sportradar.base_url.clone(), api_key).with_limit(args.max_requests),
    );

    let date = match args.date {
        Some(d) => d,
        // US Eastern calendar date; see `event_slug` for the fixed offset.
        None => (Timestamp::now() - SignedDuration::from_hours(5)).to_string()[..10].to_string(),
    };
    let path = format!("/games/{}/schedule.json", date.replace('-', "/"));
    let games = parse_daily_schedule(&sr.get(&path).await.context("fetch daily schedule")?)?;
    let games: Vec<ScheduledGame> = games
        .into_iter()
        .filter(|g| args.games.is_empty() || args.games.contains(&g.id))
        .collect();
    info!(%date, games = games.len(), "paper trading started");

    let mut tasks = Vec::new();
    for game in games {
        let (sr, http, cfg) = (sr.clone(), http.clone(), cfg.clone());
        tasks.push(tokio::spawn(async move {
            let label = format!("{}@{}", game.away.alias, game.home.alias);
            let mut record = watch_game(&sr, &http, &cfg, &game).await;
            record.sportradar_requests = sr.requests.load(Ordering::Relaxed);
            info!(
                game = %label,
                traded = record.traded,
                side = ?record.side,
                size_usdc = ?record.size_usdc,
                skip_reason = ?record.skip_reason,
                p_over = ?record.p_over,
                line = ?record.line,
                "paper decision"
            );
            if let Err(e) = append_record(Path::new(DECISIONS_PATH), &record) {
                warn!(game = %label, error = ?e, "failed to write decision");
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    info!(sportradar_requests = sr.requests.load(Ordering::Relaxed), "paper trading finished");
    Ok(())
}

async fn watch_game(
    sr: &SportRadar,
    http: &reqwest::Client,
    cfg: &Config,
    game: &ScheduledGame,
) -> DecisionRecord {
    let slug = event_slug(&game.away.alias, &game.home.alias, game.scheduled);
    let record = DecisionRecord::new(game, &slug);
    let label = format!("{}@{}", game.away.alias, game.home.alias);

    if matches!(game.status.as_str(), "closed" | "complete" | "postponed" | "cancelled" | "unnecessary") {
        return record.skipped(format!("game status is {}", game.status));
    }
    match fetch_main_totals_market(http, &slug).await {
        Ok(Some(m)) => info!(game = %label, line = %m.line, "found polymarket totals market"),
        Ok(None) => return record.skipped("no polymarket totals market"),
        Err(e) => warn!(game = %label, error = ?e, "market lookup failed; will retry at decision"),
    }

    let sizing = SizingParams {
        bankroll_usdc: cfg.sizing.bankroll_usdc.to_f64().unwrap_or(0.0),
        kelly_fraction: cfg.sizing.kelly_fraction,
        ci_ref_width: cfg.sizing.ci_ref_width,
        min_order_usdc: cfg.sizing.min_order_usdc.to_f64().unwrap_or(0.0),
        max_order_usdc: cfg.risk.max_position_usdc.to_f64().unwrap_or(0.0),
    };
    let mut engine = StrategyEngine::new(cfg.strategy.clone(), sizing, Decimal::ZERO);
    let mut cursor = PbpCursor::default();
    let pbp_path = format!("/games/{}/pbp.json", game.id);
    let deadline = game.scheduled + GIVE_UP_AFTER;

    sleep_until_wall(game.scheduled + FIRST_POLL_AFTER).await;
    // (period, clock) at the newest event seen so far, for pacing the next
    // poll. Kept across polls: one that returns nothing new (a timeout, a
    // quiet stretch) must not reset the pacing to "game not started".
    let mut position = (0u8, 720u32);
    let mut decision_point_at: Option<Timestamp> = None;
    loop {
        if Timestamp::now() > deadline {
            return record.with_engine(&engine).skipped("game did not reach the decision point in time");
        }
        match sr.get(&pbp_path).await.and_then(|body| parse_pbp(&body, &mut cursor)) {
            Ok(events) => {
                for ev in &events {
                    engine.handle_event(ev);
                    if let GameEvent::Score { period, game_clock_seconds, .. }
                    | GameEvent::ClockUpdate { period, game_clock_seconds, .. } = ev
                    {
                        position = (*period, *game_clock_seconds);
                    }
                    if matches!(engine.phase, Phase::EdgeEvaluation | Phase::Done) {
                        decision_point_at = event_time(ev);
                        break;
                    }
                }
                info!(game = %label, period = position.0, clock = position.1, new_events = events.len(), "polled");
            }
            Err(_) if sr.exhausted() => {
                return record.with_engine(&engine).skipped("sportradar request budget exhausted");
            }
            Err(e) => {
                warn!(game = %label, error = ?e, "play-by-play poll failed");
                sleep_wall(Duration::from_secs(60)).await;
                continue;
            }
        }

        match engine.phase {
            Phase::EdgeEvaluation => break,
            Phase::Done => return record.with_engine(&engine).skipped("sample invalidated"),
            _ => sleep_wall(next_poll_delay(position.0, position.1, cfg.strategy.q2_window_minutes)).await,
        }
    }

    let market = match fetch_main_totals_market(http, &slug).await {
        Ok(Some(m)) => m,
        Ok(None) => return record.with_engine(&engine).skipped("no polymarket totals market"),
        Err(e) => return record.with_engine(&engine).skipped(format!("market lookup failed: {e}")),
    };
    match fetch_book(http, &market.over_token_id).await {
        Ok(book) => {
            let r = decide(record, &mut engine, &market, book).at_decision_point(decision_point_at);
            if r.is_late() {
                warn!(game = %label, lag_secs = ?r.decision_lag_secs, "book fetched too late; trades voided");
            }
            r
        }
        Err(e) => {
            record.with_market(&market).with_engine(&engine).skipped(format!("order book unavailable: {e}"))
        }
    }
}

/// A book fetched later than this after the decision point already prices in
/// scoring the model never saw, so trades decided on it are voided.
pub const LATE_DECISION_SECS: i64 = 180;

/// Longest single timer in `sleep_until_wall`.
const WALL_CHECK: Duration = Duration::from_secs(15);

fn event_time(ev: &GameEvent) -> Option<Timestamp> {
    match ev {
        GameEvent::Tipoff { at }
        | GameEvent::Score { at, .. }
        | GameEvent::ClockUpdate { at, .. }
        | GameEvent::PeriodEnd { at, .. }
        | GameEvent::RosterChange { at, .. }
        | GameEvent::FeedGap { at, .. } => Some(*at),
    }
}

fn until(t: Timestamp) -> Duration {
    let left = t.duration_since(Timestamp::now());
    // jiff 0.1's `TryFrom<SignedDuration> for Duration` panics (rather than
    // erroring) on some negative values, so never hand it one.
    if left.is_negative() || left.is_zero() {
        return Duration::ZERO;
    }
    Duration::try_from(left).unwrap_or(Duration::ZERO)
}

/// Sleep until wall-clock time `t`.
///
/// Tokio timers run on the monotonic clock, which on macOS stops while the
/// machine sleeps, so one long `sleep` overshoots by however long the
/// laptop was asleep. Sleeping in short steps and re-reading the wall clock
/// each time means we wake at most `WALL_CHECK` after `t`, or right after
/// the machine resumes if it slept through `t`.
async fn sleep_until_wall(t: Timestamp) {
    loop {
        let left = until(t);
        if left.is_zero() {
            return;
        }
        sleep(left.min(WALL_CHECK)).await;
    }
}

async fn sleep_wall(d: Duration) {
    let d = SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX);
    sleep_until_wall(Timestamp::now().saturating_add(d)).await;
}

fn append_record(path: &Path, record: &DecisionRecord) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyConfig;
    use crate::polymarket::public::parse_book;
    use std::str::FromStr;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sportradar_pbp_okc_hou.json");

    fn game() -> ScheduledGame {
        let body = br#"{"date":"2025-10-21","games":[{"id":"820c5325","status":"inprogress",
            "scheduled":"2025-10-21T23:30:00+00:00","home":{"alias":"OKC","name":"Thunder"},
            "away":{"alias":"HOU","name":"Rockets"}}]}"#;
        parse_daily_schedule(body).unwrap().remove(0)
    }

    fn market() -> TotalsMarket {
        TotalsMarket {
            slug: "nba-hou-okc-2025-10-21".into(),
            question: "Rockets vs. Thunder: O/U 224.5".into(),
            line: Decimal::from_str("224.5").unwrap(),
            over_token_id: "21".into(),
            under_token_id: "22".into(),
            volume: 1000.0,
            taker_base_fee: Some(1000.0),
            fees_enabled: true,
            closed: false,
            over_won: None,
        }
    }

    /// Engine driven through the real game to its decision point.
    fn engine_at_decision() -> StrategyEngine {
        let cfg = StrategyConfig {
            q2_window_minutes: 2,
            lr_p_threshold: 0.05,
            bootstrap_iters: 200,
            q1_scaling_factor: 0.97,
            min_coverage_ratio: 0.95,
            min_edge: 0.05,
            max_q1_margin: 12,
            max_book_age_ms: 500,
            blend: None,
        };
        let sizing = SizingParams {
            bankroll_usdc: 500.0,
            kelly_fraction: 0.25,
            ci_ref_width: 60.0,
            min_order_usdc: 1.0,
            max_order_usdc: 100.0,
        };
        let mut engine = StrategyEngine::new(cfg, sizing, Decimal::ZERO);
        for ev in parse_pbp(FIXTURE, &mut PbpCursor::default()).unwrap() {
            engine.handle_event(&ev);
            if engine.phase == Phase::EdgeEvaluation {
                break;
            }
        }
        assert_eq!(engine.phase, Phase::EdgeEvaluation);
        engine
    }

    fn book(bid: &str, ask: &str) -> OrderBookSnapshot {
        let body = format!(
            r#"{{"bids":[{{"price":"{bid}","size":"100000"}}],"asks":[{{"price":"{ask}","size":"100000"}}]}}"#
        );
        parse_book("21", body.as_bytes()).unwrap()
    }

    #[test]
    fn schedule_parses_ids_times_and_aliases() {
        let g = game();
        assert_eq!((g.away.alias.as_str(), g.home.alias.as_str()), ("HOU", "OKC"));
        assert_eq!(g.scheduled, "2025-10-21T23:30:00Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn poll_delay_is_capped_early_in_q1() {
        assert_eq!(next_poll_delay(1, 720, 2), Duration::from_secs(600));
    }

    #[test]
    fn poll_delay_tracks_remaining_game_time_late_in_q1() {
        // 1:00 left in Q1 + 2:00 window = 180 game seconds -> 270 s.
        assert_eq!(next_poll_delay(1, 60, 2), Duration::from_secs(270));
    }

    #[test]
    fn poll_delay_bottoms_out_near_the_decision_point() {
        // 10:30 left in Q2 with a 2-minute window: 30 game seconds to go.
        assert_eq!(next_poll_delay(2, 630, 2), Duration::from_secs(45));
    }

    #[test]
    fn decision_on_a_cheap_under_is_a_sized_under_trade() {
        // Q1 pace (57 in 12 min) projects far below a 224.5 line, and the
        // market still prices OVER at 0.90, so UNDER at 0.10 is a clear buy.
        let mut engine = engine_at_decision();
        let mut stale = DecisionRecord::new(&game(), "slug");
        stale.decided_at = stale.scheduled;
        let r = decide(stale, &mut engine, &market(), book("0.90", "0.91"));
        assert!(r.traded, "skip_reason: {:?}", r.skip_reason);
        assert_eq!(r.side.as_deref(), Some("under"));
        assert!(Decimal::from_str(r.size_usdc.as_deref().unwrap()).unwrap() >= Decimal::ONE);
        assert_eq!(r.line.as_deref(), Some("224.5"));
        // Stamped when decided, not when the 2025 fixture game was scheduled.
        assert!(r.decided_at > r.scheduled + SignedDuration::from_hours(24));
        assert_eq!(r.best_ask.as_deref(), Some("0.91"));
        assert_eq!(r.q1_per_minute.iter().sum::<u32>(), 57);
        assert!(r.p_over.unwrap() < 0.85);
        // Flat comparison takes the same side at a fixed $10.
        assert!(r.flat_traded, "flat_skip_reason: {:?}", r.flat_skip_reason);
        assert_eq!(r.flat_side.as_deref(), Some("under"));
        assert_eq!(Decimal::from_str(r.flat_size_usdc.as_deref().unwrap()).unwrap(), Decimal::TEN);
    }

    #[test]
    fn decision_without_edge_is_recorded_as_a_skip_with_model_outputs() {
        let mut engine = engine_at_decision();
        engine.line = market().line;
        let p_over = engine.evaluate(Arc::new(book("0.01", "0.99"))).p_over;
        // Quote the market right at the model's probability: no edge either way.
        let bid = format!("{:.2}", (p_over - 0.01).clamp(0.01, 0.98));
        let ask = format!("{:.2}", (p_over + 0.01).clamp(0.02, 0.99));
        let r = decide(DecisionRecord::new(&game(), "slug"), &mut engine, &market(), book(&bid, &ask));
        assert!(!r.traded);
        assert!(r.skip_reason.is_some());
        assert!(r.projected_total.is_some() && r.side.is_none());
        assert!(!r.flat_traded && r.flat_skip_reason.is_some());
    }

    /// A blended decision with trades for every model and sizing.
    fn traded_blended_record() -> DecisionRecord {
        let mut engine = engine_at_decision();
        engine.cfg.blend = Some(crate::config::BlendConfig {
            intercept: 27.5,
            pace_weight: 0.286,
            line_weight: 0.597,
            sigma: 16.6,
        });
        let r = decide(DecisionRecord::new(&game(), "slug"), &mut engine, &market(), book("0.90", "0.91"));
        assert!(r.pace_only.as_ref().unwrap().traded && r.pace_only.as_ref().unwrap().flat_traded);
        r
    }

    #[test]
    fn late_book_voids_every_trade_but_keeps_the_model_outputs() {
        let r = traded_blended_record();
        let (projection, p_over) = (r.projected_total, r.p_over);
        let late = r.decided_at - SignedDuration::from_secs(LATE_DECISION_SECS + 1);
        let r = r.at_decision_point(Some(late));
        assert!(r.is_late());
        assert_eq!(r.decision_lag_secs, Some(LATE_DECISION_SECS + 1));
        assert!(!r.traded && r.side.is_none() && r.size_usdc.is_none());
        assert!(r.skip_reason.as_deref().unwrap().starts_with("late:"));
        assert!(!r.flat_traded && r.flat_skip_reason.as_deref().unwrap().starts_with("late:"));
        let p = r.pace_only.as_ref().unwrap();
        assert!(!p.traded && !p.flat_traded && p.skip_reason.as_deref().unwrap().starts_with("late:"));
        assert_eq!((r.projected_total, r.p_over), (projection, p_over));
        assert!(r.best_ask.is_some());
    }

    #[test]
    fn prompt_book_keeps_the_trades() {
        let r = traded_blended_record();
        let on_time = r.decided_at - SignedDuration::from_secs(LATE_DECISION_SECS);
        let r = r.at_decision_point(Some(on_time));
        assert!(!r.is_late());
        assert!(r.traded && r.flat_traded && r.pace_only.as_ref().unwrap().traded);
    }

    #[test]
    fn skip_before_sizing_applies_to_flat_too() {
        let r = DecisionRecord::new(&game(), "slug").skipped("no polymarket totals market");
        assert_eq!(r.flat_skip_reason.as_deref(), Some("no polymarket totals market"));
    }

    #[tokio::test]
    async fn wall_sleep_returns_at_once_for_a_past_time() {
        // A laptop that slept through the wake-up time resumes with the
        // target already past: the poll must happen right away.
        let past = Timestamp::now() - SignedDuration::from_mins(5);
        tokio::time::timeout(Duration::from_millis(200), sleep_until_wall(past)).await.unwrap();
    }

    #[test]
    fn until_is_zero_for_any_past_time() {
        assert_eq!(until(Timestamp::now() - SignedDuration::from_millis(1500)), Duration::ZERO);
        assert_eq!(until(Timestamp::now() - SignedDuration::from_hours(2)), Duration::ZERO);
    }

    #[tokio::test]
    async fn wall_sleep_wakes_at_the_target_time() {
        let target = Timestamp::now() + SignedDuration::from_millis(300);
        sleep_until_wall(target).await;
        assert!(Timestamp::now() >= target);
    }

    #[test]
    fn blended_decision_also_records_what_the_pace_model_would_do() {
        // Same game and book as the cheap-under test. Pace alone projects far
        // under the line; the blend sits closer to it and so is less sure.
        let mut engine = engine_at_decision();
        engine.cfg.blend = Some(crate::config::BlendConfig {
            intercept: 27.5,
            pace_weight: 0.286,
            line_weight: 0.597,
            sigma: 16.6,
        });
        let r = decide(DecisionRecord::new(&game(), "slug"), &mut engine, &market(), book("0.90", "0.91"));
        assert_eq!(r.model.as_deref(), Some("blended"));
        assert!(engine.cfg.blend.is_some(), "blend must be restored after the comparison");

        let pace = r.pace_only.clone().expect("pace-only comparison recorded");
        assert!(pace.traded);
        assert_eq!(pace.side.as_deref(), Some("under"));
        assert!(pace.projected_total < r.projected_total.unwrap());
        assert!(pace.p_over < r.p_over.unwrap());
        assert!(pace.flat_traded);
        assert_eq!(pace.flat_size_usdc.as_deref().map(|s| Decimal::from_str(s).unwrap()), Some(Decimal::TEN));
        assert!(engine.sizing.max_order_usdc > 10.0, "Kelly sizing must be restored after the flat comparison");

        // An old-format line without the new fields still parses.
        let mut json: serde_json::Value = serde_json::to_value(&r).unwrap();
        json.as_object_mut().unwrap().remove("model");
        json.as_object_mut().unwrap().remove("pace_only");
        let old: DecisionRecord = serde_json::from_value(json).unwrap();
        assert!(old.model.is_none() && old.pace_only.is_none());
    }

    #[test]
    fn pace_only_decision_records_no_comparison() {
        let mut engine = engine_at_decision();
        let r = decide(DecisionRecord::new(&game(), "slug"), &mut engine, &market(), book("0.90", "0.91"));
        assert!(r.model.is_none() && r.pace_only.is_none());
    }

    #[test]
    fn record_round_trips_through_json() {
        let r = DecisionRecord::new(&game(), "nba-hou-okc-2025-10-21").skipped("no polymarket totals market");
        let back: DecisionRecord = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
