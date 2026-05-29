//! Poisson-process model of per-minute scoring.
//!
//! Each minute of game-clock is a "trial" with observed point total `x_i`.
//! Under a homogeneous Poisson assumption with rate `lambda` (points per
//! minute), the MLE of `lambda` is the sample mean.
//!
//! We use a likelihood-ratio test to check whether the Q1 and early-Q2
//! samples were drawn from the same Poisson process. Bootstrap CIs are
//! produced for the projected full-game total.

use rand::Rng;
use rand::seq::SliceRandom;

/// Result of fitting a Poisson process to a per-minute scoring sample.
#[derive(Debug, Clone)]
pub struct PoissonFit {
    /// MLE of points-per-minute.
    pub lambda: f64,
    /// Number of minutes observed.
    pub n: usize,
    /// Total points observed.
    pub total_points: u32,
}

impl PoissonFit {
    pub fn fit(per_minute: &[u32]) -> Self {
        let n = per_minute.len();
        let total: u32 = per_minute.iter().sum();
        let lambda = if n == 0 { 0.0 } else { total as f64 / n as f64 };
        Self { lambda, n, total_points: total }
    }
}

/// Two-sample likelihood-ratio test for equality of Poisson rates.
///
/// H0: lambda_1 == lambda_2.  H1: lambda_1 != lambda_2.
/// Test statistic: 2 * [ log L(unrestricted) - log L(pooled) ], chi-squared
/// distributed with 1 d.o.f. under H0. Returns the upper-tail p-value.
pub fn lr_test_two_sample(a: &PoissonFit, b: &PoissonFit) -> f64 {
    if a.n == 0 || b.n == 0 {
        return 0.0;
    }
    let pooled_lambda = (a.total_points + b.total_points) as f64 / (a.n + b.n) as f64;
    if pooled_lambda <= 0.0 {
        return 1.0;
    }

    let ln = |x: f64| if x > 0.0 { x.ln() } else { 0.0 };
    let log_l_pooled = (a.total_points as f64 + b.total_points as f64) * ln(pooled_lambda)
        - ((a.n + b.n) as f64) * pooled_lambda;
    let log_l_free = (a.total_points as f64) * ln(a.lambda) - (a.n as f64) * a.lambda
        + (b.total_points as f64) * ln(b.lambda)
        - (b.n as f64) * b.lambda;

    let stat = 2.0 * (log_l_free - log_l_pooled);
    chi2_sf_1dof(stat)
}

/// Upper-tail (survival) of chi-squared with 1 d.o.f. evaluated at `x`.
/// Uses the identity SF_chi2_1(x) = 2 * (1 - Phi(sqrt(x))) = erfc(sqrt(x/2)).
fn chi2_sf_1dof(x: f64) -> f64 {
    if x <= 0.0 { return 1.0; }
    erfc((x * 0.5).sqrt())
}

/// Abramowitz & Stegun 7.1.26 approximation of the complementary error function.
fn erfc(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-x * x).exp();
    1.0 - sign * y
}

