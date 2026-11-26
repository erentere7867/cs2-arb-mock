use cs2_arb_core::{Error, MarketEvent, Result};

pub async fn poll_cs2_branches() -> Result<Vec<MarketEvent>> {
    Err(Error::NotImplemented("CS2 branch watcher"))
}
