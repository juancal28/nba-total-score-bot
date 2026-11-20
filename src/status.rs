//! `status`: read-only views over what the paper trader and backtester have
//! written. Paper decisions are graded against Polymarket's settled result
//! (a free, unauthenticated lookup); nothing here spends SportRadar requests.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use jiff::{SignedDuration, Timestamp};

use crate::backtest::fetch::ROOT as BACKTEST_ROOT;
use crate::backtest::run::pnl;
use crate::paper::{DECISIONS_PATH, DecisionRecord};
use crate::polymarket::public::{fetch_event_raw, totals_market_for_token};
use crate::strategy::BuyOutcome;

pub enum StatusView {
    Overview,
    Paper { date: Option<String>, all: bool },
    Backtest { list: bool, report: Option<String> },
}

/// A paper decision plus how its market settled, when known.
#[derive(Debug, Clone)]
pub struct GradedDecision {
    pub record: DecisionRecord,
    /// `None` until Polymarket has resolved the market.
    pub over_won: Option<bool>,
}

/// Profit or loss in USDC of a recorded bet once its market has settled.
fn bet_pnl(side: Option<&str>, size: Option<&str>, vwap: Option<&str>, over_won: Option<bool>) -> Option<f64> {
    let side = match side? {
        "over" => BuyOutcome::Over,
        _ => BuyOutcome::Under,
    };
    let stake: f64 = size?.parse().ok()?;
    let vwap: f64 = vwap?.parse().ok()?;
    Some(pnl(side, stake, vwap, over_won?))
}

/// One model's bet on one game, reduced to what the running record needs.
struct Bet {
    traded: bool,
    stake: Option<f64>,
    pnl: Option<f64>,
}

impl GradedDecision {
    /// Profit or loss in USDC for a settled trade.
    pub fn pnl(&self) -> Option<f64> {
        let r = &self.record;
        bet_pnl(r.side.as_deref(), r.size_usdc.as_deref(), r.vwap.as_deref(), self.over_won)
    }

    /// Profit or loss of the pace-only comparison bet, when one was recorded.
    pub fn pace_only_pnl(&self) -> Option<f64> {
        let p = self.record.pace_only.as_ref()?;
        bet_pnl(p.side.as_deref(), p.size_usdc.as_deref(), p.vwap.as_deref(), self.over_won)
    }

    fn main_bet(&self) -> Bet {
        let r = &self.record;
        Bet { traded: r.traded, stake: r.size_usdc.as_deref().and_then(|s| s.parse().ok()), pnl: self.pnl() }
    }

    /// The pure pace model's bet: the comparison bet on a blended record, or
    /// the decision itself on a record from before the blend existed.
    fn pace_bet(&self) -> Option<Bet> {
        match (&self.record.pace_only, &self.record.model) {
            (Some(p), _) => Some(Bet {
                traded: p.traded,
                stake: p.size_usdc.as_deref().and_then(|s| s.parse().ok()),
                pnl: self.pace_only_pnl(),
            }),
            (None, None) => Some(self.main_bet()),
            (None, Some(_)) => None,
        }
    }

    /// Profit or loss of the flat $10 comparison bet.
    pub fn flat_pnl(&self) -> Option<f64> {
        let r = &self.record;
        bet_pnl(r.flat_side.as_deref(), r.flat_size_usdc.as_deref(), r.flat_vwap.as_deref(), self.over_won)
    }

    pub fn pace_flat_pnl(&self) -> Option<f64> {
        let p = self.record.pace_only.as_ref()?;
        bet_pnl(p.flat_side.as_deref(), p.flat_size_usdc.as_deref(), p.flat_vwap.as_deref(), self.over_won)
    }

    /// The decision sized flat $10, or `None` for records written before the
    /// flat comparison existed (they carry neither a flat trade nor a reason).
    fn flat_bet(&self) -> Option<Bet> {
        let r = &self.record;
        (r.flat_traded || r.flat_skip_reason.is_some()).then(|| Bet {
            traded: r.flat_traded,
            stake: r.flat_size_usdc.as_deref().and_then(|s| s.parse().ok()),
            pnl: self.flat_pnl(),
        })
    }

