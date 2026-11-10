# Backtester design

Date: 2026-10-28
Status: draft for review

## Why

The bot has never been run. Its owner wants to (a) find out whether the Q1
Poisson model actually predicts NBA game totals, (b) compare position-sizing
schemes (flat vs fractional Kelly, added on `feature/kelly-sizing`), and later
(c) measure their own model changes against a baseline. The project is also a
resume piece, so results need to be defensible: real games, real market
prices, reproducible numbers, and clear statements of what is simulated.

The primary purpose is **checking the model**. Profit numbers are secondary
and come with explicit caveats.

## Goals

- Replay past NBA games through the bot's real decision code
  (`StrategyEngine`), not a re-implementation, using real play-by-play and the
  real Polymarket price at the moment the bot would have decided.
- Produce a side-by-side report where each column is a variant (a set of
  config overrides) and rows are metrics. Initial variants: flat $10,
  quarter Kelly, half Kelly, full Kelly.
- Make adding a variant a config change, so the owner's later model changes
  become new columns.
- Download every external datum once, cache it on disk, and never re-fetch.

## Non-goals

- Simulating the order book faithfully. Only a one-price-per-minute history
  exists; book depth is assumed (see "Synthetic book").
- Modelling the early-exit logic (`executor::exit`, take-profit at 0.98 /
  cut-loss at 0.02). Positions are held to resolution.
- Changing the model itself. That is the next project.
- A UI. Output is files.

## Anti-goals

- A backtest whose numbers can't be reproduced run-to-run (the bootstrap
  currently uses an unseeded RNG).
- Silently mixing simulated and real data. Every simulated input must be named
  in the report.
- Spending the SportRadar trial budget carelessly. The owner wants calls left
  for future games this season.

## Data sources (verified 2026-10-28)

### SportRadar NBA v8 (trial)

- Base URL `https://api.sportradar.com/nba/trial/v8/en`. Auth via the
  `x-api-key` header, key from `.env` as `NPB_SPORTRADAR__API_KEY`. Never log
  the key or put it in a URL that gets logged.
- Schedule: `/games/{year}/REG/schedule.json` returns the whole regular
  season in one request. `2025` = the 2025-26 season (1,240 games, 1,237
  `closed`). `2026` = the current season.
- Play-by-play: `/games/{game_id}/pbp.json` returns the full game in one
  request (~1.7 MB). Events carry `clock` ("MM:SS"), `wall_clock` (UTC real
  time), `event_type`, `home_points`/`away_points` (running score after the
  event), `attribution.market` (team city name, e.g. "Oklahoma City"), and a
  `statistics` array whose entries carry `points` on scoring plays.
- Trial limits: ~1,000 requests total, 1 request/second. Two are already
  spent; their responses are cached at
  `data/backtest/sportradar/schedule_2025_REG.json` and
  `data/backtest/sportradar/pbp_820c5325-360f-4b9e-a67c-77fe71338871.json`.

### Polymarket (no auth)

- Gamma API: `https://gamma-api.polymarket.com/events?slug=<slug>`. NBA game
  events have slugs like `nba-por-lal-2026-10-27`
  (`nba-{team}-{team}-{date}`). The date appears to be the ET game date (that
  game was 11:00 PM ET Oct 27 = 03:00 UTC Oct 28). Team order appears to be
  away-home but this is **unverified**.
- Each event has ~25 markets with `sportsMarketType == "totals"`, one per line
  (e.g. 207.5 … 233.5), plus quarter / half totals and other props. Each
  market has `line`, `volume`, `clobTokenIds` (JSON string, index 0 = Over),
  `outcomes` (`["Over","Under"]`), `outcomePrices` (resolved: `["0","1"]`
  means Under won), and `gameStartTime`.
- "Main line" = the totals market with the highest `volume`.
- Price history:
  `https://clob.polymarket.com/prices-history?market=<over_token>&startTs=<unix>&endTs=<unix>&fidelity=1`
  returns `{history: [{t, p}]}` at one point per minute through the game, for
  both this season and last. `interval=max` returns **nothing** for older
  markets, so always pass `startTs`/`endTs`.
