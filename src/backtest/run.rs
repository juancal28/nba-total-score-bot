//! Replays the cached sample through `StrategyEngine` and writes the
//! side-by-side report. Runs offline: it only reads files `fetch` cached.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use tracing::{info, warn};

use crate::backtest::fetch::{SampleGame, event_path, load_sample, pbp_path, prices_path};
use crate::config::Config;
use crate::feed::GameEvent;
use crate::feed::sportradar::{PbpCursor, parse_pbp, pbp_summary};
use crate::polymarket::book::{OrderBookSnapshot, PriceLevel};
use crate::polymarket::public::{main_totals_market, parse_price_history, price_at};
use crate::strategy::sizing::SizingParams;
use crate::strategy::{BuyOutcome, Phase, StrategyEngine};

/// A price older than this at decision time is treated as missing.
const MAX_PRICE_AGE_SECS: i64 = 300;
pub const START_BANKROLL: f64 = 500.0;

/// Assumed order book around the historical minute price (no depth history
/// exists): `levels` price levels each side, one cent apart, starting
/// `half_spread` from the mid, each holding `level_usdc` of notional.
#[derive(Debug, Clone, Copy)]
pub struct BookModel {
    pub half_spread: f64,
    pub levels: u32,
    pub level_usdc: f64,
}

pub const BOOK_MODEL: BookModel = BookModel { half_spread: 0.01, levels: 5, level_usdc: 500.0 };

