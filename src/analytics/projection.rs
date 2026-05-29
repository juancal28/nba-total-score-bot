//! Maps a fitted Poisson model + current scoreboard state to a projected
//! final game total and the probability that the total exceeds the
//! Polymarket line.

use crate::analytics::poisson::PoissonFit;

/// Full NBA game = 4 quarters × 12 minutes (excluding overtime).
pub const GAME_MINUTES: f64 = 48.0;

#[derive(Debug, Clone, Copy)]
pub struct Projection {
    pub current_score: u32,
    pub projected_total: f64,
    pub ci_low: f64,
    pub ci_high: f64,
}

impl Projection {
    /// `remaining_minutes` is the on-clock time left in regulation when the
    /// projection is built (e.g. after Q1+Q2_window = 12 + window minutes,
    /// `remaining = 48 - 12 - window`). `scaling` corrects for the Q1-is-high
    /// bias.
    pub fn build(
        fit: &PoissonFit,
        current_score: u32,
        remaining_minutes: f64,
        scaling: f64,
        ci: (f64, f64),
    ) -> Self {
        let projected_total = current_score as f64 + fit.lambda * scaling * remaining_minutes;
        let (lo, hi) = ci;
        Self { current_score, projected_total, ci_low: lo, ci_high: hi }
    }

    /// Probability that the projected final total exceeds `line`. Approximated
    /// from the bootstrap CI by assuming the projected total is approximately
    /// normal with mean = point estimate, sigma derived from the symmetric
    /// 95% CI half-width.
    pub fn p_over(&self, line: f64) -> f64 {
        let half_width = (self.ci_high - self.ci_low) * 0.5;
        // 95% CI = 1.96 sigma.
        let sigma = (half_width / 1.96).max(1e-6);
        let z = (line - self.projected_total) / sigma;
        // 1 - Phi(z)
        0.5 * crate::analytics::poisson_compat::erfc(z / std::f64::consts::SQRT_2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p_over_below_line_is_lt_half() {
        let fit = PoissonFit { lambda: 2.0, n: 12, total_points: 24 };
        let proj = Projection::build(&fit, 24, 36.0, 1.0, (90.0, 100.0));
        // Projected = 24 + 72 = 96; line above projected -> P(over) < 0.5.
        let p = proj.p_over(110.0);
        assert!(p < 0.5, "p={p}");
    }

    #[test]
    fn p_over_above_line_is_gt_half() {
        let fit = PoissonFit { lambda: 2.0, n: 12, total_points: 24 };
        let proj = Projection::build(&fit, 24, 36.0, 1.0, (90.0, 100.0));
        let p = proj.p_over(80.0);
        assert!(p > 0.5, "p={p}");
    }
}
