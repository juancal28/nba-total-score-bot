//! SportRadar NBA play-by-play poller against the trial REST endpoint.
//!
//! For v1, we don't have a Realtime contract for push streaming. Instead we
//! poll `/games/{game_id}/pbp.json?api_key=...` every `poll_interval_ms` ms
//! and diff against the last seen play, emitting `GameEvent`s for new events.
//!
//! The trial tier rate-limits at 1 QPS, so the minimum useful poll interval
//! is ~5s — enough for analytics validation but materially slower than the
//! push feed. Swap in a push-feed client when a Realtime contract is signed.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::feed::{GameEvent, Team};

pub struct SportRadarPoller {
    pub base_url: String,
    pub api_key: SecretString,
    pub game_id: String,
    pub poll_interval: Duration,
    http: reqwest::Client,
}

impl SportRadarPoller {
    pub fn new(
        base_url: String,
        api_key: SecretString,
        game_id: String,
        poll_interval_ms: u64,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self {
            base_url,
            api_key,
            game_id,
            poll_interval: Duration::from_millis(poll_interval_ms),
            http,
        })
    }

    /// Run the poll loop until `tx` is closed.
    pub async fn run(self, tx: mpsc::Sender<GameEvent>) -> Result<()> {
        let url = format!(
            "{}/games/{}/pbp.json?api_key={}",
            self.base_url,
            self.game_id,
            self.api_key.expose_secret(),
        );
        let mut cursor = PbpCursor::default();

        loop {
            if tx.is_closed() {
                return Ok(());
            }
            match self.http.get(&url).send().await {
                Ok(resp) => match resp.bytes().await.map_err(anyhow::Error::from).and_then(|b| parse_pbp(&b, &mut cursor)) {
                    Ok(events) => {
                        for ev in events {
                            if tx.send(ev).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => warn!(error = ?e, "failed to parse pbp"),
                },
                Err(e) => {
                    warn!(error = ?e, "pbp http error");
                    // Emit a feed-gap marker so coverage tracking drops.
                    let _ = tx.send(GameEvent::FeedGap {
                        at: Timestamp::now(),
                        duration_ms: self.poll_interval.as_millis() as u64,
                    }).await;
                }
            }
            sleep(self.poll_interval).await;
        }
    }
}

/// Incremental parse state for a game's play-by-play: which events have
/// already been emitted, and the running score after the last one (points and
/// scoring team are derived from the change in running score).
#[derive(Debug, Default)]
pub struct PbpCursor {
    seen: HashSet<String>,
    home_points: u32,
    away_points: u32,
}

/// Game status and running score from a play-by-play body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PbpSummary {
    pub status: String,
    pub home_points: u32,
    pub away_points: u32,
}

/// Parse a `/games/{id}/pbp.json` body, returning only the events `cursor`
/// has not emitted before.
pub fn parse_pbp(body: &[u8], cursor: &mut PbpCursor) -> Result<Vec<GameEvent>> {
    let pbp: PbpResponse = serde_json::from_slice(body)?;
    let mut out = Vec::new();
    for period in &pbp.periods {
        // Overtime periods restart their numbering at 1.
        let number = if period.kind == "overtime" { period.number + 4 } else { period.number };
        for ev in &period.events {
            if !cursor.seen.insert(ev.id.clone()) {
                continue; // already emitted
            }
            match pbp_to_game_event(number, ev, cursor) {
                Some(game_event) => out.push(game_event),
                None => debug!(id = %ev.id, kind = %ev.event_type, "unhandled pbp event"),
            }
        }
    }
    Ok(out)
}

pub fn pbp_summary(body: &[u8]) -> Result<PbpSummary> {
    let pbp: PbpResponse = serde_json::from_slice(body)?;
    Ok(PbpSummary {
        status: pbp.status,
        home_points: pbp.home.points,
        away_points: pbp.away.points,
    })
}

/// Trimmed view of SportRadar's NBA play-by-play response. Field names are
/// SportRadar v8 conventions: see developer.sportradar.com nba/v8 schema.
#[derive(Debug, Deserialize)]
struct PbpResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    home: TeamBlock,
    #[serde(default)]
    away: TeamBlock,
    #[serde(default)]
    periods: Vec<PeriodBlock>,
}

#[derive(Debug, Default, Deserialize)]
struct TeamBlock {
    #[serde(default)]
    points: u32,
}

#[derive(Debug, Deserialize)]
struct PeriodBlock {
    #[serde(default)]
    number: u8,
    /// "quarter" or "overtime".
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    events: Vec<PbpEvent>,
}

#[derive(Debug, Deserialize)]
struct PbpEvent {
    id: String,
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    clock: String,
    #[serde(default)]
    wall_clock: Option<Timestamp>,
    /// Running score after this event.
    #[serde(default)]
    home_points: u32,
    #[serde(default)]
    away_points: u32,
}