/// Bootstrap a confidence interval for the projected full-game total given a
/// per-minute sample from Q1 and a remaining-minutes count to extrapolate over.
/// Returns (lower, point_estimate, upper) at the requested two-sided alpha.
pub fn bootstrap_projected_total(
    per_minute: &[u32],
    current_score: u32,
    remaining_minutes: f64,
    scaling: f64,
    iters: usize,
    alpha: f64,
) -> (f64, f64, f64) {
    if per_minute.is_empty() {
        let point = current_score as f64;
        return (point, point, point);
    }

    let mut rng = rand::thread_rng();
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let n = per_minute.len();

    for _ in 0..iters {
        let mut sum = 0u32;
        for _ in 0..n {
            sum += per_minute[rng.gen_range(0..n)];
        }
        let lambda = sum as f64 / n as f64;
        let projected = current_score as f64 + lambda * scaling * remaining_minutes;
        samples.push(projected);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let lo_idx = ((alpha * 0.5) * iters as f64).floor() as usize;
    let hi_idx = (((1.0 - alpha * 0.5)) * iters as f64).floor() as usize;
    let point = current_score as f64
        + (per_minute.iter().sum::<u32>() as f64 / n as f64) * scaling * remaining_minutes;

    (samples[lo_idx], point, samples[hi_idx.min(iters - 1)])
}

/// Permutation/bootstrap of (per_minute_q1, per_minute_q2_window) under H0 that
/// they share a single Poisson rate. Returns the proportion of bootstrap LR
/// statistics that exceed the observed one — a Monte Carlo p-value that is
/// more conservative than the asymptotic chi-squared for very small samples.
pub fn lr_test_bootstrap(q1: &[u32], q2: &[u32], iters: usize) -> f64 {
    let a = PoissonFit::fit(q1);
    let b = PoissonFit::fit(q2);
    if a.n == 0 || b.n == 0 { return 1.0; }

    let pooled_lambda = (a.total_points + b.total_points) as f64 / (a.n + b.n) as f64;
    let ln = |x: f64| if x > 0.0 { x.ln() } else { 0.0 };
    let observed_stat = 2.0
        * ((a.total_points as f64) * ln(a.lambda) - (a.n as f64) * a.lambda
            + (b.total_points as f64) * ln(b.lambda)
            - (b.n as f64) * b.lambda
            - ((a.total_points + b.total_points) as f64) * ln(pooled_lambda)
            + ((a.n + b.n) as f64) * pooled_lambda);

    let pooled: Vec<u32> = q1.iter().chain(q2.iter()).copied().collect();
    let mut rng = rand::thread_rng();
    let mut extreme = 0usize;

    for _ in 0..iters {
        let mut shuffled = pooled.clone();
        shuffled.shuffle(&mut rng);
        let (sa, sb) = shuffled.split_at(q1.len());
        let fa = PoissonFit::fit(sa);
        let fb = PoissonFit::fit(sb);
        if fa.lambda <= 0.0 || fb.lambda <= 0.0 {
            continue;
        }
        let stat = 2.0
            * ((fa.total_points as f64) * ln(fa.lambda) - (fa.n as f64) * fa.lambda
                + (fb.total_points as f64) * ln(fb.lambda)
                - (fb.n as f64) * fb.lambda
                - ((fa.total_points + fb.total_points) as f64) * ln(pooled_lambda)
                + ((fa.n + fb.n) as f64) * pooled_lambda);
        if stat >= observed_stat {
            extreme += 1;
        }
    }

    (extreme as f64) / (iters as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn mle_lambda_is_sample_mean() {
        let f = PoissonFit::fit(&[2, 3, 1, 4, 0, 2, 3, 1, 2, 0, 3, 2]);
        assert_relative_eq!(f.lambda, 23.0 / 12.0, epsilon = 1e-9);
    }

    #[test]
    fn lr_test_identical_samples_yields_high_p() {
        // Truly identical means: both samples have lambda = 2.
        let q1 = vec![2u32; 12];
        let q2 = vec![2u32; 4];
        let a = PoissonFit::fit(&q1);
        let b = PoissonFit::fit(&q2);
        let p = lr_test_two_sample(&a, &b);
        assert!(p > 0.9, "identical sample means should give p~1; got {p}");
    }

    #[test]
    fn lr_test_divergent_samples_yields_low_p() {
        let q1 = vec![0u32; 12];
        let q2 = vec![5u32, 5, 5, 5, 5];
        let a = PoissonFit::fit(&q1);
        let b = PoissonFit::fit(&q2);
        let p = lr_test_two_sample(&a, &b);
        assert!(p < 0.05, "diverging sample means should give small p; got {p}");
    }

    #[test]
    fn bootstrap_interval_contains_point_estimate() {
        let q1 = vec![2u32, 3, 1, 4, 0, 2, 3, 1, 2, 0, 3, 2];
        let (lo, point, hi) =
            bootstrap_projected_total(&q1, 23, 36.0, 0.97, 1000, 0.05);
        assert!(lo <= point && point <= hi);
        assert!(point > 23.0);
    }
}
