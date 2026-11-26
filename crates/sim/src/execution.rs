use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, FeeSchedule, Fill, Listing, Order, Result, Sale, SimulationMode};

pub fn simulate_fill(
    _mode: SimulationMode,
    _order: &Order,
    _listing: &Listing,
    _sales: &[Sale],
    _fees: FeeSchedule,
    _now: DateTime<Utc>,
) -> Result<Fill> {
    Err(Error::NotImplemented("mock order fill"))
}
