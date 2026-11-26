use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, ItemId, Listing, MarketClient, PriceSnapshot, Result, Sale,
};

/// CSFloat listings and sale-history reads.
pub struct CsFloatClient {
    fees: FeeSchedule,
}

impl CsFloatClient {
    pub fn new(fees: FeeSchedule) -> Result<Self> {
        fees.validate()?;
        Ok(Self { fees })
    }
}

#[async_trait]
impl MarketClient for CsFloatClient {
    async fn list_listings(&self, _item: &ItemId) -> Result<Vec<Listing>> {
        Err(Error::NotImplemented("csfloat listings"))
    }

    async fn recent_sales(&self, _item: &ItemId, _since: DateTime<Utc>) -> Result<Vec<Sale>> {
        Err(Error::NotImplemented("csfloat sales history"))
    }

    async fn price_snapshot(&self, _item: &ItemId) -> Result<PriceSnapshot> {
        Err(Error::NotImplemented("csfloat price snapshot"))
    }

    async fn fees(&self) -> Result<FeeSchedule> {
        Ok(self.fees)
    }
}
