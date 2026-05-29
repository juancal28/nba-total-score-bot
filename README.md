# nba_poly_bot

NBA Q1-signal trading bot for Polymarket game-total over/under markets. Watches a live NBA game's first quarter via SportRadar, fits a Poisson model to per-minute scoring, validates the rate against the opening minutes of Q2, and submits a marketable FOK order to Polymarket's CLOB when the model implies edge against the market line. Holds to UMA resolution with extreme-probability early-close triggers.

Built on the official [`polymarket_client_sdk_v2`](https://github.com/Polymarket/rs-clob-client-v2) Rust SDK (handles EIP-712 signing, L1→L2 API auth, and CLOB WebSocket subscriptions) and alloy 1.x for on-chain approvals.

---

## Strategy

The bot's hypothesis: when Q1 scoring is well-characterized by a homogeneous Poisson process, and the opening of Q2 confirms the same rate, the final game total is predictable enough to beat the market line.

Mechanics:

1. During Q1, accumulate per-minute point totals (12 buckets) and track a `coverage_ratio` of observed seconds vs total seconds.
2. At Q1 end, gate on `coverage_ratio >= 0.95`. Fit Poisson `lambda` via MLE.
3. At the configured Q2 window (default 2 minutes), fit a second `lambda` from the Q2 sample.
4. Run a two-sample likelihood-ratio test. Skip the trade if the p-value is below `lr_p_threshold`.
5. Bootstrap (1000 resamples) a 95% CI for the projected full-game total, applying a Q1→game scaling factor (`q1_scaling_factor`, default 0.97) to correct for Q1 being the league's highest-scoring quarter on average.
6. Compute `P(total > line)` from the projected total + CI, and the implied market probability from the OVER token's best ask. If the edge meets `min_edge` and the book passes a staleness check, build a FOK market order via the SDK, sign it with the wallet, and submit.
7. Once filled, hold the position. Watch the order-book mid for an implied resolution probability ≥ 98% (take profit) or ≤ 2% (cut loss). Otherwise hold to UMA resolution.

### What this does *not* model

- Overtime probability when projected total is near the line (still treats OT as a 0-probability event)
- Pace shifts from blowouts (a blowout filter at `max_q1_margin = 12` instead approximates this)
- Roster changes during Q1 (a `RosterChange` event from the feed invalidates the sample, but the trial REST feed doesn't surface these events the same way the push feed does)

---

## Prerequisites

- macOS or Linux. Tested on macOS (Darwin 25.5 / aarch64).
- **Rust 1.95+**. Install with `brew install rust` (or rustup, your call — the SDK requires 1.88+).
- A **Polygon EOA wallet** (private key in hex). If you already have a Polymarket account, export it: Polymarket app → Settings → Export Private Key.
- A small **MATIC balance** in that wallet (~$0.50 of MATIC covers the four approval transactions).
- **USDC** in the wallet, equal to or larger than your intended position size.
- A **SportRadar API key** ([free trial signup](https://developer.sportradar.com/) gives 1k requests / 1 QPS for ~30 days; push streaming requires a Realtime contract, not implemented yet).

---

## Installation

```bash
git clone <your-fork-url>
cd nba_poly_bot
cargo build --release
```

First build takes ~5 minutes (alloy + the Polymarket SDK pull in a lot of crates). Subsequent builds are incremental.

---

## Configuration

The bot uses a three-layer config: `config/default.toml` (committed), `config/local.toml` (optional overrides), then env vars prefixed with `NPB_`. Nested keys are separated by `__` in env-var form.

### Secrets (`.env`)

Copy `.env.example` to `.env` and fill in:

```dotenv
# Funded Polygon EOA private key. Export from Polymarket UI if needed.
# No 0x prefix.
NPB_WALLET__PRIVATE_KEY=abc123...

# SportRadar API key (trial tier is fine for development).
NPB_SPORTRADAR__API_KEY=your_sr_key_here

# Optional: filter logs.
RUST_LOG=nba_poly_bot=info
```

The bot reads `.env` automatically. **Do not commit it** — `.gitignore` already excludes it.

### Tunable parameters (`config/default.toml`)

| Section | Key | Default | Meaning |
|---------|-----|---------|---------|
| `strategy` | `q2_window_minutes` | 2 | Minutes of Q2 to observe before deciding |
| `strategy` | `lr_p_threshold` | 0.05 | Skip trade if Q1↔Q2 LR p-value below this |
| `strategy` | `bootstrap_iters` | 1000 | Bootstrap resamples for projected-total CI |
| `strategy` | `q1_scaling_factor` | 0.97 | Empirical correction for Q1-is-highest-scoring bias |
| `strategy` | `min_coverage_ratio` | 0.95 | Required fraction of Q1 minutes observed live |
| `strategy` | `min_edge` | 0.05 | Required probability-edge over market implied prob |
| `strategy` | `max_q1_margin` | 12 | Skip if Q1 ends with score margin > this (blowout filter) |
| `strategy` | `max_book_age_ms` | 500 | Reject decision if book older than this |
| `execution` | `order_size_usdc` | "10" | USDC amount per trade |
| `execution` | `max_slippage` | "0.02" | Acceptable VWAP slippage from top-of-book |
| `exit` | `take_profit_prob` | 0.98 | Close position if implied resolution prob ≥ this |
| `exit` | `cut_loss_prob` | 0.02 | Close position if implied resolution prob ≤ this |
| `risk` | `max_position_usdc` | "100" | Hard cap on open position size |
| `risk` | `max_daily_loss_usdc` | "50" | Stop trading after this realized loss |
| `risk` | `max_consecutive_feed_gaps` | 3 | Kill switch trips after this many gaps in a row |
| `sportradar` | `poll_interval_ms` | 5000 | Poll cadence for the trial REST endpoint (5s respects the 1 QPS limit) |

Override any key locally without editing the committed defaults:

```toml
# config/local.toml
[strategy]
min_edge = 0.08

[execution]
order_size_usdc = "25"
```

Or via env var (note the `__` separator):

```bash
NPB_STRATEGY__MIN_EDGE=0.08 cargo run --release -- run ...
```

---

## One-time setup

This submits the four Polymarket on-chain approvals (USDC + CTF for both the regular and neg-risk Exchanges) and derives your Polymarket L2 API credentials. Approvals are idempotent — the bot reads current state first and skips anything already set.

```bash
cargo run --bin setup
```

Options:
- `--rpc <url>` — use a private RPC (Alchemy, QuickNode) instead of the default `https://polygon-rpc.com`. Recommended if you trade frequently.
- `--skip-approvals` — skip the on-chain step, only derive L2 credentials. Use if you've already approved via the Polymarket web UI.

Expected output (first run):

```
INFO  approving USDC -> 0xE111... (Exchange)
INFO  USDC approved tx=0x...
INFO  approving USDC -> 0xe222... (NegRiskExchange)
INFO  USDC approved tx=0x...
INFO  CTF.setApprovalForAll -> 0xE111... (Exchange)
INFO  CTF approved tx=0x...
INFO  CTF.setApprovalForAll -> 0xe222... (NegRiskExchange)
INFO  CTF approved tx=0x...
INFO  on-chain approvals complete new_txs=4
INFO  authenticated against Polymarket CLOB
INFO  setup complete: approvals on-chain, L2 credentials derived server-side
```

On subsequent runs, all four approvals report "already approved" with no new transactions.

---

## Running the bot

The bot needs:
- A **SportRadar game ID** for the NBA game you want to trade.
- The **Polymarket condition ID's two outcome token IDs** — `over_token` (YES on "total > line") and `under_token`. Find these via Polymarket's market metadata API or the market URL.
- The **market's total-points line** (e.g. `224.5`).

### Dry run (recommended first)

Computes the trade decision and logs it, but does not submit:

```bash
cargo run --release -- run \
    --game 1234abcd-... \
    --over-token 12345678901234567890... \
    --under-token 98765432109876543210... \
    --line 224.5 \
    --dry-run
```

Watch the logs through Q1 + Q2 opening. The bot will log its Poisson fit, LR p-value, bootstrap projection, and the would-trade decision. Compare against the post-game actual total. Do this for at least 10 games before going live.

### Live

```bash
cargo run --release -- run \
    --game 1234abcd-... \
    --over-token 12345678901234567890... \
    --under-token 98765432109876543210... \
    --line 224.5
```

The bot will submit a FOK market order when its edge threshold is met and then monitor the book for an exit trigger or hold to resolution.

---

## How the runtime is wired

```
SportRadar trial REST  --(poll 5s)--> feed::sportradar::SportRadarPoller
                                                  |
                                                  v
                                            (GameEvent channel)
                                                  |
                                                  v
                                    +-->  strategy::StrategyEngine
                                    |       (Poisson MLE + LR test + edge)
                                    |             |
                                    |             v
                                    |       (TradeIntent)
                                    |             |
Polymarket CLOB WS --(book updates)-+             v
                                            executor::Executor
                                            (redb-persisted idempotent submit)
                                                  |
                                                  v
                                       polymarket::client::PolymarketClient
                                            (SDK build + sign + post_order)
                                                  |
                                                  v
                                       executor::exit::watch_for_exit
                                            (TP at >=98%, SL at <=2%)
```

- Multi-thread tokio runtime (4 workers).
- EIP-712 signing offloaded inside the SDK; we don't block the runtime hot path.
- Order book lives on the WS reader task; consumers read lock-free `Arc<OrderBookSnapshot>` via `watch::Receiver`.
- Client-order-IDs persisted to `data/orders.redb` *before* submission so restarts can reconcile without double-filling.

---

## Project structure

```
src/
  main.rs                   - CLI dispatch (run subcommand)
  lib.rs                    - run() and setup() entry points
  bin/setup.rs              - one-time setup binary
  config.rs                 - figment-based layered config
  error.rs                  - typed BotError
  risk.rs                   - kill-switch controller (position cap, daily loss, feed gaps)
  analytics/
    poisson.rs              - Poisson MLE + asymptotic chi-squared LR test + bootstrap LR + bootstrap projected-total CI
    projection.rs           - projected_total + P(over) from CI
    poisson_compat.rs       - shared erfc implementation
  feed/
    mod.rs                  - GameEvent enum
    sportradar.rs           - trial REST poller
    coverage.rs             - per-minute coverage_ratio tracker
  polymarket/
    book.rs                 - OrderBookSnapshot + best_bid/ask + vwap_buy + age_ms
    client.rs               - SDK Client wrapper, submit_intent
    marketdata.rs           - SDK ws::Client wrapper, BookUpdate -> snapshot
    approvals.rs            - one-time USDC + CTF approvals on Polygon
  strategy/
    mod.rs                  - state machine: PreGame -> Q1Collecting -> Q1End -> Q2Validating -> EdgeEvaluation -> Holding
    edge.rs                 - VWAP edge calc against book
  executor/
    mod.rs                  - redb-persisted idempotent order submit
    exit.rs                 - implied-probability exit loop
config/
  default.toml              - committed defaults
  local.toml                - your overrides (gitignored)
```

---

## Testing

```bash
cargo test --lib
```

Current coverage:
- Poisson MLE produces sample mean
- LR test gives high p for identical samples, low p for divergent
- Bootstrap CI contains its point estimate
- P(over) is below 0.5 when line is above projection, above 0.5 when below
- Clock-string parser handles `MM:SS` and `M:SS.t`

---

## Known limitations

- **SportRadar feed is REST-polled at 5s intervals**, not the sub-second push feed. The 5s lag eats into the strategy's edge because Polymarket's order book reacts to scoring within ~1–3s. Swapping in the push feed requires (a) a SportRadar Realtime contract, no self-serve pricing, and (b) implementing an HTTP chunked-stream client in `feed/sportradar.rs`.
- **Partial-fill reconciliation on restart is not implemented.** `Executor::load(client_order_id)` exists but no startup loop calls it. If the bot crashes mid-submit, the redb record marks the order Pending and a manual check is currently required.
- **Risk controller is instantiated but not consulted** before the executor submits. The kill-switch state machine works; the wiring is missing one call site.
- **Overtime is not modeled.** Projections assume a 48-minute game.
- **Single-game scope per process.** No portfolio/multi-game support.
- **No metrics export.** `tracing` produces JSON logs; no Prometheus exporter yet.

---

## Operational notes

- **First runs cost gas.** Approvals are four Polygon transactions. Subsequent setup runs report "already approved" and submit no transactions.
- **Polymarket's public RPC is rate-limited.** If approvals fail with `429`, retry with `--rpc <private_url>`.
- **Trial SportRadar keys expire after ~30 days.** Re-issue for continued development.
- **`config/local.toml` is gitignored** — safe to keep your overrides there.
- **Wallet key handling.** The private key is loaded via `secrecy::SecretString`, so it doesn't appear in `Debug` output or panic messages. It is in process memory while the bot runs.

---

## License

This project is not licensed for redistribution. See the source tree.
