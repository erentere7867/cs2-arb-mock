use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use cs2_arb_backtest::integrated::{DipAdapter, MarketDepth, SimFillModel, SpreadAdapter};
use cs2_arb_backtest::{fixtures, BacktestConfig, Backtester, PerformanceReport, StrategyAdapter};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedules, MarketEvent, Platform, PriceSnapshot, Result, SimulationMode,
};
use cs2_arb_strategy::{
    DipConfig, KillSwitch, PegBidConfig, RelistConfig, RiskLimits, SpreadConfig,
};
use serde::Deserialize;

mod live;
mod peg_runner;
mod watch;

const DEFAULT_SNAPSHOTS: &str = "data/fixtures/price_snapshots.json";
const DEFAULT_EVENTS: &str = "data/fixtures/market_events.json";
const DEFAULT_DB: &str = "data/events.db";

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
    /// Run simulated strategies, offline unless live read-only data is explicitly enabled.
    RunMock {
        #[command(flatten)]
        history: HistoricalArgs,
        #[arg(long, conflicts_with_all = ["snapshots", "events", "sales", "buy_orders"])]
        live_market_data: bool,
        #[arg(long, value_enum, requires = "live_market_data")]
        platform: Vec<Market>,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..), requires = "live_market_data")]
        samples: u16,
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..), requires = "live_market_data")]
        interval_seconds: u64,
        #[arg(long, default_value = DEFAULT_DB, requires = "live_market_data")]
        database: String,
    },
    /// Replay recorded prices through the simulator.
    Backtest {
        #[command(flatten)]
        history: HistoricalArgs,
        #[arg(long, default_value = "reports")]
        output_dir: PathBuf,
    },
    /// Poll public CS2 branch metadata and persist events to SQLite.
    Watch {
        #[arg(long, default_value = DEFAULT_DB)]
        database: String,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Args)]
