use std::str::FromStr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;

#[derive(Parser, Debug)]
#[command(version, about = "NBA Q1-signal Polymarket trading bot")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the bot against a live game.
    Run {
        /// SportRadar game_id for the NBA game.
        #[arg(long)]
        game: String,
        /// Polymarket token_id for the OVER outcome (numeric, decimal string).
        #[arg(long)]
        over_token: String,
        /// Polymarket token_id for the UNDER outcome.
        #[arg(long)]
        under_token: String,
        /// The market's total-points line (e.g. 224.5).
        #[arg(long)]
        line: String,
        /// Compute decisions but do not submit orders.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Watch a day's games and log the trades the bot would make. Needs no
    /// wallet and submits nothing.
    Paper {
        /// Game date (US Eastern), YYYY-MM-DD. Defaults to today.
        #[arg(long)]
        date: Option<String>,
        /// Only these SportRadar game ids (comma-separated). Default: all.
        #[arg(long, value_delimiter = ',')]
        games: Vec<String>,
    },
    /// Download and cache the data the backtest replays.
    BacktestFetch {
        /// Games to sample from last season.
        #[arg(long, default_value_t = 500)]
        last_season_games: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Re-download the current-season schedule to pick up new games.
        #[arg(long, default_value_t = false)]
        refresh_current: bool,
        /// Stop after this many SportRadar requests in this run.
        #[arg(long, default_value_t = 720)]
        max_requests: u32,
    },
    /// Replay the cached games for every sizing variant and write a report.
    BacktestRun,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("nba_poly_bot=info")),
        )
        .json()
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Run { game, over_token, under_token, line, dry_run } => {
            let line_dec = Decimal::from_str(&line)?;
            tracing::info!(%game, %over_token, %under_token, %line, dry_run, "starting bot");
            nba_poly_bot::run(nba_poly_bot::RunArgs {
                game_id: game,
                over_token_id: over_token,
                under_token_id: under_token,
                line: line_dec,
                dry_run,
            })
            .await
        }
        Cmd::BacktestFetch { last_season_games, seed, refresh_current, max_requests } => {
            nba_poly_bot::backtest::fetch::run(nba_poly_bot::backtest::fetch::FetchArgs {
                last_season_games,
                seed,
                refresh_current,
                max_requests,
            })
            .await
        }
        Cmd::BacktestRun => {
            let dir = nba_poly_bot::backtest::run::run()?;
            tracing::info!(dir = %dir.display(), "report written");
            Ok(())
        }
        Cmd::Paper { date, games } => {
            nba_poly_bot::paper::run(nba_poly_bot::paper::PaperArgs { date, games }).await
        }
    }
}
