//! Unauthenticated Polymarket endpoints: Gamma market discovery and the CLOB
//! REST order book. Used by paper trading, which needs no wallet.

use std::str::FromStr;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use jiff::{SignedDuration, Timestamp};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::polymarket::book::{OrderBookSnapshot, PriceLevel};

pub const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
pub const CLOB_BASE: &str = "https://clob.polymarket.com";

/// A game's total-points market for one line.
#[derive(Debug, Clone, PartialEq)]
pub struct TotalsMarket {
    pub slug: String,
    pub question: String,
    pub line: Decimal,
    pub over_token_id: String,
    pub under_token_id: String,
    pub volume: f64,
    pub taker_base_fee: Option<f64>,
    pub fees_enabled: bool,
    pub closed: bool,
    /// How the market resolved; `None` while open or if it resolved 50-50.
    pub over_won: Option<bool>,
}

/// Polymarket's event slug for an NBA game: `nba-{away}-{home}-{date}`, with
/// lowercase team aliases and the US Eastern calendar date of tipoff.
pub fn event_slug(away_alias: &str, home_alias: &str, scheduled: Timestamp) -> String {
    // Tipoffs fall between noon and 11 PM Eastern, so a fixed UTC-5 shift
    // lands on the right calendar date in both standard and daylight time.
    let eastern = scheduled - SignedDuration::from_hours(5);
    let date = eastern.to_string();
    format!("nba-{}-{}-{}", away_alias.to_lowercase(), home_alias.to_lowercase(), &date[..10])
}

/// From a Gamma `/events?slug=` response, pick the full-game totals market
/// with the highest volume. `None` if the event or a totals market is missing.
pub fn main_totals_market(gamma_events_json: &[u8]) -> Result<Option<TotalsMarket>> {
    let events: Vec<GammaEvent> = serde_json::from_slice(gamma_events_json)?;
    let Some(event) = events.into_iter().next() else { return Ok(None) };
    let main = event
        .markets
        .into_iter()
        .filter(|m| m.sports_market_type.as_deref() == Some("totals") && m.line.is_some())
        .max_by(|a, b| a.volume().total_cmp(&b.volume()));
    let Some(m) = main else { return Ok(None) };

    let tokens: Vec<String> =
        serde_json::from_str(&m.clob_token_ids).context("clobTokenIds is not a JSON array")?;
    let [over, under] = <[String; 2]>::try_from(tokens)
        .map_err(|t| anyhow!("expected 2 outcome tokens, got {}", t.len()))?;
    let line = m.line.unwrap_or_default();
    // Resolved markets report outcome prices of exactly "1" and "0".
    let over_won = m
        .outcome_prices
        .as_deref()
        .filter(|_| m.closed)
        .and_then(|p| serde_json::from_str::<Vec<String>>(p).ok())
        .and_then(|p| match p.first().map(String::as_str) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        });
    Ok(Some(TotalsMarket {
        slug: event.slug,
        volume: m.volume(),
        question: m.question,
        line: Decimal::from_str(&line.to_string())?,
        over_token_id: over,
        under_token_id: under,
        taker_base_fee: m.taker_base_fee,
        fees_enabled: m.fees_enabled,
        closed: m.closed,
        over_won,
    }))
}

/// Parse a CLOB `/book` response into a snapshot with bids descending and
/// asks ascending (the API returns both with the best price last).
pub fn parse_book(token_id: &str, body: &[u8]) -> Result<OrderBookSnapshot> {
    let raw: RawBook = serde_json::from_slice(body)?;
    if let Some(error) = raw.error {
        return Err(anyhow!("no order book for token {token_id}: {error}"));
    }
    let levels = |raw: Vec<RawLevel>| -> Result<Vec<PriceLevel>> {
        raw.into_iter()
            .map(|l| {
                Ok(PriceLevel {
                    price: Decimal::from_str(&l.price)?,
                    size: Decimal::from_str(&l.size)?,
                })
            })
            .collect()
    };
    let mut bids = levels(raw.bids)?;
    let mut asks = levels(raw.asks)?;
    bids.sort_by(|a, b| b.price.cmp(&a.price));
    asks.sort_by(|a, b| a.price.cmp(&b.price));
    Ok(OrderBookSnapshot { token_id: token_id.to_string(), bids, asks, last_update: Instant::now() })
}

