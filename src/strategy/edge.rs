//! Translates a model probability into a concrete trade decision against the
//! Polymarket order book, accounting for VWAP slippage and book staleness.

use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;

use crate::polymarket::book::OrderBookSnapshot;
use crate::strategy::{BuyOutcome, TradeIntent};

pub enum EdgeDecision {
    Trade(TradeIntent),
    Skip(&'static str),
}

/// Given the modeled `p_over` (probability that final total exceeds the line),
/// compare against the implied market probability for the OVER token and
/// decide whether to buy OVER, buy UNDER, or skip.
///
/// `book` is the snapshot for the OVER token. Implied P(over) = best ask.
/// (Polymarket prices are USDC per share, with each share paying $1 if the
/// outcome resolves YES — so the ask is the market's break-even probability.)
pub fn evaluate_edge(
    book: &Arc<OrderBookSnapshot>,
    p_over: f64,
    min_edge: f64,
    max_book_age_ms: u64,
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
        let size = Decimal::from_str("10").unwrap(); // placeholder — wired from config in caller
        let Some(vwap) = book.vwap_buy(size) else {
            return EdgeDecision::Skip("insufficient depth for OVER");
        };
        return EdgeDecision::Trade(TradeIntent {
            buy_side: BuyOutcome::Over,
            size_usdc: size,
            max_vwap: vwap,
        });
    }

    // Buying UNDER profits if true P(under) > (1 - bid).
    let p_under = 1.0 - p_over;
    let implied_under_ask = 1.0 - bid;
    if p_under - implied_under_ask >= min_edge {
        let size = Decimal::from_str("10").unwrap();
        return EdgeDecision::Trade(TradeIntent {
            buy_side: BuyOutcome::Under,
            size_usdc: size,
            max_vwap: Decimal::from_str(&implied_under_ask.to_string()).unwrap_or(Decimal::ONE),
        });
    }

    EdgeDecision::Skip("no edge in either direction")
}
