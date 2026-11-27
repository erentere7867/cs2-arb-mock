//! Wires [`PegBidStrategy`], [`BuyOrderFillSimulator`], and [`RelistEngine`] into
//! the CLI mock simulation runner and formats the CLI dashboard/logs.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    BuyOrder, FeeSchedules, FloatBucket, ItemId, Money, Platform, PriceSnapshot, Result, Sale,
    SimulationMode,
};
use cs2_arb_sim::venues::{VenueAccounts, VenueBalance};
use cs2_arb_sim::{BuyOrderFillConfig, BuyOrderFillSimulator};
use cs2_arb_strategy::regime::{MarketRegime, RegimeState, RegimeStatus};
use cs2_arb_strategy::{
    ActivePegBid, KillSwitch, PegBidConfig, PegBidPlacement, PegBidStrategy, RealizedRelistSale,
    RelistConfig, RelistEngine, RiskLimits, RiskManager,
};
use serde::Serialize;

pub struct BidRefresh {
    pub at: DateTime<Utc>,
    pub platform: Platform,
    pub item: ItemId,
    pub bids: Vec<BuyOrder>,
}

/// One trade-locked inventory entry displayed on the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockedInventoryRow {
    pub position_id: String,
    pub platform: Platform,
    pub item: String,
    pub quantity: u32,
    pub cost_basis: Money,
    pub locked_until: DateTime<Utc>,
    pub remaining_lock_secs: i64,
}

/// Summary snapshot of the mock peg-bidding & auto-relisting engine.
#[derive(Debug, Clone, Serialize)]
pub struct PegBidDashboard {
    pub as_of: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_regime: Option<RegimeState>,
    pub active_mock_bids: Vec<ActivePegBid>,
    pub locked_inventory: Vec<LockedInventoryRow>,
    pub ready_to_sell_count: usize,
    pub active_asks_count: usize,
    pub bids_placed: usize,
    pub bids_filled: usize,
    pub realized_sales: Vec<RealizedRelistSale>,
    pub realized_buy_order_pnl: Money,
    #[serde(skip_serializing_if = "money_is_zero")]
    pub prepaid_open_bid_fees: Money,
    pub total_seller_fees: Money,
    #[serde(skip_serializing_if = "money_is_zero")]
    pub transfer_fees_paid: Money,
    pub cash_balance: Option<Money>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub venue_balances: Vec<VenueBalance>,
}

impl PegBidDashboard {
    /// Emit structured log lines for Active Mock Bids, Locked Inventory, and Realized P&L.
    pub fn log_summary(&self) {
        tracing::info!(
            active_bids = self.active_mock_bids.len(),
            locked_inventory = self.locked_inventory.len(),
            bids_placed = self.bids_placed,
            bids_filled = self.bids_filled,
            relisted_sold = self.realized_sales.len(),
            realized_buy_order_pnl_cents = self.realized_buy_order_pnl.0,
            seller_fees_cents = self.total_seller_fees.0,
            "peg-bid dashboard summary"
        );
        for bid in &self.active_mock_bids {
            tracing::info!(
                platform = ?bid.order.platform,
                item = %bid.order.item.market_hash_name,
                bid_cents = bid.order.limit_price.0,
                median_cents = bid.rolling_median.0,
                expected_margin_pct = format!("{:.2}", bid.expected_net_margin_pct),
                "active mock bid"
            );
        }
        for row in &self.locked_inventory {
            tracing::info!(
                position_id = %row.position_id,
                platform = ?row.platform,
                item = %row.item,
                cost_basis_cents = row.cost_basis.0,
                locked_until = %row.locked_until,
                remaining_lock = %format_countdown(row.remaining_lock_secs),
                "locked inventory"
            );
        }
    }

