#!/bin/bash
# Run the bot inside the Rust container (Windows Smart App Control blocks
# locally compiled binaries). Usage: ./bot.sh status, ./bot.sh status paper --all,
# ./bot.sh backtest-run, ./bot.sh paper --date 2026-11-10
repo="$(cd "$(dirname "$0")" && pwd -W 2>/dev/null || pwd)"
MSYS_NO_PATHCONV=1 docker run --rm --env-file "$repo/.env" -e RUST_LOG=nba_poly_bot=warn \
  -v "$repo:/app" -v npb-cargo:/usr/local/cargo/registry -v npb-target:/target \
  -e CARGO_TARGET_DIR=/target -w /app rust:1 \
  cargo run --quiet --bin nba_poly_bot -- "$@"