fn pbp_to_game_event(period: u8, ev: &PbpEvent, cursor: &mut PbpCursor) -> Option<GameEvent> {
    let at = ev.wall_clock.unwrap_or_else(Timestamp::now);
    let clock_seconds = parse_clock(&ev.clock).unwrap_or(0);

    // The payload carries no per-event point value or home/away flag, so both
    // come from the change in running score.
    let home_delta = ev.home_points.saturating_sub(cursor.home_points);
    let away_delta = ev.away_points.saturating_sub(cursor.away_points);
    cursor.home_points = ev.home_points;
    cursor.away_points = ev.away_points;
    if home_delta + away_delta > 0 {
        let (team, points) =
            if home_delta >= away_delta { (Team::Home, home_delta) } else { (Team::Away, away_delta) };
        return Some(GameEvent::Score {
            at,
            period,
            game_clock_seconds: clock_seconds,
            team,
            points: points.min(u8::MAX as u32) as u8,
            home_score: ev.home_points,
            away_score: ev.away_points,
        });
    }

    match ev.event_type.as_str() {
        "endperiod" => Some(GameEvent::PeriodEnd { at, period }),
        "opentip" => Some(GameEvent::Tipoff { at }),
        // Treat anything else as a clock update if we have a clock value.
        _ if !ev.clock.is_empty() => {
            Some(GameEvent::ClockUpdate { at, period, game_clock_seconds: clock_seconds })
        }
        _ => None,
    }
}

/// Parse "MM:SS" or "M:SS.t" → seconds remaining.
fn parse_clock(s: &str) -> Option<u32> {
    let mut parts = s.splitn(2, ':');
    let mins: u32 = parts.next()?.parse().ok()?;
    let secs_part = parts.next()?;
    let secs: f32 = secs_part.parse().ok()?;
    Some(mins * 60 + secs.floor() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_parsing() {
        assert_eq!(parse_clock("11:42"), Some(11 * 60 + 42));
        assert_eq!(parse_clock("0:23.5"), Some(23));
        assert_eq!(parse_clock("garbage"), None);
    }

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sportradar_pbp_okc_hou.json");

    fn fixture_events() -> Vec<GameEvent> {
        parse_pbp(FIXTURE, &mut PbpCursor::default()).unwrap()
    }

    /// (period, game_clock_seconds, team, points) for every Score event.
    fn scores(events: &[GameEvent]) -> Vec<(u8, u32, Team, u8)> {
        events
            .iter()
            .filter_map(|e| match e {
                GameEvent::Score { period, game_clock_seconds, team, points, .. } => {
                    Some((*period, *game_clock_seconds, *team, *points))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn q1_points_bucket_by_minute_like_the_real_game() {
        let mut buckets = [0u32; 12];
        for (period, clock, _, points) in scores(&fixture_events()) {
            if period == 1 {
                buckets[((720 - clock) / 60).min(11) as usize] += points as u32;
            }
        }
        assert_eq!(buckets, [5, 4, 5, 5, 6, 7, 8, 6, 1, 2, 8, 0]);
    }

    #[test]
    fn q1_points_are_attributed_to_the_right_team() {
        let s = scores(&fixture_events());
        let q1 = |t: Team| -> u32 {
            s.iter().filter(|x| x.0 == 1 && x.2 == t).map(|x| x.3 as u32).sum()
        };
        // OKC (home) 27, HOU (away) 30 after Q1.
        assert_eq!(q1(Team::Home), 27);
        assert_eq!(q1(Team::Away), 30);
    }

    #[test]
    fn free_throws_and_threes_carry_their_real_value() {
        let s = scores(&fixture_events());
        // The game opens with a two at 11:26 followed by its and-one free throw.
        assert_eq!((s[0].1, s[0].3), (11 * 60 + 26, 2));
        assert_eq!((s[1].1, s[1].3), (11 * 60 + 26, 1));
        assert!(s.iter().any(|x| x.3 == 3));
        assert!(s.iter().all(|x| (1..=3).contains(&x.3)));
    }

    #[test]
    fn events_are_stamped_with_their_wall_clock() {
        let events = fixture_events();
        let first_q2 = events
            .iter()
            .find_map(|e| match e {
                GameEvent::ClockUpdate { at, period: 2, .. } => Some(*at),
                _ => None,
            })
            .unwrap();
        assert_eq!(first_q2, "2025-10-22T00:14:45Z".parse::<Timestamp>().unwrap());
    }

    #[test]
    fn overtime_periods_are_numbered_after_regulation() {
        let ends: Vec<u8> = fixture_events()
            .iter()
            .filter_map(|e| match e {
                GameEvent::PeriodEnd { period, .. } => Some(*period),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn reparsing_with_the_same_cursor_emits_nothing_new() {
        let mut cursor = PbpCursor::default();
        assert!(!parse_pbp(FIXTURE, &mut cursor).unwrap().is_empty());
        assert!(parse_pbp(FIXTURE, &mut cursor).unwrap().is_empty());
    }

    #[test]
    fn summary_reports_status_and_final_score() {
        let s = pbp_summary(FIXTURE).unwrap();
        assert_eq!(s, PbpSummary { status: "closed".into(), home_points: 125, away_points: 124 });
    }
}
