#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use cs2_arb_core::{FeeSchedule, ItemId, Money, Result, WearTier};
use cs2_arb_markets::{Endpoint, ManualClock, RequestGate};

pub fn fees() -> FeeSchedule {
    FeeSchedule {
        buyer_fee_pct: 1.5,
        seller_fee_pct: 4.0,
        buyer_fixed_fee: Money(3),
        seller_fixed_fee: Money(0),
        ..FeeSchedule::default()
    }
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0).unwrap()
}

pub fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(t0()))
}

pub fn redline() -> ItemId {
    ItemId {
        market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
        wear_tier: WearTier::FieldTested,
    }
}

#[derive(Default)]
pub struct RecordingGate {
    pub acquired: Mutex<Vec<&'static str>>,
    pub responses: Mutex<Vec<(&'static str, u16, Option<Duration>)>>,
}

#[async_trait]
impl RequestGate for RecordingGate {
    async fn acquire(&self, endpoint: &Endpoint) -> Result<()> {
        self.acquired.lock().unwrap().push(endpoint.name);
        Ok(())
    }

    fn on_response(&self, endpoint: &Endpoint, status: u16, retry_after: Option<Duration>) {
        self.responses
            .lock()
            .unwrap()
            .push((endpoint.name, status, retry_after));
    }
}