- Resolution rule (from market description): Over if the combined score is
  above the line "in this game"; no overtime exclusion. Use `outcomePrices`
  as ground truth for P&L.

## Sample

- **This season (2026-27):** every game with status `closed` at fetch time
  (~100 as of 2026-10-28).
- **Last season (2025-26):** 500 games sampled with a fixed seed, spread
  evenly across the season (my call: stratify by month so the sample isn't all
  October pace). Polymarket is free to query and SportRadar calls are scarce,
  so the 500 are drawn **only from games that have a Polymarket totals
  market** (my call). The report states the sample that way and reports how
  many of the 1,237 closed games had a market.
- **Refreshing the current season** (my call): `fetch` re-downloads the
  current-season schedule (1 request) only when passed `--refresh-current`;
  otherwise it uses the cached one. Each report writes a manifest of the
  game IDs it used, so any report can be reproduced after the sample grows.
- Budget: 2 spent + 1 (2026 schedule) + ~100 + 500 ≈ 600 of ~1,000, leaving
  ~400 for future games this season.
- Current-season games with no matching Polymarket totals market have no
  line, so they are excluded from every metric and only counted ("no market")
  in the report. Their pbp is still cached.

## Prerequisite fixes to the live parser (`src/feed/sportradar.rs`)

Found while verifying the real pbp payload. These are bugs in the live bot as
well, and the backtest is meaningless without them:

1. **Points per play.** The parser reads a top-level `points` field that
   SportRadar does not send, so `points.unwrap_or(2)` counts every free throw
   and three as 2. Derive points from the `statistics` entries (or from the
   change in `home_points + away_points`).
2. **Team attribution.** The parser compares `attribution.market` to
   `"home"`, but SportRadar sends the team's city name. Every basket is
   credited to Away, so the Q1 margin equals total Q1 points and the blowout
   gate (`max_q1_margin = 12`) rejects every game. Derive the scoring team
   from which of `home_points` / `away_points` changed.
3. **Event time.** Events are stamped `Timestamp::now()`; use the event's
   `wall_clock` instead (needed to look up the market price at decision time).
4. **Coverage counts only non-scoring events** (`src/strategy/mod.rs`
   `handle_event`: only `ClockUpdate` calls `coverage.observe`). A Q1 minute
   whose only events are made baskets counts as unobserved, dropping coverage
   to 11/12 ≈ 0.917 and failing the 0.95 gate with no data lost. Any event
   carrying a period + clock (including `Score`) should mark its minute as
   observed. (My call: treat as a bug, since the gate exists to catch feed
   loss, not quiet minutes.)

Each fix gets a test built from the cached real pbp file.

## How a game is replayed

1. Load the cached pbp, convert it to `GameEvent`s with the (fixed) live
   parser, in order.
2. Feed them to the unmodified `StrategyEngine` with the game's main line.
3. When the engine reaches `EdgeEvaluation` (Q1 + `q2_window_minutes` of Q2),
   take the `wall_clock` of the event that triggered it, look up the latest
   Polymarket price at or before that time, and give the engine a synthetic
   order book built around it.
4. Record: whether it traded, which gate skipped it if not, the projection
   (point and CI), `p_over`, market price, side, size, the regulation total,
   the final total, and the resolution.

**Projection for every game** (my call): today `StrategyEngine::evaluate`
returns at the first failed gate, before any projection exists. Refactor it
so the projection and `p_over` are always computed and returned alongside
the decision (as diagnostics), with the decision logic unchanged. That lets
model accuracy be measured on all games, not just the subset the gates let
through, and lets the report show whether the gates help.

Determinism: the bootstrap in `analytics::poisson::bootstrap_projected_total`
uses `thread_rng()`. The backtest needs a seedable RNG threaded through (or an
injectable one), seeded per game, so reruns give identical numbers.

