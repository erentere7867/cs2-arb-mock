use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const CS2_APP_ID: u32 = 730;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarketEvent {
    BranchUpdated {
        app_id: u32,
        branch: String,
        change_number: u64,
        observed_at: DateTime<Utc>,
    },
    PanicWindowOpened {
        branch: String,
        opened_at: DateTime<Utc>,
    },
    PanicWindowClosed {
        branch: String,
        closed_at: DateTime<Utc>,
    },
}
