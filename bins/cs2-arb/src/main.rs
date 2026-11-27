use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use cs2_arb_backtest::integrated::{DipAdapter, MarketDepth, SimFillModel, SpreadAdapter};
use cs2_arb_backtest::{
    fixtures, BacktestConfig, Backtester, PerformanceReport, Secrets, StrategyAdapter,
};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedules, Game, ItemId, MarketEvent, Platform, PriceSnapshot, Result,
    SimulationMode,
};
use cs2_arb_strategy::{
    DipConfig, KillSwitch, PegBidConfig, RelistConfig, RiskLimits, SpreadConfig,
};
use serde::Deserialize;

mod live;
mod peg_runner;
mod watch;

const DEFAULT_DB: &str = "data/events.db";

#[derive(Parser)]
#[command(
    name = "cs2-arb",
    about = "Skin market analysis — MOCK SIMULATION ONLY"
)]
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
        /// Report directory; defaults to reports for CS2 or reports/rust for Rust.
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Poll public CS2 branch metadata and persist events to SQLite.
    Watch {
        #[arg(long, default_value = DEFAULT_DB)]
        database: String,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Args, Default)]
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
    Waxpeer,
}

impl From<Market> for Platform {
    fn from(value: Market) -> Self {
        match value {
            Market::Skinport => Self::Skinport,
            Market::Csfloat => Self::CsFloat,
            Market::Dmarket => Self::DMarket,
            Market::Waxpeer => Self::Waxpeer,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Settings {
    game: Game,
    markets: Option<Vec<Platform>>,
    dmarket_if_configured: bool,
    spread: SpreadConfig,
    dip: DipConfig,
    risk: RiskLimits,
    peg_bid: PegBidConfig,
    relist: RelistConfig,
}

impl Settings {
    fn market_allowlist(&self) -> Result<&[Platform]> {
        let markets = self.markets.as_deref().unwrap_or(match self.game {
            Game::Cs2 => &[Platform::Skinport, Platform::CsFloat, Platform::DMarket],
            Game::Rust => &[Platform::Skinport, Platform::Waxpeer, Platform::DMarket],
        });
        if markets.is_empty() {
            return Err(Error::InvalidConfig("markets must not be empty".into()));
        }
        for &platform in markets {
            if !self.game.supports_market(platform) {
                return Err(Error::InvalidConfig(format!(
                    "{platform:?} is not supported for {:?}",
                    self.game
                )));
            }
        }
        Ok(markets)
    }

    fn live_platforms(&self, requested: Vec<Market>, secrets: &Secrets) -> Result<Vec<Platform>> {
        let auto_dmarket = self.dmarket_if_configured && requested.is_empty();
        let mut platforms = live::select_platforms(
            self.game,
            requested,
            self.market_allowlist()?,
            self.markets.is_some(),
        )?;
        if auto_dmarket {
            platforms.retain(|&platform| platform != Platform::DMarket);
            if self.game.supports_market(Platform::DMarket) && secrets.require_dmarket().is_ok() {
                platforms.push(Platform::DMarket);
            }
        }
        Ok(platforms)
    }
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
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct WatchGame {
            game: Game,
        }
        let WatchGame { game } = toml::from_str(&config)
            .map_err(|err| Error::InvalidConfig(err.message().to_owned()))?;
        if game != Game::Cs2 {
            return Err(Error::InvalidConfig(
                "watch only supports CS2 (App ID 730)".into(),
            ));
        }
        return watch::run(&config, &database, once).await;
    }
    let fees = FeeSchedules::from_toml(&config)?;
    let settings: Settings =
        toml::from_str(&config).map_err(|err| Error::InvalidConfig(err.message().to_owned()))?;
    let game = settings.game;
    let markets = settings.market_allowlist()?;
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
                live::collect(
                    &config,
                    &fees,
                    settings.live_platforms(platform, &Secrets::from_env())?,
                    samples,
                    interval_seconds,
                    (game == Game::Cs2)
                        .then_some((database.as_str(), settings.dip.branch_update_window_hours)),
                    game,
                )
                .await?
            } else {
                historical(game, markets, history)?
            };
            (inputs, None)
        }
        Command::Backtest {
            history,
            output_dir,
        } => (
            historical(game, markets, history)?,
            Some(backtest_output_dir(game, output_dir)),
        ),
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

fn bundled_snapshots(game: Game) -> Result<Vec<PriceSnapshot>> {
    fixtures::parse_snapshots(match game {
        Game::Cs2 => include_str!("../../../data/fixtures/price_snapshots.json"),
        Game::Rust => include_str!("../../../data/fixtures/rust_price_snapshots.json"),
    })
}

fn validate_history_item(
    game: Game,
    platform: Platform,
    item: &ItemId,
    has_float: bool,
) -> Result<()> {
    if !game.supports_market(platform)
        || !game.supports_wear(item.wear_tier)
        || (item.wear_tier == cs2_arb_core::WearTier::NotApplicable && has_float)
    {
        return Err(Error::InvalidData(format!(
            "{platform:?} history for {:?} is incompatible with {game:?}",
            item.market_hash_name
        )));
    }
    Ok(())
}

fn filter_history_events(game: Game, events: &mut Vec<MarketEvent>) {
    events.retain(|event| match event {
        MarketEvent::BranchUpdated { app_id, .. } => *app_id == game.app_id(),
        MarketEvent::PanicWindowOpened { .. } | MarketEvent::PanicWindowClosed { .. } => true,
    });
}

fn historical(game: Game, markets: &[Platform], args: HistoricalArgs) -> Result<Inputs> {
    let bundled = args.snapshots.is_none();
    let mut snapshots = match args.snapshots {
        Some(path) => fixtures::load_snapshots(path)?,
        None if game == Game::Cs2 => {
            fixtures::load_snapshots("data/fixtures/price_snapshots.json")?
        }
        None => bundled_snapshots(game)?,
    };
    for snapshot in &snapshots {
        validate_history_item(
            game,
            snapshot.platform,
            &snapshot.item,
            snapshot.float_bucket.is_some(),
        )?;
    }
    snapshots.retain(|snapshot| markets.contains(&snapshot.platform));
    let mut events = match args.events {
        Some(path) => fixtures::load_events(path)?,
        None if bundled && game == Game::Cs2 => {
            fixtures::load_events("data/fixtures/market_events.json")?
        }
        None => Vec::new(),
    };
    filter_history_events(game, &mut events);
    let mut depth = MarketDepth::from_snapshots(&snapshots);
    if let Some(path) = args.sales {
        let sales: Vec<cs2_arb_core::Sale> = serde_json::from_str(&read(&path)?)
            .map_err(|err| Error::InvalidData(format!("invalid sales JSON: {err}")))?;
        for sale in &sales {
            validate_history_item(game, sale.platform, &sale.item, sale.float_value.is_some())?;
        }
        depth.add_sales(
            sales
                .into_iter()
                .filter(|sale| markets.contains(&sale.platform)),
        );
    } else if bundled {
        depth.add_bundled_mock_sales(&snapshots);
    }
    let mut buy_orders: Vec<BuyOrder> = if let Some(path) = args.buy_orders {
        serde_json::from_str(&read(&path)?)
            .map_err(|err| Error::InvalidData(format!("invalid buy orders JSON: {err}")))?
    } else if bundled {
        peg_runner::derive_offline_buy_orders(&snapshots)
    } else {
        Vec::new()
    };
    for buy_order in &buy_orders {
        validate_history_item(
            game,
            buy_order.platform,
            &buy_order.item,
            buy_order.float_bucket.is_some(),
        )?;
    }
    buy_orders.retain(|bid| markets.contains(&bid.platform));
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

fn backtest_output_dir(game: Game, output_dir: Option<PathBuf>) -> PathBuf {
    output_dir.unwrap_or_else(|| {
        PathBuf::from(match game {
            Game::Cs2 => "reports",
            Game::Rust => "reports/rust",
        })
    })
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
    use cs2_arb_backtest::ops::secrets::vars::{DMARKET_PUBLIC_KEY, DMARKET_SECRET_KEY};

    use super::*;

    #[test]
    fn cli_exposes_only_mock_and_read_only_commands() {
        for command in ["run-mock", "backtest", "watch"] {
            assert!(Cli::try_parse_from(["cs2-arb", command]).is_ok());
        }
        assert!(Cli::try_parse_from(["cs2-arb", "run-live"]).is_err());
    }

    #[test]
    fn backtest_reports_default_to_separate_directories_and_allow_overrides() {
        for (game, default) in [(Game::Cs2, "reports"), (Game::Rust, "reports/rust")] {
            for override_dir in [None, Some("reports/custom")] {
                let mut args = vec!["cs2-arb", "backtest"];
                if let Some(path) = override_dir {
                    args.extend(["--output-dir", path]);
                }
                let Command::Backtest { output_dir, .. } =
                    Cli::try_parse_from(args).unwrap().command
                else {
                    panic!("expected backtest command");
                };
                assert_eq!(
                    backtest_output_dir(game, output_dir),
                    Path::new(override_dir.unwrap_or(default))
                );
            }
        }
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

    #[test]
    fn rust_profile_replays_exteriorless_items() {
        let default: Settings = toml::from_str("").unwrap();
        assert_eq!(default.game, Game::Cs2);
        let config = include_str!("../../../config/rust.toml");
        let settings: Settings = toml::from_str(config).unwrap();
        assert_eq!(settings.game, Game::Rust);
        assert!(toml::from_str::<Settings>("game = 'unknown'").is_err());
        let inputs = historical(
            settings.game,
            settings.market_allowlist().unwrap(),
            HistoricalArgs::default(),
        )
        .unwrap();
        assert!(inputs.events.is_empty());
        assert!(!inputs.depth.sales().is_empty());
        assert!(inputs.snapshots.iter().all(|snapshot| {
            snapshot.item.wear_tier == cs2_arb_core::WearTier::NotApplicable
                && snapshot.float_bucket.is_none()
        }));
        let report = simulate(
            &inputs,
            FeeSchedules::from_toml(config).unwrap(),
            settings,
            KillSwitch::new(),
        )
        .unwrap();
        assert!(report.strategy("spread").unwrap().trade_count > 0);
    }

    #[test]
    fn historical_branch_events_are_filtered_for_both_games() {
        let observed_at = chrono::DateTime::from_timestamp(1_740_000_000, 0).unwrap();
        let branch = |game: Game| MarketEvent::BranchUpdated {
            app_id: game.app_id(),
            branch: "staging".into(),
            change_number: 1,
            observed_at,
        };
        let explicit = MarketEvent::PanicWindowOpened {
            branch: "staging".into(),
            opened_at: observed_at,
        };
        for game in [Game::Cs2, Game::Rust] {
            let mut events = vec![branch(Game::Cs2), branch(Game::Rust), explicit.clone()];
            filter_history_events(game, &mut events);
            assert_eq!(events, [branch(game), explicit.clone()]);
        }
    }

    #[test]
    fn rust_live_platforms_exclude_csfloat() {
        assert_eq!(
            live::select_platforms(Game::Cs2, Vec::new(), Platform::ALL.as_slice(), false).unwrap(),
            [Platform::Skinport, Platform::CsFloat]
        );
        assert_eq!(
            live::select_platforms(Game::Rust, Vec::new(), Platform::ALL.as_slice(), false)
                .unwrap(),
            [Platform::Skinport, Platform::Waxpeer]
        );
        assert_eq!(
            live::select_platforms(
                Game::Rust,
                vec![Market::Dmarket],
                Platform::ALL.as_slice(),
                false
            )
            .unwrap(),
            [Platform::DMarket]
        );
        assert!(live::select_platforms(
            Game::Rust,
            vec![Market::Csfloat],
            Platform::ALL.as_slice(),
            false
        )
        .is_err());
    }

    #[test]
    fn shipped_profiles_add_dmarket_to_default_live_reads_only_with_both_keys() {
        for (config, other_venue) in [
            (
                include_str!("../../../config/default.toml"),
                Platform::CsFloat,
            ),
            (include_str!("../../../config/rust.toml"), Platform::Waxpeer),
        ] {
            let settings: Settings = toml::from_str(config).unwrap();
            for (public, secret) in [
                (None, None),
                (Some("example"), None),
                (None, Some("example")),
                (Some("example"), Some(" ")),
                (Some("example"), Some("example")),
            ] {
                let secrets = Secrets::from_lookup(|name| match name {
                    DMARKET_PUBLIC_KEY => public.map(str::to_owned),
                    DMARKET_SECRET_KEY => secret.map(str::to_owned),
                    _ => None,
                });
                let mut expected = vec![Platform::Skinport, other_venue];
                if public == Some("example") && secret == Some("example") {
                    expected.push(Platform::DMarket);
                }
                assert_eq!(settings.live_platforms(vec![], &secrets).unwrap(), expected);
                assert_eq!(
                    settings
                        .live_platforms(vec![Market::Skinport], &secrets)
                        .unwrap(),
                    [Platform::Skinport]
                );
            }
        }
    }

    #[test]
    fn market_allowlist_filters_all_replay_inputs_and_live_selection() {
        let settings: Settings = toml::from_str("game = 'rust'\nmarkets = ['waxpeer']").unwrap();
        let markets = settings.market_allowlist().unwrap();
        let inputs = historical(settings.game, markets, HistoricalArgs::default()).unwrap();
        assert!(!inputs.snapshots.is_empty());
        assert!(inputs
            .snapshots
            .iter()
            .all(|q| q.platform == Platform::Waxpeer));
        assert!(!inputs.depth.sales().is_empty());
        assert!(inputs
            .depth
            .sales()
            .iter()
            .all(|s| s.platform == Platform::Waxpeer));
        assert!(!inputs.buy_orders.is_empty());
        assert!(inputs
            .buy_orders
            .iter()
            .all(|bid| bid.platform == Platform::Waxpeer));
        assert_eq!(
            live::select_platforms(settings.game, vec![], markets, true).unwrap(),
            markets
        );
        assert!(
            live::select_platforms(settings.game, vec![Market::Skinport], markets, true).is_err()
        );
        let invalid: Settings = toml::from_str("game = 'rust'\nmarkets = ['csfloat']").unwrap();
        assert!(invalid.market_allowlist().is_err());
    }
}
