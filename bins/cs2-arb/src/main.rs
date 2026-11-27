use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::TimeDelta;
use clap::{Args, Parser, Subcommand, ValueEnum};
use cs2_arb_backtest::integrated::{DipAdapter, MarketDepth, SimFillModel, SpreadAdapter};
use cs2_arb_backtest::{
    fixtures, BacktestConfig, Backtester, PerformanceReport, Secrets, StrategyAdapter,
};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedules, Game, ItemId, MarketEvent, Platform, PriceSnapshot, Result,
    SimulationMode,
};
use cs2_arb_sim::venues::{VenueAccounts, VenueAccountsConfig};
use cs2_arb_sim::{FillLatency, LiquidityConfig, SimConfig};
use cs2_arb_strategy::regime::{MarketRegime, RegimeConfig};
use cs2_arb_strategy::{
    DipConfig, KillSwitch, PegBidConfig, RelistConfig, RiskLimits, SpreadConfig,
};
use serde::Deserialize;

mod live;
mod peg_runner;
mod platform_audit;
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
    venue_accounts: Option<VenueAccountsConfig>,
    platform_audit: bool,
    simulation: SimulationSettings,
    dmarket_if_configured: bool,
    spread: SpreadConfig,
    dip: DipConfig,
    risk: RiskLimits,
    peg_bid: PegBidConfig,
    market_regime: RegimeConfig,
    relist: RelistConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SimulationSettings {
    maximum_listing_age_seconds: Option<u32>,
    fill_latency_min_milliseconds: u32,
    fill_latency_max_milliseconds: u32,
    minimum_verified_sales: Option<u32>,
    liquidity_window_days: Option<u32>,
    maximum_adv_fraction_bps: Option<u32>,
}

impl Settings {
    fn live_sales_lookback_days(&self) -> i64 {
        let simulation = self
            .simulation
            .liquidity_window_days
            .unwrap_or_else(|| SimConfig::default().liquidity.window_days);
        i64::from(simulation)
            .max(self.dip.volume_history_days())
            .max(self.relist.lookback_days)
            .max(self.relist.volume_window_days)
            .max(self.peg_bid.lookback_days)
            .max(8)
    }
}

