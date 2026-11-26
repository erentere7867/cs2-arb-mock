//! CS2 branch watcher and durable event queue (Workstream C).
//!
//! Read-only: the watcher only issues HTTP GETs for public Steam metadata and
//! the queue stores [`cs2_arb_core::MarketEvent`] signals for the simulated
//! strategy. Nothing here can place a trade or order.
//!
//! Typical wiring:
//!
//! ```no_run
//! # async fn wire() -> cs2_arb_core::Result<()> {
//! use cs2_arb_events::{EventQueue, SteamCmdApiSource, Watcher, WatcherConfig};
//!
//! let queue = EventQueue::open("data/events.db").await?;
//! let watcher = Watcher::new(SteamCmdApiSource::new()?, queue.clone(), WatcherConfig::default())?;
//! let (_stop, stop_rx) = tokio::sync::watch::channel(false);
//! tokio::spawn(async move { watcher.run(stop_rx).await });
//! while let Some(claim) = queue.claim("strategy").await? {
//!     // ... handle claim.event idempotently ...
//!     queue.ack(&claim).await?;
//! }
//! # Ok(()) }
//! ```

pub mod clock;
pub mod queue;
pub mod source;
pub mod watcher;

pub use clock::{Clock, ManualClock, SystemClock};
pub use queue::{
    ClaimedEvent, EnqueueOptions, Enqueued, EventQueue, EventStatus, NackOutcome, NewTransition,
    PanicWindow, PollOutcome, QueueConfig, QueueStats, QueuedEvent, StrategyTransition,
    WindowAction,
};
pub use source::{
    parse_steamcmd_info, parse_steamdb_rss, AppBranchSnapshot, BranchInfo, BranchInfoSource,
    ScriptedSource, SteamCmdApiSource, SteamDbRssSource, APP_CHANGELIST_BRANCH,
};
pub use watcher::{
    detect_changes, Baseline, BranchChange, BranchChangeKind, Detection, PollSink, Watcher,
    WatcherConfig,
};
