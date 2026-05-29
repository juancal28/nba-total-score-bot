//! Tracks how much of each game-clock minute we actually observed live.
//! If the WS dropped events during Q1, `coverage_ratio` falls below 1.0
//! and the strategy refuses to trade on a corrupted sample.

#[derive(Debug, Clone)]
pub struct CoverageTracker {
    minutes: usize,
    observed_seconds: Vec<u32>,
}

impl CoverageTracker {
    pub fn new(minutes: usize) -> Self {
        Self { minutes, observed_seconds: vec![0; minutes] }
    }

    /// Record `seconds` of observed game-clock time within minute `m` (0-indexed).
    pub fn observe(&mut self, m: usize, seconds: u32) {
        if m < self.minutes {
            self.observed_seconds[m] =
                (self.observed_seconds[m] + seconds).min(60);
        }
    }

    /// Mark a gap of `seconds` of unobserved time at minute `m`. (Negative
    /// observation — bounded subtraction.)
    pub fn gap(&mut self, m: usize, seconds: u32) {
        if m < self.minutes {
            self.observed_seconds[m] =
                self.observed_seconds[m].saturating_sub(seconds);
        }
    }

    /// Fraction of total seconds across all tracked minutes that we observed.
    pub fn coverage_ratio(&self) -> f64 {
        let observed: u32 = self.observed_seconds.iter().sum();
        let total = (self.minutes as u32) * 60;
        if total == 0 { 0.0 } else { observed as f64 / total as f64 }
    }
}