impl SimulationSettings {
    fn build(&self) -> Result<SimConfig> {
        let defaults = SimConfig::default();
        let liquidity = defaults.liquidity;
        let config = SimConfig {
            max_listing_age: self
                .maximum_listing_age_seconds
                .map_or(defaults.max_listing_age, |secs| {
                    TimeDelta::seconds(i64::from(secs))
                }),
            liquidity: LiquidityConfig {
                min_verified_sales: self
                    .minimum_verified_sales
                    .unwrap_or(liquidity.min_verified_sales),
                window_days: self.liquidity_window_days.unwrap_or(liquidity.window_days),
                max_adv_fraction_bps: self
                    .maximum_adv_fraction_bps
                    .unwrap_or(liquidity.max_adv_fraction_bps),
                ..liquidity
            },
            fill_latency: FillLatency {
                min: TimeDelta::milliseconds(i64::from(self.fill_latency_min_milliseconds)),
                max: TimeDelta::milliseconds(i64::from(self.fill_latency_max_milliseconds)),
            },
            ..defaults
        };
        config.validate()?;
        Ok(config)
    }
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
        let allowlist = self.market_allowlist()?;
        let mut platforms =
            live::select_platforms(self.game, requested, allowlist, self.markets.is_some())?;
        if auto_dmarket {
            platforms.retain(|&platform| platform != Platform::DMarket);
            if allowlist.contains(&Platform::DMarket) && secrets.require_dmarket().is_ok() {
                match live::dmarket_credentials(secrets) {
                    Ok(_) => platforms.push(Platform::DMarket),
                    Err(_) => tracing::warn!("invalid DMarket credential pair; skipping DMarket"),
                }
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
    let sources = live::MarketSources {
        secrets: &Secrets::from_env(),
        base_url_override: None,
    };
    if let Err(err) = run(cli, &sources, io::stdout()).await {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(
    cli: Cli,
    market_sources: &live::MarketSources<'_>,
    mut mock_report: impl Write,
) -> Result<()> {
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
    let mut fees = FeeSchedules::from_toml(&config)?;
    let settings: Settings =
        toml::from_str(&config).map_err(|err| Error::InvalidConfig(err.message().to_owned()))?;
    let game = settings.game;
    let markets = settings.market_allowlist()?;
    let audit_enabled = settings.platform_audit
        || settings.venue_accounts.is_some()
        || settings.relist.instant_sale;
    let live_platforms = match &cli.command {
        Command::RunMock {
            live_market_data: true,
            platform,
            ..
        } => Some(settings.live_platforms(platform.clone(), market_sources.secrets)?),
        _ => None,
    };
    let audited_markets = live_platforms.as_deref().unwrap_or(markets).to_vec();
    let accounts = if let Some(rules) = &settings.venue_accounts {
        rules.validate(markets)?;
        rules.validate_transfer_venues(&audited_markets)?;
        let accounts = VenueAccounts::new(rules.clone(), fees.clone());
        for schedule in [
            &mut fees.skinport,
            &mut fees.csfloat,
            &mut fees.dmarket,
            &mut fees.waxpeer,
        ] {
            schedule.funding.crypto_deposit = Default::default();
            schedule.funding.crypto_withdrawal = Default::default();
            schedule.funding.bank_cashout = Default::default();
        }
        Some(accounts)
    } else {
        None
    };
    if let Some(accounts) = &accounts {
        SpreadAdapter::validate_holds(
            &settings.spread,
            &accounts.trade_holds_for(&audited_markets),
        )?;
    }
    let (inputs, output, history) = match cli.command {
        Command::RunMock {
            history,
            samples,
            interval_seconds,
            database,
            ..
        } => {
            if let Some(platforms) = &live_platforms {
                let (inputs, history) = live::collect(
                    &config,
                    &fees,
                    platforms.clone(),
                    samples,
                    interval_seconds,
                    &database,
                    settings.dip.branch_update_window_hours,
                    game,
                    settings.relist.instant_sale,
                    settings.live_sales_lookback_days(),
                    settings.simulation.build()?.max_listing_age,
                    market_sources,
                )
                .await?;
                (inputs, None, Some(history))
            } else {
                (historical(game, markets, history)?, None, None)
            }
        }
        Command::Backtest {
            history,
            output_dir,
        } => (
            historical(game, markets, history)?,
            Some(backtest_output_dir(game, output_dir)),
            None,
        ),
        Command::Watch { .. } => unreachable!("handled before loading market configuration"),
    };
    let kill_switch = KillSwitch::with_defaults();
    let peg_config = settings.peg_bid.clone();
    let regime = Arc::new(MarketRegime::new(
        settings.market_regime.clone(),
        history
            .as_ref()
            .map_or(&inputs.snapshots, |tape| &tape.snapshots),
        history
            .as_ref()
            .map_or(inputs.depth.sales(), |tape| &tape.sales),
    )?);
    let mut relist_config = settings.relist.clone();
    relist_config.instant_sale &= inputs.has_observed_bids;
    let risk_limits = settings.risk.clone();
    let mut report = simulate_with_accounts(
        &inputs,
        fees.clone(),
        settings,
        kill_switch.clone(),
        (accounts.clone(), &audited_markets),
        live_platforms.is_some(),
        Arc::clone(&regime),
    )?;
    if audit_enabled || live_platforms.is_some() {
        report.unverified_platform_assumptions = platform_audit::unverified(&audited_markets);
    }
    let mut peg_dashboard = peg_runner::run_peg_bid_engine(
        &inputs.snapshots,
        inputs.depth.sales(),
        &inputs.buy_orders,
        &inputs.bid_refreshes,
        &fees,
        peg_config,
        relist_config,
        risk_limits,
        &kill_switch,
        None,
        accounts,
        Some(&regime),
    )?;
    peg_dashboard.history_coverage = history.as_ref().map(peg_runner::HistoryCoverage::from);
    peg_dashboard.log_summary();
    if let Some(directory) = output {
        write_reports(&directory, &report, &peg_dashboard)?;
    } else {
        write!(
            mock_report,
            "{}{}",
            report.to_markdown(),
            peg_dashboard.to_markdown()
        )
        .map_err(|err| Error::Storage(err.to_string()))?;
    }
    Ok(())
}

struct Inputs {
    snapshots: Vec<PriceSnapshot>,
    events: Vec<MarketEvent>,
    depth: MarketDepth,
    buy_orders: Vec<BuyOrder>,
    bid_refreshes: Vec<peg_runner::BidRefresh>,
    has_observed_bids: bool,
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
    let has_observed_bids = args.buy_orders.is_some();
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
        bid_refreshes: Vec::new(),
        has_observed_bids,
    })
}

#[cfg(test)]
fn simulate(
    inputs: &Inputs,
    fees: FeeSchedules,
    settings: Settings,
    kill_switch: KillSwitch,
) -> Result<PerformanceReport> {
    let regime = Arc::new(MarketRegime::new(
        settings.market_regime.clone(),
        &inputs.snapshots,
        inputs.depth.sales(),
    )?);
    simulate_with_accounts(
        inputs,
        fees,
        settings,
        kill_switch,
        (None, &Platform::ALL),
        false,
        regime,
    )
}

fn simulate_with_accounts(
    inputs: &Inputs,
    fees: FeeSchedules,
    settings: Settings,
    kill_switch: KillSwitch,
    (accounts, active_platforms): (Option<VenueAccounts>, &[Platform]),
    live_batch: bool,
    regime: Arc<MarketRegime>,
) -> Result<PerformanceReport> {
    let mut dip = DipAdapter::new(settings.dip, &inputs.depth)?.with_market_regime(regime);
    if live_batch {
        if let Some(first_quote) = inputs.snapshots.iter().map(|q| q.observed_at).min() {
            dip.seed_daily_sale_prices(inputs.depth.sales(), first_quote);
        }
    }
    let mut strategies: Vec<Box<dyn StrategyAdapter>> =
        vec![Box::new(SpreadAdapter::new(settings.spread)), Box::new(dip)];
    let mut model = SimFillModel::new(&inputs.depth, fees.clone(), settings.risk, kill_switch)
        .with_sim_config(settings.simulation.build()?);
    if let Some(accounts) = accounts {
        model = model
            .with_venue_accounts(accounts)
            .with_active_platforms(active_platforms.to_vec());
    }
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
    use chrono::Utc;
    use cs2_arb_backtest::ops::secrets::vars::{DMARKET_PUBLIC_KEY, DMARKET_SECRET_KEY};
    use cs2_arb_core::{Money, WearTier};
    use cs2_arb_events::EventQueue;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn cs2_live_batch_writes_a_report_with_only_waxpeer_quotes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/items"))
            .and(query_param("app_id", "730"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1..)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/listings"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/prices/snapshot"))
            .and(query_param("game", "csgo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "items": [{
                    "name": "AK-47 | Redline (Field-Tested)",
                    "item_id": "ask-1",
                    "price": 25000,
                }],
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/buy-orders/snapshot"))
            .and(query_param("game", "csgo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "offers": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default.toml");
        let cli = Cli::try_parse_from([
            "cs2-arb",
            "--config",
            config,
            "run-mock",
            "--live-market-data",
            "--samples",
            "1",
            "--database",
            ":memory:",
        ])
        .unwrap();
        let sources = live::MarketSources {
            secrets: &Secrets::default(),
            base_url_override: Some(&server.uri()),
        };
        let mut report = tempfile::NamedTempFile::new().unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run(cli, &sources, report.as_file_mut()),
        )
        .await
        .expect("mock batch timed out")
        .unwrap();

        let markdown = read(report.path()).unwrap();
        assert!(markdown.starts_with("# Backtest performance report"));
        assert!(markdown.contains("· 1 snapshots ·"));
        assert!(markdown.contains("Total P&L"));
        assert!(markdown.contains("| Regime History Quotes | 1 |"));
    }

