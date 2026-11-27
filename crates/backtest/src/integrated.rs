use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, FeeSchedules, Fill, FloatBucket, ItemId, Listing, ManualClock, Money,
    Order, OrderSide, Platform, Position, PriceSnapshot, Result, Sale, SimulationMode,
};
use cs2_arb_sim::{wear::quote_bucket, MockExecutor, SimConfig};
use cs2_arb_strategy::executor::execute_with_risk;
use cs2_arb_strategy::{
    evaluate_spread, evaluate_spread_with_holds, spread_breakdown_with_hold, DipConfig,
    DipDetector, KillSwitch, RiskLimits, RiskManager, SpreadBreakdown, SpreadConfig, SubmitError,
};

use crate::{FillModel, FillOutcome, MarketView, QuoteKey, StrategyAdapter};

type PositionKey = (ItemId, Option<FloatBucket>);
type BookKey = (Platform, ItemId);
type Books = HashMap<BookKey, Vec<Listing>>;

/// Observations and individual sales used for simulated liquidity and fills.
#[derive(Default)]
pub struct MarketDepth {
    frames: BTreeMap<DateTime<Utc>, Books>,
    replaced_books: BTreeMap<DateTime<Utc>, HashSet<BookKey>>,
    sales: Vec<Sale>,
}

impl MarketDepth {
    pub fn replace_listings(
        &mut self,
        at: DateTime<Utc>,
        platform: Platform,
        item: ItemId,
        listings: Vec<Listing>,
    ) {
        self.replaced_books
            .entry(at)
            .or_default()
            .insert((platform, item.clone()));
        self.frames
            .entry(at)
            .or_default()
            .insert((platform, item), listings);
    }

    pub fn add_sales(&mut self, sales: impl IntoIterator<Item = Sale>) {
        self.sales.extend(sales);
    }

    pub fn sales(&self) -> &[Sale] {
        &self.sales
    }

    /// Convert historical quotes into one-unit mock listings. A quote is not a sale.
    pub fn from_snapshots(snapshots: &[PriceSnapshot]) -> Self {
        let mut depth = Self::default();
        for (index, quote) in snapshots.iter().enumerate() {
            depth
                .frames
                .entry(quote.observed_at)
                .or_default()
                .entry((quote.platform, quote.item.clone()))
                .or_default()
                .push(Listing {
                    id: format!("quote-{index}"),
                    platform: quote.platform,
                    item: quote.item.clone(),
                    price: quote.price,
                    quantity: 1,
                    float_value: midpoint(quote.float_bucket),
                    observed_at: quote.observed_at,
                    trade_locked_until: None,
                });
        }
        depth
    }

    /// Separate deterministic mock tape for the bundled fixture scenario only.
    /// The verified flag means a modeled settled sale, not a real marketplace record.
    pub fn add_bundled_mock_sales(&mut self, snapshots: &[PriceSnapshot]) {
        self.add_sales(snapshots.iter().enumerate().flat_map(|(index, quote)| {
            (1..=10).map(move |n| Sale {
                id: format!("fixture-sale-{index}-{n}"),
                platform: quote.platform,
                item: quote.item.clone(),
                price: quote.price,
                float_value: midpoint(quote.float_bucket),
                sold_at: quote.observed_at + Duration::hours(n),
                verified: true,
            })
        }));
    }

    fn verified_volume(&self, quote: &PriceSnapshot, days: i64) -> usize {
        let after = quote.observed_at - Duration::days(days);
        self.sales
            .iter()
            .filter(|sale| {
                sale.verified
                    && sale.platform == quote.platform
                    && sale.item == quote.item
                    && sale.sold_at > after
                    && sale.sold_at <= quote.observed_at
                    && quote_bucket(sale.item.wear_tier, sale.float_value) == Ok(quote.float_bucket)
            })
            .map(|sale| &sale.id)
            .collect::<HashSet<_>>()
            .len()
    }
}

fn midpoint(bucket: Option<FloatBucket>) -> Option<f64> {
    bucket.map(|bucket| {
        let (low, high) = bucket.float_bounds();
        (low + high) / 2.0
    })
}

