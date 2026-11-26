//! Branch change detection and the polling loop.
//!
//! Detection is a pure function ([`detect_changes`]) over the persisted
//! baseline and a fresh [`AppBranchSnapshot`]. The [`Watcher`] fetches a
//! snapshot, then [`EventQueue::record_poll`] applies detection, enqueues
//! events and advances the baseline in a single SQLite transaction, so a crash
//! can neither lose nor duplicate a detected change.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{Error, MarketEvent, Result, CS2_APP_ID};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::clock::Clock;
use crate::queue::{EventQueue, PollOutcome};
use crate::source::{AppBranchSnapshot, BranchInfoSource, APP_CHANGELIST_BRANCH};

/// Upper bound for `window_duration_secs` (one year).
pub const MAX_WINDOW_SECS: u64 = 365 * 24 * 60 * 60;

/// Watcher settings. Deserializable from an `[events]` TOML table; every field
/// has a default. Durations are whole seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatcherConfig {
    pub app_id: u32,
    /// Branches whose buildid changes are reported. Empty means all branches.
    pub watched_branches: BTreeSet<String>,
    /// Branches whose changes open a panic window. These are always watched.
    /// May include [`APP_CHANGELIST_BRANCH`] to treat unexplained app
    /// changelist bumps as triggers.
    pub trigger_branches: BTreeSet<String>,
    /// How long a panic window stays open after the latest trigger change.
    pub window_duration_secs: u64,
    pub poll_interval_secs: u64,
    /// Upper bound on the wait after a rate-limit response.
    pub max_backoff_secs: u64,
    /// Report app `_change_number` bumps not explained by any visible branch
    /// as a [`APP_CHANGELIST_BRANCH`] update when private branches exist.
    /// HEURISTIC/UNVERIFIED: PICS changelists also bump for store/config edits.
    pub track_app_changelist: bool,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            app_id: CS2_APP_ID,
            watched_branches: ["public", "staging", "prerelease"]
                .into_iter()
                .map(String::from)
                .collect(),
            trigger_branches: ["staging", "prerelease"]
                .into_iter()
                .map(String::from)
                .collect(),
            // Illustrative; matches the 4-day window in data/fixtures.
            window_duration_secs: 4 * 24 * 60 * 60,
            poll_interval_secs: 120,
            max_backoff_secs: 15 * 60,
            track_app_changelist: true,
        }
    }
}

impl WatcherConfig {
    /// Parse the `[events]` table of a TOML document; a missing table yields defaults.
    pub fn from_toml(source: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            events: Option<WatcherConfig>,
        }
        let root: Root =
            toml::from_str(source).map_err(|err| Error::InvalidConfig(err.to_string()))?;
        let config = root.events.unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.window_duration_secs == 0 || self.poll_interval_secs == 0 {
            return Err(Error::InvalidConfig(
                "events.window_duration_secs and events.poll_interval_secs must be positive".into(),
            ));
        }
        if self.window_duration_secs > MAX_WINDOW_SECS {
            return Err(Error::InvalidConfig(format!(
                "events.window_duration_secs must be at most {MAX_WINDOW_SECS}"
            )));
        }
        Ok(())
    }

    pub fn window_duration(&self) -> TimeDelta {
        let secs = self.window_duration_secs.min(MAX_WINDOW_SECS);
        TimeDelta::seconds(i64::try_from(secs).unwrap_or(0))
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub fn max_backoff(&self) -> Duration {
        Duration::from_secs(self.max_backoff_secs)
    }

    pub fn is_watched(&self, branch: &str) -> bool {
        self.watched_branches.is_empty()
            || self.watched_branches.contains(branch)
            || self.trigger_branches.contains(branch)
    }

    pub fn is_trigger(&self, branch: &str) -> bool {
        self.trigger_branches.contains(branch)
    }
}

/// Last persisted observation for an app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Baseline {
    pub change_number: Option<u64>,
    /// branch name -> last seen buildid.
    pub branches: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchChangeKind {
    /// A known branch's buildid changed (including rollbacks).
    Buildid,
    /// A branch appeared that was not in the baseline.
    NewBranch,
    /// The app changelist advanced without a visible watched-branch change.
    AppChangelist,
}

