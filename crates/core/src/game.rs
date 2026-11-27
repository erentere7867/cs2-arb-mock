use serde::{Deserialize, Serialize};

use crate::{Platform, WearTier, CS2_APP_ID};

pub const RUST_APP_ID: u32 = 252490;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Game {
    #[default]
    Cs2,
    Rust,
}

impl Game {
    pub const fn app_id(self) -> u32 {
        match self {
            Self::Cs2 => CS2_APP_ID,
            Self::Rust => RUST_APP_ID,
        }
    }

    pub const fn supports_market(self, platform: Platform) -> bool {
        match self {
            Self::Cs2 => true,
            Self::Rust => matches!(platform, Platform::Skinport | Platform::DMarket),
        }
    }

    pub const fn supports_wear(self, tier: WearTier) -> bool {
        match self {
            Self::Cs2 => !matches!(tier, WearTier::NotApplicable),
            Self::Rust => matches!(tier, WearTier::NotApplicable),
        }
    }
}