    /// Render a Markdown dashboard section for CLI output and reports.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "\n## Peg-Bidding & Auto-Relisting Dashboard (Mock Only)\n"
        );

        let _ = writeln!(out, "### Realized Buy-Order P&L\n");
        let _ = writeln!(out, "| Metric | Value |");
        let _ = writeln!(out, "| --- | ---: |");
        if let Some(regime) = self.market_regime {
            let _ = writeln!(out, "| Market Regime | {} |", regime.status.label());
            for (label, change) in [
                ("Broad Index 24h", regime.market.short_pct),
                ("Broad Index 7d", regime.market.long_pct),
                ("Case Index 24h", regime.cases.short_pct),
                ("Case Index 7d", regime.cases.long_pct),
            ] {
                let value = change.map_or_else(|| "n/a".into(), |pct| format!("{pct:+.2}%"));
                let _ = writeln!(out, "| {label} | {value} |");
            }
        }
        let _ = writeln!(out, "| Mock Bids Placed | {} |", self.bids_placed);
        let _ = writeln!(out, "| Passive Buy-Order Fills | {} |", self.bids_filled);
        let _ = writeln!(
            out,
            "| Active Mock Bids | {} |",
            self.active_mock_bids.len()
        );
        let _ = writeln!(
            out,
            "| Locked Inventory | {} |",
            self.locked_inventory.len()
        );
        let _ = writeln!(out, "| Ready to Sell | {} |", self.ready_to_sell_count);
        let _ = writeln!(out, "| Active Relist Asks | {} |", self.active_asks_count);
        let _ = writeln!(out, "| Relisted & Sold | {} |", self.realized_sales.len());
        let _ = writeln!(
            out,
            "| Seller Fees Paid | {} |",
            format_money(self.total_seller_fees)
        );
        let _ = writeln!(
            out,
            "| Realized Buy-Order P&L | {} |",
            format_money(self.realized_buy_order_pnl)
        );
        if let Some(cash) = self.cash_balance {
            let _ = writeln!(out, "| Mock Cash Balance | {} |", format_money(cash));
        }
        if !money_is_zero(&self.prepaid_open_bid_fees) {
            let _ = writeln!(
                out,
                "| Prepaid Fees on Open Bids | {} |",
                format_money(self.prepaid_open_bid_fees)
            );
        }

        let _ = writeln!(out, "\n### Active Mock Bids\n");
        if self.active_mock_bids.is_empty() {
            let _ = writeln!(out, "_None_\n");
        } else {
            let _ = writeln!(
                out,
                "| Platform | Item | Bid | Median | Ceiling | Est. Net Margin |"
            );
            let _ = writeln!(out, "| --- | --- | ---: | ---: | ---: | ---: |");
            for bid in &self.active_mock_bids {
                let _ = writeln!(
                    out,
                    "| {:?} | {} | {} | {} | {} | {:.2}% |",
                    bid.order.platform,
                    escape_md_cell(&bid.order.item.market_hash_name),
                    format_money(bid.order.limit_price),
                    format_money(bid.rolling_median),
                    format_money(bid.effective_ceiling),
                    bid.expected_net_margin_pct,
                );
            }
            out.push('\n');
        }

        let _ = writeln!(out, "### Locked Inventory\n");
        if self.locked_inventory.is_empty() {
            let _ = writeln!(out, "_None_\n");
        } else {
            let _ = writeln!(
                out,
                "| Position | Platform | Item | Qty | Cost Basis | Locked Until | Remaining Lock |"
            );
            let _ = writeln!(out, "| --- | --- | --- | ---: | ---: | --- | ---: |");
            for row in &self.locked_inventory {
                let _ = writeln!(
                    out,
                    "| {} | {:?} | {} | {} | {} | {} | {} |",
                    escape_md_cell(&row.position_id),
                    row.platform,
                    escape_md_cell(&row.item),
                    row.quantity,
                    format_money(row.cost_basis),
                    row.locked_until.format("%Y-%m-%d %H:%M UTC"),
                    format_countdown(row.remaining_lock_secs),
                );
            }
            out.push('\n');
        }

        cs2_arb_backtest::report::write_fee_breakdowns(
            &mut out,
            self.realized_sales
                .iter()
                .filter_map(|s| s.fee_breakdown.map(|f| (s.position_id.as_str(), f))),
        );
        cs2_arb_backtest::report::write_venue_balances(
            &mut out,
            std::iter::once(("peg-bid", self.venue_balances.as_slice())),
        );
        cs2_arb_backtest::report::write_transfer_fees(
            &mut out,
            std::iter::once(("peg-bid", self.transfer_fees_paid)),
        );
        out
    }
}

/// Derive deterministic mock top-of-book buy orders from historical snapshots for
/// offline bundled simulations when no live buy orders were collected.
pub fn derive_offline_buy_orders(snapshots: &[PriceSnapshot]) -> Vec<BuyOrder> {
    snapshots
        .iter()
        .filter(|q| matches!(q.platform, Platform::CsFloat | Platform::Waxpeer) && q.price.0 > 0)
        .map(|q| {
            // Illustrative top-of-book bid at 86% of ask snapshot so peg-bidding
            // stays within the strict >= 7% net margin ceiling after fees & hold.
            let bid_cents = (q.price.0 * 86 / 100).max(1);
            BuyOrder {
                platform: q.platform,
                item: q.item.clone(),
                float_bucket: q.float_bucket,
                price: Money(bid_cents),
                quantity: 1,
                observed_at: q.observed_at,
            }
        })
        .collect()
}