/// A detected change. `change_number` is the new buildid for branch changes
/// and the app PICS change number for [`BranchChangeKind::AppChangelist`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchChange {
    pub app_id: u32,
    pub branch: String,
    pub kind: BranchChangeKind,
    pub previous: Option<u64>,
    pub change_number: u64,
    /// Steam's own `timeupdated` for the branch, if reported.
    pub steam_time_updated: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub opens_window: bool,
}

impl BranchChange {
    pub fn to_event(&self) -> MarketEvent {
        MarketEvent::BranchUpdated {
            app_id: self.app_id,
            branch: self.branch.clone(),
            change_number: self.change_number,
            observed_at: self.observed_at,
        }
    }
}

/// Result of comparing a snapshot against the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// No baseline existed; the snapshot becomes the baseline silently.
    Baseline,
    /// The snapshot's app change number is older than the baseline (e.g. a
    /// stale proxy cache). It must be ignored and must not move the baseline.
    Stale {
        baseline: u64,
        observed: u64,
    },
    Changes(Vec<BranchChange>),
}

/// Pure change detection.
pub fn detect_changes(
    baseline: Option<&Baseline>,
    snapshot: &AppBranchSnapshot,
    config: &WatcherConfig,
    observed_at: DateTime<Utc>,
) -> Detection {
    let Some(baseline) = baseline else {
        return Detection::Baseline;
    };
    if let (Some(old), Some(new)) = (baseline.change_number, snapshot.change_number) {
        if new < old {
            return Detection::Stale {
                baseline: old,
                observed: new,
            };
        }
    }

    let mut changes = Vec::new();
    for (name, info) in &snapshot.branches {
        if !config.is_watched(name) {
            continue;
        }
        let previous = baseline.branches.get(name).copied();
        let kind = match previous {
            Some(old) if old == info.buildid => continue,
            Some(_) => BranchChangeKind::Buildid,
            None => BranchChangeKind::NewBranch,
        };
        changes.push(BranchChange {
            app_id: snapshot.app_id,
            branch: name.clone(),
            kind,
            previous,
            change_number: info.buildid,
            steam_time_updated: info.time_updated,
            observed_at,
            opens_window: config.is_trigger(name),
        });
    }

    if changes.is_empty() && config.track_app_changelist && snapshot.has_private_branches {
        if let (Some(old), Some(new)) = (baseline.change_number, snapshot.change_number) {
            if new > old {
                changes.push(BranchChange {
                    app_id: snapshot.app_id,
                    branch: APP_CHANGELIST_BRANCH.to_owned(),
                    kind: BranchChangeKind::AppChangelist,
                    previous: Some(old),
                    change_number: new,
                    steam_time_updated: None,
                    observed_at,
                    opens_window: config.is_trigger(APP_CHANGELIST_BRANCH),
                });
            }
        }
    }
    Detection::Changes(changes)
}

/// Where a [`Watcher`] records each fetched snapshot. [`EventQueue`] is the
/// durable implementation; tests and replays may substitute their own.
#[async_trait]
pub trait PollSink: Send + Sync {
    /// Clock used to stamp observations.
    fn clock(&self) -> Arc<dyn Clock>;
    async fn record_poll(
        &self,
        snapshot: &AppBranchSnapshot,
        config: &WatcherConfig,
        observed_at: DateTime<Utc>,
    ) -> Result<PollOutcome>;
}

#[async_trait]
impl PollSink for EventQueue {
    fn clock(&self) -> Arc<dyn Clock> {
        EventQueue::clock(self)
    }

    async fn record_poll(
        &self,
        snapshot: &AppBranchSnapshot,
        config: &WatcherConfig,
        observed_at: DateTime<Utc>,
    ) -> Result<PollOutcome> {
        EventQueue::record_poll(self, snapshot, config, observed_at).await
    }
}

/// Polls a [`BranchInfoSource`] and records results in a [`PollSink`]
/// (normally the [`EventQueue`]).
pub struct Watcher<S, K = EventQueue> {
    source: S,
    sink: K,
    config: WatcherConfig,
    clock: Arc<dyn Clock>,
}