#[derive(Debug, Deserialize)]
struct GammaEvent {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    #[serde(default)]
    question: String,
    #[serde(default)]
    sports_market_type: Option<String>,
    #[serde(default)]
    line: Option<f64>,
    /// Decimal string, e.g. "52834.93".
    #[serde(default)]
    volume: Option<String>,
    /// JSON-encoded array of [over, under] token ids.
    #[serde(default)]
    clob_token_ids: String,
    #[serde(default)]
    taker_base_fee: Option<f64>,
    #[serde(default)]
    fees_enabled: bool,
    #[serde(default)]
    closed: bool,
    /// JSON-encoded array of final prices, e.g. `["0", "1"]`.
    #[serde(default)]
    outcome_prices: Option<String>,
}

impl GammaMarket {
    fn volume(&self) -> f64 {
        self.volume.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0.0)
    }
}

#[derive(Debug, Deserialize)]
struct RawBook {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

pub async fn fetch_main_totals_market(
    http: &reqwest::Client,
    slug: &str,
) -> Result<Option<TotalsMarket>> {
    let body = http
        .get(format!("{GAMMA_BASE}/events"))
        .query(&[("slug", slug)])
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    main_totals_market(&body)
}

/// One point of a token's price history: unix seconds and price.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct PricePoint {
    pub t: i64,
    pub p: f64,
}

#[derive(Debug, Deserialize)]
struct PriceHistory {
    #[serde(default)]
    history: Vec<PricePoint>,
}

/// Parse a CLOB `/prices-history` response, sorted by time.
pub fn parse_price_history(body: &[u8]) -> Result<Vec<PricePoint>> {
    let mut history = serde_json::from_slice::<PriceHistory>(body)?.history;
    history.sort_by_key(|p| p.t);
    Ok(history)
}

/// Latest price at or before `t`, if one exists within `max_age_secs`.
pub fn price_at(history: &[PricePoint], t: i64, max_age_secs: i64) -> Option<f64> {
    let idx = history.partition_point(|p| p.t <= t);
    let point = history.get(idx.checked_sub(1)?)?;
    (t - point.t <= max_age_secs).then_some(point.p)
}

