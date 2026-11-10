//! Position sizing via the Kelly criterion, adapted to a binary-outcome
//! order book.
//!
//! A Polymarket share bought at price `c` pays $1 if the outcome resolves YES.
//! With model probability `q`, full Kelly stakes `(q - c) / (1 - c)` of the
//! bankroll at a single price. A real book has several ask levels, so each
//! extra share costs more and the edge shrinks as the order grows.
//! `full_kelly_spend` walks the book and solves the log-utility optimum
//! exactly, level by level.
//!
//! The full-Kelly stake is then scaled by a fixed fraction (the model's
//! probability is an estimate, not the truth) and by an uncertainty factor
//! derived from the bootstrap CI width, and finally clamped to the order
//! limits.

use rust_decimal::prelude::ToPrimitive;

use crate::polymarket::book::PriceLevel;

#[derive(Debug, Clone)]
pub struct SizingParams {
    pub bankroll_usdc: f64,
    /// Multiplier on full Kelly, in (0, 1]. 0.25 = quarter Kelly.
    pub kelly_fraction: f64,
    /// CI width (in points) at or below which no uncertainty haircut applies.
    pub ci_ref_width: f64,
    pub min_order_usdc: f64,
    pub max_order_usdc: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sizing {
    /// USDC to spend on the order.
    pub spend_usdc: f64,
    /// Shares the spend buys when walking the book.
    pub shares: f64,
    /// Volume-weighted average price paid.
    pub vwap: f64,
    /// Unscaled log-optimal spend, kept for logging.
    pub full_kelly_usdc: f64,
}

/// Log-optimal USDC spend when buying shares that pay $1 with probability
/// `q`, walking `asks` (ascending price) from the top.
///
/// With `spent` USDC already paid for `held` shares, wealth is
/// `W_win = B - spent + held` if the outcome hits and `W_lose = B - spent`
/// otherwise. Buying `x` more shares at price `p` changes expected log wealth
/// by a concave function of `x` whose stationary point is
///
///   x* = [q (1 - p) W_lose - (1 - q) p W_win] / (p (1 - p))
///
/// We take `min(x*, level size)` at each level and stop at the first level
/// where `x* <= 0`: prices only rise from there, so no later share helps.
pub fn full_kelly_spend(asks: &[PriceLevel], q: f64, bankroll: f64) -> f64 {
    let mut spent = 0.0;
    let mut held = 0.0;
    for level in asks {
        let (Some(p), Some(size)) = (level.price.to_f64(), level.size.to_f64()) else { break };
        if p <= 0.0 || p >= 1.0 || p >= q {
            break;
        }
        let w_lose = bankroll - spent;
        let w_win = w_lose + held;
        let x_star = (q * (1.0 - p) * w_lose - (1.0 - q) * p * w_win) / (p * (1.0 - p));
        if x_star <= 0.0 {
            break;
        }
        let take = x_star.min(size);
        spent += take * p;
        held += take;
        if take < size {
            break;
        }
    }
    spent
}

/// Haircut applied to the Kelly stake when the projection is uncertain:
/// 1.0 when `ci_width <= ref_width`, otherwise `ref_width / ci_width`.
pub fn uncertainty_scale(ci_width: f64, ref_width: f64) -> f64 {
    if ci_width <= ref_width { 1.0 } else { ref_width / ci_width }
}

/// Size an order against `asks` for model probability `q` and projection CI
/// width `ci_width`. Returns `Err` with a skip reason when no order should be
/// placed.
pub fn size_order(
    asks: &[PriceLevel],
    q: f64,
    ci_width: f64,
    params: &SizingParams,
) -> Result<Sizing, &'static str> {
    let full_kelly_usdc = full_kelly_spend(asks, q, params.bankroll_usdc);
    if full_kelly_usdc <= 0.0 {
        return Err("no kelly edge at book prices");
    }
    let spend_usdc = (full_kelly_usdc
        * params.kelly_fraction
        * uncertainty_scale(ci_width, params.ci_ref_width))
    .min(params.max_order_usdc);
    if spend_usdc < params.min_order_usdc {
        return Err("kelly size below minimum order");
    }
    let shares = shares_for_spend(asks, spend_usdc);
    Ok(Sizing { spend_usdc, shares, vwap: spend_usdc / shares, full_kelly_usdc })
}

