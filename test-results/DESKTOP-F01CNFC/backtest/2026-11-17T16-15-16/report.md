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
- **Projection:** pace blended with the line: 27.5 + 0.286 x pace projection + 0.597 x line, sigma 16.6 points.

## Model accuracy

| Metric | All games | Passed the gates |
|---|---|---|
| Games | 638 | 505 |
| Mean abs. error vs regulation total (points) | 12.5 | 12.7 |
| Bias vs regulation total (projection minus actual) | +1.0 | +0.5 |
| Mean abs. error vs final total incl. overtime | 13.0 | 13.2 |
| Bias vs final total incl. overtime | -0.2 | -0.7 |
| Regulation totals inside the model's 95% interval | 96% | 96% |
| Brier score, model P(over) (lower is better; 0.25 = coin flip) | 0.2148 | 0.2171 |
| Brier score, market price at the same moment | 0.2246 | 0.2250 |

### Model calibration (all games)

| P(over) bucket | Games | Mean predicted | Actual Over rate |
|---|---|---|---|
| 0.0-0.1 | 9 | 0.08 | 0.00 |
| 0.1-0.2 | 32 | 0.16 | 0.12 |
| 0.2-0.3 | 85 | 0.26 | 0.24 |
| 0.3-0.4 | 91 | 0.35 | 0.32 |
| 0.4-0.5 | 125 | 0.45 | 0.49 |
| 0.5-0.6 | 129 | 0.55 | 0.47 |
| 0.6-0.7 | 84 | 0.65 | 0.60 |
| 0.7-0.8 | 51 | 0.74 | 0.86 |
| 0.8-0.9 | 26 | 0.85 | 0.73 |
| 0.9-1.0 | 6 | 0.92 | 0.83 |

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
| Trades | 265 | 265 | 265 | 265 | 193 |
| Win rate | 54% | 54% | 54% | 54% | 53% |
| OVER / UNDER bets | 86 / 179 | 86 / 179 | 86 / 179 | 86 / 179 | 60 / 133 |
| Total staked | $2650 | $79476 | $228523 | $495858 | $15760 |
| Final bankroll (from $500) | $1040.87 | $17126.29 | $40823.56 | $70429.03 | $2543.11 |
| Return on bankroll | +108.2% | +3325.3% | +8064.7% | +13985.8% | +408.6% |
| Worst drawdown | 9.9% | 29.0% | 43.0% | 54.5% | 27.7% |
| Return per dollar staked | +20.4% | +20.9% | +17.6% | +14.1% | +13.0% |
| Skipped: no edge in either direction | 240 | 240 | 240 | 240 | 177 |
| Skipped: kelly size below minimum order | 0 | 0 | 0 | 0 | 0 |
| Skipped: daily loss stop | 0 | 0 | 0 | 0 | 173 |
