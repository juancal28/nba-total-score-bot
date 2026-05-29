//! In-memory representation of a Polymarket CLOB order book.
//!
//! Populated by the SDK's WebSocket subscription; consumed by `strategy::edge`
//! and `executor::exit` for fill / edge / staleness checks.

use std::time::Instant;

use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct PriceLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderBookSnapshot {
    pub token_id: String,
    /// Descending price.
    pub bids: Vec<PriceLevel>,
    /// Ascending price.
    pub asks: Vec<PriceLevel>,
    /// Monotonic clock at the moment the snapshot was built — `age_ms` measures
    /// elapsed wall-clock time since then, immune to NTP jumps.
    pub last_update: Instant,
}

impl OrderBookSnapshot {
    pub fn empty(token_id: String) -> Self {
        Self { token_id, bids: vec![], asks: vec![], last_update: Instant::now() }
    }

    pub fn best_bid(&self) -> Option<&PriceLevel> { self.bids.first() }
    pub fn best_ask(&self) -> Option<&PriceLevel> { self.asks.first() }

    pub fn mid(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / Decimal::from(2))
    }

    /// VWAP of an ask-side buy for total `size_usdc`. Returns None if depth
    /// is insufficient or no asks exist.
    pub fn vwap_buy(&self, size_usdc: Decimal) -> Option<Decimal> {
        let mut remaining = size_usdc;
        let mut spent = Decimal::ZERO;
        let mut shares = Decimal::ZERO;
        for level in &self.asks {
            let level_notional = level.price * level.size;
            if remaining <= level_notional {
                let take_shares = remaining / level.price;
                spent += remaining;
                shares += take_shares;
                remaining = Decimal::ZERO;
                break;
            }
            spent += level_notional;
            shares += level.size;
            remaining -= level_notional;
        }
        if remaining > Decimal::ZERO || shares.is_zero() { None } else { Some(spent / shares) }
    }

    pub fn age_ms(&self) -> u64 {
        self.last_update.elapsed().as_millis() as u64
    }
}
