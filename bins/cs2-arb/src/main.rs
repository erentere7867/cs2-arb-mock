use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cs2_arb_core::{Error, SimulationMode};

#[derive(Parser)]
#[command(name = "cs2-arb", about = "CS2 market analysis — MOCK SIMULATION ONLY")]
struct Cli {
    #[arg(long, global = true, default_value = "config/default.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the simulated strategy loop.
    RunMock,
    /// Replay recorded prices through the simulator.
    Backtest {
        #[arg(long, default_value = "data/fixtures/price_snapshots.json")]
        snapshots: PathBuf,
    },
    /// Observe CS2 update events.
    Watch,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _mode = SimulationMode;
    let feature = match cli.command {
        Command::RunMock => "mock strategy loop",
        Command::Backtest { .. } => "backtest replay",
        Command::Watch => "CS2 branch watcher",
    };
    eprintln!("{} (workspace scaffold)", Error::NotImplemented(feature));
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_only_mock_and_read_only_commands() {
        for command in ["run-mock", "backtest", "watch"] {
            assert!(Cli::try_parse_from(["cs2-arb", command]).is_ok());
        }
        assert!(Cli::try_parse_from(["cs2-arb", "run-live"]).is_err());
    }
}
