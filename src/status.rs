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

impl GradedDecision {
    /// Profit or loss in USDC for a settled trade.
    pub fn pnl(&self) -> Option<f64> {
        let r = &self.record;
        let side = match r.side.as_deref()? {
            "over" => BuyOutcome::Over,
            _ => BuyOutcome::Under,
        };
        let stake: f64 = r.size_usdc.as_deref()?.parse().ok()?;
        let vwap: f64 = r.vwap.as_deref()?.parse().ok()?;
        Some(pnl(side, stake, vwap, self.over_won?))
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
        if let (true, Some(token)) = (record.traded, record.over_token_id.as_deref()) {
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
    let rows: Vec<Vec<String>> = decisions
        .iter()
        .map(|d| {
            let r = &d.record;
            let decision = if r.traded {
                format!("{} ${} @ {}", opt(&r.side).to_uppercase(), opt(&r.size_usdc), opt(&r.vwap))
            } else {
                format!("skip: {}", opt(&r.skip_reason))
            };
            let result = match (r.traded, d.over_won, d.pnl()) {
                (false, _, _) => "-".to_string(),
                (true, Some(_), Some(p)) => format!("{} {:+.2}", if p > 0.0 { "WON" } else { "LOST" }, p),
                (true, _, _) => "pending".to_string(),
            };
            vec![
                eastern_date(r.scheduled),
                format!("{} @ {}", r.away, r.home),
                opt(&r.line),
                r.projected_total.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into()),
                r.p_over.map(|x| format!("{x:.2}")).unwrap_or_else(|| "-".into()),
                format!("{} / {}", opt(&r.best_bid), opt(&r.best_ask)),
                decision,
                result,
            ]
        })
        .collect();
    let mut out = table(
        &["Date", "Game", "Line", "Projection", "P(over)", "Bid / Ask", "Decision", "Result"],
        &rows,
    );
    let _ = write!(out, "\n\n{}", render_record(decisions));
    out
}

/// One-line running record over the given decisions.
pub fn render_record(decisions: &[GradedDecision]) -> String {
    let trades = decisions.iter().filter(|d| d.record.traded).count();
    let settled: Vec<f64> = decisions.iter().filter_map(|d| d.pnl()).collect();
    let wins = settled.iter().filter(|p| **p > 0.0).count();
    let staked: f64 = decisions
        .iter()
        .filter(|d| d.pnl().is_some())
        .filter_map(|d| d.record.size_usdc.as_deref()?.parse::<f64>().ok())
        .sum();
    format!(
        "{} games decided, {} trades, {} settled ({} won, {} lost), P&L {:+.2} on ${:.2} staked, {} pending",
        decisions.len(),
        trades,
        settled.len(),
        wins,
        settled.len() - wins,
        settled.iter().sum::<f64>(),
        staked,
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
            println!("\nAll paper trading to date: {}", render_record(&all));
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
        assert!(lines[2].contains("OVER $20.00 @ 0.5") && lines[2].ends_with("WON +20.00"));
        assert!(lines[3].ends_with("pending"));
        assert!(lines[4].contains("skip: no edge in either direction"));
        assert!(out.ends_with(
            "3 games decided, 2 trades, 1 settled (1 won, 0 lost), P&L +20.00 on $20.00 staked, 1 pending"
        ));
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
