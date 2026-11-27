//! Deterministic historical replay of price snapshots and market events through
//! one or more strategies and a simulated fill model.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    Error, FeeSchedules, Fill, FloatBucket, ItemId, MarketEvent, Money, Order, OrderSide, Platform,
    Position, PriceSnapshot, Result, SimulationMode, TradeFees,
};

use crate::fill::{FillModel, FillOutcome, SnapshotFillModel};
use crate::report::{
    PerformanceReport, ReplayWindow, StrategyStats, TradeRecord, REPORT_DISCLAIMER,
};
use crate::strategies::{DipStrategy, SpreadStrategy};

/// Every supported platform, in a fixed order used for deterministic iteration.
pub const PLATFORMS: [Platform; 4] = Platform::ALL;

/// Identity of a replayed quote series.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuoteKey {
    pub platform: Platform,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
}

impl QuoteKey {
    pub fn of(snapshot: &PriceSnapshot) -> Self {
        Self {
            platform: snapshot.platform,
            item: snapshot.item.clone(),
            float_bucket: snapshot.float_bucket,
        }
    }
}

fn quotes_for_key<'a: 'b, 'b>(
    latest: &'a HashMap<QuoteKey, PriceSnapshot>,
    item: &'b ItemId,
    float_bucket: Option<FloatBucket>,
) -> impl Iterator<Item = &'a PriceSnapshot> + 'b {
    PLATFORMS.into_iter().filter_map(move |platform| {
        latest.get(&QuoteKey {
            platform,
            item: item.clone(),
            float_bucket,
        })
    })
}

fn quote_is_fresh(quote: &PriceSnapshot, now: DateTime<Utc>, max_age: Duration) -> bool {
    now - quote.observed_at <= max_age
}

fn best_fresh_exit<'a>(
    quotes: impl IntoIterator<Item = &'a PriceSnapshot>,
    now: DateTime<Utc>,
    max_age: Duration,
    mut seller_proceeds: impl FnMut(&PriceSnapshot) -> Result<Money>,
) -> Result<Option<(Platform, &'a PriceSnapshot, Money)>> {
    let mut best = None;
    for quote in quotes {
        if !quote_is_fresh(quote, now, max_age) {
            continue;
        }
        let net = seller_proceeds(quote)?;
        if best.as_ref().is_none_or(|(_, _, previous)| net > *previous) {
            best = Some((quote.platform, quote, net));
        }
    }
    Ok(best)
}

/// When an event became observable.
pub fn event_time(event: &MarketEvent) -> DateTime<Utc> {
    match event {
        MarketEvent::BranchUpdated { observed_at, .. } => *observed_at,
        MarketEvent::PanicWindowOpened { opened_at, .. } => *opened_at,
        MarketEvent::PanicWindowClosed { closed_at, .. } => *closed_at,
    }
}

/// Branches whose panic window is open after applying `events` in order.
pub fn open_panic_windows(events: &[MarketEvent]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    for event in events {
        match event {
            MarketEvent::PanicWindowOpened { branch, .. } => {
                if !open.contains(branch) {
                    open.push(branch.clone());
                }
            }
            MarketEvent::PanicWindowClosed { branch, .. } => open.retain(|b| b != branch),
            MarketEvent::BranchUpdated { .. } => {}
        }
    }
    open
}

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub fees: FeeSchedules,
    /// Orders only fill against quotes at most this old.
    pub max_quote_age: Duration,
}

impl BacktestConfig {
    pub fn new(fees: FeeSchedules) -> Self {
        Self {
            fees,
            max_quote_age: Duration::days(1),
        }
    }
}

/// Read-only view handed to a strategy at each replay step.
pub struct MarketView<'a> {
    pub now: DateTime<Utc>,
    /// Snapshots observed exactly at `now`.
    pub batch: &'a [PriceSnapshot],
    /// All snapshots observed at or before `now`, oldest first.
    pub history: &'a [PriceSnapshot],
    /// All events observed at or before `now`, oldest first.
    pub events: &'a [MarketEvent],
    /// Events first observed in this step.
    pub new_events: &'a [MarketEvent],
    /// This strategy's open positions, oldest first (the FIFO sell order).
    pub positions: &'a [Position],
    pub fees: &'a FeeSchedules,
    latest: &'a HashMap<QuoteKey, PriceSnapshot>,
    max_quote_age: Duration,
    fill_model: &'a dyn FillModel,
}

impl<'a> MarketView<'a> {
    pub fn latest_quotes(&self) -> impl Iterator<Item = &'a PriceSnapshot> + '_ {
        self.latest.values()
    }

    pub fn latest_quote(
        &self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
    ) -> Option<&'a PriceSnapshot> {
        self.latest.get(&QuoteKey {
            platform,
            item: item.clone(),
            float_bucket,
        })
    }

    /// Latest quote on each platform for the same item and float bucket.
    pub fn quotes_for(
        &self,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
    ) -> Vec<&'a PriceSnapshot> {
        quotes_for_key(self.latest, item, float_bucket).collect()
    }

    pub fn panic_window_active(&self) -> bool {
        !open_panic_windows(self.events).is_empty()
    }

    pub fn open_positions_for(
        &self,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
    ) -> impl Iterator<Item = &'a Position> + '_ {
        let item = item.clone();
        self.positions
            .iter()
            .filter(move |p| p.item == item && p.float_bucket == float_bucket)
    }

    /// Best `(platform, quote, net proceeds)` for selling `quantity` units now.
    pub fn best_exit(
        &self,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        quantity: u32,
    ) -> Result<Option<(Platform, &'a PriceSnapshot, Money)>> {
        best_fresh_exit(
            quotes_for_key(self.latest, item, float_bucket),
            self.now,
            self.max_quote_age,
            |quote| self.seller_proceeds(quote, quantity),
        )
    }

    pub(crate) fn seller_proceeds(&self, quote: &PriceSnapshot, quantity: u32) -> Result<Money> {
        self.fill_model
            .seller_proceeds(quote.price, quantity, self.fees.get(quote.platform))
    }

    pub(crate) fn preview_fill(&self, strategy: &str, order: &Order) -> Option<(Money, u32)> {
        self.fill_model.preview_fill(strategy, order)
    }

    pub(crate) fn quote_is_fresh(&self, quote: &PriceSnapshot) -> bool {
        quote_is_fresh(quote, self.now, self.max_quote_age)
    }
}

/// A strategy that can be driven by the replay engine. Integration can wrap
/// `cs2-arb-strategy` in this trait.
pub trait StrategyAdapter {
    /// Unique name, used for the per-strategy breakdown.
    fn name(&self) -> &str;

    /// Decide on simulated orders for this step. Buys fill on `order.platform`
    /// against the latest quote; sells consume this strategy's unlocked
    /// positions for the same item and float bucket in FIFO order.
    fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>>;

