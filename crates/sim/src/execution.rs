//! Stateless convenience wrapper kept for the scaffold's original entry point.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, FeeSchedule, Fill, Listing, Order, Result, Sale, SimulationMode};

use crate::clock::ManualClock;
use crate::config::SimConfig;
use crate::executor::MockExecutor;
use crate::fees::PlatformFees;

/// Simulate one order against a single listing and a sales history with
/// default [`SimConfig`], applying `fees` on every platform, at time `now`.
///
/// A fresh executor with an empty portfolio is used, so sells always fail with
/// `InsufficientInventory`; use [`MockExecutor`] for anything stateful.
pub fn simulate_fill(
    mode: SimulationMode,
    order: &Order,
    listing: &Listing,
    sales: &[Sale],
    fees: FeeSchedule,
    now: DateTime<Utc>,
) -> Result<Fill> {
    let config = SimConfig::default();
    let fees = PlatformFees::uniform(fees, config.fee_rounding)?;
    let mut executor =
        MockExecutor::with_platform_fees(mode, config, fees, Arc::new(ManualClock::new(now)), 0)?;
    executor.book_mut().upsert_listing(listing.clone());
    executor.book_mut().record_sales(sales.iter().cloned());
    executor
        .submit(order.clone())
        .map_err(|rejection| Error::InvalidData(format!("simulated order rejected: {rejection}")))
}