/// Shares bought by spending `spend` USDC from the top of `asks`. The caller
/// guarantees `spend` is within the depth `full_kelly_spend` walked.
fn shares_for_spend(asks: &[PriceLevel], spend: f64) -> f64 {
    let mut remaining = spend;
    let mut shares = 0.0;
    for level in asks {
        let (Some(p), Some(size)) = (level.price.to_f64(), level.size.to_f64()) else { break };
        let take = (remaining / p).min(size);
        shares += take;
        remaining -= take * p;
        if remaining <= 0.0 {
            break;
        }
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn level(price: &str, size: &str) -> PriceLevel {
        PriceLevel { price: Decimal::from_str(price).unwrap(), size: Decimal::from_str(size).unwrap() }
    }

    fn params() -> SizingParams {
        SizingParams {
            bankroll_usdc: 1000.0,
            kelly_fraction: 1.0,
            ci_ref_width: 60.0,
            min_order_usdc: 1.0,
            max_order_usdc: 10_000.0,
        }
    }

    /// Expected log wealth after buying `asks` worth `spend` USDC.
    fn expected_log_wealth(asks: &[PriceLevel], q: f64, bankroll: f64, spend: f64) -> f64 {
        let (shares, _) = walk(asks, spend);
        q * (bankroll - spend + shares).ln() + (1.0 - q) * (bankroll - spend).ln()
    }

    fn walk(asks: &[PriceLevel], spend: f64) -> (f64, f64) {
        let mut remaining = spend;
        let mut shares = 0.0;
        for l in asks {
            let p: f64 = l.price.to_string().parse().unwrap();
            let s: f64 = l.size.to_string().parse().unwrap();
            let take = (remaining / p).min(s);
            shares += take;
            remaining -= take * p;
            if remaining <= 1e-12 { break; }
        }
        (shares, spend - remaining)
    }

    #[test]
    fn deep_book_matches_textbook_kelly() {
        // f* = (q - c) / (1 - c) = (0.6 - 0.5) / 0.5 = 0.2 of bankroll.
        let asks = [level("0.50", "1000000")];
        assert_relative_eq!(full_kelly_spend(&asks, 0.6, 1000.0), 200.0, epsilon = 1e-9);
    }

    #[test]
    fn no_edge_spends_nothing() {
        let asks = [level("0.60", "1000000")];
        assert_eq!(full_kelly_spend(&asks, 0.6, 1000.0), 0.0);
        assert_eq!(full_kelly_spend(&asks, 0.55, 1000.0), 0.0);
    }

    #[test]
    fn empty_book_spends_nothing() {
        assert_eq!(full_kelly_spend(&[], 0.9, 1000.0), 0.0);
    }

    #[test]
    fn stops_before_a_level_with_no_marginal_edge() {
        // Level 1 holds only $50. Level 2 at 0.58 still has positive raw edge
        // vs q = 0.6, but after the first fill the marginal log-utility of
        // another share there is negative, so Kelly stops at $50.
        let asks = [level("0.50", "100"), level("0.58", "1000000")];
        assert_relative_eq!(full_kelly_spend(&asks, 0.6, 1000.0), 50.0, epsilon = 1e-9);
    }

    #[test]
    fn never_buys_at_or_above_model_probability() {
        let asks = [level("0.50", "100"), level("0.65", "1000000")];
        assert_relative_eq!(full_kelly_spend(&asks, 0.6, 1000.0), 50.0, epsilon = 1e-9);
    }

    #[test]
    fn multi_level_spend_maximizes_expected_log_wealth() {
        let asks = [level("0.40", "100"), level("0.45", "200"), level("0.50", "1000000")];
        let (q, b) = (0.62, 1000.0);
        let k = full_kelly_spend(&asks, q, b);
        assert!(k > 130.0, "should go past the first two levels, got {k}");
        let best = expected_log_wealth(&asks, q, b, k);
        // Brute-force grid: no other spend does better.
        for i in 1..1000 {
            let s = i as f64 * 0.5;
            assert!(
                expected_log_wealth(&asks, q, b, s) <= best + 1e-12,
                "spend {s} beats Kelly spend {k}"
            );
        }
    }

    #[test]
    fn uncertainty_scale_is_one_for_tight_ci() {
        assert_eq!(uncertainty_scale(40.0, 60.0), 1.0);
        assert_eq!(uncertainty_scale(60.0, 60.0), 1.0);
    }

    #[test]
    fn uncertainty_scale_shrinks_inversely_with_wide_ci() {
        assert_relative_eq!(uncertainty_scale(120.0, 60.0), 0.5);
    }

    #[test]
    fn fraction_and_uncertainty_scale_the_spend() {
        let asks = [level("0.50", "1000000")];
        let p = SizingParams { kelly_fraction: 0.25, ..params() };
        // 200 full Kelly * 0.25 fraction * 0.5 uncertainty = 25.
        let s = size_order(&asks, 0.6, 120.0, &p).unwrap();
        assert_relative_eq!(s.spend_usdc, 25.0, epsilon = 1e-9);
        assert_relative_eq!(s.full_kelly_usdc, 200.0, epsilon = 1e-9);
        assert_relative_eq!(s.shares, 50.0, epsilon = 1e-9);
        assert_relative_eq!(s.vwap, 0.5, epsilon = 1e-9);
    }

    #[test]
    fn spend_is_capped_by_max_order() {
        let asks = [level("0.50", "1000000")];
        let p = SizingParams { max_order_usdc: 30.0, ..params() };
        let s = size_order(&asks, 0.6, 10.0, &p).unwrap();
        assert_relative_eq!(s.spend_usdc, 30.0, epsilon = 1e-9);
    }

    #[test]
    fn vwap_reflects_levels_walked() {
        let asks = [level("0.40", "100"), level("0.50", "1000000")];
        let p = SizingParams { max_order_usdc: 90.0, ..params() };
        // $40 buys 100 @ 0.40, then $50 buys 100 @ 0.50 -> 200 shares, vwap 0.45.
        let s = size_order(&asks, 0.7, 10.0, &p).unwrap();
        assert_relative_eq!(s.shares, 200.0, epsilon = 1e-9);
        assert_relative_eq!(s.vwap, 0.45, epsilon = 1e-9);
    }

    #[test]
    fn below_minimum_order_is_skipped() {
        let asks = [level("0.50", "1000000")];
        let p = SizingParams { bankroll_usdc: 10.0, kelly_fraction: 0.25, min_order_usdc: 1.0, ..params() };
        // 10 * 0.2 * 0.25 * (60/240) = 0.125 < 1.
        assert!(size_order(&asks, 0.6, 240.0, &p).is_err());
    }

    #[test]
    fn no_edge_is_skipped() {
        let asks = [level("0.70", "1000000")];
        assert!(size_order(&asks, 0.6, 10.0, &params()).is_err());
    }
}
