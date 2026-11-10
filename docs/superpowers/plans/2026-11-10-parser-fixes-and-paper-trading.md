# Parser Fixes and Paper Trading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the live SportRadar parser so the engine sees real scores, and add a `paper` command that watches tonight's games and logs the trades the bot would make, without a wallet and within the SportRadar trial budget.

**Architecture:** The parser fixes are the "Prerequisite fixes" section of the backtester spec, done first because paper trading and the backtest both depend on them. Paper mode discovers the day's games (SportRadar daily schedule + Polymarket Gamma), then per game polls play-by-play sparingly, drives the unmodified decision logic of `StrategyEngine` synchronously, and at decision time sizes against the real Polymarket order book fetched over public REST. Decisions are appended to a JSONL log; nothing is signed or submitted.

**Tech Stack:** Rust (existing crate), tokio, reqwest, serde, jiff.

**Spec:** `docs/superpowers/specs/2026-10-28-backtester-design.md` (prerequisite fixes, data-source facts). Paper mode itself was requested directly by the owner on 2026-11-10 and is specified here. The rest of the backtester gets its own plan.

## Global Constraints

- SportRadar trial: 1,000 requests per rolling 30 days, 1 request/second. Paper mode must average roughly 10 requests per game or fewer. Never log the API key.
- Paper mode must not require `NPB_WALLET__PRIVATE_KEY` and must never construct `PolymarketClient` or submit an order.
- The engine's decision logic (gates, thresholds, sizing) is not changed, apart from the coverage fix below.
- Polymarket slug: `nba-{away}-{home}-{YYYY-MM-DD}` with lowercase SportRadar aliases and the US Eastern game date (verified for all six 2026-11-10 games).
- Main line = the `sportsMarketType == "totals"` market with the highest `volume`; `clobTokenIds[0]` is Over.

## Review Focus

- Overtime periods arrive as `type: "overtime"`, `number: 1, 2…`; they must not be read as Q1/Q2.
- A poll that lands deep in Q2 (several minutes past the window) must still produce exactly one decision, using only the first `q2_window_minutes` of Q2.
- A game with no Polymarket totals market, or an empty order book at decision time, is logged as skipped, not a crash, and does not stop other games.
- SportRadar HTTP errors / 429s during a poll are retried on the next poll, not treated as lost Q1 coverage that kills the game.
- A game that is postponed or never starts stops polling after a bounded time.

---

### Task 1: Parser reads real SportRadar payloads

**Files:**
- Modify: `src/feed/sportradar.rs`
- Create: `tests/fixtures/sportradar_pbp_okc_hou.json` (trimmed from the cached game `820c5325-…`: all periods; per event only `id`, `event_type`, `clock`, `wall_clock`, `home_points`, `away_points`; per period `number`, `type`)

**Interfaces:**
- Produces: `pub struct PbpCursor` (`Default`; holds seen ids and last home/away score); `pub fn parse_pbp(body: &[u8], cursor: &mut PbpCursor) -> anyhow::Result<Vec<GameEvent>>` returning only events not yet seen; `pub struct PbpSummary { status: String, home_points: u32, away_points: u32 }` via `pub fn pbp_summary(body: &[u8]) -> anyhow::Result<PbpSummary>`.
- `GameEvent::Score.points` and `team` come from the change in `home_points` / `away_points` versus the previous event; `at` on every event is the event's `wall_clock`; period = `number` for `type == "quarter"`, `number + 4` for `"overtime"`.

- [ ] Tests (fixture-driven): Q1 `Score` events sum to 57 with per-minute buckets `[5,4,5,5,6,7,8,6,1,2,8,0]`; home Q1 = 27, away Q1 = 30; a `threepointmade` yields `points == 3` and a `freethrowmade` yields 1; first Q2 event's `at` equals its `wall_clock`; the overtime periods produce `period` 5 and 6 and exactly one `PeriodEnd { period: 1 }` in the whole game; parsing the same body twice with one cursor yields no events the second time.
- [ ] Run, see them fail; implement; run, see them pass; the poller (`SportRadarPoller::run`) uses `parse_pbp` with a `PbpCursor`.
- [ ] Commit.