    #[tokio::test]
    async fn rust_batches_reopen_regime_history_and_stay_closed_until_both_anchors_exist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/prices/snapshot"))
            .and(query_param("game", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "items": [{"name": "Alien Red", "item_id": "ask-1", "price": 29000}],
            })))
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/buy-orders/snapshot"))
            .and(query_param("game", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "offers": [],
            })))
            .expect(3)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let db = db.to_str().unwrap();
        let config = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/rust.toml");
        let sources = live::MarketSources {
            secrets: &Secrets::default(),
            base_url_override: Some(&server.uri()),
        };
        let now = Utc::now();
        for (prior_day, label, quotes) in [
            (None, "Risk-off: insufficient history", 1),
            (Some(7), "Risk-off: insufficient history", 3),
            (Some(1), "Risk-on", 5),
        ] {
            if let Some(days) = prior_day {
                let at = now - TimeDelta::days(days) - TimeDelta::hours(1);
                let queue = EventQueue::open(db).await.unwrap();
                queue
                    .record_market_history(
                        Game::Rust,
                        &[PriceSnapshot {
                            item: ItemId {
                                market_hash_name: "Alien Red".into(),
                                wear_tier: WearTier::NotApplicable,
                            },
                            platform: Platform::Waxpeer,
                            float_bucket: None,
                            price: Money(2900),
                            observed_at: at,
                        }],
                        &[],
                        at - TimeDelta::days(10),
                    )
                    .await
                    .unwrap();
                queue.close().await;
            }
            let cli = Cli::try_parse_from([
                "cs2-arb",
                "--config",
                config,
                "run-mock",
                "--live-market-data",
                "--platform",
                "waxpeer",
                "--samples",
                "1",
                "--database",
                db,
            ])
            .unwrap();
            let mut report = Vec::new();
            run(cli, &sources, &mut report).await.unwrap();
            let markdown = String::from_utf8(report).unwrap();
            assert!(
                markdown.contains(&format!("| Market Regime | {label} |")),
                "{markdown}"
            );
            assert!(markdown.contains(&format!("| Regime History Quotes | {quotes} |")));
            assert!(
                markdown.contains("· 1 snapshots ·"),
                "prior quotes must not be replayed"
            );
        }
    }

    #[test]
    fn live_sales_lookback_covers_configured_simulation_window() {
        let settings: Settings =
            toml::from_str("[simulation]\nliquidity_window_days = 45").unwrap();
        assert_eq!(settings.live_sales_lookback_days(), 45);
        let settings: Settings = toml::from_str(
            "[simulation]\nliquidity_window_days = 14\n[relist]\nlookback_days = 10\nvolume_window_days = 60",
        ).unwrap();
        assert_eq!(settings.live_sales_lookback_days(), 60);
        assert_eq!(Settings::default().live_sales_lookback_days(), 37);
    }
    const EXAMPLE_DMARKET_PUBLIC: &str =
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const EXAMPLE_DMARKET_SEED: &str =
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn dmarket_secrets(public: Option<&str>, secret: Option<&str>) -> Secrets {
        Secrets::from_lookup(|name| match name {
            DMARKET_PUBLIC_KEY => public.map(str::to_owned),
            DMARKET_SECRET_KEY => secret.map(str::to_owned),
            _ => None,
        })
    }

    #[test]
    fn selected_live_venues_scope_spread_hold_validation() {
        let settings: Settings = toml::from_str(
            r#"
markets = ["skinport", "csfloat"]

[spread]
price_drift_pct_per_day = 10.0

[venue_accounts.venues.skinport]
starting_cash = 1000
trade_hold_seconds = 86400
sale_balance_hold_seconds = 0
transfer_in_seconds = 0
transfer_out_seconds = 0

[venue_accounts.venues.csfloat]
starting_cash = 1000
trade_hold_seconds = 864000
sale_balance_hold_seconds = 0
transfer_in_seconds = 0
transfer_out_seconds = 0
"#,
        )
        .unwrap();
        let rules = settings.venue_accounts.clone().unwrap();
        rules
            .validate(settings.market_allowlist().unwrap())
            .unwrap();
        let selected = settings
            .live_platforms(vec![Market::Skinport], &dmarket_secrets(None, None))
            .unwrap();
        let fees = FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap();
        let accounts = VenueAccounts::new(rules, fees.clone());
        SpreadAdapter::validate_holds(&settings.spread, &accounts.trade_holds_for(&selected))
            .unwrap();
        assert!(SpreadAdapter::validate_holds(
            &settings.spread,
            &accounts.trade_holds_for(&[Platform::CsFloat])
        )
        .is_err());
        let inputs = Inputs {
            snapshots: Vec::new(),
            events: Vec::new(),
            depth: MarketDepth::default(),
            buy_orders: Vec::new(),
            bid_refreshes: Vec::new(),
            has_observed_bids: true,
        };
        let report = simulate_with_accounts(
            &inputs,
            fees,
            settings,
            KillSwitch::new(),
            (Some(accounts), &selected),
            true,
            Arc::new(MarketRegime::new(RegimeConfig::default(), &[], &[]).unwrap()),
        )
        .unwrap();
        assert_eq!(report.venue_trade_holds.len(), 1);
        assert_eq!(report.venue_trade_holds[0].platform, Platform::Skinport);
    }

    #[test]
    fn selected_live_venues_scope_scheduled_transfers() {
        let rules: VenueAccountsConfig = toml::from_str(
            r#"
[[transfers]]
at = "2025-01-08T12:00:00Z"
from = "skinport"
to = "csfloat"
amount = 100
"#,
        )
        .unwrap();
        let settings: Settings = toml::from_str(r#"markets = ["skinport", "csfloat"]"#).unwrap();
        let secrets = dmarket_secrets(None, None);
        for market in [Market::Skinport, Market::Csfloat] {
            let selected = settings.live_platforms(vec![market], &secrets).unwrap();
            let error = rules.validate_transfer_venues(&selected).unwrap_err();
            assert!(matches!(error, Error::InvalidConfig(message)
                if message.contains("requires both venues to be selected")));
        }
        let selected = settings.live_platforms(vec![], &secrets).unwrap();
        rules.validate_transfer_venues(&selected).unwrap();
    }

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
            bid_refreshes: Vec::new(),
            has_observed_bids: false,
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
        assert_eq!(report.strategy("dip").unwrap().trade_count, 0);
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
        let regime = MarketRegime::new(
            settings.market_regime.clone(),
            &inputs.snapshots,
            inputs.depth.sales(),
        )
        .unwrap();
        assert!(inputs
            .snapshots
            .iter()
            .any(|quote| regime.at(quote.observed_at).allows(&quote.item)));
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
        let profiles: [(&str, &[Platform]); 2] = [
            (
                include_str!("../../../config/default.toml"),
                &[Platform::Skinport, Platform::CsFloat, Platform::Waxpeer],
            ),
            (
                include_str!("../../../config/rust.toml"),
                &[Platform::Skinport, Platform::Waxpeer],
            ),
        ];
        for (config, public_platforms) in profiles {
            let settings: Settings = toml::from_str(config).unwrap();
            let public = Some(EXAMPLE_DMARKET_PUBLIC);
            let secret = Some(EXAMPLE_DMARKET_SEED);
            for (credentials, enable) in [
                (dmarket_secrets(None, None), false),
                (dmarket_secrets(public, None), false),
                (dmarket_secrets(None, secret), false),
                (dmarket_secrets(public, Some(" ")), false),
                (dmarket_secrets(public, secret), true),
            ] {
                let mut expected = public_platforms.to_vec();
                if enable {
                    expected.push(Platform::DMarket);
                }
                assert_eq!(
                    settings.live_platforms(vec![], &credentials).unwrap(),
                    expected
                );
                assert_eq!(
                    settings
                        .live_platforms(vec![Market::Skinport], &credentials)
                        .unwrap(),
                    [Platform::Skinport]
                );
            }
        }
    }

    #[test]
    fn malformed_or_mismatched_dmarket_keys_do_not_break_default_live_reads() {
        let profiles: [(&str, &[Platform]); 2] = [
            (
                include_str!("../../../config/default.toml"),
                &[Platform::Skinport, Platform::CsFloat, Platform::Waxpeer],
            ),
            (
                include_str!("../../../config/rust.toml"),
                &[Platform::Skinport, Platform::Waxpeer],
            ),
        ];
        for (config, public_platforms) in profiles {
            let settings: Settings = toml::from_str(config).unwrap();
            let mismatched_seed = "00".repeat(32);
            for seed in ["not-hex", mismatched_seed.as_str()] {
                let secrets = dmarket_secrets(Some(EXAMPLE_DMARKET_PUBLIC), Some(seed));
                assert!(live::dmarket_credentials(&secrets).is_err());
                assert_eq!(
                    settings.live_platforms(vec![], &secrets).unwrap(),
                    public_platforms
                );
            }
        }
    }

    #[test]
    fn automatic_dmarket_selection_respects_an_explicit_allowlist() {
        let settings: Settings =
            toml::from_str("game = 'rust'\nmarkets = ['waxpeer']\ndmarket_if_configured = true")
                .unwrap();
        let secrets = dmarket_secrets(Some(EXAMPLE_DMARKET_PUBLIC), Some(EXAMPLE_DMARKET_SEED));
        assert!(live::dmarket_credentials(&secrets).is_ok());
        assert_eq!(
            settings.live_platforms(vec![], &secrets).unwrap(),
            [Platform::Waxpeer]
        );
        assert!(settings
            .live_platforms(vec![Market::Dmarket], &secrets)
            .is_err());
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