struct Session {
    clock: Arc<ManualClock>,
    executor: MockExecutor,
    risk: RiskManager,
}

/// Historical adapter for the real mock executor. Each strategy gets its own
/// portfolio and risk ledger; each fill reported is a fill from MockExecutor.
pub struct SimFillModel<'a> {
    depth: &'a MarketDepth,
    fees: FeeSchedules,
    risk_limits: RiskLimits,
    kill_switch: KillSwitch,
    sessions: RefCell<HashMap<String, Session>>,
    venue_accounts: Option<cs2_arb_sim::venues::VenueAccounts>,
    sim_config: SimConfig,
}

impl<'a> SimFillModel<'a> {
    pub fn new(
        depth: &'a MarketDepth,
        fees: FeeSchedules,
        risk_limits: RiskLimits,
        kill_switch: KillSwitch,
    ) -> Self {
        Self {
            depth,
            fees,
            risk_limits,
            kill_switch,
            sessions: RefCell::default(),
            venue_accounts: None,
            sim_config: SimConfig::default(),
        }
    }

    pub fn with_venue_accounts(mut self, accounts: cs2_arb_sim::venues::VenueAccounts) -> Self {
        self.venue_accounts = Some(accounts);
        self
    }

    pub fn with_sim_config(mut self, config: SimConfig) -> Self {
        self.sim_config = config;
        self
    }
}

