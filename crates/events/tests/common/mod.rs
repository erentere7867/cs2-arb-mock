#![allow(dead_code)]

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use cs2_arb_events::{
    parse_steamcmd_info, AppBranchSnapshot, EventQueue, ManualClock, QueueConfig,
};

pub fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path}: {err}"))
}

pub fn snapshot(name: &str) -> AppBranchSnapshot {
    parse_steamcmd_info(&fixture(name), 730).unwrap()
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap()
}

pub async fn open(path: &std::path::Path, clock: &ManualClock, config: QueueConfig) -> EventQueue {
    EventQueue::open_with(path.to_str().unwrap(), config, Arc::new(clock.clone()))
        .await
        .unwrap()
}
