//! Local abstraction over the in-process **simulated** executor (crates/sim).
//!
//! This crate has no network dependency. The concrete sink is the in-process simulator.

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Fill, Order};

use crate::risk::{RiskManager, RiskRejection};

/// A mock-only order sink that returns simulated fills.
pub trait SimExecutor {
    fn submit(&mut self, order: Order) -> Result<Fill, String>;
}

impl SimExecutor for cs2_arb_sim::MockExecutor {
    fn submit(&mut self, order: Order) -> Result<Fill, String> {
        cs2_arb_sim::MockExecutor::submit(self, order).map_err(|err| err.to_string())
    }
}

impl<E: SimExecutor + ?Sized> SimExecutor for &mut E {
    fn submit(&mut self, order: Order) -> Result<Fill, String> {
        (**self).submit(order)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("rejected by risk manager: {0}")]
    Rejected(#[from] RiskRejection),
    #[error("simulated execution failed: {0}")]
    Execution(String),
    #[error("fill accounting failed: {0}")]
    Accounting(#[from] Error),
}

/// Checks `order` against `risk` at `now`, submits it to the simulator, and records the
/// resulting fill in the risk manager.
pub fn submit_with_risk<E: SimExecutor + ?Sized>(
    risk: &mut RiskManager,
    executor: &mut E,
    order: Order,
    now: DateTime<Utc>,
) -> Result<Fill, SubmitError> {
    risk.check(&order, now)?;
    let fill = executor.submit(order).map_err(SubmitError::Execution)?;
    risk.record_fill(&fill)?;
    Ok(fill)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{ItemId, Money, OrderSide, Platform, WearTier};

    use super::*;
    use crate::{KillSwitch, RiskLimits};

    #[derive(Default)]
    struct FakeSim {
        submitted: Vec<Order>,
    }

    impl SimExecutor for FakeSim {
        fn submit(&mut self, order: Order) -> Result<Fill, String> {
            self.submitted.push(order.clone());
            Ok(Fill {
                quantity: order.quantity,
                unit_price: order.limit_price,
                fee: Money(0),
                filled_at: order.submitted_at,
                order,
            })
        }
    }

    #[test]
    fn gate_blocks_before_reaching_simulator() {
        let now = Utc.with_ymd_and_hms(2025, 2, 6, 12, 0, 0).unwrap();
        let ks = KillSwitch::new();
        let mut risk = RiskManager::new(RiskLimits::default(), ks.clone()).unwrap();
        let mut sim = FakeSim::default();
        let order = Order {
            id: "o1".into(),
            side: OrderSide::Buy,
            platform: Platform::DMarket,
            item: ItemId {
                market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
                wear_tier: WearTier::FieldTested,
            },
            float_bucket: None,
            quantity: 1,
            limit_price: Money(2_000),
            submitted_at: now,
        };
        submit_with_risk(&mut risk, &mut sim, order.clone(), now).unwrap();
        assert_eq!(risk.total_exposure(), Money(2_000));
        ks.engage();
        assert!(matches!(
            submit_with_risk(&mut risk, &mut sim, order, now),
            Err(SubmitError::Rejected(RiskRejection::KillSwitch))
        ));
        assert_eq!(sim.submitted.len(), 1);
    }
}