    /// The pure pace model sized flat $10; same fallbacks as `pace_bet`.
    fn pace_flat_bet(&self) -> Option<Bet> {
        match (&self.record.pace_only, &self.record.model) {
            (Some(p), _) => (p.flat_traded || p.flat_skip_reason.is_some()).then(|| Bet {
                traded: p.flat_traded,
                stake: p.flat_size_usdc.as_deref().and_then(|s| s.parse().ok()),
                pnl: self.pace_flat_pnl(),
            }),
            (None, None) => self.flat_bet(),
            (None, Some(_)) => None,
        }
    }
}

pub fn eastern_date(t: Timestamp) -> String {
    (t - SignedDuration::from_hours(5)).to_string()[..10].to_string()
}

pub fn parse_decisions(body: &str) -> Vec<DecisionRecord> {
    body.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn load_decisions() -> Vec<DecisionRecord> {
    std::fs::read_to_string(DECISIONS_PATH).map(|b| parse_decisions(&b)).unwrap_or_default()
}

async fn grade(records: Vec<DecisionRecord>) -> Vec<GradedDecision> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default();
    let mut events: HashMap<String, Vec<u8>> = HashMap::new();
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        let mut over_won = None;
        let any_trade = record.traded || record.pace_only.as_ref().is_some_and(|p| p.traded);
        if let (true, Some(token)) = (any_trade, record.over_token_id.as_deref()) {
            if !events.contains_key(&record.slug) {
                if let Ok(body) = fetch_event_raw(&http, &record.slug).await {
                    events.insert(record.slug.clone(), body);
                }
            }
            over_won = events
                .get(&record.slug)
                .and_then(|b| totals_market_for_token(b, token).ok().flatten())
                .and_then(|m| m.over_won);
        }
        out.push(GradedDecision { record, over_won });
    }
    out
}

fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

/// Plain-text table with columns padded to their widest cell.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).chain([headers[i].len()]).max().unwrap_or(0))
        .collect();
    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    out.push('\n');
    out.push_str(&line(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>()));
    for r in rows {
        out.push('\n');
        out.push_str(&line(r));
    }
    out
}

pub fn render_paper(decisions: &[GradedDecision]) -> String {
    if decisions.is_empty() {
        return "No paper decisions recorded for this selection.".to_string();
    }
    let describe = |traded: bool, side: &Option<String>, size: &Option<String>, vwap: &Option<String>, skip: &Option<String>| {
        if traded {
            format!("{} ${} @ {}", opt(side).to_uppercase(), opt(size), opt(vwap))
        } else {
            format!("skip: {}", opt(skip))
        }
    };
    let outcome = |traded: bool, over_won: Option<bool>, pnl: Option<f64>| match (traded, over_won, pnl) {
        (false, _, _) => "-".to_string(),
        (true, Some(_), Some(p)) => format!("{} {:+.2}", if p > 0.0 { "WON" } else { "LOST" }, p),
        (true, _, _) => "pending".to_string(),
    };
    // The comparison columns appear only when some game has a comparison.
    let compare = decisions.iter().any(|d| d.record.pace_only.is_some());
    let rows: Vec<Vec<String>> = decisions
        .iter()
        .map(|d| {
            let r = &d.record;
            let decision = describe(r.traded, &r.side, &r.size_usdc, &r.vwap, &r.skip_reason);
            let result = outcome(r.traded, d.over_won, d.pnl());
            let flat = |b: Option<Bet>| match b {
                Some(b) => outcome(b.traded, d.over_won, b.pnl),
                None => "n/a".to_string(),
            };
            let mut row = vec![
                eastern_date(r.scheduled),
                format!("{} @ {}", r.away, r.home),
                opt(&r.line),
                r.projected_total.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into()),
                r.p_over.map(|x| format!("{x:.2}")).unwrap_or_else(|| "-".into()),
                format!("{} / {}", opt(&r.best_bid), opt(&r.best_ask)),
                decision,
                result,
                flat(d.flat_bet()),
            ];
            if compare {
                match &r.pace_only {
                    Some(p) => row.extend([
                        format!("{:.1}", p.projected_total),
                        format!("{:.2}", p.p_over),
                        describe(p.traded, &p.side, &p.size_usdc, &p.vwap, &p.skip_reason),
                        outcome(p.traded, d.over_won, d.pace_only_pnl()),
                        flat(d.pace_flat_bet()),
                    ]),
                    // A record from before the blend: the decision was pace-only.
                    None => row.extend(["(same)", "(same)", "(same)", "(same)", "(same)"].map(String::from)),
                }
            }
            row
        })
        .collect();
    let mut headers =
        vec!["Date", "Game", "Line", "Projection", "P(over)", "Bid / Ask", "Kelly decision", "Kelly result", "Flat $10"];
    if compare {
        headers.extend(["Pace proj.", "Pace P(over)", "Pace-only Kelly", "Pace Kelly result", "Pace flat $10"]);
    }
    let mut out = table(&headers, &rows);
    let _ = write!(out, "\n\n{}", render_record(decisions));
    out
}

