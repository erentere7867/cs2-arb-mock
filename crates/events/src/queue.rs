use cs2_arb_core::{Error, MarketEvent, Result};

pub struct EventQueue {
    _private: (),
}

impl EventQueue {
    pub async fn open(_sqlite_url: &str) -> Result<Self> {
        Err(Error::NotImplemented("SQLite event queue"))
    }

    pub async fn enqueue(&self, _event: &MarketEvent) -> Result<()> {
        Err(Error::NotImplemented("event enqueue"))
    }
}