/// Run the mock peg-bidding, passive buy-order fill, and auto-relisting pipeline
/// chronologically across all observed snapshots, buy orders, and sales.
#[allow(clippy::too_many_arguments)]
pub fn run_peg_bid_engine(
    snapshots: &[PriceSnapshot],
    sales: &[Sale],
    buy_orders: &[BuyOrder],
    bid_refreshes: &[BidRefresh],
    fees: &FeeSchedules,
    peg_config: PegBidConfig,
    relist_config: RelistConfig,
    risk_limits: RiskLimits,
    kill_switch: &KillSwitch,
    starting_cash: Option<Money>,
    accounts: Option<VenueAccounts>,
    market_regime: Option<&MarketRegime>,
) -> Result<PegBidDashboard> {
    let resting_ttl = peg_config
        .resting_bid_ttl_secs
        .map(|seconds| Duration::seconds(i64::from(seconds)));
    let mut strategy = PegBidStrategy::new(peg_config)?;
    let mut risk = RiskManager::new(risk_limits, kill_switch.clone())?;
    let sim_config = BuyOrderFillConfig {
        starting_cash,
        ..BuyOrderFillConfig::default()
    };
    let mut simulator = BuyOrderFillSimulator::new(SimulationMode, sim_config, fees, 730)?;
    let mut relist = RelistEngine::new(relist_config)?;
    if let Some(accounts) = accounts {
        for platform in Platform::ALL {
            if let Some(seconds) = accounts.sale_balance_hold(platform) {
                relist.set_balance_hold(platform, seconds);
            }
        }
        simulator.set_venue_accounts(accounts);
    }

    let mut sorted_snapshots = snapshots.to_vec();
    sorted_snapshots.sort_by_key(|s| s.observed_at);

    let mut seen_sales = std::collections::HashSet::new();
    let mut sorted_sales: Vec<Sale> = sales
        .iter()
        .filter(|s| seen_sales.insert((s.platform, s.id.clone())))
        .cloned()
        .collect();
    sorted_sales.sort_by_key(|s| s.sold_at);

    let mut sorted_bids = buy_orders.to_vec();
    sorted_bids.sort_by_key(|b| b.observed_at);
    let mut bid_refreshes: Vec<_> = bid_refreshes.iter().collect();
    bid_refreshes.sort_by_key(|refresh| refresh.at);

    let mut times: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    times.extend(sorted_snapshots.iter().map(|s| s.observed_at));
    times.extend(sorted_sales.iter().map(|s| s.sold_at));
    times.extend(sorted_bids.iter().map(|b| b.observed_at));
    times.extend(bid_refreshes.iter().map(|refresh| refresh.at));
    let replay_end = times.last().copied();

    let mut snap_idx = 0usize;
    let mut sale_idx = 0usize;
    let mut bid_idx = 0usize;
    let mut refresh_idx = 0usize;
    let mut bids_placed = 0usize;
    let mut bids_filled = 0usize;
    let mut last_now = None;
    let mut latest_prices: HashMap<(Platform, ItemId, Option<FloatBucket>), Money> = HashMap::new();
    let mut accounted_fee_losses = Money(0);
    let mut last_regime = market_regime.map(|_| RegimeState::default());
    let mut last_status = None;

    while let Some(now) = times.pop_first() {
        let snapshot_tick = sorted_snapshots
            .get(snap_idx)
            .is_some_and(|s| s.observed_at == now);
        let market_tick = snapshot_tick
            || sorted_sales.get(sale_idx).is_some_and(|s| s.sold_at == now)
            || sorted_bids
                .get(bid_idx)
                .is_some_and(|b| b.observed_at == now)
            || bid_refreshes
                .get(refresh_idx)
                .is_some_and(|refresh| refresh.at == now);
        if !market_tick
            && !resting_ttl.is_some_and(|ttl| {
                simulator
                    .active_bids()
                    .iter()
                    .any(|bid| now - bid.placed_at >= ttl)
            })
        {
            continue;
        }
        // Transition any lots whose 7-day lock expired before `now` so they are
        // listed at `locked_until` before crossing prints at `now` are evaluated.
        for unlock_at in relist.pending_unlocks_before(now) {
            relist.list_at_unlock(
                &sorted_sales[..sale_idx],
                &sorted_snapshots[..snap_idx],
                fees,
                unlock_at,
            )?;
        }

        simulator.advance_venue_accounts(now)?;
        last_now = Some(now);
        let regime = market_regime.map(|model| model.at(now));
        let risk_on = regime
            .as_ref()
            .is_none_or(|regime| regime.state.status == RegimeStatus::RiskOn);
        if let Some(regime) = &regime {
            let status = regime.state.status;
            if last_status != Some(status) && status == RegimeStatus::InsufficientHistory {
                tracing::warn!("market regime is risk-off: insufficient comparable index history");
            }
            last_status = Some(status);
            last_regime = Some(regime.state);
        }

        if let Some(ttl) = resting_ttl {
            for id in simulator.expire_bids(now, ttl)? {
                strategy.mark_filled(&id);
            }
            account_bid_fee_losses(
                &mut risk,
                &simulator,
                &latest_prices,
                now,
                &mut accounted_fee_losses,
            )?;
        }

        while snap_idx < sorted_snapshots.len() && sorted_snapshots[snap_idx].observed_at <= now {
            let snap = &sorted_snapshots[snap_idx];
            if snap.price.0 > 0 {
                latest_prices.insert(
                    (snap.platform, snap.item.clone(), snap.float_bucket),
                    snap.price,
                );
                if snap.float_bucket.is_some() {
                    latest_prices.insert((snap.platform, snap.item.clone(), None), snap.price);
                }
            }
            strategy.observe_snapshot(snap);
            snap_idx += 1;
        }
        let refresh_start = refresh_idx;
        while let Some(refresh) = bid_refreshes
            .get(refresh_idx)
            .filter(|refresh| refresh.at == now)
        {
            relist.replace_bid_book(refresh.platform, &refresh.item, &refresh.bids);
            strategy.replace_buy_orders(refresh.platform, &refresh.item, &refresh.bids, now);
            refresh_idx += 1;
        }
        let bid_start = bid_idx;
        while bid_idx < sorted_bids.len() && sorted_bids[bid_idx].observed_at <= now {
            bid_idx += 1;
        }
        let historical_bids = &sorted_bids[bid_start..bid_idx];
        relist.observe_bids(historical_bids);
        strategy.ingest_buy_orders(historical_bids.iter().cloned());
        strategy.cancel_unprofitable_bids(fees, now)?;
        cancel_discarded_bids(&strategy, &mut simulator, now)?;
        account_bid_fee_losses(
            &mut risk,
            &simulator,
            &latest_prices,
            now,
            &mut accounted_fee_losses,
        )?;

        while sale_idx < sorted_sales.len() && sorted_sales[sale_idx].sold_at <= now {
            let sale = &sorted_sales[sale_idx];
            if sale.price.0 > 0 {
                let bucket = sale.float_value.and_then(FloatBucket::from_float);
                latest_prices.insert((sale.platform, sale.item.clone(), bucket), sale.price);
                if bucket.is_some() {
                    latest_prices.insert((sale.platform, sale.item.clone(), None), sale.price);
                }
            }
            strategy.observe_sale(sale);
            for report in simulator.process_transaction_with(sale, now, |order| {
                regime
                    .as_ref()
                    .is_none_or(|state| state.allows(&order.item))
            }) {
                bids_filled += 1;
                let unrealized = compute_unrealized_pnl(&simulator, &latest_prices);
                let _ = risk.record_marked_fill(&report.fill, unrealized, Money(0))?;
                if simulator.is_order_filled(&report.fill.order.id) {
                    strategy.mark_filled(&report.fill.order.id);
                }
                if let Some(pos) = report.position {
                    relist.track_filled_position(
                        pos,
                        &report.fill,
                        report
                            .allocated_deposit
                            .expect("passive buy allocates deposit"),
                    );
                }
            }
            sale_idx += 1;
        }

        let relist_step = relist.step(
            &sorted_sales[..sale_idx],
            &sorted_snapshots[..snap_idx],
            fees,
            now,
        )?;
        for settled in &relist_step.realized_sales {
            match simulator.settle_relist_sale(
                &settled.position_id,
                settled.platform,
                settled.ask_price,
                settled.sold_at,
            ) {
                Ok(report) => {
                    let unrealized = compute_unrealized_pnl(&simulator, &latest_prices);
                    let _ = risk.record_marked_fill(
                        &report.fill,
                        unrealized,
                        report.ledger_entry.cost_basis_released.unwrap_or(Money(0)),
                    )?;
                }
                Err(reason) => tracing::warn!(
                    position_id = %settled.position_id,
                    ?reason,
                    "relist settlement rejected by simulator"
                ),
            }
        }

        let unrealized = compute_unrealized_pnl(&simulator, &latest_prices);
        risk.set_unrealized_pnl(unrealized, now);

        if risk_on
            && !kill_switch.is_engaged()
            && (snapshot_tick || !historical_bids.is_empty() || refresh_idx > refresh_start)
        {
            strategy.evaluate_with(&[], fees, now, |order, resting| {
                if regime.as_ref().is_some_and(|state| !state.allows(&order.item)) {
                    return Ok(PegBidPlacement::CancelResting);
                }
                let cost = fees
                    .get(order.platform)
                    .buyer_order_total(order.limit_price, order.quantity)?;
                let can_afford = simulator
                    .portfolio()
                    .available_on(order.platform)
                    .is_none_or(|cash| cash >= cost);
                let accepted = can_afford
                    && match risk.check(order, now) {
                        Ok(()) => simulator.place_bid(order.clone(), now).is_ok(),
                        Err(reason) => {
                            tracing::debug!(order_id = %order.id, %reason, "peg bid blocked by risk limits");
                            false
                        }
                    };
                if !accepted {
                    let still_allowed = resting.is_some_and(|order| {
                        simulator.active_bids().into_iter().any(|bid| {
                            bid.order.id == order.id
                                && risk.allows(
                                    &cs2_arb_core::Order {
                                        quantity: bid.remaining_quantity,
                                        ..order.clone()
                                    },
                                    now,
                                )
                        })
                    });
                    return Ok(if still_allowed {
                        PegBidPlacement::KeepResting
                    } else {
                        PegBidPlacement::CancelResting
                    });
                }
                bids_placed += 1;
                if let Some(expiry) = resting_ttl.and_then(|ttl| now.checked_add_signed(ttl)) {
                    if Some(expiry) <= replay_end {
                        times.insert(expiry);
                    }
                }
                account_bid_fee_losses(
                    &mut risk,
                    &simulator,
                    &latest_prices,
                    now,
                    &mut accounted_fee_losses,
                )?;
                Ok(PegBidPlacement::Accepted)
            })?;
        }
        if let Some(regime) = &regime {
            strategy.retain_bids(|item| regime.allows(item));
        }
        strategy.cancel_unprofitable_bids(fees, now)?;
        cancel_discarded_bids(&strategy, &mut simulator, now)?;
        account_bid_fee_losses(
            &mut risk,
            &simulator,
            &latest_prices,
            now,
            &mut accounted_fee_losses,
        )?;
    }

    if let Some(now) = last_now {
        simulator.advance_venue_accounts(now)?;
    }
    let active_mock_bids = strategy.active_bids().into_iter().cloned().collect();
    let locked_inventory = match last_now {
        Some(as_of) => relist
            .locked_inventory(as_of)
            .into_iter()
            .map(|(pos, rem)| LockedInventoryRow {
                position_id: pos.id.clone(),
                platform: pos.acquired_on,
                item: pos.item.market_hash_name.clone(),
                quantity: pos.quantity,
                cost_basis: pos.cost_basis,
                locked_until: pos.locked_until,
                remaining_lock_secs: rem.num_seconds().max(0),
            })
            .collect(),
        None => Vec::new(),
    };

    Ok(PegBidDashboard {
        as_of: last_now,
        market_regime: last_regime,
        active_mock_bids,
        locked_inventory,
        ready_to_sell_count: relist.ready_to_sell().len(),
        active_asks_count: relist.active_asks().len(),
        bids_placed,
        bids_filled,
        realized_sales: relist.realized_sales().to_vec(),
        realized_buy_order_pnl: relist
            .total_realized_pnl()
            .checked_sub(simulator.realized_bid_fee_losses())?,
        prepaid_open_bid_fees: simulator.unallocated_buy_fees(),
        total_seller_fees: relist.total_seller_fees(),
        transfer_fees_paid: simulator.portfolio().transfer_fees_paid(),
        cash_balance: simulator.cash(),
        venue_balances: simulator.portfolio().venue_balances()?,
    })
}