## Synthetic book

There is no historical depth. The book is built around the minute price `p`
as the mid: best bid `p − h`, best ask `p + h`, with N levels on each side,
one cent apart, each holding a fixed USDC notional. `h`, N, and the per-level
notional are config values with stated defaults (my call: h = 0.01, 5 levels,
$500 per level; the builder may pick better-justified numbers if Polymarket
data suggests them). The report states these assumptions at the top.

## Fees

My call: read the market's taker fee from the Gamma market fields if
Polymarket publishes one for that market, apply it to each simulated fill,
and otherwise assume zero. The report states which was used.

## Variants

A file (e.g. `config/backtest.toml`) lists named variants, each a set of
overrides on the normal config:

- `flat_10`: fixed $10 per trade. Kelly sizing replaced, so a flat-size
  mode is needed in the sizing path (e.g. `kelly_fraction = 0` meaning "use
  a fixed size", or an explicit mode field; builder's call).
- `kelly_quarter`, `kelly_half`, `kelly_full`: `kelly_fraction` 0.25 / 0.5 /
  1.0.

- `kelly_quarter_live_limits`: quarter Kelly with the live limits (fixed
  $100 per-bet cap, $50 daily-loss stop), to show what the real limits cost.

**Risk limits in the backtest** (owner's decision): except in
`kelly_quarter_live_limits`, the fixed $100 cap is replaced by a cap of
**20% of the current bankroll** per bet, and the daily-loss stop is off.
A fixed cap would make the Kelly variants converge as the bankroll moves,
hiding the comparison. The per-bet cap therefore needs to be expressible as
either a fixed amount or a fraction of bankroll, and the daily-loss stop
needs a simulated implementation (keyed on the ET game date) for the one
variant that uses it.

All variants start from the same bankroll ($500) and process games in
chronological order, so bankroll compounding is realistic.

## Report

Written to `data/backtest/reports/<timestamp>/`:

- `games.csv`: one row per game per variant with everything recorded in the
  replay step.
- `report.md`: the side-by-side table plus the model section.

Metrics:

- **Model accuracy** (variant-independent unless a variant changes the model),
  each reported twice: over **all games with a market**, and over **games
  that passed the gates** (my call):
  - mean absolute error and bias of the projected total vs the **regulation**
    total (what the model projects, since it treats overtime as impossible),
    and separately vs the final total including overtime (my call: both);
  - CI coverage (share of regulation totals inside the 95% CI);
  - calibration table (bucket `p_over` into deciles; actual **settled** Over
    rate per bucket; count per bucket);
  - Brier score of `p_over` against the settled result, next to the Brier
    score of the market's price at the same moment as a baseline.
- **Gate funnel:** games in → no market → coverage gate → blowout gate →
  Q1/Q2 consistency test → no edge → sized below minimum → traded.
- **Per variant:** trades, win rate, total staked, final bankroll, return on
  bankroll, max drawdown, return per dollar staked.
- **Header:** sample definition, date range, seed, synthetic-book assumptions,
  count of games that went to overtime.

## Structure

My call: Rust, in this crate, so it runs the real engine.

- `src/backtest/` module: `fetch` (SportRadar + Polymarket download and
  cache, rate-limited), `replay` (one game through the engine), `report`.
- `src/bin/backtest.rs` with subcommands `fetch` (download what's missing for
  the sample) and `run` (replay all cached games for all variants, offline).
- Cache under `data/backtest/` (already gitignored via `/data`).

## Left to the builder

- Exact cache file layout and the game ↔ Polymarket matching implementation
  (including verifying slug team order and handling misses, e.g. by falling
  back to a Gamma search by date and team names).
- SportRadar alias ↔ Polymarket slug abbreviation mapping.
- How `StrategyEngine` is driven synchronously (it currently runs as an async
  loop over channels) as long as the engine's decision logic is unchanged.
- Report formatting.
