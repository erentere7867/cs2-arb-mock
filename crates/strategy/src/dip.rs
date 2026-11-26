use cs2_arb_core::{Error, MarketEvent, Order, PriceSnapshot, Result};

pub fn evaluate_dip(_quotes: &[PriceSnapshot], _events: &[MarketEvent]) -> Result<Vec<Order>> {
    Err(Error::NotImplemented("update-window dip strategy"))
}