    fn on_fill(&mut self, _fill: &Fill) {}

    fn on_reject(&mut self, _order: &Order, _reason: &str) {}
}

#[derive(Debug, Clone)]
struct Lot {
    position: Position,
    /// Remaining purchase subtotal (excluding fees).
    entry_subtotal: Money,
    /// Remaining entry fees.
    entry_fee: Money,
    entry_deposit: Money,
}

#[derive(Default)]
struct Book {
    lots: Vec<Lot>,
    next_position: u64,
    trades: Vec<TradeRecord>,
    submitted: u32,
    filled: u32,
    rejected: u32,
    equity: Vec<Money>,
}

/// `amount * part / whole` with i128 intermediates; `part <= whole`.
fn prorate(amount: Money, part: u32, whole: u32) -> Money {
    if part == whole {
        return amount;
    }
    Money((i128::from(amount.0) * i128::from(part) / i128::from(whole)) as i64)
}

pub struct Backtester<F: FillModel = SnapshotFillModel> {
    config: BacktestConfig,
    fill_model: F,
}

impl Backtester<SnapshotFillModel> {
    pub fn new(config: BacktestConfig) -> Self {
        Self::with_fill_model(config, SnapshotFillModel::default())
    }
}

impl<F: FillModel> Backtester<F> {
    pub fn with_fill_model(config: BacktestConfig, fill_model: F) -> Self {
        Self { config, fill_model }
    }

    /// Replay `snapshots` and `events` in time order. Inputs need not be sorted;
    /// ties keep input order. Events are delivered at the first step whose time is
    /// at or after the event time.
    pub fn run(
        &self,
        _mode: SimulationMode,
        snapshots: &[PriceSnapshot],
        events: &[MarketEvent],
        strategies: &mut [Box<dyn StrategyAdapter + '_>],
    ) -> Result<PerformanceReport> {
        let mut names = HashSet::new();
        for strategy in strategies.iter() {
            if !names.insert(strategy.name().to_owned()) {
                return Err(Error::InvalidConfig(format!(
                    "duplicate strategy name {}",
                    strategy.name()
                )));
            }
        }
        if let Some(bad) = snapshots.iter().find(|s| s.price.0 < 0) {
            return Err(Error::InvalidData(format!(
                "negative price for {} at {}",
                bad.item.market_hash_name, bad.observed_at
            )));
        }

        let mut snapshots = snapshots.to_vec();
        snapshots.sort_by_key(|s| s.observed_at);
        let mut events = events.to_vec();
        events.sort_by_key(event_time);

        let mut books: Vec<Book> = strategies.iter().map(|_| Book::default()).collect();
        let mut latest: HashMap<QuoteKey, PriceSnapshot> = HashMap::new();
        let mut steps = 0u32;
        let mut delivered = 0usize;
        let mut start = 0usize;
        self.fill_model.prepare_run();
        let mut times: BTreeSet<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.observed_at)
            .collect();
        self.fill_model.add_quote_update_times(&mut times);
        let first = times.first().copied();
        let last = times.last().copied();

        for now in times {
            let end = start
                + snapshots[start..]
                    .iter()
                    .take_while(|s| s.observed_at == now)
                    .count();
            self.fill_model.invalidate_quotes(now, &mut latest);
            for snapshot in &snapshots[start..end] {
                latest.insert(QuoteKey::of(snapshot), snapshot.clone());
            }
            let first_new_event = delivered;
            while delivered < events.len() && event_time(&events[delivered]) <= now {
                delivered += 1;
            }

            for (strategy, book) in strategies.iter_mut().zip(books.iter_mut()) {
                let positions: Vec<Position> =
                    book.lots.iter().map(|l| l.position.clone()).collect();
                let view = MarketView {
                    now,
                    batch: &snapshots[start..end],
                    history: &snapshots[..end],
                    events: &events[..delivered],
                    new_events: &events[first_new_event..delivered],
                    positions: &positions,
                    fees: &self.config.fees,
                    latest: &latest,
                    max_quote_age: self.config.max_quote_age,
                    fill_model: &self.fill_model,
                };
                self.fill_model.prepare_step(strategy.name(), &view)?;
                let orders = strategy.on_step(&view)?;
                for order in orders {
                    book.submitted += 1;
                    match self.execute(strategy.name(), book, &order, &latest, now)? {
                        Ok(fill) => {
                            self.fill_model.after_fill(strategy.name(), &view, &fill)?;
                            book.filled += 1;
                            tracing::debug!(
                                strategy = strategy.name(),
                                order = %order.id,
                                side = ?order.side,
                                price = fill.unit_price.0,
                                "simulated fill"
                            );
                            strategy.on_fill(&fill);
                        }
                        Err(reason) => {
                            book.rejected += 1;
                            tracing::debug!(
                                strategy = strategy.name(),
                                order = %order.id,
                                %reason,
                                "simulated order rejected"
                            );
                            strategy.on_reject(&order, &reason);
                        }
                    }
                }
            }
            for book in &mut books {
                let equity = self.equity(book, &latest, now)?;
                book.equity.push(equity);
            }
            steps += 1;
            start = end;
        }

        let window = ReplayWindow {
            start: first,
            end: last,
            steps,
            snapshots: snapshots.len() as u32,
            events_delivered: delivered as u32,
            events_total: events.len() as u32,
        };
        self.build_report(window, strategies, &books, &latest)
    }