### Task 2: Coverage counts scoring events; engine is drivable synchronously

**Files:**
- Modify: `src/strategy/mod.rs`

**Interfaces:**
- Produces: `pub fn handle_event(&mut self, ev: &GameEvent)` and `pub fn evaluate(&self, book: Arc<OrderBookSnapshot>) -> Evaluation` where `pub struct Evaluation { pub intent: Option<TradeIntent>, pub skip_reason: Option<String>, pub projection: Option<Projection>, pub p_over: Option<f64>, pub coverage: f64, pub q1_margin: i32, pub lr_p: Option<f64> }`. `run` keeps its signature and behavior, built on these.
- `Score` events in period 1 mark their minute observed, same as `ClockUpdate`.

- [ ] Tests: feeding the Task 1 fixture's events through `handle_event` reaches `Phase::EdgeEvaluation`, with `coverage.coverage_ratio() == 1.0` and `q1_home`/`q1_away` = 27/30; a synthetic Q1 where one minute has only `Score` events still has coverage 1.0; `evaluate` on a blowout Q1 returns `skip_reason` containing "blowout" and `intent == None`.
- [ ] Red, green, commit.

### Task 3: Polymarket public data (Gamma discovery + REST book)

**Files:**
- Create: `src/polymarket/public.rs`; register in `src/polymarket/mod.rs`

**Interfaces:**
- Produces: `pub struct TotalsMarket { pub slug: String, pub question: String, pub line: Decimal, pub over_token_id: String, pub under_token_id: String, pub volume: f64, pub taker_base_fee: Option<f64>, pub fees_enabled: bool }`; `pub fn event_slug(away_alias: &str, home_alias: &str, scheduled_utc: Timestamp) -> String`; `pub fn main_totals_market(gamma_events_json: &[u8]) -> anyhow::Result<Option<TotalsMarket>>`; `pub fn parse_book(token_id: &str, body: &[u8]) -> anyhow::Result<OrderBookSnapshot>` (bids descending, asks ascending, regardless of the API's order); async `fetch_main_totals_market(http, slug)` and `fetch_book(http, token_id)` wrappers over `https://gamma-api.polymarket.com/events?slug=` and `https://clob.polymarket.com/book?token_id=`.

- [ ] Tests (inline JSON samples in the real shapes): `event_slug("MIL","NYK", 2026-11-11T00:30Z) == "nba-mil-nyk-2026-11-10"`; `main_totals_market` picks the highest-volume totals market and ignores `q1_totals`; returns `None` for `[]`; `parse_book` sorts sides correctly and returns an empty-sided snapshot for `{"error": …}`-free empty books.
- [ ] Red, green, commit.

### Task 4: `paper` command

**Files:**
- Create: `src/paper.rs`; modify `src/lib.rs`, `src/main.rs`, `README.md`

**Interfaces:**
- CLI: `nba_poly_bot paper [--date YYYY-MM-DD] [--games <id,id,…>]` (default date: today, US Eastern).
- Flow per game: sleep until `scheduled + 12 min`; poll pbp; feed new events to the engine until `Phase::EdgeEvaluation` or `Done`; if not there yet, sleep `clamp(1.5 × game-seconds until Q2 window end, 45 s, 600 s)` and poll again; give up 3 h after scheduled start. At decision: re-fetch the main totals market and its Over book, `evaluate`, append one JSON line to `data/paper/decisions.jsonl` with game id, teams, slug, line, decision time, `Evaluation` fields, best bid/ask, side, size, vwap, fee fields, and the number of SportRadar requests used. All SportRadar requests go through one shared 1.1 s-spaced limiter.
- Produces: `pub fn next_poll_delay(period: u8, game_clock_seconds: u32, q2_window_minutes: u32) -> Duration` (pure, tested).

- [ ] Tests: `next_poll_delay` at Q1 12:00 remaining returns 600 s (clamped), at Q2 10:30 remaining with a 2-minute window returns 45 s; a decision record round-trips through serde.
- [ ] Red, green; run `cargo test`; smoke-run `paper --date` against a finished game from the cache path to see one decision line (no SportRadar requests when the body is supplied from disk via a hidden `--pbp-file` flag).
- [ ] Commit.
