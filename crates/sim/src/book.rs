//! In-memory market data the simulator fills against.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use cs2_arb_core::{ItemId, Listing, Platform, Sale};

type Key = (Platform, String);

/// Listings and sales fed in from read-only market clients or fixtures.
///
/// Simulated buys consume listing quantity. The consumption is remembered per
/// `(platform, listing id)` so a later snapshot that still shows the listing
/// (the real market never saw our mock buy) cannot be bought twice.
#[derive(Debug, Clone, Default)]
pub struct MarketBook {
    listings: HashMap<Key, Listing>,
    consumed: HashMap<Key, u32>,
    sales: HashMap<ItemId, Vec<Sale>>,
    sale_keys: HashSet<Key>,
}

impl MarketBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a listing (keyed by platform and listing id). Quantity
    /// already consumed by simulated fills is subtracted.
    pub fn upsert_listing(&mut self, mut listing: Listing) {
        let key = (listing.platform, listing.id.clone());
        if let Some(consumed) = self.consumed.get(&key) {
            listing.quantity = listing.quantity.saturating_sub(*consumed);
        }
        self.listings.insert(key, listing);
    }

    pub fn upsert_listings(&mut self, listings: impl IntoIterator<Item = Listing>) {
        for listing in listings {
            self.upsert_listing(listing);
        }
    }

    /// Replace the full snapshot for one `(platform, item)`: listings not present
    /// in `listings` are removed. Listings for other platforms/items are kept.
    pub fn replace_listings(
        &mut self,
        platform: Platform,
        item: &ItemId,
        listings: impl IntoIterator<Item = Listing>,
    ) {
        self.listings
            .retain(|_, listing| !(listing.platform == platform && &listing.item == item));
        self.upsert_listings(listings);
    }

    pub fn remove_listing(&mut self, platform: Platform, id: &str) -> Option<Listing> {
        self.listings.remove(&(platform, id.to_owned()))
    }

    pub fn listing(&self, platform: Platform, id: &str) -> Option<&Listing> {
        self.listings.get(&(platform, id.to_owned()))
    }

    /// All listings, in unspecified order.
    pub fn listings(&self) -> impl Iterator<Item = &Listing> {
        self.listings.values()
    }

    /// Record a sale; returns false if `(platform, id)` was already recorded.
    pub fn record_sale(&mut self, sale: Sale) -> bool {
        if !self.sale_keys.insert((sale.platform, sale.id.clone())) {
            return false;
        }
        self.sales.entry(sale.item.clone()).or_default().push(sale);
        true
    }

    /// Record many sales; returns how many were new.
    pub fn record_sales(&mut self, sales: impl IntoIterator<Item = Sale>) -> usize {
        sales
            .into_iter()
            .map(|sale| usize::from(self.record_sale(sale)))
            .sum()
    }

    /// Drop sales strictly older than `cutoff` (keeps memory bounded in long replays).
    pub fn prune_sales_before(&mut self, cutoff: DateTime<Utc>) {
        for sales in self.sales.values_mut() {
            sales.retain(|sale| {
                let keep = sale.sold_at >= cutoff;
                if !keep {
                    self.sale_keys.remove(&(sale.platform, sale.id.clone()));
                }
                keep
            });
        }
        self.sales.retain(|_, sales| !sales.is_empty());
    }

    /// Sales recorded for an item (all platforms), in insertion order.
    pub fn sales_for(&self, item: &ItemId) -> &[Sale] {
        self.sales.get(item).map_or(&[], Vec::as_slice)
    }

    /// Units of a listing already taken by simulated fills.
    pub fn consumed_quantity(&self, platform: Platform, id: &str) -> u32 {
        self.consumed
            .get(&(platform, id.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn listings_for<'a>(
        &'a self,
        platform: Platform,
        item: &'a ItemId,
    ) -> impl Iterator<Item = &'a Listing> + 'a {
        self.listings
            .values()
            .filter(move |listing| listing.platform == platform && &listing.item == item)
    }

    pub(crate) fn consume(&mut self, platform: Platform, id: &str, quantity: u32) {
        let key = (platform, id.to_owned());
        if let Some(listing) = self.listings.get_mut(&key) {
            listing.quantity = listing.quantity.saturating_sub(quantity);
        }
        let consumed = self.consumed.entry(key).or_insert(0);
        *consumed = consumed.saturating_add(quantity);
    }
}