/// Running record over the given decisions, one line per model and sizing.
/// The blended lines appear once the blended model has records; flat lines
/// cover only games that recorded a flat comparison. Each line counts the
/// games it covers, so lines over different game sets are not mistaken for
/// a like-for-like comparison.
pub fn render_record(decisions: &[GradedDecision]) -> String {
    let blended: Vec<&GradedDecision> = decisions.iter().filter(|d| d.record.model.is_some()).collect();
    let mut lines: Vec<(&str, Vec<Bet>)> = Vec::new();
    if blended.is_empty() {
        lines.push(("Kelly", decisions.iter().map(|d| d.main_bet()).collect()));
        lines.push(("Flat $10", decisions.iter().filter_map(|d| d.flat_bet()).collect()));
    } else {
        lines.push(("Blended, Kelly", blended.iter().map(|d| d.main_bet()).collect()));
        lines.push(("Blended, flat $10", blended.iter().filter_map(|d| d.flat_bet()).collect()));
        lines.push(("Pace-only, Kelly", decisions.iter().filter_map(|d| d.pace_bet()).collect()));
        lines.push(("Pace-only, flat $10", decisions.iter().filter_map(|d| d.pace_flat_bet()).collect()));
    }
    lines.retain(|(_, bets)| !bets.is_empty());
    let width = lines.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 1;
    lines
        .iter()
        .map(|(name, bets)| format!("{:<width$} {}", format!("{name}:"), record_line(bets)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn record_line(bets: &[Bet]) -> String {
    let trades = bets.iter().filter(|b| b.traded).count();
    let settled: Vec<f64> = bets.iter().filter_map(|b| b.pnl).collect();
    let wins = settled.iter().filter(|p| **p > 0.0).count();
    let staked: f64 = bets.iter().filter(|b| b.pnl.is_some()).filter_map(|b| b.stake).sum();
    format!(
        "{} games decided, {} trades, {} settled ({} won, {} lost), P&L {:+.2} on ${:.2} staked, {} pending",
        bets.len(),
        trades,
        settled.len(),
        wins,
        settled.len() - wins,
        // `+ 0.0` turns the empty sum's negative zero into plain zero.
        settled.iter().sum::<f64>() + 0.0,
        staked + 0.0,
        trades - settled.len()
    )
}

/// Report directories, oldest first.
pub fn list_reports(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("reports"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("report.md").exists()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// The section of a markdown report under `heading`, up to the next
/// same-level heading.
pub fn section<'a>(report: &'a str, heading: &str) -> Option<&'a str> {
    let start = report.find(heading)?;
    let body = &report[start..];
    let end = body[heading.len()..].find("\n## ").map(|i| i + heading.len()).unwrap_or(body.len());
    Some(body[..end].trim_end())
}

fn read_report(dir: &Path) -> Result<String> {
    std::fs::read_to_string(dir.join("report.md")).with_context(|| format!("read {}", dir.display()))
}

fn name(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub async fn run(view: StatusView) -> Result<()> {
    let root = Path::new(BACKTEST_ROOT);
    match view {
        StatusView::Paper { date, all } => {
            let today = eastern_date(Timestamp::now());
            let wanted = date.unwrap_or(today);
            let records: Vec<DecisionRecord> = load_decisions()
                .into_iter()
                .filter(|r| all || eastern_date(r.scheduled) == wanted)
                .collect();
            println!("Paper trading, {}\n", if all { "all dates".to_string() } else { wanted });
            println!("{}", render_paper(&grade(records).await));
        }
        StatusView::Backtest { list, report } => {
            let reports = list_reports(root);
            if list {
                if reports.is_empty() {
                    println!("No backtest reports yet. Run `backtest-run` first.");
                }
                for dir in &reports {
                    println!("{}", name(dir));
                }
                return Ok(());
            }
            let dir = match report {
                Some(wanted) => reports
                    .iter()
                    .find(|d| name(d) == wanted)
                    .ok_or_else(|| anyhow!("no report named {wanted}; see `status backtest --list`"))?,
                None => reports.last().ok_or_else(|| anyhow!("no backtest reports yet; run `backtest-run` first"))?,
            };
            println!("Report {}\n", name(dir));
            println!("{}", read_report(dir)?);
        }
        StatusView::Overview => {
            let today = eastern_date(Timestamp::now());
            let all = grade(load_decisions()).await;
            let tonight: Vec<GradedDecision> =
                all.iter().filter(|d| eastern_date(d.record.scheduled) == today).cloned().collect();
            println!("Paper trading, {today}\n");
            println!("{}", render_paper(&tonight));
            println!("\nAll paper trading to date:\n{}", render_record(&all));
            match list_reports(root).last() {
                Some(dir) => {
                    let report = read_report(dir)?;
                    println!("\nLatest backtest ({})\n", name(dir));
                    for heading in ["## Sizing variants", "## Model accuracy"] {
                        // Stop the accuracy section before its calibration tables.
                        let text = section(&report, heading).unwrap_or("");
                        println!("{}\n", text.split("\n### ").next().unwrap_or(text).trim_end());
                    }
                    println!("Full report: status backtest");
                }
                None => println!("\nNo backtest reports yet. Run `backtest-run` first."),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(traded: bool) -> DecisionRecord {
        let json = format!(
            r#"{{"game_id":"g1","away":"MIL","home":"NYK","scheduled":"2026-11-11T00:30:00Z",
            "decided_at":"2026-11-11T01:10:00Z","slug":"nba-mil-nyk-2026-11-10","question":null,
            "line":"222.5","over_token_id":"21","under_token_id":"22","taker_base_fee":null,"fees_enabled":null,
            "traded":{traded},"skip_reason":{skip},"side":{side},"size_usdc":{size},"vwap":{vwap},
            "best_bid":"0.48","best_ask":"0.49","projected_total":231.4,"ci_low":200.0,"ci_high":260.0,
            "p_over":0.71,"coverage":1.0,"q1_margin":3,"lr_p":0.4,"q1_per_minute":[],"q2_per_minute":[],
            "sportradar_requests":7}}"#,
            skip = if traded { "null" } else { r#""no edge in either direction""# },
            side = if traded { r#""over""# } else { "null" },
            size = if traded { r#""20.00""# } else { "null" },
            vwap = if traded { r#""0.5""# } else { "null" },
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn decisions_file_skips_corrupt_lines() {
        let line = serde_json::to_string(&record(true)).unwrap();
        let parsed = parse_decisions(&format!("{line}\nnot json\n{line}\n"));
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn pnl_needs_a_trade_and_a_settled_market() {
        let won = GradedDecision { record: record(true), over_won: Some(true) };
        // $20 at 0.50 buys 40 shares: +$20 if OVER hits.
        assert_eq!(won.pnl(), Some(20.0));
        assert_eq!(GradedDecision { record: record(true), over_won: Some(false) }.pnl(), Some(-20.0));
        assert_eq!(GradedDecision { record: record(true), over_won: None }.pnl(), None);
        assert_eq!(GradedDecision { record: record(false), over_won: Some(true) }.pnl(), None);
    }

    #[test]
    fn paper_table_shows_decision_and_result_per_game() {
        let out = render_paper(&[
            GradedDecision { record: record(true), over_won: Some(true) },
            GradedDecision { record: record(true), over_won: None },
            GradedDecision { record: record(false), over_won: None },
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("Date        Game"));
        assert!(lines[2].contains("MIL @ NYK") && lines[2].contains("2026-11-10"));
        assert!(lines[2].contains("OVER $20.00 @ 0.5") && lines[2].contains("WON +20.00"));
        // Written before the flat comparison existed.
        assert!(lines[2].ends_with("n/a"));
        assert!(lines[3].contains("pending"));
        assert!(lines[4].contains("skip: no edge in either direction"));
        // No flat line: none of these games has a flat comparison.
        assert!(out.ends_with(
            "Kelly: 3 games decided, 2 trades, 1 settled (1 won, 0 lost), P&L +20.00 on $20.00 staked, 1 pending"
        ), "{out}");
    }

    #[test]
    fn record_with_nothing_settled_shows_plain_zeros() {
        let line = render_record(&[GradedDecision { record: record(true), over_won: None }]);
        assert!(line.ends_with("P&L +0.00 on $0.00 staked, 1 pending"), "{line}");
    }

    /// `record(traded)` plus a flat $10 comparison taking the same side.
    fn record_with_flat(traded: bool) -> DecisionRecord {
        let mut r = record(traded);
        if traded {
            r.flat_traded = true;
            r.flat_side = Some("over".into());
            r.flat_size_usdc = Some("10".into());
            r.flat_vwap = Some("0.5".into());
        } else {
            r.flat_skip_reason = Some("no edge in either direction".into());
        }
        r
    }

    #[test]
    fn flat_comparison_is_graded_beside_kelly() {
        let out = render_paper(&[
            GradedDecision { record: record_with_flat(true), over_won: Some(true) },
            GradedDecision { record: record_with_flat(false), over_won: None },
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("Kelly result") && lines[0].contains("Flat $10"));
        // $10 at 0.50 buys 20 shares: +$10 if OVER hits.
        assert!(lines[2].contains("WON +20.00") && lines[2].ends_with("WON +10.00"), "{}", lines[2]);
        assert!(out.ends_with(
            "Kelly:    2 games decided, 1 trades, 1 settled (1 won, 0 lost), P&L +20.00 on $20.00 staked, 0 pending\n\
             Flat $10: 2 games decided, 1 trades, 1 settled (1 won, 0 lost), P&L +10.00 on $10.00 staked, 0 pending"
        ), "{out}");
    }

    /// A blended-model record whose pace-only comparison bet went the other way.
    fn blended_record() -> DecisionRecord {
        let mut r = record_with_flat(true);
        r.model = Some("blended".into());
        r.pace_only = Some(crate::paper::ModelDecision {
            projected_total: 190.0,
            ci_low: 150.0,
            ci_high: 230.0,
            p_over: 0.06,
            traded: true,
            skip_reason: None,
            side: Some("under".into()),
            size_usdc: Some("40.00".into()),
            vwap: Some("0.8".into()),
            flat_traded: true,
            flat_side: Some("under".into()),
            flat_size_usdc: Some("10".into()),
            flat_vwap: Some("0.8".into()),
            flat_skip_reason: None,
        });
        r
    }

    #[test]
    fn comparison_bet_is_graded_and_shown_beside_the_decision() {
        // OVER hit: the blended OVER wins $20, the pace-only UNDER loses its $40.
        let d = GradedDecision { record: blended_record(), over_won: Some(true) };
        assert_eq!(d.pnl(), Some(20.0));
        assert_eq!(d.pace_only_pnl(), Some(-40.0));

        let old = GradedDecision { record: record(true), over_won: Some(true) };
        let out = render_paper(&[d, old]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("Pace-only Kelly") && lines[0].contains("Pace flat $10"));
        assert!(lines[2].contains("WON +20.00") && lines[2].contains("UNDER $40.00 @ 0.8"));
        assert!(lines[2].contains("LOST -40.00") && lines[2].ends_with("LOST -10.00"), "{}", lines[2]);
        assert!(lines[3].ends_with("(same)"));
        // The pace-only Kelly line also counts the older game it decided
        // itself; that game has no flat comparison, so the flat lines skip it.
        assert!(out.ends_with(
            "Blended, Kelly:      1 games decided, 1 trades, 1 settled (1 won, 0 lost), P&L +20.00 on $20.00 staked, 0 pending\n\
             Blended, flat $10:   1 games decided, 1 trades, 1 settled (1 won, 0 lost), P&L +10.00 on $10.00 staked, 0 pending\n\
             Pace-only, Kelly:    2 games decided, 2 trades, 2 settled (1 won, 1 lost), P&L -20.00 on $60.00 staked, 0 pending\n\
             Pace-only, flat $10: 1 games decided, 1 trades, 1 settled (0 won, 1 lost), P&L -10.00 on $10.00 staked, 0 pending"
        ), "{out}");
    }

    #[test]
    fn empty_selection_says_so() {
        assert_eq!(render_paper(&[]), "No paper decisions recorded for this selection.");
    }

    #[test]
    fn section_stops_at_the_next_heading() {
        let report = "# R\n\n## A\n\nalpha\n\n### sub\n\nx\n\n## B\n\nbeta\n";
        assert_eq!(section(report, "## A"), Some("## A\n\nalpha\n\n### sub\n\nx"));
        assert_eq!(section(report, "## B"), Some("## B\n\nbeta"));
        assert_eq!(section(report, "## C"), None);
    }
}
