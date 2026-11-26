//! Loaders for recorded or synthetic replay inputs.

use std::path::Path;

use cs2_arb_core::{Error, FeeSchedules, MarketEvent, PriceSnapshot, Result};

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|err| Error::Storage(format!("reading {}: {err}", path.display())))
}

/// A JSON array of `PriceSnapshot` (the `data/fixtures/price_snapshots.json` format).
pub fn load_snapshots(path: impl AsRef<Path>) -> Result<Vec<PriceSnapshot>> {
    parse_snapshots(&read(path.as_ref())?)
}

pub fn parse_snapshots(source: &str) -> Result<Vec<PriceSnapshot>> {
    serde_json::from_str(source).map_err(|err| Error::InvalidData(err.to_string()))
}

/// A JSON array of `MarketEvent` (the `data/fixtures/market_events.json` format).
pub fn load_events(path: impl AsRef<Path>) -> Result<Vec<MarketEvent>> {
    parse_events(&read(path.as_ref())?)
}

pub fn parse_events(source: &str) -> Result<Vec<MarketEvent>> {
    serde_json::from_str(source).map_err(|err| Error::InvalidData(err.to_string()))
}

/// Fee schedules from a TOML config with a `[fees.*]` table (e.g. `config/default.toml`).
pub fn load_fee_schedules(path: impl AsRef<Path>) -> Result<FeeSchedules> {
    FeeSchedules::from_toml(&read(path.as_ref())?)
}
