//! One-time setup: approve USDC + CTF to Polymarket Exchange and derive API
//! credentials. Required before the main bot can run.

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Polymarket one-time approvals + L2 credential derivation")]
struct Cli {
    /// Optional RPC URL override (otherwise uses the configured public RPC).
    #[arg(long)]
    rpc: Option<String>,
    /// Skip the on-chain approvals (use if already approved).
    #[arg(long, default_value_t = false)]
    skip_approvals: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    nba_poly_bot::setup(cli.rpc.as_deref(), cli.skip_approvals).await
}