struct HistoricalArgs {
    /// Historical snapshots JSON; defaults to the bundled synthetic history.
    #[arg(long)]
    snapshots: Option<PathBuf>,
    /// Historical update events JSON; the bundled history has a matching event file.
    #[arg(long)]
    events: Option<PathBuf>,
    /// Individual sales JSON for historical liquidity; custom snapshots never invent sales.
    #[arg(long)]
    sales: Option<PathBuf>,
    /// Optional top-of-book buy orders JSON for custom peg-bidding replays.
    #[arg(long)]
    buy_orders: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Market {
    Skinport,
    Csfloat,
    Dmarket,
}

impl From<Market> for Platform {
    fn from(value: Market) -> Self {
        match value {
            Market::Skinport => Self::Skinport,
            Market::Csfloat => Self::CsFloat,
            Market::Dmarket => Self::DMarket,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Settings {
    spread: SpreadConfig,
    dip: DipConfig,
    risk: RiskLimits,
    peg_bid: PegBidConfig,
    relist: RelistConfig,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    if let Err(err) = run(cli).await {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(cli: Cli) -> Result<()> {
    let config = read(&cli.config)?;
    if let Command::Watch { database, once } = cli.command {
        return watch::run(&config, &database, once).await;
    }
    let fees = FeeSchedules::from_toml(&config)?;
    let settings: Settings =
        toml::from_str(&config).map_err(|err| Error::InvalidConfig(err.message().to_owned()))?;
    let (inputs, output) = match cli.command {
        Command::RunMock {
            history,
            live_market_data,
            platform,
            samples,
            interval_seconds,
            database,
        } => {
            let inputs = if live_market_data {
                let platforms = if platform.is_empty() {
                    vec![Platform::Skinport, Platform::CsFloat]
                } else {
                    platform.into_iter().map(Platform::from).collect()
                };
                live::collect(
                    &config,
                    &fees,
                    platforms,
                    samples,
                    interval_seconds,
                    &database,
                    settings.dip.branch_update_window_hours,
                )
                .await?
            } else {
                historical(history)?
            };
            (inputs, None)
        }
        Command::Backtest {
            history,
            output_dir,
        } => (historical(history)?, Some(output_dir)),
        Command::Watch { .. } => unreachable!("handled before loading market configuration"),
    };
    let kill_switch = KillSwitch::with_defaults();
    let peg_config = settings.peg_bid.clone();
    let relist_config = settings.relist.clone();
    let risk_limits = settings.risk.clone();
    let report = simulate(&inputs, fees.clone(), settings, kill_switch.clone())?;
    let peg_dashboard = peg_runner::run_peg_bid_engine(
        &inputs.snapshots,
        inputs.depth.sales(),
        &inputs.buy_orders,
        &fees,
        peg_config,
        relist_config,
        risk_limits,
        &kill_switch,
        None,
    )?;
    peg_dashboard.log_summary();
    if let Some(directory) = output {
        write_reports(&directory, &report, &peg_dashboard)?;
    } else {
        print!("{}{}", report.to_markdown(), peg_dashboard.to_markdown());
    }
    Ok(())
}

struct Inputs {
    snapshots: Vec<PriceSnapshot>,
    events: Vec<MarketEvent>,
    depth: MarketDepth,
    buy_orders: Vec<BuyOrder>,
}

fn historical(args: HistoricalArgs) -> Result<Inputs> {
    let bundled = args.snapshots.is_none();
    let snapshots = fixtures::load_snapshots(
        args.snapshots
            .as_deref()
            .unwrap_or_else(|| Path::new(DEFAULT_SNAPSHOTS)),
    )?;
    let event_path = args
        .events
        .or_else(|| bundled.then(|| PathBuf::from(DEFAULT_EVENTS)));
    let events = event_path
        .map(fixtures::load_events)
        .transpose()?
        .unwrap_or_default();
    let mut depth = MarketDepth::from_snapshots(&snapshots);
    if let Some(path) = args.sales {
        let sales: Vec<_> = serde_json::from_str(&read(&path)?)
            .map_err(|err| Error::InvalidData(format!("invalid sales JSON: {err}")))?;
        depth.add_sales(sales);
    } else if bundled {
        depth.add_bundled_mock_sales(&snapshots);
    }
    let buy_orders = if let Some(path) = args.buy_orders {
        serde_json::from_str(&read(&path)?)
            .map_err(|err| Error::InvalidData(format!("invalid buy orders JSON: {err}")))?
    } else if bundled {
        peg_runner::derive_offline_buy_orders(&snapshots)
    } else {
        Vec::new()
    };
    Ok(Inputs {
        snapshots,
        events,
        depth,
        buy_orders,
    })
}

fn simulate(
    inputs: &Inputs,
    fees: FeeSchedules,
    settings: Settings,
    kill_switch: KillSwitch,
) -> Result<PerformanceReport> {
    let mut strategies: Vec<Box<dyn StrategyAdapter>> = vec![
        Box::new(SpreadAdapter::new(settings.spread)),
        Box::new(DipAdapter::new(settings.dip, &inputs.depth)?),
    ];
    let model = SimFillModel::new(&inputs.depth, fees.clone(), settings.risk, kill_switch);
    Backtester::with_fill_model(BacktestConfig::new(fees), model).run(
        SimulationMode,
        &inputs.snapshots,
        &inputs.events,
        &mut strategies,
    )
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|err| Error::Storage(format!("{}: {err}", path.display())))
}

fn write_reports(
    directory: &Path,
    report: &PerformanceReport,
    peg_dashboard: &peg_runner::PegBidDashboard,
) -> Result<()> {
    std::fs::create_dir_all(directory).map_err(|err| Error::Storage(err.to_string()))?;
    let json = directory.join("backtest.json");
    let peg_json = directory.join("peg_dashboard.json");
    let markdown = directory.join("backtest.md");
    std::fs::write(&json, report.to_json()?).map_err(|err| Error::Storage(err.to_string()))?;
    let peg_body = serde_json::to_string_pretty(peg_dashboard)
        .map_err(|err| Error::Storage(err.to_string()))?;
    std::fs::write(&peg_json, peg_body).map_err(|err| Error::Storage(err.to_string()))?;
    let mut full_md = report.to_markdown();
    full_md.push_str(&peg_dashboard.to_markdown());
    std::fs::write(&markdown, full_md).map_err(|err| Error::Storage(err.to_string()))?;
    println!(
        "Wrote {}, {}, and {}",
        json.display(),
        peg_json.display(),
        markdown.display()
    );
    Ok(())
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

    #[test]
    fn live_options_are_explicit_and_incompatible_with_historical_input() {
        assert!(Cli::try_parse_from(["cs2-arb", "run-mock", "--platform", "dmarket"]).is_err());
        assert!(Cli::try_parse_from([
            "cs2-arb",
            "run-mock",
            "--live-market-data",
            "--snapshots",
            "history.json"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "cs2-arb",
            "run-mock",
            "--live-market-data",
            "--platform",
            "dmarket"
        ])
        .is_ok());
    }

    #[test]
    fn bundled_replay_uses_real_strategy_and_simulator() {
        let config = include_str!("../../../config/default.toml");
        let fees = FeeSchedules::from_toml(config).unwrap();
        let settings = toml::from_str(config).unwrap();
        let snapshots =
            fixtures::parse_snapshots(include_str!("../../../data/fixtures/price_snapshots.json"))
                .unwrap();
        let events =
            fixtures::parse_events(include_str!("../../../data/fixtures/market_events.json"))
                .unwrap();
        let mut inputs = Inputs {
            depth: MarketDepth::from_snapshots(&snapshots),
            buy_orders: peg_runner::derive_offline_buy_orders(&snapshots),
            snapshots,
            events,
        };
        let no_sales = simulate(&inputs, fees.clone(), settings, KillSwitch::new()).unwrap();
        assert_eq!(no_sales.totals.orders_filled, 0);
        inputs.depth.add_bundled_mock_sales(&inputs.snapshots);
        let report = simulate(
            &inputs,
            fees,
            toml::from_str(config).unwrap(),
            KillSwitch::new(),
        )
        .unwrap();
        assert!(report.strategy("spread").unwrap().trade_count > 0);
        assert!(report.strategy("dip").unwrap().trade_count > 0);
        assert!(report
            .trades
            .iter()
            .all(|trade| trade.closed_at >= trade.locked_until));
        assert!(report.totals.total_fees.0 > 0);
    }
}
