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
        let mut seen: HashSet<String> = HashSet::new();

        loop {
            if tx.is_closed() {
                return Ok(());
            }
            match self.http.get(&url).send().await {
                Ok(resp) => match resp.json::<PbpResponse>().await {
                    Ok(pbp) => {
                        for ev in pbp.into_events(&mut seen) {
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

/// Trimmed view of SportRadar's NBA play-by-play response. Field names are
/// SportRadar v8 conventions: see developer.sportradar.com nba/v8 schema.
#[derive(Debug, Deserialize)]
struct PbpResponse {
    #[serde(default)]
    periods: Vec<PeriodBlock>,
}

#[derive(Debug, Deserialize)]
struct PeriodBlock {
    #[serde(default)]
    number: u8,
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
    home_points: u32,
    #[serde(default)]
    away_points: u32,
    #[serde(default)]
    attribution: Option<EventAttribution>,
    #[serde(default)]
    points: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct EventAttribution {
    #[serde(default)]
    market: Option<String>, // "home" / "away"
}

impl PbpResponse {
    fn into_events(self, seen: &mut HashSet<String>) -> Vec<GameEvent> {
        let mut out = Vec::new();
        for period in self.periods {
            for ev in period.events {
                if !seen.insert(ev.id.clone()) {
                    continue; // already emitted
                }
                if let Some(game_event) = pbp_to_game_event(period.number, &ev) {
                    out.push(game_event);
                } else {
                    debug!(id = %ev.id, kind = %ev.event_type, "unhandled pbp event");
                }
            }
        }
        out
    }
}

fn pbp_to_game_event(period: u8, ev: &PbpEvent) -> Option<GameEvent> {
    let at = Timestamp::now();
    let clock_seconds = parse_clock(&ev.clock).unwrap_or(0);

    match ev.event_type.as_str() {
        "twopointmade" | "threepointmade" | "freethrowmade" => {
            let team = ev
                .attribution
                .as_ref()
                .and_then(|a| a.market.as_deref())
                .map(|s| if s == "home" { Team::Home } else { Team::Away })
                .unwrap_or(Team::Home);
            Some(GameEvent::Score {
                at,
                period,
                game_clock_seconds: clock_seconds,
                team,
                points: ev.points.unwrap_or(2),
                home_score: ev.home_points,
                away_score: ev.away_points,
            })
        }
        "endperiod" => Some(GameEvent::PeriodEnd { at, period }),
        "tipoff" | "openinbound" => Some(GameEvent::Tipoff { at }),
        _ => {
            // Treat anything else as a clock update if we have a clock value.
            if !ev.clock.is_empty() {
                Some(GameEvent::ClockUpdate {
                    at,
                    period,
                    game_clock_seconds: clock_seconds,
                })
            } else {
                None
            }
        }
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
}