pub fn synthetic_book(mid: f64, model: BookModel) -> OrderBookSnapshot {
    let level = |price: f64| -> Option<PriceLevel> {
        if !(0.01..=0.99).contains(&price) {
            return None;
        }
        Some(PriceLevel {
            price: Decimal::from_f64(price)?.round_dp(2),
            size: Decimal::from_f64(model.level_usdc / price)?.round_dp(2),
        })
    };
    let step = |i: u32| model.half_spread + 0.01 * i as f64;
    OrderBookSnapshot {
        token_id: "synthetic".into(),
        bids: (0..model.levels).filter_map(|i| level(mid - step(i))).collect(),
        asks: (0..model.levels).filter_map(|i| level(mid + step(i))).collect(),
        last_update: Instant::now(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cap {
    /// Per-bet cap as a fraction of the current bankroll.
    BankrollFraction(f64),
    Fixed(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Variant {
    pub name: &'static str,
    /// `None` = flat stake of `cap` dollars whenever the edge gate passes.
    pub kelly_fraction: Option<f64>,
    pub cap: Cap,
    /// Stop trading for the (US Eastern) day after losing this much.
    pub daily_loss_stop: Option<f64>,
}

pub const VARIANTS: [Variant; 5] = [
    Variant { name: "flat_10", kelly_fraction: None, cap: Cap::Fixed(10.0), daily_loss_stop: None },
    Variant { name: "kelly_quarter", kelly_fraction: Some(0.25), cap: Cap::BankrollFraction(0.2), daily_loss_stop: None },
    Variant { name: "kelly_half", kelly_fraction: Some(0.5), cap: Cap::BankrollFraction(0.2), daily_loss_stop: None },
    Variant { name: "kelly_full", kelly_fraction: Some(1.0), cap: Cap::BankrollFraction(0.2), daily_loss_stop: None },
    Variant { name: "kelly_quarter_live_limits", kelly_fraction: Some(0.25), cap: Cap::Fixed(100.0), daily_loss_stop: Some(50.0) },
];

impl Variant {
    pub fn sizing(&self, bankroll: f64, cfg: &Config) -> SizingParams {
        let min_order_usdc = cfg.sizing.min_order_usdc.to_f64().unwrap_or(1.0);
        let cap = match self.cap {
            Cap::BankrollFraction(f) => f * bankroll,
            Cap::Fixed(x) => x,
        }
        .min(bankroll);
        match self.kelly_fraction {
            Some(kelly_fraction) => SizingParams {
                bankroll_usdc: bankroll,
                kelly_fraction,
                ci_ref_width: cfg.sizing.ci_ref_width,
                min_order_usdc,
                max_order_usdc: cap,
            },
            // An effectively infinite bankroll makes Kelly exceed the cap
            // whenever there is any edge, so the stake is always the cap.
            None => SizingParams {
                bankroll_usdc: 1e12,
                kelly_fraction: 1.0,
                ci_ref_width: f64::MAX,
                min_order_usdc,
                max_order_usdc: cap,
            },
        }
    }
}

/// Model outputs and ground truth for one replayed game.
#[derive(Debug, Clone)]
pub struct GameReplay {
    pub game: SampleGame,
    pub line: f64,
    pub decision_at: Timestamp,
    /// Polymarket OVER price at the decision minute.
    pub market_price: f64,
    pub projected_total: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub p_over: f64,
    pub coverage: f64,
    pub q1_margin: i32,
    pub lr_p: f64,
    /// The coverage / blowout / consistency gate that failed, if any.
    pub gate_failed: Option<String>,
    pub regulation_total: u32,
    pub final_total: u32,
    pub overtime: bool,
    pub over_won: bool,
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub side: BuyOutcome,
    pub stake: f64,
    pub vwap: f64,
    pub pnl: f64,
}

/// Why a sampled game produced no `GameReplay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Excluded {
    NoPlayByPlay,
    NoMarket,
    Unresolved,
    NoDecisionPoint,
    NoPrice,
}

pub fn pnl(side: BuyOutcome, stake: f64, vwap: f64, over_won: bool) -> f64 {
    let won = matches!(side, BuyOutcome::Over) == over_won;
    if won { stake / vwap - stake } else { -stake }
}

/// Drive `engine` through a game's events to its decision point. Returns the
/// wall-clock time of the event that reached it, and the regulation total.
fn drive(engine: &mut StrategyEngine, events: &[GameEvent]) -> (Option<Timestamp>, u32) {
    let mut decision_at = None;
    let mut regulation_total = 0;
    for ev in events {
        if decision_at.is_none() && engine.phase != Phase::Done {
            engine.handle_event(ev);
            if engine.phase == Phase::EdgeEvaluation {
                decision_at = Some(match ev {
                    GameEvent::Tipoff { at }
                    | GameEvent::Score { at, .. }
                    | GameEvent::ClockUpdate { at, .. }
                    | GameEvent::PeriodEnd { at, .. }
                    | GameEvent::RosterChange { at, .. }
                    | GameEvent::FeedGap { at, .. } => *at,
                });
            }
        }
        if let GameEvent::Score { period, home_score, away_score, .. } = ev {
            if *period <= 4 {
                regulation_total = home_score + away_score;
            }
        }
    }
    (decision_at, regulation_total)
}

fn seed_for(game_id: &str) -> u64 {
    // FNV-1a: stable across runs and platforms, unlike `DefaultHasher`.
    game_id.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Replay one game. The returned engine sits at its decision point with the
/// market's line and a per-game bootstrap seed, ready for `evaluate`.
pub fn replay_game(game: &SampleGame, cfg: &Config) -> Result<Result<(GameReplay, StrategyEngine), Excluded>> {
    let Ok(pbp) = std::fs::read(pbp_path(&game.id)) else { return Ok(Err(Excluded::NoPlayByPlay)) };
    let market = std::fs::read(event_path(&game.slug))
        .ok()
        .and_then(|b| main_totals_market(&b).ok().flatten());
    let Some(market) = market else { return Ok(Err(Excluded::NoMarket)) };
    let Some(over_won) = market.over_won else { return Ok(Err(Excluded::Unresolved)) };

    let events = parse_pbp(&pbp, &mut PbpCursor::default()).with_context(|| format!("parse pbp {}", game.id))?;
    let summary = pbp_summary(&pbp)?;
    let sizing = VARIANTS[0].sizing(START_BANKROLL, cfg);
    let mut engine = StrategyEngine::new(cfg.strategy.clone(), sizing, market.line);
    engine.bootstrap_seed = Some(seed_for(&game.id));
    let (decision_at, regulation_total) = drive(&mut engine, &events);
    let Some(decision_at) = decision_at else { return Ok(Err(Excluded::NoDecisionPoint)) };

    let history = std::fs::read(prices_path(&market.over_token_id))
        .ok()
        .and_then(|b| parse_price_history(&b).ok())
        .unwrap_or_default();
    let Some(market_price) = price_at(&history, decision_at.as_second(), MAX_PRICE_AGE_SECS) else {
        return Ok(Err(Excluded::NoPrice));
    };

    let eval = engine.evaluate(Arc::new(synthetic_book(market_price, BOOK_MODEL)));
    let gate_failed = eval
        .skip_reason
        .filter(|r| r.ends_with("gate failed") || r.starts_with("consistency"));
    let final_total = summary.home_points + summary.away_points;
    Ok(Ok((
        GameReplay {
            game: game.clone(),
            line: market.line.to_f64().unwrap_or(0.0),
            decision_at,
            market_price,
            projected_total: eval.projection.projected_total,
            ci_low: eval.projection.ci_low,
            ci_high: eval.projection.ci_high,
            p_over: eval.p_over,
            coverage: eval.coverage,
            q1_margin: eval.q1_margin,
            lr_p: eval.lr_p,
            gate_failed,
            regulation_total,
            final_total,
            overtime: final_total != regulation_total,
            over_won,
        },
        engine,
    )))
}

/// Bankroll path of one variant over the chronologically ordered games.
#[derive(Debug, Clone)]
pub struct VariantResult {
    pub variant: Variant,
    /// One entry per replayed game: the trade, or why there was none.
    pub outcomes: Vec<Result<Trade, String>>,
    pub final_bankroll: f64,
    pub max_drawdown: f64,
}

impl VariantResult {
    fn trades(&self) -> impl Iterator<Item = &Trade> {
        self.outcomes.iter().filter_map(|o| o.as_ref().ok())
    }
}

fn eastern_date(t: Timestamp) -> String {
    (t - SignedDuration::from_hours(5)).to_string()[..10].to_string()
}

pub fn run_variant(variant: Variant, games: &mut [(GameReplay, StrategyEngine)], cfg: &Config) -> VariantResult {
    let mut bankroll = START_BANKROLL;
    let mut peak = bankroll;
    let mut max_drawdown = 0.0f64;
    let mut day = String::new();
    let mut day_loss = 0.0;
    let mut outcomes = Vec::with_capacity(games.len());

    for (replay, engine) in games.iter_mut() {
        let date = eastern_date(replay.game.scheduled);
        if date != day {
            day = date;
            day_loss = 0.0;
        }
        if variant.daily_loss_stop.is_some_and(|stop| day_loss >= stop) {
            outcomes.push(Err("daily loss stop".to_string()));
            continue;
        }
        engine.sizing = variant.sizing(bankroll, cfg);
        let eval = engine.evaluate(Arc::new(synthetic_book(replay.market_price, BOOK_MODEL)));
        let Some(intent) = eval.intent else {
            outcomes.push(Err(eval.skip_reason.unwrap_or_else(|| "no trade".into())));
            continue;
        };
        let stake = intent.size_usdc.to_f64().unwrap_or(0.0);
        let vwap = intent.max_vwap.to_f64().unwrap_or(1.0);
        let pnl = pnl(intent.buy_side, stake, vwap, replay.over_won);
        bankroll += pnl;
        if pnl < 0.0 {
            day_loss -= pnl;
        }
        peak = peak.max(bankroll);
        max_drawdown = max_drawdown.max((peak - bankroll) / peak);
        outcomes.push(Ok(Trade { side: intent.buy_side, stake, vwap, pnl }));
    }
    VariantResult { variant, outcomes, final_bankroll: bankroll, max_drawdown }
}

/// Accuracy of the model over a set of games.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Accuracy {
    pub n: usize,
    pub mae_regulation: f64,
    pub bias_regulation: f64,
    pub mae_final: f64,
    pub bias_final: f64,
    /// Share of regulation totals inside the model's 95% CI.
    pub ci_coverage: f64,
    pub brier_model: f64,
    pub brier_market: f64,
}

pub fn accuracy(games: &[&GameReplay]) -> Accuracy {
    let n = games.len();
    if n == 0 {
        return Accuracy::default();
    }
    let mean = |f: &dyn Fn(&GameReplay) -> f64| games.iter().map(|g| f(g)).sum::<f64>() / n as f64;
    let outcome = |g: &GameReplay| if g.over_won { 1.0 } else { 0.0 };
    Accuracy {
        n,
        mae_regulation: mean(&|g| (g.projected_total - g.regulation_total as f64).abs()),
        bias_regulation: mean(&|g| g.projected_total - g.regulation_total as f64),
        mae_final: mean(&|g| (g.projected_total - g.final_total as f64).abs()),
        bias_final: mean(&|g| g.projected_total - g.final_total as f64),
        ci_coverage: mean(&|g| {
            let t = g.regulation_total as f64;
            if g.ci_low <= t && t <= g.ci_high { 1.0 } else { 0.0 }
        }),
        brier_model: mean(&|g| (g.p_over - outcome(g)).powi(2)),
        brier_market: mean(&|g| (g.market_price - outcome(g)).powi(2)),
    }
}

/// Per-decile calibration rows: (bucket low edge, games, mean predicted
/// P(over), actual Over rate). Empty buckets are omitted.
pub fn calibration(games: &[&GameReplay], prob: &dyn Fn(&GameReplay) -> f64) -> Vec<(f64, usize, f64, f64)> {
    (0..10)
        .filter_map(|b| {
            let bucket: Vec<&&GameReplay> =
                games.iter().filter(|g| ((prob(g) * 10.0).floor() as i32).clamp(0, 9) == b).collect();
            let n = bucket.len();
            (n > 0).then(|| {
                (
                    b as f64 / 10.0,
                    n,
                    bucket.iter().map(|g| prob(g)).sum::<f64>() / n as f64,
                    bucket.iter().filter(|g| g.over_won).count() as f64 / n as f64,
                )
            })
        })
        .collect()
}

pub fn render_report(
    sample: &[SampleGame],
    excluded: &[(Excluded, usize)],
    replays: &[GameReplay],
    results: &[VariantResult],
    cfg: &Config,
) -> String {
    let mut out = String::new();
    let w = &mut out;
    let all: Vec<&GameReplay> = replays.iter().collect();
    let passed: Vec<&GameReplay> = replays.iter().filter(|g| g.gate_failed.is_none()).collect();

    let _ = writeln!(w, "# Backtest report\n");
    let _ = writeln!(w, "## What this is\n");
    if let (Some(first), Some(last)) = (replays.first(), replays.last()) {
        let _ = writeln!(
            w,
            "- **Games:** {} replayed of {} sampled, {} to {}.",
            replays.len(),
            sample.len(),
            eastern_date(first.game.scheduled),
            eastern_date(last.game.scheduled)
        );
    }
    for season in [crate::backtest::fetch::LAST_SEASON, crate::backtest::fetch::CURRENT_SEASON] {
        let _ = writeln!(
            w,
            "  - Season {}-{}: {} replayed.",
            season,
            (season + 1) % 100,
            replays.iter().filter(|g| g.game.season == season).count()
        );
    }
    for (why, n) in excluded {
        let _ = writeln!(w, "  - Excluded, {why:?}: {n}.");
    }
    let _ = writeln!(w, "- **Overtime games:** {}.", replays.iter().filter(|g| g.overtime).count());
    let _ = writeln!(
        w,
        "- **Real:** play-by-play (SportRadar), the line, the OVER price at the decision minute, and the settled result (Polymarket)."
    );
    let _ = writeln!(
        w,
        "- **Assumed:** order-book depth ({} levels each side, 1 cent apart, starting {} from the price, ${} per level); zero trading fees; positions held to settlement; bets placed one at a time in game order.",
        BOOK_MODEL.levels, BOOK_MODEL.half_spread, BOOK_MODEL.level_usdc
    );
    let _ = writeln!(
        w,
        "- **Settings:** min_edge {}, lr_p_threshold {}, max_q1_margin {}, q1_scaling_factor {}, q2_window_minutes {}, ci_ref_width {}, starting bankroll ${}.\n",
        cfg.strategy.min_edge,
        cfg.strategy.lr_p_threshold,
        cfg.strategy.max_q1_margin,
        cfg.strategy.q1_scaling_factor,
        cfg.strategy.q2_window_minutes,
        cfg.sizing.ci_ref_width,
        START_BANKROLL
    );

    let _ = writeln!(w, "## Model accuracy\n");
    let _ = writeln!(w, "| Metric | All games | Passed the gates |\n|---|---|---|");
    let (a, p) = (accuracy(&all), accuracy(&passed));
    let row = |w: &mut String, name: &str, f: &dyn Fn(&Accuracy) -> String| {
        let _ = writeln!(w, "| {name} | {} | {} |", f(&a), f(&p));
    };
    row(w, "Games", &|x| x.n.to_string());
    row(w, "Mean abs. error vs regulation total (points)", &|x| format!("{:.1}", x.mae_regulation));
    row(w, "Bias vs regulation total (projection minus actual)", &|x| format!("{:+.1}", x.bias_regulation));
    row(w, "Mean abs. error vs final total incl. overtime", &|x| format!("{:.1}", x.mae_final));
    row(w, "Bias vs final total incl. overtime", &|x| format!("{:+.1}", x.bias_final));
    row(w, "Regulation totals inside the model's 95% interval", &|x| format!("{:.0}%", x.ci_coverage * 100.0));
    row(w, "Brier score, model P(over) (lower is better; 0.25 = coin flip)", &|x| format!("{:.4}", x.brier_model));
    row(w, "Brier score, market price at the same moment", &|x| format!("{:.4}", x.brier_market));

    for (title, prob) in [
        ("Model calibration (all games)", (&|g: &GameReplay| g.p_over) as &dyn Fn(&GameReplay) -> f64),
        ("Market calibration (all games)", (&|g: &GameReplay| g.market_price) as &dyn Fn(&GameReplay) -> f64),
    ] {
        let _ = writeln!(w, "\n### {title}\n");
        let _ = writeln!(w, "| P(over) bucket | Games | Mean predicted | Actual Over rate |\n|---|---|---|---|");
        for (lo, n, predicted, actual) in calibration(&all, prob) {
            let _ = writeln!(w, "| {:.1}-{:.1} | {n} | {:.2} | {:.2} |", lo, lo + 0.1, predicted, actual);
        }
    }

    let _ = writeln!(w, "\n## Gate funnel\n");
    let _ = writeln!(w, "| Stage | Games |\n|---|---|");
    let _ = writeln!(w, "| Replayed | {} |", replays.len());
    for gate in ["coverage gate failed", "blowout gate failed", "consistency test failed"] {
        let n = replays.iter().filter(|g| g.gate_failed.as_deref() == Some(gate)).count();
        let _ = writeln!(w, "| Stopped: {gate} | {n} |");
    }
    let _ = writeln!(w, "| Passed all gates | {} |", passed.len());

    let _ = writeln!(w, "\n## Sizing variants\n");
    let _ = write!(w, "| Metric |");
    for r in results {
        let _ = write!(w, " {} |", r.variant.name);
    }
    let _ = writeln!(w, "\n|---|{}", "---|".repeat(results.len()));
    let vrow = |w: &mut String, name: &str, f: &dyn Fn(&VariantResult) -> String| {
        let _ = write!(w, "| {name} |");
        for r in results {
            let _ = write!(w, " {} |", f(r));
        }
        let _ = writeln!(w);
    };
    let staked = |r: &VariantResult| r.trades().map(|t| t.stake).sum::<f64>();
    vrow(w, "Trades", &|r| r.trades().count().to_string());
    vrow(w, "Win rate", &|r| {
        let n = r.trades().count();
        if n == 0 { "-".into() } else { format!("{:.0}%", 100.0 * r.trades().filter(|t| t.pnl > 0.0).count() as f64 / n as f64) }
    });
    vrow(w, "OVER / UNDER bets", &|r| {
        let over = r.trades().filter(|t| matches!(t.side, BuyOutcome::Over)).count();
        format!("{over} / {}", r.trades().count() - over)
    });
    vrow(w, "Total staked", &|r| format!("${:.0}", staked(r)));
    vrow(w, "Final bankroll (from $500)", &|r| format!("${:.2}", r.final_bankroll));
    vrow(w, "Return on bankroll", &|r| format!("{:+.1}%", 100.0 * (r.final_bankroll / START_BANKROLL - 1.0)));
    vrow(w, "Worst drawdown", &|r| format!("{:.1}%", 100.0 * r.max_drawdown));
    vrow(w, "Return per dollar staked", &|r| {
        let s = staked(r);
        if s == 0.0 { "-".into() } else { format!("{:+.1}%", 100.0 * (r.final_bankroll - START_BANKROLL) / s) }
    });
    for reason in ["no edge in either direction", "kelly size below minimum order", "daily loss stop"] {
        vrow(w, &format!("Skipped: {reason}"), &|r| {
            r.outcomes.iter().filter(|o| o.as_ref().err().map(String::as_str) == Some(reason)).count().to_string()
        });
    }
    out
}

fn render_csv(replays: &[GameReplay], results: &[VariantResult]) -> String {
    let mut out = String::from(
        "game_id,date,away,home,season,line,decision_at,market_price,projected_total,ci_low,ci_high,p_over,coverage,q1_margin,lr_p,gate_failed,regulation_total,final_total,overtime,over_won",
    );
    for r in results {
        let n = r.variant.name;
        let _ = write!(out, ",{n}_side,{n}_stake,{n}_vwap,{n}_pnl,{n}_skip");
    }
    out.push('\n');
    for (i, g) in replays.iter().enumerate() {
        let _ = write!(
            out,
            "{},{},{},{},{},{},{},{:.4},{:.2},{:.2},{:.2},{:.4},{:.3},{},{:.4},{},{},{},{},{}",
            g.game.id,
            eastern_date(g.game.scheduled),
            g.game.away,
            g.game.home,
            g.game.season,
            g.line,
            g.decision_at,
            g.market_price,
            g.projected_total,
            g.ci_low,
            g.ci_high,
            g.p_over,
            g.coverage,
            g.q1_margin,
            g.lr_p,
            g.gate_failed.as_deref().unwrap_or(""),
            g.regulation_total,
            g.final_total,
            g.overtime,
            g.over_won
        );
        for r in results {
            match &r.outcomes[i] {
                Ok(t) => {
                    let side = if matches!(t.side, BuyOutcome::Over) { "over" } else { "under" };
                    let _ = write!(out, ",{side},{:.2},{:.4},{:.2},", t.stake, t.vwap, t.pnl);
                }
                Err(reason) => {
                    let _ = write!(out, ",,,,,{reason}");
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Replay every cached sample game for every variant and write the report.
/// Returns the report directory.
pub fn run() -> Result<PathBuf> {
    let cfg = Config::load()?;
    let sample = load_sample()?;
    let mut games: Vec<(GameReplay, StrategyEngine)> = Vec::new();
    let mut excluded: std::collections::BTreeMap<Excluded, usize> = Default::default();
    for game in &sample {
        match replay_game(game, &cfg) {
            Ok(Ok(pair)) => games.push(pair),
            Ok(Err(why)) => *excluded.entry(why).or_default() += 1,
            Err(e) => {
                warn!(game = %game.id, error = ?e, "replay failed");
                *excluded.entry(Excluded::NoPlayByPlay).or_default() += 1;
            }
        }
    }
    games.sort_by(|a, b| a.0.decision_at.cmp(&b.0.decision_at));
    info!(replayed = games.len(), sampled = sample.len(), "replay complete");

    let results: Vec<VariantResult> =
        VARIANTS.iter().map(|v| run_variant(*v, &mut games, &cfg)).collect();
    let replays: Vec<GameReplay> = games.into_iter().map(|(r, _)| r).collect();
    let excluded: Vec<(Excluded, usize)> = excluded.into_iter().collect();
    let report = render_report(&sample, &excluded, &replays, &results, &cfg);

    let stamp = Timestamp::now().to_string().replace(':', "-");
    let dir = Path::new(crate::backtest::fetch::ROOT).join("reports").join(&stamp[..19]);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("report.md"), &report)?;
    std::fs::write(dir.join("games.csv"), render_csv(&replays, &results))?;
    let ids: Vec<&str> = replays.iter().map(|g| g.game.id.as_str()).collect();
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&ids)?)?;
    println!("{report}");
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    fn replay(p_over: f64, market_price: f64, projected: f64, regulation: u32, final_total: u32, over_won: bool) -> GameReplay {
        GameReplay {
            game: SampleGame {
                id: "g".into(),
                season: 2025,
                scheduled: "2026-01-10T00:30:00Z".parse().unwrap(),
                away: "A".into(),
                home: "B".into(),
                slug: "s".into(),
            },
            line: 220.5,
            decision_at: "2026-01-10T01:10:00Z".parse().unwrap(),
            market_price,
            projected_total: projected,
            ci_low: projected - 10.0,
            ci_high: projected + 10.0,
            p_over,
            coverage: 1.0,
            q1_margin: 0,
            lr_p: 0.5,
            gate_failed: None,
            regulation_total: regulation,
            final_total,
            overtime: regulation != final_total,
            over_won,
        }
    }

    #[test]
    fn synthetic_book_brackets_the_price() {
        let b = synthetic_book(0.50, BOOK_MODEL);
        let prices = |ls: &[PriceLevel]| ls.iter().map(|l| l.price.to_string()).collect::<Vec<_>>();
        assert_eq!(prices(&b.asks), ["0.51", "0.52", "0.53", "0.54", "0.55"]);
        assert_eq!(prices(&b.bids), ["0.49", "0.48", "0.47", "0.46", "0.45"]);
        // $500 of notional at 0.51.
        assert_relative_eq!(b.asks[0].size.to_f64().unwrap(), 980.39, epsilon = 0.01);
    }

    #[test]
    fn synthetic_book_drops_levels_outside_valid_prices() {
        let b = synthetic_book(0.97, BOOK_MODEL);
        assert_eq!(b.asks.len(), 2); // 0.98, 0.99
        assert_eq!(b.bids.len(), 5);
    }

    #[test]
    fn pnl_pays_a_dollar_per_share_on_a_win_and_loses_the_stake_otherwise() {
        // $20 at 0.40 buys 50 shares.
        assert_relative_eq!(pnl(BuyOutcome::Over, 20.0, 0.40, true), 30.0);
        assert_relative_eq!(pnl(BuyOutcome::Over, 20.0, 0.40, false), -20.0);
        assert_relative_eq!(pnl(BuyOutcome::Under, 20.0, 0.40, false), 30.0);
        assert_relative_eq!(pnl(BuyOutcome::Under, 20.0, 0.40, true), -20.0);
    }

    #[test]
    fn accuracy_measures_error_bias_coverage_and_brier() {
        let a = replay(0.8, 0.6, 230.0, 220, 220, true); // +10, inside CI
        let b = replay(0.4, 0.5, 210.0, 225, 240, true); // -15 vs reg, -30 vs final, outside CI
        let acc = accuracy(&[&a, &b]);
        assert_eq!(acc.n, 2);
        assert_relative_eq!(acc.mae_regulation, 12.5);
        assert_relative_eq!(acc.bias_regulation, -2.5);
        assert_relative_eq!(acc.mae_final, 20.0);
        assert_relative_eq!(acc.bias_final, -10.0);
        assert_relative_eq!(acc.ci_coverage, 0.5);
        assert_relative_eq!(acc.brier_model, (0.04 + 0.36) / 2.0);
        assert_relative_eq!(acc.brier_market, (0.16 + 0.25) / 2.0);
    }

    #[test]
    fn calibration_buckets_by_decile() {
        let games = [
            replay(0.62, 0.5, 0.0, 0, 0, true),
            replay(0.68, 0.5, 0.0, 0, 0, false),
            replay(1.0, 0.5, 0.0, 0, 0, true),
        ];
        let refs: Vec<&GameReplay> = games.iter().collect();
        let rows = calibration(&refs, &|g| g.p_over);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].0, rows[0].1), (0.6, 2));
        assert_relative_eq!(rows[0].2, 0.65);
        assert_relative_eq!(rows[0].3, 0.5);
        // p = 1.0 lands in the top bucket rather than overflowing.
        assert_eq!((rows[1].0, rows[1].1), (0.9, 1));
    }

    #[test]
    fn seed_is_stable_per_game() {
        assert_eq!(seed_for("abc"), seed_for("abc"));
        assert_ne!(seed_for("abc"), seed_for("abd"));
    }
}
