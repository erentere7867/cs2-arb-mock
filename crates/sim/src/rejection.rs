use chrono::{DateTime, Utc};
use cs2_arb_core::{FloatBucket, Money, WearTier};
use serde::Serialize;

/// Why the simulator refused to fill an order. Rejected orders change no state
/// other than being appended to the executor's rejection log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Rejection {
    // --- Order validation ---
    #[error("order id {order_id} was already filled")]
    DuplicateOrderId { order_id: String },
    #[error("order quantity must be positive")]
    ZeroQuantity,
    #[error("limit price {limit:?} must be positive")]
    InvalidLimitPrice { limit: Money },
    #[error("order float bucket {bucket:?} is not part of wear tier {tier:?}")]
    OrderWearInconsistent { tier: WearTier, bucket: FloatBucket },

    // --- Liquidity gate ---
    #[error("only {found} verified sales in trailing {window_days}d; {required} required")]
    InsufficientVerifiedSales {
        found: u32,
        required: u32,
        window_days: u32,
    },
    #[error("liquidity cap is zero: {verified_sales} sales over {window_days}d is below one unit of allowed volume")]
    LiquidityCapZero {
        verified_sales: u32,
        window_days: u32,
    },

    // --- Buy-side listing selection ---
    #[error("no visible listings for this item on this platform")]
    NoListings,
    #[error("{candidates} listings exist but none match the order's wear tier and float bucket")]
    WearMismatch { candidates: u32 },
    #[error("every wear-matching listing is trade locked (earliest unlock {earliest_unlock})")]
    ListingTradeLocked { earliest_unlock: DateTime<Utc> },
    #[error("every eligible listing is stale (newest observed {newest_observed_at}, max age {max_age_secs}s)")]
    StaleListing {
        newest_observed_at: DateTime<Utc>,
        max_age_secs: i64,
    },
    #[error("limit {limit:?} is below best eligible ask {best_ask:?}")]
    LimitBelowBestAsk { limit: Money, best_ask: Money },
    #[error("insufficient cash: need {required:?}, have {available:?}")]
    InsufficientFunds { required: Money, available: Money },

    // --- Sell side ---
    #[error("requested {requested} units but only {available} are held")]
    InsufficientInventory { requested: u32, available: u32 },
    #[error("only {unlocked} of {requested} units are tradable; next unlock {earliest_unlock}")]
    PositionTradeLocked {
        requested: u32,
        unlocked: u32,
        earliest_unlock: DateTime<Utc>,
    },
    #[error("sell limit {limit:?} is above market reference {reference:?}")]
    SellLimitAboveMarket { limit: Money, reference: Money },
    #[error("seller fee {unit_fee:?} leaves no proceeds at {unit_price:?}")]
    NonPositiveProceeds { unit_price: Money, unit_fee: Money },

    // --- Arithmetic ---
    #[error("monetary arithmetic overflowed or was invalid")]
    Arithmetic,
}

impl From<cs2_arb_core::Error> for Rejection {
    fn from(_: cs2_arb_core::Error) -> Self {
        Self::Arithmetic
    }
}

/// A rejected order, as stored in the executor's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectionRecord {
    pub order_id: String,
    pub rejected_at: DateTime<Utc>,
    pub rejection: Rejection,
}