impl<S: BranchInfoSource, K: PollSink> Watcher<S, K> {
    /// Uses the sink's clock for observation timestamps.
    pub fn new(source: S, sink: K, config: WatcherConfig) -> Result<Self> {
        config.validate()?;
        let clock = sink.clock();
        Ok(Self {
            source,
            sink,
            config,
            clock,
        })
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn sink(&self) -> &K {
        &self.sink
    }

    pub fn config(&self) -> &WatcherConfig {
        &self.config
    }

    /// Fetch once and persist detected changes and window events.
    pub async fn poll_once(&self) -> Result<PollOutcome> {
        let snapshot = self.source.fetch().await?;
        if snapshot.app_id != self.config.app_id {
            return Err(Error::InvalidData(format!(
                "source {} returned app {} but watcher is configured for {}",
                self.source.name(),
                snapshot.app_id,
                self.config.app_id
            )));
        }
        let observed_at = self.clock.now();
        let outcome = self
            .sink
            .record_poll(&snapshot, &self.config, observed_at)
            .await?;
        for change in &outcome.changes {
            tracing::info!(
                branch = %change.branch,
                previous = ?change.previous,
                change_number = change.change_number,
                opens_window = change.opens_window,
                "CS2 branch change detected"
            );
        }
        Ok(outcome)
    }

    /// Poll every `poll_interval` until `shutdown` becomes `true` (or its
    /// sender is dropped). Errors are logged and never end the loop; a
    /// rate-limit response delays the next poll by its `retry_after`, capped
    /// at `max_backoff`. Uses `tokio::time`, so it runs under paused time.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let delay = match self.poll_once().await {
                Ok(_) => self.config.poll_interval(),
                Err(Error::RateLimited { retry_after }) => {
                    let wait = retry_after
                        .max(self.config.poll_interval())
                        .min(self.config.max_backoff().max(self.config.poll_interval()));
                    tracing::warn!(
                        source = self.source.name(),
                        ?wait,
                        "branch source rate limited"
                    );
                    wait
                }
                Err(err) => {
                    tracing::warn!(source = self.source.name(), error = %err, "branch poll failed");
                    self.config.poll_interval()
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::BranchInfo;

    fn snapshot(change: u64, branches: &[(&str, u64)]) -> AppBranchSnapshot {
        AppBranchSnapshot {
            app_id: CS2_APP_ID,
            change_number: Some(change),
            has_private_branches: true,
            branches: branches
                .iter()
                .map(|(name, buildid)| {
                    (
                        (*name).to_owned(),
                        BranchInfo {
                            buildid: *buildid,
                            time_updated: None,
                            time_build_updated: None,
                            description: None,
                            password_required: false,
                        },
                    )
                })
                .collect(),
        }
    }

    fn baseline(change: u64, branches: &[(&str, u64)]) -> Baseline {
        Baseline {
            change_number: Some(change),
            branches: branches
                .iter()
                .map(|(n, b)| ((*n).to_owned(), *b))
                .collect(),
        }
    }

    #[test]
    fn config_defaults_and_toml_overrides() {
        let config = WatcherConfig::from_toml("[fees]\nx = 1\n").unwrap();
        assert_eq!(config, WatcherConfig::default());
        let config = WatcherConfig::from_toml(
            "[events]\nwindow_duration_secs = 3600\ntrigger_branches = [\"staging\", \"_changelist\"]\n",
        )
        .unwrap();
        assert_eq!(config.window_duration(), TimeDelta::hours(1));
        assert!(config.is_trigger(APP_CHANGELIST_BRANCH));
        assert!(WatcherConfig::from_toml("[events]\nwindow_duration_secs = 0\n").is_err());
        assert!(WatcherConfig::from_toml("[events]\nbogus = 1\n").is_err());
    }

    #[test]
    fn changelist_heuristic_only_without_visible_change() {
        let config = WatcherConfig::default();
        let now = Utc::now();
        let base = baseline(10, &[("public", 1)]);
        let Detection::Changes(changes) =
            detect_changes(Some(&base), &snapshot(11, &[("public", 1)]), &config, now)
        else {
            panic!("expected changes");
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, BranchChangeKind::AppChangelist);
        assert!(!changes[0].opens_window);

        let Detection::Changes(changes) =
            detect_changes(Some(&base), &snapshot(11, &[("public", 2)]), &config, now)
        else {
            panic!("expected changes");
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].branch, "public");

        let disabled = WatcherConfig {
            track_app_changelist: false,
            ..WatcherConfig::default()
        };
        assert_eq!(
            detect_changes(Some(&base), &snapshot(11, &[("public", 1)]), &disabled, now),
            Detection::Changes(vec![])
        );
    }
}
