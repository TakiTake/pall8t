use clap::Parser;
use pall8t::holder::{run, HolderArgs};

/// Session holder: owns one tab's PTY so it survives pall8t exiting.
/// Spawned by pall8t; not meant to be run by hand.
#[derive(Parser)]
#[command(name = "pall8t-tab", version)]
struct Cli {
    /// Tab id (for ps visibility and the registry)
    #[arg(long)]
    id: String,
    /// Unix socket to serve
    #[arg(long)]
    socket: std::path::PathBuf,
    #[arg(long, default_value_t = 24)]
    rows: u16,
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Command to run on the PTY (after `--`)
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(HolderArgs {
        id: cli.id,
        socket: cli.socket,
        rows: cli.rows,
        cols: cli.cols,
        argv: cli.argv,
    })
}
