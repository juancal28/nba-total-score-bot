pub mod coverage;
pub mod sportradar;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A typed event from the play-by-play feed. Timestamps are real-time;
/// `period` and `game_clock_seconds` are the in-game clock at the event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GameEvent {
    /// Game-clock tipoff observed.
    Tipoff { at: Timestamp },
    /// A score occurred. `points` is the value of the made bucket (1/2/3).
    Score {
        at: Timestamp,
        period: u8,
        game_clock_seconds: u32,
        team: Team,
        points: u8,
        home_score: u32,
        away_score: u32,
    },
    /// Periodic game-clock tick (no scoring change).
    ClockUpdate {
        at: Timestamp,
        period: u8,
        game_clock_seconds: u32,
    },
    /// End of a quarter / period.
    PeriodEnd { at: Timestamp, period: u8 },
    /// Roster change: starter ejected, injury sub, etc. Used to invalidate the
    /// Q1 sample if the team's offensive profile materially changed.
    RosterChange { at: Timestamp, team: Team, reason: String },
    /// Marker emitted by the feed task when it lost connection or dropped events.
    FeedGap { at: Timestamp, duration_ms: u64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Team {
    Home,
    Away,
}
