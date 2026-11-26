use cs2_arb_core::{Error, FeeSchedules, Order, PriceSnapshot, Result};

pub fn evaluate_spread(_quotes: &[PriceSnapshot], _fees: &FeeSchedules) -> Result<Vec<Order>> {
    Err(Error::NotImplemented("cross-market spread strategy"))
}
