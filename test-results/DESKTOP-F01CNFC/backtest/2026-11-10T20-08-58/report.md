# Backtest report

## What this is

- **Games:** 638 replayed of 653 sampled, 2025-10-21 to 2026-11-09.
  - Season 2025-26: 485 replayed.
  - Season 2026-27: 153 replayed.
  - Excluded, NoPrice: 15.
- **Overtime games:** 30.
- **Real:** play-by-play (SportRadar), the line, the OVER price at the decision minute, and the settled result (Polymarket).
- **Assumed:** order-book depth (5 levels each side, 1 cent apart, starting 0.01 from the price, $500 per level); zero trading fees; positions held to settlement; bets placed one at a time in game order.
- **Settings:** min_edge 0.05, lr_p_threshold 0.05, max_q1_margin 12, q1_scaling_factor 0.97, q2_window_minutes 2, ci_ref_width 60, starting bankroll $500.

## Model accuracy

| Metric | All games | Passed the gates |
|---|---|---|
| Games | 638 | 505 |
| Mean abs. error vs regulation total (points) | 21.6 | 22.0 |
| Bias vs regulation total (projection minus actual) | -3.0 | -4.2 |
| Mean abs. error vs final total incl. overtime | 22.2 | 22.7 |
| Bias vs final total incl. overtime | -4.2 | -5.4 |
| Regulation totals inside the model's 95% interval | 87% | 86% |
| Brier score, model P(over) (lower is better; 0.25 = coin flip) | 0.2465 | 0.2516 |
| Brier score, market price at the same moment | 0.2246 | 0.2250 |

### Model calibration (all games)

| P(over) bucket | Games | Mean predicted | Actual Over rate |
|---|---|---|---|
| 0.0-0.1 | 144 | 0.03 | 0.24 |
| 0.1-0.2 | 69 | 0.15 | 0.32 |
| 0.2-0.3 | 61 | 0.25 | 0.39 |
| 0.3-0.4 | 41 | 0.36 | 0.56 |
| 0.4-0.5 | 54 | 0.45 | 0.43 |
| 0.5-0.6 | 48 | 0.55 | 0.54 |
| 0.6-0.7 | 40 | 0.65 | 0.42 |
| 0.7-0.8 | 46 | 0.76 | 0.52 |
| 0.8-0.9 | 53 | 0.85 | 0.66 |
| 0.9-1.0 | 82 | 0.96 | 0.78 |

### Market calibration (all games)

| P(over) bucket | Games | Mean predicted | Actual Over rate |
|---|---|---|---|
| 0.0-0.1 | 6 | 0.07 | 0.00 |
| 0.1-0.2 | 25 | 0.16 | 0.12 |
| 0.2-0.3 | 58 | 0.26 | 0.29 |
| 0.3-0.4 | 71 | 0.35 | 0.34 |
| 0.4-0.5 | 140 | 0.45 | 0.38 |
| 0.5-0.6 | 151 | 0.53 | 0.51 |
| 0.6-0.7 | 97 | 0.64 | 0.49 |
| 0.7-0.8 | 67 | 0.74 | 0.79 |
| 0.8-0.9 | 17 | 0.84 | 0.65 |
| 0.9-1.0 | 6 | 0.93 | 1.00 |

## Gate funnel

| Stage | Games |
|---|---|
| Replayed | 638 |
| Stopped: coverage gate failed | 0 |
| Stopped: blowout gate failed | 88 |
| Stopped: consistency test failed | 45 |
| Passed all gates | 505 |

## Sizing variants

| Metric | flat_10 | kelly_quarter | kelly_half | kelly_full | kelly_quarter_live_limits |
|---|---|---|---|---|---|
| Trades | 442 | 442 | 442 | 442 | 263 |
| Win rate | 63% | 63% | 63% | 63% | 63% |
| OVER / UNDER bets | 158 / 284 | 158 / 284 | 158 / 284 | 158 / 284 | 90 / 173 |
| Total staked | $4420 | $204361 | $400537 | $404356 | $23596 |
| Final bankroll (from $500) | $811.11 | $6762.96 | $6130.11 | $606.95 | $2169.97 |
| Return on bankroll | +62.2% | +1252.6% | +1126.0% | +21.4% | +334.0% |
| Worst drawdown | 18.6% | 73.7% | 90.0% | 98.7% | 45.0% |
| Return per dollar staked | +7.0% | +3.1% | +1.4% | +0.0% | +7.1% |
| Skipped: no edge in either direction | 63 | 63 | 63 | 63 | 35 |
| Skipped: kelly size below minimum order | 0 | 0 | 0 | 0 | 0 |
| Skipped: daily loss stop | 0 | 0 | 0 | 0 | 256 |
