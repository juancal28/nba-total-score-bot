# Test results

Backtest reports and paper-trading decisions, one subfolder per machine that
ran the bot, named after that machine's hostname. Results from different
machines are never mixed.

```
test-results/<hostname>/backtest/<run timestamp>/   report.md, games.csv, manifest.json
test-results/<hostname>/paper/decisions.jsonl
```

The bot itself writes to `data/` (not tracked by git). These are copies of
`data/backtest/reports/` and `data/paper/decisions.jsonl` from that machine.
