use std::time::Duration;

use cs2_arb_core::{Error, MarketEvent, Money, PriceSnapshot, Result, SimulationMode};

pub struct PerformanceReport {
    pub net_pnl: Money,
    pub trade_durations: Vec<Duration>,
    pub fee_drag: Money,
}

pub fn replay(
    _mode: SimulationMode,
    _snapshots: &[PriceSnapshot],
    _events: &[MarketEvent],
) -> Result<PerformanceReport> {
    Err(Error::NotImplemented("historical snapshot replay"))
}
