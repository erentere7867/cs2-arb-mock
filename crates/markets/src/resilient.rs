use std::future::Future;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, ItemId, Listing, MarketClient, PriceSnapshot, Result, Sale,
};
use cs2_arb_net::{retry_with_backoff, RetryError, RetryPolicy, TokioClock};
use rand::{rngs::StdRng, SeedableRng};

/// Retries individual reads, so every attempt passes the request gate again and
/// DMarket obtains a fresh signature for each attempt.
pub struct ResilientMarket<M> {
    inner: M,
    policy: RetryPolicy,
}

impl<M> ResilientMarket<M> {
    pub fn new(inner: M, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }

    async fn read<T, F: Future<Output = Result<T>>>(
        &self,
        mut operation: impl FnMut() -> F,
    ) -> Result<T> {
        let mut rng = StdRng::from_os_rng();
        retry_with_backoff(&self.policy, &TokioClock, &mut rng, |_| {
            let read = operation();
            async move {
                read.await.map_err(|err| match err {
                    Error::Network(_) => RetryError::transient(err),
                    _ => RetryError::Permanent(err),
                })
            }
        })
        .await
    }
}

#[async_trait]
impl<M: MarketClient> MarketClient for ResilientMarket<M> {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>> {
        self.read(|| self.inner.list_listings(item)).await
    }

    async fn recent_sales(&self, item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
        self.read(|| self.inner.recent_sales(item, since)).await
    }

    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot> {
        self.read(|| self.inner.price_snapshot(item)).await
    }

    async fn fees(&self) -> Result<FeeSchedule> {
        self.inner.fees().await
    }
}