impl FillModel for SimFillModel<'_> {
    fn schedule_fill(&self, strategy: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        self.sessions
            .borrow_mut()
            .get_mut(strategy)
            .expect("strategy prepared")
            .executor
            .sample_fill_time(now)
    }

    fn uses_venue_holds(&self) -> bool {
        self.venue_accounts.is_some()
    }

    fn venue_trade_holds(&self) -> Vec<cs2_arb_sim::venues::VenueTradeHold> {
        self.venue_accounts
            .as_ref()
            .map_or_else(Vec::new, cs2_arb_sim::venues::VenueAccounts::trade_holds)
    }

    fn preview_fill(&self, strategy: &str, order: &Order) -> Option<(Money, u32)> {
        self.sessions
            .borrow()
            .get(strategy)?
            .executor
            .preview_fill(order)
            .ok()
    }

    fn venue_balances(&self, strategy: &str) -> Result<Vec<cs2_arb_sim::venues::VenueBalance>> {
        self.sessions.borrow().get(strategy).map_or_else(
            || {
                self.venue_accounts
                    .as_ref()
                    .map_or_else(|| Ok(Vec::new()), |a| a.balances())
            },
            |session| session.executor.portfolio().venue_balances(),
        )
    }

    fn available_cash(&self, strategy: &str, platform: Platform) -> Option<Money> {
        self.sessions
            .borrow()
            .get(strategy)
            .and_then(|session| session.executor.portfolio().available_on(platform))
    }

    fn transfer_fees_paid(&self, strategy: &str) -> Money {
        self.sessions.borrow().get(strategy).map_or_else(
            || {
                self.venue_accounts.as_ref().map_or(
                    Money(0),
                    cs2_arb_sim::venues::VenueAccounts::transfer_fees_paid,
                )
            },
            |session| session.executor.portfolio().transfer_fees_paid(),
        )
    }

    fn add_quote_update_times(&self, times: &mut BTreeSet<DateTime<Utc>>) {
        times.extend(self.depth.replaced_books.keys().copied());
    }

    fn invalidate_quotes(&self, at: DateTime<Utc>, quotes: &mut HashMap<QuoteKey, PriceSnapshot>) {
        if let Some(books) = self.depth.replaced_books.get(&at) {
            quotes.retain(|key, _| !books.contains(&(key.platform, key.item.clone())));
        }
    }

    fn prepare_run(&self) {
        self.sessions.borrow_mut().clear();
    }

    fn prepare_step(&self, strategy: &str, view: &MarketView<'_>) -> Result<()> {
        let mut sessions = self.sessions.borrow_mut();
        if !sessions.contains_key(strategy) {
            let clock = Arc::new(ManualClock::new(view.now));
            let mut executor = MockExecutor::new(
                SimulationMode,
                self.sim_config.clone(),
                &self.fees,
                clock.clone(),
                730,
            )?
            .with_order_fees(self.fees.clone())
            .with_replay_scheduling();
            if let Some(accounts) = &self.venue_accounts {
                executor.set_venue_accounts(accounts.clone());
            }
            executor
                .book_mut()
                .record_sales(self.depth.sales.iter().cloned());
            sessions.insert(
                strategy.to_owned(),
                Session {
                    clock,
                    executor,
                    risk: RiskManager::new(self.risk_limits.clone(), self.kill_switch.clone())?,
                },
            );
        }
        let session = sessions.get_mut(strategy).expect("session initialized");
        session.clock.set(view.now);
        session.executor.advance_venue_accounts(view.now)?;
        if let Some(books) = self.depth.frames.get(&view.now) {
            for ((platform, item), listings) in books {
                session
                    .executor
                    .book_mut()
                    .replace_listings(*platform, item, listings.clone());
            }
        }
        let unrealized = marked_unrealized(view, session.executor.portfolio().positions())?;
        session.risk.set_unrealized_pnl(unrealized, view.now);
        Ok(())
    }

    fn after_fill(&self, strategy: &str, view: &MarketView<'_>, fill: &Fill) -> Result<()> {
        let mut sessions = self.sessions.borrow_mut();
        let session = sessions
            .get_mut(strategy)
            .ok_or_else(|| Error::InvalidData("simulator step was not prepared".into()))?;
        let portfolio = session.executor.portfolio();
        let entry = portfolio
            .ledger()
            .last()
            .filter(|entry| entry.order_id == fill.order.id)
            .ok_or_else(|| Error::InvalidData("missing simulator ledger entry for fill".into()))?;
        let released_cost = match fill.order.side {
            OrderSide::Buy => Money(0),
            OrderSide::Sell => entry.cost_basis_released.ok_or_else(|| {
                Error::InvalidData("missing simulator cost basis for sell".into())
            })?,
        };
        let unrealized = marked_unrealized(view, portfolio.positions())?;
        session
            .risk
            .record_marked_fill(fill, unrealized, released_cost)?;
        Ok(())
    }

    fn fill(
        &self,
        _: &Order,
        _: &PriceSnapshot,
        _: FeeSchedule,
        _: DateTime<Utc>,
    ) -> Result<FillOutcome> {
        Err(Error::InvalidData(
            "simulator requires a replay strategy and prepared step".into(),
        ))
    }

    fn fill_for(
        &self,
        strategy: &str,
        order: &Order,
        _: &PriceSnapshot,
        _: FeeSchedule,
        now: DateTime<Utc>,
    ) -> Result<FillOutcome> {
        let mut sessions = self.sessions.borrow_mut();
        let Session { executor, risk, .. } = sessions
            .get_mut(strategy)
            .ok_or_else(|| Error::InvalidData("simulator step was not prepared".into()))?;
        match execute_with_risk(risk, executor, order.clone(), now) {
            Ok(fill) => Ok(FillOutcome::Filled(fill)),
            Err(SubmitError::Accounting(err)) => Err(err),
            Err(err) => Ok(FillOutcome::Rejected(err.to_string())),
        }
    }

    fn locked_until(&self, fill: &Fill) -> DateTime<Utc> {
        if let Some(unlock) = self.venue_accounts.as_ref().and_then(|accounts| {
            accounts
                .trade_unlock(fill.order.platform, fill.filled_at)
                .ok()
        }) {
            return unlock;
        }
        fill.filled_at + cs2_arb_sim::valve_trade_hold()
    }
}

fn marked_unrealized(view: &MarketView<'_>, positions: &[Position]) -> Result<Money> {
    positions.iter().try_fold(Money(0), |total, position| {
        if let Some((_, _, value)) =
            view.best_exit(&position.item, position.float_bucket, position.quantity)?
        {
            total.checked_add(value.checked_sub(position.cost_basis)?)
        } else {
            Ok(total)
        }
    })
}

