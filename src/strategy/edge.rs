//! Translates a model probability into a concrete trade decision against the
//! Polymarket order book, accounting for VWAP slippage and book staleness.

use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::sync::Arc;

use crate::polymarket::book::{OrderBookSnapshot, PriceLevel};
use crate::strategy::sizing::{SizingParams, size_order};
use crate::strategy::{BuyOutcome, TradeIntent};

pub enum EdgeDecision {
    Trade(TradeIntent),
    Skip(&'static str),
}

/// Given the modeled `p_over` (probability that final total exceeds the line),
/// compare against the implied market probability for the OVER token and
/// decide whether to buy OVER, buy UNDER, or skip. A trade is sized with
/// fractional Kelly (see `strategy::sizing`); `ci_width` is the width of the
/// projection's bootstrap CI.
///
/// `book` is the snapshot for the OVER token. Implied P(over) = best ask.
/// (Polymarket prices are USDC per share, with each share paying $1 if the
/// outcome resolves YES — so the ask is the market's break-even probability.)
pub fn evaluate_edge(
    book: &Arc<OrderBookSnapshot>,
    p_over: f64,
    ci_width: f64,
    min_edge: f64,
    max_book_age_ms: u64,
    sizing: &SizingParams,
) -> EdgeDecision {
    if book.age_ms() > max_book_age_ms {
        return EdgeDecision::Skip("book stale");
    }
    let Some(best_ask) = book.best_ask() else {
        return EdgeDecision::Skip("no asks");
    };
    let Some(best_bid) = book.best_bid() else {
        return EdgeDecision::Skip("no bids");
    };

    let ask = best_ask.price.to_string().parse::<f64>().unwrap_or(1.0);
    let bid = best_bid.price.to_string().parse::<f64>().unwrap_or(0.0);

    // Buying OVER profits if true P(over) > ask price.
    if p_over - ask >= min_edge {
        return sized_trade(BuyOutcome::Over, &book.asks, p_over, ci_width, sizing);
    }

    // Buying UNDER profits if true P(under) > (1 - bid). An UNDER buy at
    // 1 - b is matched against an OVER bid at b, so the OVER bids (descending)
    // become the UNDER asks (ascending) with the same share sizes.
    let p_under = 1.0 - p_over;
    let implied_under_ask = 1.0 - bid;
    if p_under - implied_under_ask >= min_edge {
        let under_asks: Vec<PriceLevel> = book
            .bids
            .iter()
            .map(|l| PriceLevel { price: Decimal::ONE - l.price, size: l.size })
            .collect();
        return sized_trade(BuyOutcome::Under, &under_asks, p_under, ci_width, sizing);
    }

    EdgeDecision::Skip("no edge in either direction")
}

fn sized_trade(
    buy_side: BuyOutcome,
    asks: &[PriceLevel],
    q: f64,
    ci_width: f64,
    sizing: &SizingParams,
) -> EdgeDecision {
    let s = match size_order(asks, q, ci_width, sizing) {
        Ok(s) => s,
        Err(reason) => return EdgeDecision::Skip(reason),
    };
    let (Some(size_usdc), Some(max_vwap)) = (Decimal::from_f64(s.spend_usdc), Decimal::from_f64(s.vwap))
    else {
        return EdgeDecision::Skip("non-finite kelly size");
    };
    EdgeDecision::Trade(TradeIntent {
        buy_side,
        size_usdc: size_usdc.round_dp_with_strategy(2, RoundingStrategy::ToZero),
        max_vwap: max_vwap.round_dp_with_strategy(4, RoundingStrategy::AwayFromZero),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::time::Instant;

    fn level(price: &str, size: &str) -> PriceLevel {
        PriceLevel { price: Decimal::from_str(price).unwrap(), size: Decimal::from_str(size).unwrap() }
    }

    fn book(bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) -> Arc<OrderBookSnapshot> {
        Arc::new(OrderBookSnapshot { token_id: "over".into(), bids, asks, last_update: Instant::now() })
    }

    fn sizing() -> SizingParams {
        SizingParams {
            bankroll_usdc: 1000.0,
            kelly_fraction: 0.25,
            ci_ref_width: 60.0,
            min_order_usdc: 1.0,
            max_order_usdc: 100.0,
        }
    }

    #[test]
    fn over_trade_is_kelly_sized() {
        let b = book(vec![level("0.48", "1000000")], vec![level("0.50", "1000000")]);
        // Full Kelly = 1000 * (0.6 - 0.5) / 0.5 = 200; quarter Kelly = 50.
        let EdgeDecision::Trade(t) = evaluate_edge(&b, 0.6, 10.0, 0.05, 500, &sizing()) else {
            panic!("expected trade");
        };
        assert!(matches!(t.buy_side, BuyOutcome::Over));
        assert_eq!(t.size_usdc, Decimal::from_str("50.00").unwrap());
        assert_eq!(t.max_vwap, Decimal::from_str("0.5").unwrap());
    }

    #[test]
    fn under_trade_is_limited_by_bid_depth() {
        // Buying UNDER at 1 - 0.40 = 0.60 matches against the OVER bids;
        // only 50 shares ($30) are available there.
        let b = book(vec![level("0.40", "50")], vec![level("0.65", "1000000")]);
        let p = SizingParams { kelly_fraction: 1.0, ..sizing() };
        let EdgeDecision::Trade(t) = evaluate_edge(&b, 0.2, 10.0, 0.05, 500, &p) else {
            panic!("expected trade");
        };
        assert!(matches!(t.buy_side, BuyOutcome::Under));
        assert_eq!(t.size_usdc, Decimal::from_str("30.00").unwrap());
        assert_eq!(t.max_vwap, Decimal::from_str("0.6").unwrap());
    }

    #[test]
    fn tiny_kelly_size_skips() {
        let b = book(vec![level("0.48", "1000000")], vec![level("0.50", "1000000")]);
        let p = SizingParams { bankroll_usdc: 10.0, ..sizing() };
        // 10 * 0.2 * 0.25 * (60 / 240) = 0.125 USDC, under the $1 minimum.
        assert!(matches!(evaluate_edge(&b, 0.6, 240.0, 0.05, 500, &p), EdgeDecision::Skip(_)));
    }
}