pub fn format_countdown(secs: i64) -> String {
    let clamped = secs.max(0);
    let dur = Duration::seconds(clamped);
    let days = dur.num_days();
    let hours = dur.num_hours() % 24;
    let mins = dur.num_minutes() % 60;
    format!("{days}d {hours:02}h {mins:02}m")
}

fn cancel_discarded_bids(
    strategy: &PegBidStrategy,
    simulator: &mut BuyOrderFillSimulator,
    now: DateTime<Utc>,
) -> Result<()> {
    let active = strategy.active_bids();
    let discarded: Vec<_> = simulator
        .active_bids()
        .into_iter()
        .filter(|bid| {
            !active
                .iter()
                .any(|resting| resting.order.id == bid.order.id)
        })
        .map(|bid| {
            (
                bid.order.platform,
                bid.order.item.clone(),
                bid.order.float_bucket,
            )
        })
        .collect();
    for (platform, item, bucket) in discarded {
        simulator.cancel_bids_for_item(platform, &item, bucket, now)?;
    }
    Ok(())
}

fn format_money(money: Money) -> String {
    let sign = if money.0 < 0 { "-" } else { "" };
    let abs = money.0.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

fn escape_md_cell(value: &str) -> String {
    value.replace('|', "\\|")
}

fn money_is_zero(money: &Money) -> bool {
    money.0 == 0
}

fn account_bid_fee_losses(
    risk: &mut RiskManager,
    simulator: &BuyOrderFillSimulator,
    prices: &HashMap<(Platform, ItemId, Option<FloatBucket>), Money>,
    now: DateTime<Utc>,
    accounted: &mut Money,
) -> Result<()> {
    let realized = simulator.realized_bid_fee_losses();
    if realized != *accounted {
        risk.record_marked_pnl_adjustment(
            accounted.checked_sub(realized)?,
            compute_unrealized_pnl(simulator, prices),
            now,
        );
        *accounted = realized;
    }
    Ok(())
}

fn compute_unrealized_pnl(
    simulator: &BuyOrderFillSimulator,
    latest_prices: &HashMap<(Platform, ItemId, Option<FloatBucket>), Money>,
) -> Money {
    let cents: i64 = simulator
        .portfolio()
        .positions()
        .iter()
        .filter_map(|pos| {
            let mark = latest_prices.get(&(pos.acquired_on, pos.item.clone(), pos.float_bucket))?;
            let market_value = mark.0.saturating_mul(i64::from(pos.quantity));
            Some(market_value.saturating_sub(pos.cost_basis.0))
        })
        .fold(0_i64, |acc, delta| acc.saturating_add(delta));
    Money(cents.saturating_sub(simulator.unallocated_buy_fees().0))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{FloatBucket, ItemId, WearTier};

    use super::*;

    fn fees() -> FeeSchedules {
        FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap()
    }

    fn item() -> ItemId {
        ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    fn t(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn prepaid_fixed_fee_counts_toward_peg_drawdown_before_and_after_cancellation() {
        let at = t(4, 12);
        let mut fees = fees();
        fees.csfloat.buyer_fixed_fee = Money(200);
        let mut sim = BuyOrderFillSimulator::new(
            SimulationMode,
            BuyOrderFillConfig {
                min_recorded_sales_7d: 1,
                ..BuyOrderFillConfig::default()
            },
            &fees,
            1,
        )
        .unwrap();
        let order = cs2_arb_core::Order {
            id: "bid".into(),
            side: cs2_arb_core::OrderSide::Buy,
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            quantity: 2,
            limit_price: Money(8500),
            submitted_at: at,
        };
        sim.place_bid(order.clone(), at).unwrap();
        let fill_at = at + Duration::minutes(1);
        let fill = sim
            .process_transaction(
                &Sale {
                    id: "first".into(),
                    platform: order.platform,
                    item: order.item.clone(),
                    price: Money(8499),
                    float_value: None,
                    sold_at: fill_at,
                    verified: true,
                },
                fill_at,
            )
            .pop()
            .unwrap();
        let mark = fill.position.unwrap().cost_basis;
        let prices = HashMap::from([((order.platform, order.item.clone(), None), mark)]);
        let mut risk = RiskManager::new(
            RiskLimits {
                daily_drawdown_cap: Money(100),
                ..RiskLimits::default()
            },
            KillSwitch::new(),
        )
        .unwrap();
        risk.set_unrealized_pnl(Money(0), at);
        risk.record_marked_fill(&fill.fill, compute_unrealized_pnl(&sim, &prices), Money(0))
            .unwrap();
        assert_eq!(risk.day_drawdown(), Money(100));
        assert!(risk.entries_halted(fill_at));
        let cancel_at = fill_at + Duration::minutes(1);
        sim.cancel_bids_for_item(order.platform, &order.item, None, cancel_at)
            .unwrap();
        account_bid_fee_losses(&mut risk, &sim, &prices, cancel_at, &mut Money(0)).unwrap();
        assert_eq!(risk.day_drawdown(), Money(100));
        assert_eq!(sim.portfolio().realized_pnl(), Money(-100));
    }

    #[test]
    fn filled_bid_expiry_does_not_fill_a_relist_between_market_ticks() {
        let snapshots: Vec<_> = (1..=11)
            .map(|day| t(day, 12))
            .chain([t(11, 15), t(11, 17)])
            .map(|observed_at| PriceSnapshot {
                platform: Platform::CsFloat,
                item: item(),
                float_bucket: Some(FloatBucket::FieldTestedLow),
                price: Money(10_000),
                observed_at,
            })
            .collect();
        let mut sales: Vec<_> = (1..=10)
            .flat_map(|day| (16..=20).map(move |hour| (day, hour)))
            .map(|(day, hour)| Sale {
                id: format!("{day}-{hour}"),
                platform: Platform::CsFloat,
                item: item(),
                price: Money(10_000),
                float_value: Some(0.20),
                sold_at: t(day, hour),
                verified: true,
            })
            .collect();
        sales.push(Sale {
            id: "trigger".into(),
            price: Money(8_450),
            sold_at: t(4, 14),
            ..sales[0].clone()
        });
        let bids = [BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(8_500),
            quantity: 1,
            observed_at: t(4, 12),
        }];
        let dashboard = run_peg_bid_engine(
            &snapshots,
            &sales,
            &bids,
            &[],
            &fees(),
            PegBidConfig {
                resting_bid_ttl_secs: Some(7 * 86_400 + 4 * 3_600),
                ..PegBidConfig::default()
            },
            RelistConfig {
                min_listing_age_secs: 2 * 3_600,
                ..RelistConfig::default()
            },
            RiskLimits::default(),
            &KillSwitch::new(),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(dashboard.bids_filled, 1);
        assert_eq!(dashboard.realized_sales.len(), 1);
        assert_eq!(dashboard.realized_sales[0].sold_at, t(11, 17));
    }

    #[test]
    fn end_to_end_peg_bid_fill_seven_day_lock_relist_and_dashboard() {
        let mut snapshots = Vec::new();
        let mut sales = Vec::new();

        // Build 4 days of median $100.00 (10_000c) history with >= 15 verified sales.
        for day in 1..=4 {
            let ts = t(day, 12);
            snapshots.push(PriceSnapshot {
                platform: Platform::CsFloat,
                item: item(),
                float_bucket: Some(FloatBucket::FieldTestedLow),
                price: Money(10_000),
                observed_at: ts,
            });
            for h in 1..=5 {
                sales.push(Sale {
                    id: format!("hist-{day}-{h}"),
                    platform: Platform::CsFloat,
                    item: item(),
                    price: Money(10_000),
                    float_value: Some(0.20),
                    sold_at: ts + Duration::hours(h),
                    verified: true,
                });
            }
        }

        // Top-of-book buy order at $85.00 on day 4 -> PegBid places at $85.01 (within 7% net ceiling).
        let buy_orders = vec![BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(8_500),
            quantity: 1,
            observed_at: t(4, 12),
        }];

        // Verified print at $84.50 on day 4 14:00 triggers passive buy-order fill -> locked until day 11 14:00.
        sales.push(Sale {
            id: "trigger-fill".into(),
            platform: Platform::CsFloat,
            item: item(),
            price: Money(8_450),
            float_value: Some(0.20),
            sold_at: t(4, 14),
            verified: true,
        });

        // Mid-lock check (day 8): item must still be locked with ~3d remaining, 0 realized sales.
        let mid_dashboard = run_peg_bid_engine(
            &snapshots,
            &sales,
            &buy_orders,
            &[],
            &fees(),
            PegBidConfig::default(),
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(mid_dashboard.bids_placed, 1);
        assert_eq!(mid_dashboard.bids_filled, 1);
        assert_eq!(mid_dashboard.locked_inventory.len(), 1);
        assert!(mid_dashboard.locked_inventory[0].remaining_lock_secs > 0);
        assert_eq!(mid_dashboard.realized_sales.len(), 0);
        assert_eq!(mid_dashboard.cash_balance, Some(Money(50_000 - 8_501)));

        // Advance past 7-day lock (day 11 14:00 -> relisted at median $100.00; day 11 16:00 -> filled on volume).
        for day in 5..=11 {
            let ts = t(day, 12);
            snapshots.push(PriceSnapshot {
                platform: Platform::CsFloat,
                item: item(),
                float_bucket: Some(FloatBucket::FieldTestedLow),
                price: Money(10_000),
                observed_at: ts,
            });
            for h in 1..=3 {
                sales.push(Sale {
                    id: format!("post-{day}-{h}"),
                    platform: Platform::CsFloat,
                    item: item(),
                    price: Money(10_000),
                    float_value: Some(0.20),
                    sold_at: ts + Duration::hours(h),
                    verified: true,
                });
            }
        }
        // Final tick after listing age (16:00) to settle volume-based ask fill.
        snapshots.push(PriceSnapshot {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(10_000),
            observed_at: t(11, 16),
        });

        let final_dashboard = run_peg_bid_engine(
            &snapshots,
            &sales,
            &buy_orders,
            &[],
            &fees(),
            PegBidConfig::default(),
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();

        assert_eq!(final_dashboard.bids_filled, 1);
        assert_eq!(final_dashboard.locked_inventory.len(), 0);
        assert_eq!(final_dashboard.realized_sales.len(), 1);
        // Gross $100.00 - 2% CSFloat seller fee ($2.00) = $98.00 net proceeds.
        // Cost basis = $85.01 -> realized net P&L = $12.99 (1_299c).
        assert_eq!(final_dashboard.realized_buy_order_pnl, Money(1_299));
        assert_eq!(final_dashboard.total_seller_fees, Money(200));
        assert_eq!(
            final_dashboard.cash_balance,
            Some(Money(50_000 - 8_501 + 9_800))
        );

        let md = final_dashboard.to_markdown();
        assert!(md.contains("Active Mock Bids"));
        assert!(md.contains("Locked Inventory"));
        assert!(md.contains("Realized Buy-Order P&L"));
        assert!(md.contains("$12.99"));
    }

    #[test]
    fn market_selloff_blocks_a_crossing_print_and_cancels_the_resting_bid() {
        use cs2_arb_strategy::regime::RegimeConfig;

        let mut snapshots = Vec::new();
        for (day, market_price) in [
            (1, 10_000),
            (2, 10_000),
            (7, 10_000),
            (8, 10_000),
            (9, 9_000),
        ] {
            for (item, price) in [
                (item(), 10_000),
                (
                    ItemId {
                        market_hash_name: "Other One".into(),
                        ..item()
                    },
                    market_price,
                ),
                (
                    ItemId {
                        market_hash_name: "Other Two".into(),
                        ..item()
                    },
                    market_price,
                ),
            ] {
                snapshots.push(PriceSnapshot {
                    platform: Platform::CsFloat,
                    item,
                    float_bucket: None,
                    price: Money(price),
                    observed_at: t(day, 12),
                });
            }
        }
        let bids = [7, 8, 9].map(|day| BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_500),
            quantity: 1,
            observed_at: t(day, 12),
        });
        let mut sales: Vec<_> = (0..20)
            .map(|i| Sale {
                id: format!("liquidity-{i}"),
                platform: Platform::CsFloat,
                item: item(),
                price: Money(10_000),
                float_value: Some(0.20),
                sold_at: t(7, 12) + Duration::hours(i),
                verified: true,
            })
            .collect();
        sales.push(Sale {
            id: "crossing".into(),
            sold_at: t(9, 12),
            price: Money(8_400),
            ..sales[0].clone()
        });
        let run = |regime| {
            run_peg_bid_engine(
                &snapshots,
                &sales,
                &bids,
                &[],
                &fees(),
                PegBidConfig {
                    max_order_age_secs: None,
                    min_median_samples: 1,
                    ..PegBidConfig::default()
                },
                RelistConfig::default(),
                RiskLimits::default(),
                &KillSwitch::new(),
                Some(Money(50_000)),
                None,
                regime,
            )
            .unwrap()
        };
        let regime = MarketRegime::new(RegimeConfig::default(), &snapshots, &sales).unwrap();
        let unguarded = run(None);
        assert_eq!(unguarded.bids_filled, 1);
        let guarded = run(Some(&regime));
        assert_eq!(guarded.bids_placed, 1);
        assert_eq!(guarded.bids_filled, 0);
        assert!(guarded.active_mock_bids.is_empty());
        assert_eq!(guarded.cash_balance, Some(Money(50_000)));
        assert!(guarded
            .to_markdown()
            .contains("| Market Regime | Risk-off: market downtrend |"));
        assert!(guarded
            .to_markdown()
            .contains("| Broad Index 24h | -10.00% |"));
    }

    #[test]
    fn snapshot_only_tick_replaces_an_unsafe_bid_using_the_fresh_book() {
        let placed_at = t(3, 12);
        let repriced_at = placed_at + Duration::seconds(1);
        let platform = Platform::CsFloat;
        let quote = PriceSnapshot {
            platform,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(10_000),
            observed_at: placed_at,
        };
        let bid = BuyOrder {
            platform,
            item: quote.item.clone(),
            float_bucket: quote.float_bucket,
            price: Money(8_500),
            quantity: 1,
            observed_at: placed_at,
        };
        let snapshots = [
            quote.clone(),
            PriceSnapshot {
                price: Money(6_000),
                observed_at: repriced_at,
                ..quote
            },
        ];
        let config = PegBidConfig {
            lower_to_ceiling: true,
            min_median_samples: 1,
            ..PegBidConfig::default()
        };
        let fees = fees();
        let ceiling =
            cs2_arb_strategy::compute_effective_ceiling(Money(8_000), fees.get(platform), &config)
                .unwrap()
                .unwrap();
        let dashboard = run_peg_bid_engine(
            &snapshots,
            &[],
            &[bid],
            &[],
            &fees,
            config,
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(dashboard.bids_placed, 2);
        assert_eq!(dashboard.bids_filled, 0);
        let [replacement] = dashboard.active_mock_bids.as_slice() else {
            panic!("expected one replacement bid");
        };
        assert_eq!(replacement.updated_at, repriced_at);
        assert_eq!(replacement.order.limit_price, ceiling);
    }

    #[test]
    fn snapshot_only_replacement_rejected_by_risk_keeps_the_safe_resting_bid() {
        let placed_at = t(3, 12);
        let repriced_at = placed_at + Duration::minutes(6);
        let platform = Platform::CsFloat;
        let quote = PriceSnapshot {
            platform,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(8_000),
            observed_at: placed_at,
        };
        let bid = BuyOrder {
            platform,
            item: quote.item.clone(),
            float_bucket: quote.float_bucket,
            price: Money(8_500),
            quantity: 1,
            observed_at: placed_at,
        };
        let snapshots = [
            quote.clone(),
            PriceSnapshot {
                price: Money(12_000),
                observed_at: repriced_at,
                ..quote
            },
        ];
        let config = PegBidConfig {
            lower_to_ceiling: true,
            min_median_samples: 1,
            ..PegBidConfig::default()
        };
        let fees = fees();
        let ceiling =
            cs2_arb_strategy::compute_effective_ceiling(Money(8_000), fees.get(platform), &config)
                .unwrap()
                .unwrap();
        let dashboard = run_peg_bid_engine(
            &snapshots,
            &[],
            &[bid],
            &[],
            &fees,
            config,
            RelistConfig::default(),
            RiskLimits {
                max_position_notional: Money(8_000),
                ..RiskLimits::default()
            },
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(dashboard.bids_placed, 1);
        let [resting] = dashboard.active_mock_bids.as_slice() else {
            panic!("expected the original bid to remain active");
        };
        assert_eq!(resting.updated_at, placed_at);
        assert_eq!(resting.order.limit_price, ceiling);
    }

    #[test]
    fn rejected_replacement_cancels_the_resting_bid_if_exposure_has_increased() {
        let placed_at = t(3, 12);
        let repriced_at = placed_at + Duration::minutes(6);
        let platform = Platform::CsFloat;
        let quote = PriceSnapshot {
            platform,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(8_000),
            observed_at: placed_at,
        };
        let other = PriceSnapshot {
            item: ItemId {
                market_hash_name: "AK-47 | Slate (Field-Tested)".into(),
                ..item()
            },
            price: Money(10_000),
            ..quote.clone()
        };
        let bids = [
            (&quote, Money(8_500), placed_at),
            (&other, Money(5_000), placed_at - Duration::minutes(55)),
        ]
        .map(|(snapshot, price, observed_at)| BuyOrder {
            platform,
            item: snapshot.item.clone(),
            float_bucket: snapshot.float_bucket,
            price,
            quantity: 1,
            observed_at,
        });
        let mut sales = Vec::new();
        for (snapshot, crossed_at, price) in [
            (&other, placed_at + Duration::minutes(1), Money(4_900)),
            (&quote, repriced_at + Duration::seconds(1), Money(7_000)),
        ] {
            let crossing = Sale {
                id: format!("crossing-{}", snapshot.item.market_hash_name),
                platform,
                item: snapshot.item.clone(),
                price,
                float_value: Some(0.20),
                sold_at: crossed_at,
                verified: true,
            };
            sales.extend((0..15).map(|i| Sale {
                id: format!("{}-{i}", crossing.id),
                sold_at: t(2, 10),
                ..crossing.clone()
            }));
            sales.push(crossing);
        }
        let snapshots = [
            quote.clone(),
            other.clone(),
            PriceSnapshot {
                price: Money(12_000),
                observed_at: repriced_at,
                ..quote
            },
        ];
        let dashboard = run_peg_bid_engine(
            &snapshots,
            &sales,
            &bids,
            &[],
            &fees(),
            PegBidConfig {
                lower_to_ceiling: true,
                min_median_samples: 1,
                lookback_days: 1,
                ..PegBidConfig::default()
            },
            RelistConfig::default(),
            RiskLimits {
                max_total_exposure: Money(12_000),
                ..RiskLimits::default()
            },
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(dashboard.bids_placed, 2);
        assert_eq!(dashboard.bids_filled, 1);
        assert!(dashboard.active_mock_bids.is_empty());
        assert_eq!(
            dashboard.locked_inventory[0].item,
            other.item.market_hash_name
        );
    }

    #[test]
    fn an_empty_book_refresh_cancels_the_resting_bid_before_the_next_print() {
        let quote = PriceSnapshot {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(10_000),
            observed_at: t(3, 12),
        };
        let bid = BuyOrder {
            platform: quote.platform,
            item: quote.item.clone(),
            float_bucket: quote.float_bucket,
            price: Money(8_500),
            quantity: 1,
            observed_at: t(3, 13),
        };
        let mut sales: Vec<_> = (0..15)
            .map(|n| Sale {
                id: format!("history-{n}"),
                platform: quote.platform,
                item: quote.item.clone(),
                price: quote.price,
                float_value: Some(0.20),
                sold_at: t(3, 10),
                verified: true,
            })
            .collect();
        sales.push(Sale {
            id: "cross".into(),
            price: Money(8_400),
            sold_at: t(3, 14),
            ..sales[0].clone()
        });
        let refreshes = [
            BidRefresh {
                at: bid.observed_at,
                platform: bid.platform,
                item: bid.item.clone(),
                bids: vec![bid.clone()],
            },
            BidRefresh {
                at: t(3, 14),
                platform: bid.platform,
                item: bid.item,
                bids: vec![],
            },
        ];
        let dashboard = run_peg_bid_engine(
            &[quote],
            &sales,
            &[],
            &refreshes,
            &fees(),
            PegBidConfig::default(),
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(dashboard.bids_placed, 1);
        assert_eq!(dashboard.bids_filled, 0);
        assert!(dashboard.active_mock_bids.is_empty());
        assert_eq!(dashboard.cash_balance, Some(Money(50_000)));
    }
}
