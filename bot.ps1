# Run the bot inside the Rust container (Windows Smart App Control blocks
# locally compiled binaries). Usage: .\bot status, .\bot status paper --all,
# .\bot backtest-run, .\bot paper --date 2026-11-10
$repo = $PSScriptRoot
docker run --rm --env-file "$repo\.env" -e RUST_LOG=nba_poly_bot=warn `
    -v "${repo}:/app" -v npb-cargo:/usr/local/cargo/registry -v npb-target:/target `
    -e CARGO_TARGET_DIR=/target -w /app rust:1 `
    cargo run --quiet --bin nba_poly_bot -- @args
