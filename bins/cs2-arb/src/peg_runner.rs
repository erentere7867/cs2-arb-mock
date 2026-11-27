//! Wires [`PegBidStrategy`], [`BuyOrderFillSimulator`], and [`RelistEngine`] into
//! the CLI mock simulation runner and formats the CLI dashboard/logs.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    BuyOrder, FeeSchedules, Money, Platform, PriceSnapshot, Result, Sale, SimulationMode,
};
use cs2_arb_sim::{BuyOrderFillConfig, BuyOrderFillSimulator};
use cs2_arb_strategy::{
    ActivePegBid, KillSwitch, PegBidConfig, PegBidStrategy, RealizedRelistSale, RelistConfig,
    RelistEngine, RiskLimits, RiskManager,
};
use serde::Serialize;

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
    pub active_mock_bids: Vec<ActivePegBid>,
    pub locked_inventory: Vec<LockedInventoryRow>,
    pub ready_to_sell_count: usize,
    pub active_asks_count: usize,
    pub bids_placed: usize,
    pub bids_filled: usize,
    pub realized_sales: Vec<RealizedRelistSale>,
    pub realized_buy_order_pnl: Money,
    pub total_seller_fees: Money,
    pub cash_balance: Option<Money>,
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

        out
    }
}

/// Derive deterministic mock top-of-book buy orders from historical snapshots for
/// offline bundled simulations when no live buy orders were collected.
pub fn derive_offline_buy_orders(snapshots: &[PriceSnapshot]) -> Vec<BuyOrder> {
    snapshots
        .iter()
        .filter(|q| matches!(q.platform, Platform::CsFloat | Platform::Skinport) && q.price.0 > 0)
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
    fees: &FeeSchedules,
    peg_config: PegBidConfig,
    relist_config: RelistConfig,
    risk_limits: RiskLimits,
    kill_switch: &KillSwitch,
    starting_cash: Option<Money>,
) -> Result<PegBidDashboard> {
    let mut strategy = PegBidStrategy::new(peg_config)?;
    let mut risk = RiskManager::new(risk_limits, kill_switch.clone())?;
    let sim_config = BuyOrderFillConfig {
        starting_cash,
        ..BuyOrderFillConfig::default()
    };
    let mut simulator = BuyOrderFillSimulator::new(SimulationMode, sim_config, fees, 730)?;
    let mut relist = RelistEngine::new(relist_config)?;

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

    let mut times: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    times.extend(sorted_snapshots.iter().map(|s| s.observed_at));
    times.extend(sorted_sales.iter().map(|s| s.sold_at));
    times.extend(sorted_bids.iter().map(|b| b.observed_at));

    let mut snap_idx = 0usize;
    let mut sale_idx = 0usize;
    let mut bid_idx = 0usize;
    let mut bids_placed = 0usize;
    let mut bids_filled = 0usize;
    let mut last_now = None;

    for now in times {
        // Transition any lots whose 7-day lock expired before `now` so they are
        // listed at `locked_until` before crossing prints at `now` are evaluated.
        for unlock_at in relist.pending_unlocks_before(now) {
            let unlock_step = relist.step(
                &sorted_sales[..sale_idx],
                &sorted_snapshots[..snap_idx],
                fees,
                unlock_at,
            )?;
            for settled in &unlock_step.realized_sales {
                match simulator.settle_relist_sale(
                    &settled.position_id,
                    settled.platform,
                    settled.ask_price,
                    settled.sold_at,
                ) {
                    Ok(report) => {
                        let _ = risk.record_fill(&report.fill)?;
                    }
                    Err(reason) => tracing::warn!(
                        position_id = %settled.position_id,
                        ?reason,
                        "relist settlement rejected by simulator"
                    ),
                }
            }
        }

        last_now = Some(now);

        while snap_idx < sorted_snapshots.len() && sorted_snapshots[snap_idx].observed_at <= now {
            strategy.observe_snapshot(&sorted_snapshots[snap_idx]);
            snap_idx += 1;
        }

        while sale_idx < sorted_sales.len() && sorted_sales[sale_idx].sold_at <= now {
            let sale = &sorted_sales[sale_idx];
            strategy.observe_sale(sale);
            for report in simulator.process_transaction(sale, now) {
                bids_filled += 1;
                let _ = risk.record_fill(&report.fill)?;
                if simulator.is_order_filled(&report.fill.order.id) {
                    strategy.mark_filled(&report.fill.order.id);
                }
                if let Some(pos) = report.position {
                    relist.track_position(pos, report.fill.filled_at);
                }
            }
            sale_idx += 1;
        }

        let bid_start = bid_idx;
        while bid_idx < sorted_bids.len() && sorted_bids[bid_idx].observed_at <= now {
            bid_idx += 1;
        }

        if !kill_switch.is_engaged() && bid_start < bid_idx {
            let batch = &sorted_bids[bid_start..bid_idx];
            let orders = strategy.evaluate(batch, fees, now)?;
            for order in orders {
                if let Err(reason) = risk.check(&order, now) {
                    tracing::debug!(order_id = %order.id, %reason, "peg bid blocked by risk limits");
                    strategy.mark_filled(&order.id);
                    continue;
                }
                if simulator.place_bid(order.clone(), now).is_ok() {
                    bids_placed += 1;
                } else {
                    strategy.mark_filled(&order.id);
                }
            }
            // Cancel any resting simulator bids that the strategy discarded (e.g., ceiling exceeded).
            let active_keys: std::collections::HashSet<_> = strategy
                .active_bids()
                .into_iter()
                .map(|b| (b.order.platform, b.order.item.clone(), b.order.float_bucket))
                .collect();
            for obs in batch {
                let key = (obs.platform, obs.item.clone(), obs.float_bucket);
                if !active_keys.contains(&key) {
                    simulator.cancel_bids_for_item(obs.platform, &obs.item, obs.float_bucket);
                }
            }
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
                    let _ = risk.record_fill(&report.fill)?;
                }
                Err(reason) => tracing::warn!(
                    position_id = %settled.position_id,
                    ?reason,
                    "relist settlement rejected by simulator"
                ),
            }
        }
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
        active_mock_bids,
        locked_inventory,
        ready_to_sell_count: relist.ready_to_sell().len(),
        active_asks_count: relist.active_asks().len(),
        bids_placed,
        bids_filled,
        realized_sales: relist.realized_sales().to_vec(),
        realized_buy_order_pnl: relist.total_realized_pnl(),
        total_seller_fees: relist.total_seller_fees(),
        cash_balance: simulator.cash(),
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

fn format_money(money: Money) -> String {
    let sign = if money.0 < 0 { "-" } else { "" };
    let abs = money.0.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

fn escape_md_cell(value: &str) -> String {
    value.replace('|', "\\|")
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
            &fees(),
            PegBidConfig::default(),
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
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
            &fees(),
            PegBidConfig::default(),
            RelistConfig::default(),
            RiskLimits::default(),
            &KillSwitch::new(),
            Some(Money(50_000)),
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
}