pub struct SpreadAdapter {
    config: SpreadConfig,
    targets: HashMap<PositionKey, (Platform, DateTime<Utc>)>,
    pending: HashMap<String, SpreadBreakdown>,
}

impl SpreadAdapter {
    pub fn new(config: SpreadConfig) -> Self {
        Self {
            config,
            targets: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}

impl StrategyAdapter for SpreadAdapter {
    fn name(&self) -> &str {
        "spread"
    }

    fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>> {
        let mut occupied = occupied_keys(view.positions);
        self.targets.retain(|key, _| occupied.contains(key));
        let mut orders = Vec::new();
        for position in view.positions {
            let key = (position.item.clone(), position.float_bucket);
            if let Some(&(platform, not_before)) = self.targets.get(&key) {
                if view.now >= not_before && !position.is_trade_locked_at(view.now) {
                    if let Some(quote) =
                        view.latest_quote(platform, &position.item, position.float_bucket)
                    {
                        let price = quote.price.checked_sub(self.config.sell_undercut)?;
                        if price.0 > 0 {
                            if let Some((order, _)) =
                                profitable_exit(view, self.name(), position, quote, price)?
                            {
                                orders.push(order);
                            }
                        }
                    }
                }
            }
        }
        let quotes: Vec<_> = view.latest_quotes().cloned().collect();
        let mut executable = Vec::new();
        let venue_holds = view.venue_trade_holds();
        let hold = |platform| {
            view.uses_venue_holds().then(|| {
                venue_holds
                    .iter()
                    .find(|hold| hold.platform == platform)
                    .map_or_else(cs2_arb_sim::valve_trade_hold, |hold| {
                        chrono::Duration::seconds(hold.seconds)
                    })
            })
        };
        let candidates = if view.uses_venue_holds() {
            evaluate_spread_with_holds(&quotes, view.fees, &self.config, view.now, hold)?
        } else {
            evaluate_spread(&quotes, view.fees, &self.config, view.now)?
        };
        for candidate in candidates {
            let order = candidate.buy_order(view.now);
            let actual = view.preview_fill(self.name(), &order);
            let candidate = if let Some((price, quantity)) = actual {
                let Some(buy) = view.latest_quote(
                    candidate.buy_platform,
                    &candidate.item,
                    candidate.float_bucket,
                ) else {
                    continue;
                };
                let Some(sell) = view.latest_quote(
                    candidate.sell_platform,
                    &candidate.item,
                    candidate.float_bucket,
                ) else {
                    continue;
                };
                let config = SpreadConfig {
                    quantity,
                    ..self.config.clone()
                };
                spread_breakdown_with_hold(
                    &PriceSnapshot {
                        price,
                        ..buy.clone()
                    },
                    sell,
                    view.fees,
                    &config,
                    Some(candidate.hold),
                )?
            } else {
                candidate
            };
            if candidate.expected_net_profit > self.config.min_net_profit.max(Money(0)) {
                executable.push((actual.is_some(), candidate));
            }
        }
        executable.sort_by_key(|(can_fill, candidate)| {
            std::cmp::Reverse((*can_fill, candidate.expected_net_profit))
        });
        for (_, candidate) in executable {
            if view
                .available_cash(self.name(), candidate.buy_platform)
                .is_some_and(|cash| cash < candidate.buy_cost)
            {
                continue;
            }
            let key = (candidate.item.clone(), candidate.float_bucket);
            if occupied.insert(key) {
                let order = candidate.buy_order(view.now);
                self.pending.insert(order.id.clone(), candidate);
                orders.push(order);
            }
        }
        Ok(orders)
    }

    fn on_fill(&mut self, fill: &Fill) {
        if let Some(candidate) = self.pending.remove(&fill.order.id) {
            self.targets.insert(
                (fill.order.item.clone(), fill.order.float_bucket),
                (
                    candidate.sell_platform,
                    candidate.sell_not_before(fill.filled_at),
                ),
            );
        }
    }

    fn on_reject(&mut self, order: &Order, _reason: &str) {
        self.pending.remove(&order.id);
    }
}

pub struct DipAdapter<'a> {
    detector: DipDetector,
    depth: &'a MarketDepth,
}

impl<'a> DipAdapter<'a> {
    pub fn new(config: DipConfig, depth: &'a MarketDepth) -> Result<Self> {
        Ok(Self {
            detector: DipDetector::new(config)?,
            depth,
        })
    }
}

impl StrategyAdapter for DipAdapter<'_> {
    fn name(&self) -> &str {
        "dip"
    }

    fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>> {
        for event in view.new_events {
            self.detector.apply_event(event);
        }
        let mut occupied = occupied_keys(view.positions);
        let mut orders = Vec::new();
        for position in view
            .positions
            .iter()
            .filter(|p| !p.is_trade_locked_at(view.now))
        {
            let mut best = None;
            for quote in view.quotes_for(&position.item, position.float_bucket) {
                if let Some((order, profit)) =
                    profitable_exit(view, self.name(), position, quote, quote.price)?
                {
                    if best.as_ref().is_none_or(|(_, previous)| profit > *previous) {
                        best = Some((order, profit));
                    }
                }
            }
            if let Some((order, _)) = best {
                orders.push(order);
            }
        }
        for quote in view.batch {
            let depth = self
                .depth
                .verified_volume(quote, self.detector.config().liquidity_window_days);
            if let Some(signal) = self.detector.observe_with_liquidity(quote, depth)? {
                let schedule = view.fees.get(quote.platform);
                let (price, quantity) = view
                    .preview_fill(self.name(), &signal.order)
                    .unwrap_or((quote.price, signal.order.quantity));
                let cost = schedule.buyer_order_total(price, quantity)?;
                if view
                    .available_cash(self.name(), quote.platform)
                    .is_some_and(|cash| cash < cost)
                {
                    continue;
                }
                let mean = PriceSnapshot {
                    price: Money(signal.stats.mean.floor() as i64),
                    ..quote.clone()
                };
                if view.seller_proceeds(&mean, quantity)? <= cost {
                    continue;
                }
                if occupied.insert((quote.item.clone(), quote.float_bucket)) {
                    orders.push(Order {
                        quantity,
                        ..signal.order
                    });
                }
            }
        }
        Ok(orders)
    }
}

fn profitable_exit(
    view: &MarketView<'_>,
    strategy: &str,
    position: &Position,
    quote: &PriceSnapshot,
    price: Money,
) -> Result<Option<(Order, Money)>> {
    if !view.quote_is_fresh(quote) || price.0 <= 0 {
        return Ok(None);
    }
    let mut order = exit_order(position, quote, price, view.now);
    let Some((price, quantity)) = view.preview_fill(strategy, &order) else {
        return Ok(None);
    };
    let released =
        i128::from(position.cost_basis.0) * i128::from(quantity) / i128::from(position.quantity);
    let proceeds = view
        .fees
        .get(quote.platform)
        .seller_order_proceeds(price, quantity)?;
    let profit = proceeds.checked_sub(Money(released as i64))?;
    order.quantity = quantity;
    Ok((profit.0 > 0).then_some((order, profit)))
}

fn occupied_keys(positions: &[Position]) -> HashSet<PositionKey> {
    positions
        .iter()
        .map(|p| (p.item.clone(), p.float_bucket))
        .collect()
}

fn exit_order(
    position: &Position,
    quote: &PriceSnapshot,
    limit_price: Money,
    now: DateTime<Utc>,
) -> Order {
    debug_assert!(!position.is_trade_locked_at(now));
    Order {
        id: format!("exit-{}-{now}", position.id),
        side: OrderSide::Sell,
        platform: quote.platform,
        item: position.item.clone(),
        float_bucket: position.float_bucket,
        quantity: position.quantity,
        limit_price,
        submitted_at: now,
    }
}
