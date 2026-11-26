use cs2_arb_core::{Error, Order, Result};

pub struct RiskGuard {
    _private: (),
}

impl RiskGuard {
    pub fn allows(&self, _order: &Order) -> Result<bool> {
        Err(Error::NotImplemented(
            "position size, drawdown, and kill switch checks",
        ))
    }
}
