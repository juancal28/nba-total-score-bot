# Backtest report

## What this is

- **Games:** 638 replayed of 653 sampled, 2025-10-21 to 2026-11-09.
  - Season 2025-26: 485 replayed.
  - Season 2026-27: 153 replayed.
  - Excluded, NoPrice: 15.
- **Overtime games:** 30.
- **Real:** play-by-play (SportRadar), the line, the OVER price at the decision minute, and the settled result (Polymarket).
- **Assumed:** order-book depth (5 levels each side, 1 cent apart, starting 0.01 from the price, $500 per level); zero trading fees; positions held to settlement; bets placed one at a time in game order.
- **Settings:** min_edge 0.05, lr_p_threshold 0.05, max_q1_margin 12, q1_scaling_factor 1, q2_window_minutes 2, ci_ref_width 60, starting bankroll $500.

## Model accuracy

| Metric | All games | Passed the gates |
|---|---|---|
| Games | 638 | 505 |
| Mean abs. error vs regulation total (points) | 22.0 | 22.2 |
| Bias vs regulation total (projection minus actual) | +1.9 | +0.6 |
| Mean abs. error vs final total incl. overtime | 22.5 | 22.8 |
| Bias vs final total incl. overtime | +0.7 | -0.5 |
| Regulation totals inside the model's 95% interval | 88% | 87% |
| Brier score, model P(over) (lower is better; 0.25 = coin flip) | 0.2482 | 0.2519 |
| Brier score, market price at the same moment | 0.2246 | 0.2250 |

### Model calibration (all games)

| P(over) bucket | Games | Mean predicted | Actual Over rate |
|---|---|---|---|
| 0.0-0.1 | 116 | 0.03 | 0.20 |
| 0.1-0.2 | 55 | 0.15 | 0.31 |
| 0.2-0.3 | 49 | 0.24 | 0.41 |
| 0.3-0.4 | 58 | 0.35 | 0.38 |
| 0.4-0.5 | 46 | 0.46 | 0.54 |
| 0.5-0.6 | 50 | 0.55 | 0.48 |
| 0.6-0.7 | 50 | 0.65 | 0.46 |
| 0.7-0.8 | 44 | 0.75 | 0.48 |
| 0.8-0.9 | 66 | 0.85 | 0.61 |
| 0.9-1.0 | 104 | 0.96 | 0.74 |

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
| Trades | 430 | 430 | 430 | 430 | 267 |
| Win rate | 64% | 64% | 64% | 64% | 63% |
| OVER / UNDER bets | 202 / 228 | 202 / 228 | 202 / 228 | 202 / 228 | 121 / 146 |
| Total staked | $4300 | $114878 | $310225 | $254357 | $23447 |
| Final bankroll (from $500) | $812.59 | $1999.01 | $3477.57 | $380.96 | $1351.45 |
| Return on bankroll | +62.5% | +299.8% | +595.5% | -23.8% | +170.3% |
| Worst drawdown | 13.7% | 86.8% | 91.3% | 98.7% | 60.5% |
| Return per dollar staked | +7.3% | +1.3% | +1.0% | -0.0% | +3.6% |
| Skipped: no edge in either direction | 75 | 75 | 75 | 75 | 51 |
| Skipped: kelly size below minimum order | 0 | 0 | 0 | 0 | 0 |
| Skipped: daily loss stop | 0 | 0 | 0 | 0 | 232 |
