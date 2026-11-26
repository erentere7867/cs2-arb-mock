//! Local abstraction over the in-process **simulated** executor (crates/sim).
//!
//! This crate has no network dependency. The concrete sink is the in-process simulator.

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Fill, Money, Order, OrderSide};

use crate::risk::{RiskManager, RiskRejection};

/// A mock-only order sink that returns simulated fills.
pub trait SimExecutor {
    fn buy_cost_at_limit(&self, order: &Order) -> cs2_arb_core::Result<Money>;

    fn submit(&mut self, order: Order) -> Result<Fill, String>;
}

impl SimExecutor for cs2_arb_sim::MockExecutor {
    fn buy_cost_at_limit(&self, order: &Order) -> cs2_arb_core::Result<Money> {
        Ok(self
            .fees()
            .get(order.platform)
            .buy(order.limit_price, order.quantity)?
            .net)
    }

    fn submit(&mut self, order: Order) -> Result<Fill, String> {
        cs2_arb_sim::MockExecutor::submit(self, order).map_err(|err| err.to_string())
    }
}

impl<E: SimExecutor + ?Sized> SimExecutor for &mut E {
    fn buy_cost_at_limit(&self, order: &Order) -> cs2_arb_core::Result<Money> {
        (**self).buy_cost_at_limit(order)
    }

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
    if order.side == OrderSide::Buy {
        let cost = executor
            .buy_cost_at_limit(&order)
            .map_err(|_| RiskRejection::InvalidOrder("buy cost overflow"))?;
        risk.check_entry_exposure(&order, cost)?;
    }
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
        fee: Money,
    }

    impl SimExecutor for FakeSim {
        fn buy_cost_at_limit(&self, order: &Order) -> cs2_arb_core::Result<Money> {
            order
                .limit_price
                .checked_mul(order.quantity)?
                .checked_add(self.fee)
        }

        fn submit(&mut self, order: Order) -> Result<Fill, String> {
            self.submitted.push(order.clone());
            Ok(Fill {
                quantity: order.quantity,
                unit_price: order.limit_price,
                fee: self.fee,
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

    #[test]
    fn buyer_fees_count_toward_both_exposure_caps_before_submission() {
        let now = Utc.with_ymd_and_hms(2025, 2, 6, 12, 0, 0).unwrap();
        let order = Order {
            id: "fee-bearing-buy".into(),
            side: OrderSide::Buy,
            platform: Platform::DMarket,
            item: ItemId {
                market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
                wear_tier: WearTier::FieldTested,
            },
            float_bucket: None,
            quantity: 2,
            limit_price: Money(500),
            submitted_at: now,
        };
        let mut sim = FakeSim {
            fee: Money(30),
            ..FakeSim::default()
        };
        for (item_cap, total_cap) in [(1_005, 2_000), (2_000, 1_005)] {
            let limits = RiskLimits {
                max_position_notional: Money(1000),
                max_item_exposure: Money(item_cap),
                max_total_exposure: Money(total_cap),
                ..RiskLimits::default()
            };
            let mut risk = RiskManager::new(limits, KillSwitch::new()).unwrap();
            let err = submit_with_risk(&mut risk, &mut sim, order.clone(), now).unwrap_err();
            if item_cap < total_cap {
                assert!(matches!(
                    err,
                    SubmitError::Rejected(RiskRejection::ItemExposureLimit {
                        projected: Money(1030),
                        ..
                    })
                ));
            } else {
                assert!(matches!(
                    err,
                    SubmitError::Rejected(RiskRejection::TotalExposureLimit {
                        projected: Money(1030),
                        ..
                    })
                ));
            }
        }
        assert!(sim.submitted.is_empty());
    }
}