/// Raw one-minute price history for `token_id` between two unix times.
/// (`interval=max` returns nothing for older markets; always pass a range.)
pub async fn fetch_price_history_raw(
    http: &reqwest::Client,
    token_id: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<Vec<u8>> {
    let resp = http
        .get(format!("{CLOB_BASE}/prices-history"))
        .query(&[
            ("market", token_id.to_string()),
            ("startTs", start_ts.to_string()),
            ("endTs", end_ts.to_string()),
            ("fidelity", "1".to_string()),
        ])
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Raw Gamma `/events?slug=` response body.
pub async fn fetch_event_raw(http: &reqwest::Client, slug: &str) -> Result<Vec<u8>> {
    let resp = http
        .get(format!("{GAMMA_BASE}/events"))
        .query(&[("slug", slug)])
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

pub async fn fetch_book(http: &reqwest::Client, token_id: &str) -> Result<OrderBookSnapshot> {
    let body = http
        .get(format!("{CLOB_BASE}/book"))
        .query(&[("token_id", token_id)])
        .send()
        .await?
        .bytes()
        .await?;
    parse_book(token_id, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: &str = r#"[{
        "slug": "nba-mil-nyk-2026-11-10",
        "title": "Bucks vs. Knicks",
        "markets": [
            {"question": "Bucks vs. Knicks: O/U 220.5", "sportsMarketType": "totals", "line": 220.5,
             "volume": "1500.5", "clobTokenIds": "[\"11\", \"12\"]", "takerBaseFee": 1000, "feesEnabled": true},
            {"question": "Bucks vs. Knicks: O/U 222.5", "sportsMarketType": "totals", "line": 222.5,
             "volume": "52834.932857", "clobTokenIds": "[\"21\", \"22\"]", "takerBaseFee": 1000, "feesEnabled": true},
            {"question": "Bucks vs. Knicks: 1Q O/U 56.5", "sportsMarketType": "q1_totals", "line": 56.5,
             "volume": "900000", "clobTokenIds": "[\"31\", \"32\"]"},
            {"question": "Bucks vs. Knicks", "sportsMarketType": "moneyline",
             "volume": "2000000", "clobTokenIds": "[\"41\", \"42\"]"}
        ]
    }]"#;

    #[test]
    fn slug_uses_away_home_and_eastern_date() {
        // 00:30 UTC on Nov 11 is 7:30 PM ET on Nov 10.
        let t: Timestamp = "2026-11-11T00:30:00Z".parse().unwrap();
        assert_eq!(event_slug("MIL", "NYK", t), "nba-mil-nyk-2026-11-10");
    }

    #[test]
    fn main_market_is_highest_volume_full_game_total() {
        let m = main_totals_market(EVENTS.as_bytes()).unwrap().unwrap();
        assert_eq!(m.line, Decimal::from_str("222.5").unwrap());
        assert_eq!(m.over_token_id, "21");
        assert_eq!(m.under_token_id, "22");
        assert_eq!(m.slug, "nba-mil-nyk-2026-11-10");
        assert_eq!(m.taker_base_fee, Some(1000.0));
        assert!(m.fees_enabled);
    }

    #[test]
    fn missing_event_or_totals_market_is_none() {
        assert_eq!(main_totals_market(b"[]").unwrap(), None);
        let no_totals = r#"[{"slug": "x", "markets": [{"sportsMarketType": "moneyline", "volume": "5", "clobTokenIds": "[\"1\", \"2\"]"}]}]"#;
        assert_eq!(main_totals_market(no_totals.as_bytes()).unwrap(), None);
    }

    #[test]
    fn book_sides_are_sorted_best_first() {
        let body = r#"{"asset_id": "21",
            "bids": [{"price": "0.01", "size": "100"}, {"price": "0.47", "size": "50"}, {"price": "0.48", "size": "20"}],
            "asks": [{"price": "0.99", "size": "100"}, {"price": "0.5", "size": "30"}, {"price": "0.49", "size": "10"}]}"#;
        let b = parse_book("21", body.as_bytes()).unwrap();
        let prices = |ls: &[PriceLevel]| ls.iter().map(|l| l.price.to_string()).collect::<Vec<_>>();
        assert_eq!(prices(&b.bids), ["0.48", "0.47", "0.01"]);
        assert_eq!(prices(&b.asks), ["0.49", "0.5", "0.99"]);
        assert_eq!(b.asks[0].size, Decimal::from(10));
    }

    #[test]
    fn resolved_market_reports_the_winner() {
        let body = r#"[{"slug": "s", "markets": [{"sportsMarketType": "totals", "line": 213.5, "volume": "9",
            "clobTokenIds": "[\"1\", \"2\"]", "closed": true, "outcomePrices": "[\"0\", \"1\"]"}]}]"#;
        let m = main_totals_market(body.as_bytes()).unwrap().unwrap();
        assert!(m.closed);
        assert_eq!(m.over_won, Some(false));
        // The open market in EVENTS has no winner yet.
        assert_eq!(main_totals_market(EVENTS.as_bytes()).unwrap().unwrap().over_won, None);
    }

    #[test]
    fn price_at_takes_the_latest_point_not_after_t() {
        let h = parse_price_history(br#"{"history":[{"t":160,"p":0.6},{"t":100,"p":0.5}]}"#).unwrap();
        assert_eq!(price_at(&h, 99, 300), None);
        assert_eq!(price_at(&h, 100, 300), Some(0.5));
        assert_eq!(price_at(&h, 159, 300), Some(0.5));
        assert_eq!(price_at(&h, 400, 300), Some(0.6));
        assert_eq!(price_at(&h, 461, 300), None);
    }

    #[test]
    fn missing_book_is_an_error() {
        let body = r#"{"error": "No orderbook exists for the requested token id"}"#;
        assert!(parse_book("21", body.as_bytes()).is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Hits the real Polymarket APIs. Run with
    /// `NPB_LIVE_SLUG=nba-mil-nyk-2026-11-10 cargo test -- --ignored live_market`.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_market_and_book_parse() {
        let slug = std::env::var("NPB_LIVE_SLUG").expect("set NPB_LIVE_SLUG to an open game's slug");
        let http = reqwest::Client::new();
        let market = fetch_main_totals_market(&http, &slug).await.unwrap().expect("totals market");
        let book = fetch_book(&http, &market.over_token_id).await.unwrap();
        let (bid, ask) = (book.best_bid().unwrap().price, book.best_ask().unwrap().price);
        println!("{} line {} bid {bid} ask {ask}", market.question, market.line);
        assert!(bid < ask);
    }
}
