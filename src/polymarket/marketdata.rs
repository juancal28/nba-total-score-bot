//! Wrapper around `polymarket_client_sdk_v2::clob::ws::Client`. Converts the
//! SDK's `BookUpdate` events into our local `OrderBookSnapshot` and publishes
//! them on a `watch::Receiver` for the strategy + exit loops.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::StreamExt;
use tokio::sync::watch;
use tracing::warn;

use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
use polymarket_client_sdk_v2::clob::ws::types::response::{BookUpdate, OrderBookLevel};
use polymarket_client_sdk_v2::types::U256;

use crate::polymarket::book::{OrderBookSnapshot, PriceLevel};

pub struct MarketDataClient {
    inner: WsClient,
}

impl Default for MarketDataClient {
    fn default() -> Self { Self::new() }
}

impl MarketDataClient {
    pub fn new() -> Self {
        Self { inner: WsClient::default() }
    }

    /// Subscribe to live book updates for `asset_id` (a U256 token_id given as
    /// a decimal string). Receivers see `Arc<OrderBookSnapshot>` updated in
    /// place; reads do not block writers.
    pub fn subscribe(&self, asset_id: &str) -> Result<watch::Receiver<Arc<OrderBookSnapshot>>> {
        let asset_id_u256 = U256::from_str(asset_id)?;
        let stream = self.inner.subscribe_orderbook(vec![asset_id_u256])?;

        let (tx, rx) = watch::channel(Arc::new(OrderBookSnapshot::empty(asset_id.to_string())));
        let token_id = asset_id.to_string();

        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            while let Some(item) = stream.next().await {
                match item {
                    Ok(update) => {
                        let snap = book_update_to_snapshot(token_id.clone(), &update);
                        let _ = tx.send(Arc::new(snap));
                    }
                    Err(e) => warn!(error = ?e, "orderbook WS stream error"),
                }
            }
            warn!(token_id = %token_id, "orderbook WS stream ended");
        });

        Ok(rx)
    }
}

fn book_update_to_snapshot(token_id: String, book: &BookUpdate) -> OrderBookSnapshot {
    OrderBookSnapshot {
        token_id,
        bids: book.bids.iter().map(level).collect(),
        asks: book.asks.iter().map(level).collect(),
        last_update: Instant::now(),
    }
}

fn level(l: &OrderBookLevel) -> PriceLevel {
    PriceLevel { price: l.price, size: l.size }
}