    /// Outer `Result` is a hard error; inner `Err` is a rejection reason.
    fn execute(
        &self,
        strategy: &str,
        book: &mut Book,
        order: &Order,
        latest: &HashMap<QuoteKey, PriceSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<std::result::Result<Fill, String>> {
        let key = QuoteKey {
            platform: order.platform,
            item: order.item.clone(),
            float_bucket: order.float_bucket,
        };
        let Some(quote) = latest.get(&key) else {
            return Ok(Err("no quote for platform, item and float bucket".into()));
        };
        if !quote_is_fresh(quote, now, self.config.max_quote_age) {
            return Ok(Err("quote is stale".into()));
        }
        if order.side == OrderSide::Sell {
            let unlocked: u64 = book
                .lots
                .iter()
                .filter(|l| {
                    l.position.item == order.item
                        && l.position.float_bucket == order.float_bucket
                        && !l.position.is_trade_locked_at(now)
                })
                .map(|l| u64::from(l.position.quantity))
                .sum();
            if unlocked < u64::from(order.quantity) {
                return Ok(Err(format!(
                    "insufficient unlocked inventory: have {unlocked}, need {}",
                    order.quantity
                )));
            }
        }
        let fees = self.config.fees.get(order.platform);
        let fill = match self
            .fill_model
            .fill_for(strategy, order, quote, fees, now)?
        {
            FillOutcome::Filled(fill) => fill,
            FillOutcome::Rejected(reason) => return Ok(Err(reason)),
        };
        if fill.quantity == 0 || fill.quantity > order.quantity {
            return Err(Error::InvalidData(format!(
                "fill model returned quantity {} for order of {}",
                fill.quantity, order.quantity
            )));
        }
        let subtotal = fill.unit_price.checked_mul(fill.quantity)?;
        match order.side {
            OrderSide::Buy => {
                book.next_position += 1;
                let position = Position {
                    id: format!("{strategy}-{}", book.next_position),
                    item: order.item.clone(),
                    float_bucket: order.float_bucket,
                    acquired_on: order.platform,
                    quantity: fill.quantity,
                    cost_basis: subtotal.checked_add(fill.fee)?,
                    acquired_at: fill.filled_at,
                    locked_until: self.fill_model.locked_until(&fill),
                };
                book.lots.push(Lot {
                    position,
                    entry_subtotal: subtotal,
                    entry_fee: fill.fee,
                    entry_deposit: fees
                        .funding
                        .crypto_deposit
                        .order_fee(fill.unit_price, fill.quantity)?,
                });
            }
            OrderSide::Sell => {
                self.close_lots(strategy, book, &fill, subtotal)?;
            }
        }
        Ok(Ok(fill))
    }

    fn close_lots(
        &self,
        strategy: &str,
        book: &mut Book,
        fill: &Fill,
        exit_subtotal: Money,
    ) -> Result<()> {
        let order = &fill.order;
        let now = fill.filled_at;
        let mut remaining = fill.quantity;
        let mut exit_left = exit_subtotal;
        let funding = self.config.fees.get(order.platform).funding;
        let mut withdrawal_left = funding
            .crypto_withdrawal
            .order_fee(fill.unit_price, fill.quantity)?;
        let mut bank_left = funding
            .bank_cashout
            .order_fee(fill.unit_price, fill.quantity)?;
        let mut platform_left = fill
            .fee
            .checked_sub(withdrawal_left)?
            .checked_sub(bank_left)?;
        let mut units_left = fill.quantity;
        let mut index = 0;
        while remaining > 0 {
            let lot = &mut book.lots[index];
            if lot.position.item != order.item
                || lot.position.float_bucket != order.float_bucket
                || lot.position.is_trade_locked_at(now)
            {
                index += 1;
                continue;
            }
            let take = remaining.min(lot.position.quantity);
            let entry_subtotal = prorate(lot.entry_subtotal, take, lot.position.quantity);
            let entry_fee = prorate(lot.entry_fee, take, lot.position.quantity);
            let entry_deposit = prorate(lot.entry_deposit, take, lot.position.quantity);
            let exit_part = prorate(exit_left, take, units_left);
            let withdrawal = prorate(withdrawal_left, take, units_left);
            let bank = prorate(bank_left, take, units_left);
            let platform = prorate(platform_left, take, units_left);
            let exit_fee = withdrawal.checked_add(bank)?.checked_add(platform)?;
            exit_left = exit_left.checked_sub(exit_part)?;
            withdrawal_left = withdrawal_left.checked_sub(withdrawal)?;
            bank_left = bank_left.checked_sub(bank)?;
            platform_left = platform_left.checked_sub(platform)?;
            units_left -= take;
            remaining -= take;

            let gross = exit_part.checked_sub(entry_subtotal)?;
            let fees = entry_fee.checked_add(exit_fee)?;
            book.trades.push(TradeRecord {
                strategy: strategy.to_owned(),
                position_id: lot.position.id.clone(),
                item: lot.position.item.clone(),
                float_bucket: lot.position.float_bucket,
                quantity: take,
                buy_platform: lot.position.acquired_on,
                sell_platform: order.platform,
                entry_subtotal,
                entry_fee,
                exit_subtotal: exit_part,
                exit_fee,
                gross_pnl: gross,
                fees,
                fee_breakdown: (funding.enabled()
                    || self
                        .config
                        .fees
                        .get(lot.position.acquired_on)
                        .funding
                        .enabled())
                .then_some(TradeFees {
                    platform_buy: entry_fee.checked_sub(entry_deposit)?,
                    platform_sell: platform,
                    crypto_deposit: entry_deposit,
                    crypto_withdrawal: withdrawal,
                    bank_cashout: bank,
                }),
                net_pnl: gross.checked_sub(fees)?,
                opened_at: lot.position.acquired_at,
                locked_until: lot.position.locked_until,
                closed_at: now,
                duration_secs: (now - lot.position.acquired_at).num_seconds(),
            });

            lot.position.quantity -= take;
            lot.entry_subtotal = lot.entry_subtotal.checked_sub(entry_subtotal)?;
            lot.entry_fee = lot.entry_fee.checked_sub(entry_fee)?;
            lot.entry_deposit = lot.entry_deposit.checked_sub(entry_deposit)?;
            lot.position.cost_basis = lot.entry_subtotal.checked_add(lot.entry_fee)?;
            if lot.position.quantity == 0 {
                book.lots.remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    /// Mark value (best fresh net-of-fee exit), or cost basis without a fresh quote.
    fn mark(
        &self,
        lot: &Lot,
        latest: &HashMap<QuoteKey, PriceSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<Money> {
        let pos = &lot.position;
        let best = best_fresh_exit(
            quotes_for_key(latest, &pos.item, pos.float_bucket),
            now,
            self.config.max_quote_age,
            |quote| {
                self.fill_model.seller_proceeds(
                    quote.price,
                    pos.quantity,
                    self.config.fees.get(quote.platform),
                )
            },
        )?;
        Ok(best.map_or(pos.cost_basis, |(_, _, net)| net))
    }

    fn unrealized(
        &self,
        book: &Book,
        latest: &HashMap<QuoteKey, PriceSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<Money> {
        book.lots.iter().try_fold(Money(0), |acc, lot| {
            acc.checked_add(
                self.mark(lot, latest, now)?
                    .checked_sub(lot.position.cost_basis)?,
            )
        })
    }

    fn equity(
        &self,
        book: &Book,
        latest: &HashMap<QuoteKey, PriceSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<Money> {
        let realized = book
            .trades
            .iter()
            .try_fold(Money(0), |acc, t| acc.checked_add(t.net_pnl))?;
        realized.checked_add(self.unrealized(book, latest, now)?)
    }

    fn build_report(
        &self,
        window: ReplayWindow,
        strategies: &[Box<dyn StrategyAdapter + '_>],
        books: &[Book],
        latest: &HashMap<QuoteKey, PriceSnapshot>,
    ) -> Result<PerformanceReport> {
        let mut per_strategy = Vec::with_capacity(books.len());
        let mut all_trades: Vec<TradeRecord> = Vec::new();
        let mut combined_equity = vec![Money(0); window.steps as usize];
        let mut total_unrealized = Money(0);
        let mut total_open_basis = Money(0);
        let mut total_open = 0u32;
        let (mut submitted, mut filled, mut rejected) = (0u32, 0u32, 0u32);

        for (strategy, book) in strategies.iter().zip(books) {
            let unrealized = match window.end {
                Some(now) => self.unrealized(book, latest, now)?,
                None => Money(0),
            };
            let open_basis = book
                .lots
                .iter()
                .try_fold(Money(0), |acc, l| acc.checked_add(l.position.cost_basis))?;
            let open = book.lots.len() as u32;
            per_strategy.push(stats(
                strategy.name(),
                &book.trades,
                &book.equity,
                (book.submitted, book.filled, book.rejected),
                (open, open_basis, unrealized),
            )?);
            for (sum, point) in combined_equity.iter_mut().zip(&book.equity) {
                *sum = sum.checked_add(*point)?;
            }
            total_unrealized = total_unrealized.checked_add(unrealized)?;
            total_open_basis = total_open_basis.checked_add(open_basis)?;
            total_open += open;
            submitted += book.submitted;
            filled += book.filled;
            rejected += book.rejected;
            all_trades.extend(book.trades.iter().cloned());
        }
        // Stable sort keeps strategy order for trades closed at the same instant.
        all_trades.sort_by_key(|t| t.closed_at);
        let totals = stats(
            "all",
            &all_trades,
            &combined_equity,
            (submitted, filled, rejected),
            (total_open, total_open_basis, total_unrealized),
        )?;
        Ok(PerformanceReport {
            disclaimer: REPORT_DISCLAIMER.to_owned(),
            window,
            trade_hold_secs: hold_secs(&self.fill_model),
            totals,
            strategies: per_strategy,
            trades: all_trades,
        })
    }
}

/// Trade hold implied by the fill model, measured on a probe fill.
fn hold_secs<F: FillModel>(model: &F) -> i64 {
    let at = DateTime::<Utc>::UNIX_EPOCH;
    let probe = Fill {
        order: Order {
            id: "probe".into(),
            side: OrderSide::Buy,
            platform: Platform::CsFloat,
            item: ItemId {
                market_hash_name: String::new(),
                wear_tier: cs2_arb_core::WearTier::FactoryNew,
            },
            float_bucket: None,
            quantity: 1,
            limit_price: Money(0),
            submitted_at: at,
        },
        quantity: 1,
        unit_price: Money(0),
        fee: Money(0),
        filled_at: at,
    };
    (model.locked_until(&probe) - at).num_seconds()
}

/// Peak-to-trough decline of an equity curve that starts at zero.
pub fn max_drawdown(equity: &[Money]) -> Money {
    let mut peak = 0i64;
    let mut worst = 0i64;
    for point in equity {
        peak = peak.max(point.0);
        worst = worst.max(peak.saturating_sub(point.0));
    }
    Money(worst)
}

/// Median of integer samples; mean of the middle pair for even lengths.
pub fn median(values: &[i64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid] as f64
    } else {
        (sorted[mid - 1] as f64 + sorted[mid] as f64) / 2.0
    })
}

fn stats(
    name: &str,
    trades: &[TradeRecord],
    equity: &[Money],
    (submitted, filled, rejected): (u32, u32, u32),
    (open_positions, open_cost_basis, unrealized_pnl): (u32, Money, Money),
) -> Result<StrategyStats> {
    let sum = |f: fn(&TradeRecord) -> Money| {
        trades
            .iter()
            .try_fold(Money(0), |acc, t| acc.checked_add(f(t)))
    };
    let gross_pnl = sum(|t| t.gross_pnl)?;
    let buy_fees = sum(|t| t.entry_fee)?;
    let sell_fees = sum(|t| t.exit_fee)?;
    let total_fees = buy_fees.checked_add(sell_fees)?;
    let net_pnl = gross_pnl.checked_sub(total_fees)?;
    let entry = sum(|t| t.entry_subtotal)?;
    let exit = sum(|t| t.exit_subtotal)?;
    let closed_turnover = entry.checked_add(exit)?;
    let durations: Vec<i64> = trades.iter().map(|t| t.duration_secs).collect();
    let count = trades.len() as u32;
    let winning_trades = trades.iter().filter(|t| t.net_pnl.0 > 0).count() as u32;
    let losing_trades = trades.iter().filter(|t| t.net_pnl.0 < 0).count() as u32;
    Ok(StrategyStats {
        strategy: name.to_owned(),
        orders_submitted: submitted,
        orders_filled: filled,
        orders_rejected: rejected,
        trade_count: count,
        winning_trades,
        losing_trades,
        win_rate: (count > 0).then(|| f64::from(winning_trades) / f64::from(count)),
        gross_pnl,
        buy_fees,
        sell_fees,
        total_fees,
        net_pnl,
        fee_drag_pct_of_gross: (gross_pnl.0 > 0)
            .then(|| total_fees.0 as f64 / gross_pnl.0 as f64 * 100.0),
        fee_pct_of_turnover: (closed_turnover.0 > 0)
            .then(|| total_fees.0 as f64 / closed_turnover.0 as f64 * 100.0),
        closed_turnover,
        open_positions,
        open_cost_basis,
        unrealized_pnl,
        total_pnl: net_pnl.checked_add(unrealized_pnl)?,
        avg_trade_duration_secs: (count > 0)
            .then(|| durations.iter().map(|d| *d as f64).sum::<f64>() / f64::from(count)),
        median_trade_duration_secs: median(&durations),
        min_trade_duration_secs: durations.iter().copied().min(),
        max_trade_duration_secs: durations.iter().copied().max(),
        max_drawdown: max_drawdown(equity),
        final_equity: equity.last().copied().unwrap_or(Money(0)),
    })
}

/// The built-in strategy set used by [`replay`]: spread then dip, default parameters.
pub fn default_strategies() -> Vec<Box<dyn StrategyAdapter>> {
    vec![
        Box::new(SpreadStrategy::default()),
        Box::new(DipStrategy::default()),
    ]
}

/// Replay with the built-in fill model and the built-in spread and dip strategies.
pub fn replay(
    mode: SimulationMode,
    snapshots: &[PriceSnapshot],
    events: &[MarketEvent],
    fees: FeeSchedules,
) -> Result<PerformanceReport> {
    let mut strategies = default_strategies();
    Backtester::new(BacktestConfig::new(fees)).run(mode, snapshots, events, &mut strategies)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{FeeSchedule, WearTier};

    use super::*;

    fn t(day: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap() + Duration::days(day)
    }

    fn item() -> ItemId {
        ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    const BUCKET: Option<FloatBucket> = Some(FloatBucket::FieldTestedLow);

    fn snap(platform: Platform, day: i64, price: i64) -> PriceSnapshot {
        PriceSnapshot {
            platform,
            item: item(),
            float_bucket: BUCKET,
            price: Money(price),
            observed_at: t(day),
        }
    }

    fn fees() -> FeeSchedules {
        let schedule = |seller| FeeSchedule {
            buyer_fee_pct: 1.0,
            seller_fee_pct: seller,
            buyer_fixed_fee: Money(0),
            seller_fixed_fee: Money(0),
            ..FeeSchedule::default()
        };
        FeeSchedules {
            skinport: schedule(8.0),
            csfloat: schedule(2.0),
            dmarket: schedule(5.0),
            waxpeer: schedule(6.0),
        }
    }

    fn order(side: OrderSide, platform: Platform, qty: u32, limit: i64, day: i64) -> Order {
        Order {
            id: format!("{side:?}-{day}"),
            side,
            platform,
            item: item(),
            float_bucket: BUCKET,
            quantity: qty,
            limit_price: Money(limit),
            submitted_at: t(day),
        }
    }

    #[derive(Default)]
    struct Log {
        fills: Vec<Fill>,
        rejects: Vec<String>,
        saw_panic: Vec<(i64, bool, usize)>,
    }

    /// Emits a fixed list of orders on given days and records callbacks.
    #[derive(Default)]
    struct Scripted {
        name: String,
        script: Vec<(i64, Order)>,
        log: std::rc::Rc<std::cell::RefCell<Log>>,
    }

    impl StrategyAdapter for Scripted {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>> {
            let day = (view.now - t(0)).num_days();
            self.log.borrow_mut().saw_panic.push((
                day,
                view.panic_window_active(),
                view.new_events.len(),
            ));
            Ok(self
                .script
                .iter()
                .filter(|(d, _)| *d == day)
                .map(|(_, o)| o.clone())
                .collect())
        }
        fn on_fill(&mut self, fill: &Fill) {
            self.log.borrow_mut().fills.push(fill.clone());
        }
        fn on_reject(&mut self, _order: &Order, reason: &str) {
            self.log.borrow_mut().rejects.push(reason.to_owned());
        }
    }

    fn daily(days: i64, price: impl Fn(Platform, i64) -> i64) -> Vec<PriceSnapshot> {
        (0..days)
            .flat_map(|d| {
                [Platform::Skinport, Platform::CsFloat, Platform::DMarket]
                    .map(|p| snap(p, d, price(p, d)))
            })
            .collect()
    }

    fn run_scripted(
        snapshots: &[PriceSnapshot],
        events: &[MarketEvent],
        script: Vec<(i64, Order)>,
    ) -> (PerformanceReport, Log) {
        let log = std::rc::Rc::default();
        let mut strategies: Vec<Box<dyn StrategyAdapter>> = vec![Box::new(Scripted {
            name: "s".into(),
            script,
            log: std::rc::Rc::clone(&log),
        })];
        let report = Backtester::new(BacktestConfig::new(fees()))
            .run(SimulationMode, snapshots, events, &mut strategies)
            .unwrap();
        drop(strategies);
        let log = std::rc::Rc::try_unwrap(log).ok().unwrap().into_inner();
        (report, log)
    }

    #[test]
    fn enforces_seven_day_hold_and_applies_fees() {
        let snapshots = daily(10, |p, _| if p == Platform::DMarket { 1000 } else { 1200 });
        let script = vec![
            (0, order(OrderSide::Buy, Platform::DMarket, 1, 1000, 0)),
            (6, order(OrderSide::Sell, Platform::CsFloat, 1, 0, 6)),
            (7, order(OrderSide::Sell, Platform::CsFloat, 1, 0, 7)),
        ];
        let (report, s) = run_scripted(&snapshots, &[], script);
        assert_eq!(s.fills.len(), 2);
        assert_eq!(s.rejects.len(), 1);
        assert!(s.rejects[0].contains("insufficient unlocked inventory"));
        let trade = &report.trades[0];
        assert_eq!(trade.entry_fee, Money(10)); // 1% buyer fee on DMarket
        assert_eq!(trade.exit_fee, Money(24)); // 2% seller fee on CSFloat
        assert_eq!(trade.gross_pnl, Money(200));
        assert_eq!(trade.net_pnl, Money(166));
        assert_eq!(trade.duration_secs, Duration::days(7).num_seconds());
        let totals = &report.totals;
        assert_eq!(totals.trade_count, 1);
        assert_eq!(totals.fee_drag_pct_of_gross, Some(17.0));
        assert_eq!(totals.orders_rejected, 1);
        assert_eq!(totals.win_rate, Some(1.0));
        // Nothing is left open, so final equity is the realized net.
        assert_eq!(report.window.steps, 10);
        assert_eq!(totals.final_equity, Money(166));
    }

    #[test]
    fn fifo_partial_sells_prorate_cost_and_fees() {
        let snapshots = daily(20, |_, d| if d < 2 { 1000 } else { 900 + d });
        let script = vec![
            (0, order(OrderSide::Buy, Platform::CsFloat, 3, 1000, 0)),
            (1, order(OrderSide::Buy, Platform::CsFloat, 2, 1000, 1)),
            (8, order(OrderSide::Sell, Platform::CsFloat, 4, 0, 8)),
            (9, order(OrderSide::Sell, Platform::CsFloat, 1, 0, 9)),
        ];
        let (report, s) = run_scripted(&snapshots, &[], script);
        assert!(s.rejects.is_empty(), "{:?}", s.rejects);
        let trades = &report.trades;
        assert_eq!(
            trades.iter().map(|t| t.quantity).collect::<Vec<_>>(),
            [3, 1, 1]
        );
        assert_eq!(trades[0].position_id, "s-1");
        assert_eq!(trades[1].position_id, "s-2");
        assert_eq!(trades[2].position_id, "s-2");
        // Sell of 4 @ 908 = 3632, 2% fee = 18 each, split 3:1.
        assert_eq!(trades[0].exit_subtotal.0 + trades[1].exit_subtotal.0, 3632);
        assert_eq!(trades[0].exit_fee.0 + trades[1].exit_fee.0, 72);
        assert_eq!(trades[1].entry_subtotal, Money(1000));
        assert_eq!(trades[1].entry_fee, Money(10));
        assert_eq!(trades[2].entry_fee, Money(10));
        assert_eq!(report.totals.open_positions, 0);
        assert!(report.totals.net_pnl.0 < 0);
        assert_eq!(report.totals.losing_trades, 3);
        assert_eq!(report.totals.win_rate, Some(0.0));
        assert_eq!(report.totals.fee_drag_pct_of_gross, None);
        assert_eq!(report.totals.final_equity, report.totals.net_pnl);
        assert!(report.totals.max_drawdown.0 >= -report.totals.net_pnl.0);
    }

    fn depth_with_snapshots(rows: &[(PriceSnapshot, u32, u32)]) -> crate::integrated::MarketDepth {
        use cs2_arb_core::{Listing, Sale};
        let mut depth = crate::integrated::MarketDepth::default();
        for (index, (quote, stock, sales)) in rows.iter().enumerate() {
            depth.replace_listings(
                quote.observed_at,
                quote.platform,
                quote.item.clone(),
                vec![Listing {
                    id: format!("{index}"),
                    platform: quote.platform,
                    item: quote.item.clone(),
                    price: quote.price,
                    quantity: *stock,
                    float_value: Some(0.18),
                    observed_at: quote.observed_at,
                    trade_locked_until: None,
                }],
            );
            depth.add_sales((0..*sales).map(|n| Sale {
                id: format!("{index}-{n}"),
                platform: quote.platform,
                item: quote.item.clone(),
                price: quote.price,
                float_value: Some(0.18),
                sold_at: quote.observed_at - Duration::seconds(1),
                verified: true,
            }));
        }
        depth
    }

    #[test]
    fn dip_only_buys_when_recovery_to_the_mean_covers_fees() {
        use crate::integrated::{DipAdapter, SimFillModel};
        use cs2_arb_strategy::{DipConfig, KillSwitch, RiskLimits};

        let mut fees = fees();
        fees.csfloat = FeeSchedule {
            seller_fixed_fee: Money(75),
            ..FeeSchedule::default()
        };
        // Two at 950 would be profitable at the 1000 mean, but only one can fill:
        // one-unit recovery proceeds after the fixed charge are 925.
        for (dip_price, expected_fills) in [(950, 0), (900, 2)] {
            let snapshots = [(0, 999), (1, 1001), (2, dip_price), (9, 1000)]
                .map(|(day, price)| snap(Platform::CsFloat, day, price));
            // Restrict day 2 to one unit at the best ask even though verified sales allow more.
            let mut rows = snapshots.clone().map(|quote| (quote, 2, 70));
            rows[2].1 = 1;
            let depth = depth_with_snapshots(&rows);
            let mut strategies: Vec<Box<dyn StrategyAdapter>> = vec![Box::new(
                DipAdapter::new(
                    DipConfig {
                        min_samples: 2,
                        min_sales_in_window: 1,
                        quantity: 2,
                        ..DipConfig::default()
                    },
                    &depth,
                )
                .unwrap(),
            )];
            let events = [MarketEvent::PanicWindowOpened {
                branch: "test".into(),
                opened_at: t(2),
            }];
            let model = SimFillModel::new(
                &depth,
                fees.clone(),
                RiskLimits::default(),
                KillSwitch::new(),
            );
            let report = Backtester::with_fill_model(BacktestConfig::new(fees.clone()), model)
                .run(SimulationMode, &snapshots, &events, &mut strategies)
                .unwrap();
            assert_eq!(report.totals.orders_filled, expected_fills);
            assert_eq!(report.totals.open_positions, 0);
            if expected_fills > 0 {
                assert_eq!(report.trades[0].quantity, 1);
                assert_eq!(report.trades[0].net_pnl, Money(25));
            }
        }
    }

    #[test]
    fn spread_partial_exit_rejects_a_loss_after_the_liquidity_cap() {
        use crate::integrated::{SimFillModel, SpreadAdapter};
        use cs2_arb_strategy::{KillSwitch, RiskLimits, SpreadConfig};

        let mut fees = fees();
        fees.csfloat = FeeSchedule {
            buyer_fixed_fee: Money(50),
            ..FeeSchedule::default()
        };
        fees.dmarket = FeeSchedule {
            seller_fixed_fee: Money(80),
            ..FeeSchedule::default()
        };
        let rows = [
            (snap(Platform::CsFloat, 0, 1000), 2, 140),
            (snap(Platform::DMarket, 0, 1300), 2, 140),
            (snap(Platform::DMarket, 7, 1080), 2, 70),
            (snap(Platform::DMarket, 8, 1200), 2, 0),
        ];
        let snapshots = rows.clone().map(|(quote, _, _)| quote);
        let depth = depth_with_snapshots(&rows);
        let model = SimFillModel::new(
            &depth,
            fees.clone(),
            RiskLimits::default(),
            KillSwitch::new(),
        );
        let mut strategies: Vec<Box<dyn StrategyAdapter>> =
            vec![Box::new(SpreadAdapter::new(SpreadConfig {
                quantity: 2,
                ..SpreadConfig::default()
            }))];
        let report = Backtester::with_fill_model(BacktestConfig::new(fees), model)
            .run(SimulationMode, &snapshots, &[], &mut strategies)
            .unwrap();
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].closed_at, t(8));
        assert_eq!(report.trades[0].quantity, 1);
        assert_eq!(report.trades[0].net_pnl, Money(95));
    }

    #[test]
    fn multi_unit_spread_has_the_same_fees_and_profit_in_both_replay_models() {
        use crate::integrated::{SimFillModel, SpreadAdapter};
        use cs2_arb_strategy::{KillSwitch, RiskLimits, SpreadConfig};

        let mut fees = fees();
        fees.csfloat = FeeSchedule {
            buyer_fixed_fee: Money(100),
            ..FeeSchedule::default()
        };
        fees.dmarket = FeeSchedule::default();
        fees.dmarket.funding.crypto_withdrawal.fixed = Money(100);
        let rows = [
            (snap(Platform::CsFloat, 0, 1000), 2, 140),
            (snap(Platform::DMarket, 0, 1130), 2, 140),
            (snap(Platform::DMarket, 7, 1130), 2, 140),
        ];
        let snapshots = rows.clone().map(|(quote, _, _)| quote);
        let depth = depth_with_snapshots(&rows);
        let strategies = || -> Vec<Box<dyn StrategyAdapter>> {
            vec![Box::new(SpreadAdapter::new(SpreadConfig {
                annual_opportunity_rate_pct: 0.0,
                quantity: 2,
                ..SpreadConfig::default()
            }))]
        };
        let snapshot = Backtester::new(BacktestConfig::new(fees.clone()))
            .run(SimulationMode, &snapshots, &[], &mut strategies())
            .unwrap();
        let cli = Backtester::with_fill_model(
            BacktestConfig::new(fees.clone()),
            SimFillModel::new(&depth, fees, RiskLimits::default(), KillSwitch::new()),
        )
        .run(SimulationMode, &snapshots, &[], &mut strategies())
        .unwrap();
        for report in [snapshot, cli] {
            assert_eq!(report.trades.len(), 1);
            let trade = &report.trades[0];
            assert_eq!(trade.quantity, 2);
            assert_eq!(trade.entry_fee, Money(100));
            assert_eq!(trade.exit_fee, Money(100));
            assert_eq!(trade.net_pnl, Money(60));
        }
    }

    #[test]
    fn best_exit_prefers_fresh_quote_to_expired_higher_price() {
        let stale = snap(Platform::DMarket, 0, 2000);
        let fresh = snap(Platform::Skinport, 2, 1000);
        let latest = [stale, fresh.clone()]
            .into_iter()
            .map(|q| (QuoteKey::of(&q), q))
            .collect();
        let fees = fees();
        let view = MarketView {
            now: fresh.observed_at,
            batch: std::slice::from_ref(&fresh),
            history: &[],
            events: &[],
            new_events: &[],
            positions: &[],
            fees: &fees,
            latest: &latest,
            max_quote_age: Duration::days(1),
            fill_model: &SnapshotFillModel::default(),
        };
        let (platform, quote, _) = view
            .best_exit(&fresh.item, fresh.float_bucket, 1)
            .unwrap()
            .unwrap();
        assert_eq!(platform, fresh.platform);
        assert_eq!(quote, &fresh);
    }

    #[test]
    fn simulator_marks_multi_unit_lots_and_ranks_exits_with_per_order_fees() {
        use crate::integrated::{MarketDepth, SimFillModel};
        use cs2_arb_strategy::{KillSwitch, RiskLimits};

        let mut fees = fees();
        fees.skinport = cs2_arb_core::FeeSchedule {
            seller_fee_pct: 0.0,
            seller_fixed_fee: Money(50),
            ..fees.skinport
        };
        fees.csfloat = cs2_arb_core::FeeSchedule {
            seller_fee_pct: 0.0,
            seller_fixed_fee: Money(0),
            ..fees.csfloat
        };
        let skinport = snap(Platform::Skinport, 0, 1040);
        let csfloat = snap(Platform::CsFloat, 0, 1000);
        let latest = [skinport, csfloat.clone()]
            .into_iter()
            .map(|q| (QuoteKey::of(&q), q))
            .collect();
        let depth = MarketDepth::default();
        let model = SimFillModel::new(
            &depth,
            fees.clone(),
            RiskLimits::default(),
            KillSwitch::new(),
        );
        let view = MarketView {
            now: csfloat.observed_at,
            batch: &[],
            history: &[],
            events: &[],
            new_events: &[],
            positions: &[],
            fees: &fees,
            latest: &latest,
            max_quote_age: Duration::days(1),
            fill_model: &model,
        };
        let (venue, _, proceeds) = view
            .best_exit(&csfloat.item, csfloat.float_bucket, 2)
            .unwrap()
            .unwrap();
        assert_eq!((venue, proceeds), (Platform::Skinport, Money(2030)));
        let lot = Lot {
            position: Position {
                id: "lot".into(),
                item: csfloat.item.clone(),
                float_bucket: csfloat.float_bucket,
                acquired_on: Platform::CsFloat,
                quantity: 2,
                cost_basis: Money(1800),
                acquired_at: t(0),
                locked_until: t(7),
            },
            entry_subtotal: Money(1800),
            entry_fee: Money(0),
            entry_deposit: Money(0),
        };
        let replay = Backtester::with_fill_model(BacktestConfig::new(fees.clone()), model);
        assert_eq!(replay.mark(&lot, &latest, t(0)).unwrap(), Money(2030));
    }

    #[test]
    fn losing_sale_does_not_halt_the_next_same_step_buy_by_counting_twice() {
        use crate::integrated::{MarketDepth, SimFillModel};
        use cs2_arb_strategy::{KillSwitch, RiskLimits};

        let venue = Platform::CsFloat;
        let snapshots = [snap(venue, 0, 1000), snap(venue, 7, 600)];
        let mut depth = MarketDepth::from_snapshots(&snapshots);
        for snapshot in &snapshots {
            depth.add_sales((0..70).map(|n| cs2_arb_core::Sale {
                id: format!("{}-{n}", snapshot.observed_at),
                item: snapshot.item.clone(),
                platform: venue,
                price: snapshot.price,
                float_value: Some(0.20),
                sold_at: snapshot.observed_at,
                verified: true,
            }));
        }
        let mut fees = fees();
        fees.csfloat.seller_fee_pct = 0.0;
        fees.csfloat.buyer_fee_pct = 0.0;
        let model = SimFillModel::new(
            &depth,
            fees.clone(),
            RiskLimits {
                daily_drawdown_cap: Money(500),
                ..RiskLimits::default()
            },
            KillSwitch::new(),
        );
        let log = std::rc::Rc::default();
        let mut strategies: Vec<Box<dyn StrategyAdapter>> = vec![Box::new(Scripted {
            name: "s".into(),
            script: vec![
                (0, order(OrderSide::Buy, venue, 1, 1000, 0)),
                (7, order(OrderSide::Sell, venue, 1, 600, 7)),
                (7, order(OrderSide::Buy, venue, 1, 600, 7)),
            ],
            log: std::rc::Rc::clone(&log),
        })];
        Backtester::with_fill_model(BacktestConfig::new(fees), model)
            .run(SimulationMode, &snapshots, &[], &mut strategies)
            .unwrap();
        assert!(
            log.borrow().rejects.is_empty(),
            "{:?}",
            log.borrow().rejects
        );
        assert_eq!(log.borrow().fills.len(), 3);
    }

    #[test]
    fn break_even_sell_below_average_cost_does_not_halt_the_next_buy() {
        use crate::integrated::{MarketDepth, SimFillModel};
        use cs2_arb_strategy::{KillSwitch, RiskLimits};

        let venue = Platform::CsFloat;
        let buckets = [
            (BUCKET, 1000, 0.20),
            (Some(FloatBucket::FieldTestedHigh), 3000, 0.30),
        ];
        let mut snapshots = Vec::new();
        let mut sales = Vec::new();
        for day in [0, 7] {
            for (bucket, price, float_value) in buckets {
                let mut snapshot = snap(venue, day, price);
                snapshot.float_bucket = bucket;
                sales.extend((0..70).map(|n| cs2_arb_core::Sale {
                    id: format!("{day}-{bucket:?}-{n}"),
                    platform: venue,
                    item: snapshot.item.clone(),
                    price: snapshot.price,
                    float_value: Some(float_value),
                    sold_at: snapshot.observed_at,
                    verified: true,
                }));
                snapshots.push(snapshot);
            }
        }
        let mut depth = MarketDepth::from_snapshots(&snapshots);
        depth.add_sales(sales);
        let mut fees = fees();
        fees.csfloat.seller_fee_pct = 0.0;
        fees.csfloat.buyer_fee_pct = 0.0;
        let model = SimFillModel::new(
            &depth,
            fees.clone(),
            RiskLimits {
                daily_drawdown_cap: Money(500),
                ..RiskLimits::default()
            },
            KillSwitch::new(),
        );
        let mut high_buy = order(OrderSide::Buy, venue, 1, 3000, 0);
        high_buy.id.push_str("-high");
        high_buy.float_bucket = Some(FloatBucket::FieldTestedHigh);
        let log = std::rc::Rc::default();
        let mut strategies: Vec<Box<dyn StrategyAdapter>> = vec![Box::new(Scripted {
            name: "s".into(),
            script: vec![
                (0, order(OrderSide::Buy, venue, 1, 1000, 0)),
                (0, high_buy),
                (7, order(OrderSide::Sell, venue, 1, 1000, 7)),
                (7, order(OrderSide::Buy, venue, 1, 1000, 7)),
            ],
            log: std::rc::Rc::clone(&log),
        })];
        let report = Backtester::with_fill_model(BacktestConfig::new(fees), model)
            .run(SimulationMode, &snapshots, &[], &mut strategies)
            .unwrap();
        assert_eq!(report.trades[0].net_pnl, Money(0));
        assert!(
            log.borrow().rejects.is_empty(),
            "{:?}",
            log.borrow().rejects
        );
        assert_eq!(log.borrow().fills.len(), 4);
    }

    #[test]
    fn empty_live_book_removes_old_ask_before_next_spread_evaluation() {
        use crate::integrated::{MarketDepth, SimFillModel, SpreadAdapter};
        use cs2_arb_strategy::{KillSwitch, RiskLimits, SpreadConfig};

        let cheap = snap(Platform::CsFloat, 0, 1000);
        let mut expensive = snap(Platform::DMarket, 0, 3000);
        expensive.observed_at += Duration::minutes(2);
        let mut depth = MarketDepth::default();
        depth.replace_listings(
            t(0) + Duration::minutes(1),
            cheap.platform,
            cheap.item.clone(),
            vec![],
        );
        let fees = fees();
        let model = SimFillModel::new(
            &depth,
            fees.clone(),
            RiskLimits::default(),
            KillSwitch::new(),
        );
        let mut strategies: Vec<Box<dyn StrategyAdapter>> =
            vec![Box::new(SpreadAdapter::new(SpreadConfig::default()))];
        let report = Backtester::with_fill_model(BacktestConfig::new(fees), model)
            .run(SimulationMode, &[cheap, expensive], &[], &mut strategies)
            .unwrap();
        assert_eq!(report.window.steps, 3);
        assert_eq!(report.strategies[0].orders_submitted, 0);
    }

    #[test]
    fn expired_high_quote_does_not_mask_drawdown_or_inflate_final_mark() {
        let snapshots = [
            snap(Platform::CsFloat, 0, 1000),
            snap(Platform::Skinport, 0, 2000),
            snap(Platform::CsFloat, 2, 600),
        ];
        let (report, log) = run_scripted(
            &snapshots,
            &[],
            vec![(0, order(OrderSide::Buy, Platform::CsFloat, 1, 1000, 0))],
        );
        assert_eq!(log.fills.len(), 1);
        assert_eq!(report.totals.unrealized_pnl, Money(-422)); // 588 net exit − 1010 entry
        assert_eq!(report.totals.final_equity, Money(-422));
        assert_eq!(report.totals.max_drawdown, Money(1252)); // 1840 initial mark − 588
    }

    #[test]
    fn rejects_missing_stale_and_unmarketable_orders() {
        // Skinport stops quoting after day 0, so its quote is stale from day 2.
        let snapshots: Vec<_> = daily(4, |_, _| 1000)
            .into_iter()
            .filter(|s| s.platform != Platform::Skinport || s.observed_at == t(0))
            .collect();
        let mut other = order(OrderSide::Buy, Platform::CsFloat, 1, 1000, 1);
        other.float_bucket = Some(FloatBucket::FieldTestedHigh);
        let script = vec![
            (1, other),
            (2, order(OrderSide::Buy, Platform::Skinport, 1, 1000, 2)),
            (3, order(OrderSide::Buy, Platform::CsFloat, 1, 999, 3)),
            (3, order(OrderSide::Buy, Platform::CsFloat, 0, 1000, 3)),
        ];
        let (report, s) = run_scripted(&snapshots, &[], script);
        assert_eq!(s.rejects.len(), 4);
        assert!(s.rejects[0].contains("no quote"));
        assert!(s.rejects[1].contains("stale"));
        assert!(s.rejects[2].contains("not marketable"));
        assert!(s.rejects[3].contains("zero quantity"));
        assert_eq!(report.totals.orders_filled, 0);
        assert_eq!(report.totals.trade_count, 0);
        assert_eq!(report.totals.max_drawdown, Money(0));
    }

    #[test]
    fn delivers_events_at_first_step_at_or_after_their_time() {
        let snapshots = daily(5, |_, _| 1000);
        let events = vec![
            MarketEvent::PanicWindowClosed {
                branch: "staging".into(),
                closed_at: t(3) - Duration::hours(1),
            },
            MarketEvent::PanicWindowOpened {
                branch: "staging".into(),
                opened_at: t(1) - Duration::hours(1),
            },
            MarketEvent::BranchUpdated {
                app_id: 730,
                branch: "staging".into(),
                change_number: 1,
                observed_at: t(1) - Duration::hours(1),
            },
        ];
        let (report, s) = run_scripted(&snapshots, &events, vec![]);
        assert_eq!(
            s.saw_panic,
            [
                (0, false, 0),
                (1, true, 2),
                (2, true, 0),
                (3, false, 1),
                (4, false, 0)
            ]
        );
        assert_eq!(report.window.events_delivered, 3);
    }

    #[test]
    fn rejects_duplicate_names_and_negative_prices() {
        let backtester = Backtester::new(BacktestConfig::new(fees()));
        let mut dupes: Vec<Box<dyn StrategyAdapter>> = vec![
            Box::new(Scripted {
                name: "x".into(),
                ..Scripted::default()
            }),
            Box::new(Scripted {
                name: "x".into(),
                ..Scripted::default()
            }),
        ];
        assert!(backtester
            .run(SimulationMode, &daily(1, |_, _| 1), &[], &mut dupes)
            .is_err());
        let mut one: Vec<Box<dyn StrategyAdapter>> = vec![Box::new(Scripted::default())];
        assert!(backtester
            .run(SimulationMode, &daily(1, |_, _| -1), &[], &mut one)
            .is_err());
    }

    #[test]
    fn empty_replay_produces_empty_report() {
        let report = replay(SimulationMode, &[], &[], fees()).unwrap();
        assert_eq!(report.window.steps, 0);
        assert_eq!(report.totals.trade_count, 0);
        assert_eq!(report.totals.win_rate, None);
        assert_eq!(report.strategies.len(), 2);
        assert!(report.to_markdown().contains("No closed trades."));
    }

    #[test]
    fn drawdown_and_median_helpers() {
        assert_eq!(max_drawdown(&[]), Money(0));
        assert_eq!(
            max_drawdown(&[Money(-5), Money(10), Money(3), Money(12)]),
            Money(7)
        );
        assert_eq!(max_drawdown(&[Money(-5), Money(-2)]), Money(5));
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3, 1, 2]), Some(2.0));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2.5));
    }
}
