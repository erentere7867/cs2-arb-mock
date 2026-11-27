//! SQLite-backed durable event queue, watcher baseline and strategy-state log.
//!
//! # Delivery semantics
//!
//! At-least-once. [`EventQueue::claim`] atomically leases the oldest ready
//! event and increments its attempt counter. The consumer must
//! [`ack`](EventQueue::ack) it before the lease expires; otherwise the event
//! becomes claimable again (including after a process restart). Acks and
//! nacks are fenced by the attempt number, so a consumer whose lease was taken
//! over cannot ack the new claim. After `max_attempts` claims an event is
//! dead-lettered. Consumers must therefore be idempotent per event id.
//!
//! Events may carry an `available_at` in the future (delayed delivery); the
//! watcher uses this to schedule `PanicWindowClosed` durably.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{Error, Game, MarketEvent, PriceSnapshot, Result, Sale};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::clock::{Clock, SystemClock};
use crate::source::AppBranchSnapshot;
use crate::watcher::{detect_changes, Baseline, BranchChange, Detection, WatcherConfig};

fn storage(err: impl std::fmt::Display) -> Error {
    Error::Storage(err.to_string())
}

fn to_millis(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

fn from_millis(millis: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(millis)
        .ok_or_else(|| Error::InvalidData(format!("timestamp {millis} ms is out of range")))
}

fn opt_from_millis(millis: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    millis.map(from_millis).transpose()
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::InvalidData(format!("{value} does not fit in i64")))
}

fn to_u64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidData(format!("{value} is negative")))
}

fn delta(duration: Duration) -> Result<TimeDelta> {
    TimeDelta::from_std(duration)
        .map_err(|_| Error::InvalidConfig(format!("duration {duration:?} is too large")))
}

fn add(at: DateTime<Utc>, by: TimeDelta) -> Result<DateTime<Utc>> {
    at.checked_add_signed(by)
        .ok_or_else(|| Error::InvalidConfig(format!("{at} + {by} is out of range")))
}

fn event_kind(event: &MarketEvent) -> &'static str {
    match event {
        MarketEvent::BranchUpdated { .. } => "branch_updated",
        MarketEvent::PanicWindowOpened { .. } => "panic_window_opened",
        MarketEvent::PanicWindowClosed { .. } => "panic_window_closed",
    }
}

fn payload(event: &MarketEvent) -> Result<String> {
    serde_json::to_string(event).map_err(|err| Error::InvalidData(err.to_string()))
}

/// Queue behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    /// Claims allowed before an event is dead-lettered (minimum 1).
    pub max_attempts: u32,
    /// Lease used by [`EventQueue::claim`].
    pub default_lease: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            default_lease: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Pending,
    Claimed,
    Acked,
    Dead,
}

impl EventStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Acked => "acked",
            Self::Dead => "dead",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "pending" => Self::Pending,
            "claimed" => Self::Claimed,
            "acked" => Self::Acked,
            "dead" => Self::Dead,
            other => return Err(Error::InvalidData(format!("unknown event status {other}"))),
        })
    }
}

/// Options for [`EventQueue::enqueue_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnqueueOptions {
    /// Earliest delivery time; defaults to now.
    pub available_at: Option<DateTime<Utc>>,
    /// If set, a second enqueue with the same key is a no-op returning the
    /// original id.
    pub dedupe_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub id: i64,
    /// `false` when an event with the same dedupe key already existed.
    pub inserted: bool,
}

/// A leased event. Pass it back to `ack`/`nack`/`extend_lease`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedEvent {
    pub id: i64,
    /// 1-based attempt number; also the fencing token for ack/nack.
    pub attempt: u32,
    pub event: MarketEvent,
    pub enqueued_at: DateTime<Utc>,
    pub available_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    /// Error recorded by a previous nack, if any.
    pub last_error: Option<String>,
}

/// A queue row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedEvent {
    pub id: i64,
    pub kind: String,
    /// `None` if the stored payload no longer deserializes.
    pub event: Option<MarketEvent>,
    pub payload: String,
    pub dedupe_key: Option<String>,
    pub status: EventStatus,
    pub attempts: u32,
    pub enqueued_at: DateTime<Utc>,
    pub available_at: DateTime<Utc>,
    pub claimed_by: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub acked_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    pub pending: u64,
    pub claimed: u64,
    pub acked: u64,
    pub dead: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NackOutcome {
    Requeued { available_at: DateTime<Utc> },
    DeadLettered,
}

/// What happened to a panic window during [`EventQueue::record_poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowAction {
    Opened {
        branch: String,
        opened_at: DateTime<Utc>,
        closes_at: DateTime<Utc>,
        open_event_id: i64,
        close_event_id: i64,
    },
    /// A trigger change arrived while the window was open; the scheduled
    /// close was pushed back.
    Extended {
        branch: String,
        closes_at: DateTime<Utc>,
        close_event_id: i64,
    },
}

/// Result of one recorded poll.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PollOutcome {
    /// First observation for this app; stored silently as the baseline.
    pub baseline_initialized: bool,
    /// The snapshot was older than the baseline and was ignored.
    pub stale: bool,
    pub changes: Vec<BranchChange>,
    /// Ids of every event inserted by this poll, in delivery order.
    pub enqueued: Vec<i64>,
    pub windows: Vec<WindowAction>,
}

/// A persisted panic window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicWindow {
    pub app_id: u32,
    pub branch: String,
    pub opened_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub trigger_change: u64,
    pub open_event_id: Option<i64>,
    pub close_event_id: Option<i64>,
}

impl PanicWindow {
    pub fn is_open_at(&self, at: DateTime<Utc>) -> bool {
        self.opened_at <= at && at < self.closes_at
    }
}

/// A strategy state change to append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTransition {
    pub strategy: String,
    /// Must equal the strategy's current state (`None` for the first entry).
    pub from_state: Option<String>,
    pub to_state: String,
    pub reason: String,
    /// Queue event that caused the transition, if any.
    pub event_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyTransition {
    pub id: i64,
    pub strategy: String,
    pub from_state: Option<String>,
    pub to_state: String,
    pub reason: String,
    pub event_id: Option<i64>,
    pub occurred_at: DateTime<Utc>,
}

/// Durable event queue and state store. Cheap to clone (shares the pool).
#[derive(Debug, Clone)]
pub struct EventQueue {
    pool: SqlitePool,
    config: QueueConfig,
    clock: Arc<dyn Clock>,
}

#[derive(Debug, Default)]
pub struct MarketHistory {
    pub snapshots: Vec<PriceSnapshot>,
    pub sales: Vec<Sale>,
}

const QUEUE_COLUMNS: &str = "id, kind, payload, dedupe_key, status, attempts, enqueued_at, \
     available_at, claimed_by, lease_expires_at, acked_at, last_error";

impl EventQueue {
    /// Open (creating if needed) a queue at a file path or `sqlite:` URL with
    /// default settings and the system clock.
    pub async fn open(location: &str) -> Result<Self> {
        Self::open_with(location, QueueConfig::default(), Arc::new(SystemClock)).await
    }

    pub async fn open_with(
        location: &str,
        config: QueueConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let in_memory = location.contains(":memory:") || location.contains("mode=memory");
        let options = if location.starts_with("sqlite:") {
            SqliteConnectOptions::from_str(location).map_err(storage)?
        } else {
            SqliteConnectOptions::new().filename(location)
        }
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
        let (options, pool_options) = if in_memory {
            // Every connection to `:memory:` is a separate database.
            (
                options,
                SqlitePoolOptions::new()
                    .max_connections(1)
                    .idle_timeout(None)
                    .max_lifetime(None),
            )
        } else {
            if let Some(parent) = options
                .get_filename()
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                tokio::fs::create_dir_all(parent).await.map_err(storage)?;
            }
            (
                options.journal_mode(SqliteJournalMode::Wal),
                SqlitePoolOptions::new().max_connections(4),
            )
        };
        let pool = pool_options.connect_with(options).await.map_err(storage)?;
        sqlx::migrate!("./migrations")
            .set_ignore_missing(true)
            .run(&pool)
            .await
            .map_err(storage)?;
        Ok(Self {
            pool,
            config: QueueConfig {
                max_attempts: config.max_attempts.max(1),
                ..config
            },
            clock,
        })
    }

    /// A private in-memory queue (lost on drop).
    pub async fn open_in_memory(config: QueueConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        Self::open_with("sqlite::memory:", config, clock).await
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    pub fn config(&self) -> QueueConfig {
        self.config
    }

    /// Close the pool, flushing WAL state. Further calls fail.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn record_market_history(
        &self,
        game: Game,
        snapshots: &[PriceSnapshot],
        sales: &[Sale],
        retain_since: DateTime<Utc>,
    ) -> Result<()> {
        let app_id = i64::from(game.app_id());
        let cutoff = to_millis(retain_since);
        let mut tx = self.pool.begin().await.map_err(storage)?;
        for quote in snapshots
            .iter()
            .filter(|quote| quote.price.0 > 0 && quote.observed_at >= retain_since)
        {
            sqlx::query("INSERT INTO market_snapshots (app_id, observed_at, payload) VALUES (?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(app_id)
                .bind(to_millis(quote.observed_at))
                .bind(serde_json::to_string(quote).map_err(storage)?)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        for sale in sales
            .iter()
            .filter(|sale| sale.verified && sale.price.0 > 0 && sale.sold_at >= retain_since)
        {
            sqlx::query("INSERT INTO market_sales (app_id, platform, sale_id, sold_at, payload) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(app_id)
                .bind(serde_json::to_string(&sale.platform).map_err(storage)?)
                .bind(&sale.id)
                .bind(to_millis(sale.sold_at))
                .bind(serde_json::to_string(sale).map_err(storage)?)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        sqlx::query("DELETE FROM market_snapshots WHERE app_id = ? AND observed_at < ?")
            .bind(app_id)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        sqlx::query("DELETE FROM market_sales WHERE app_id = ? AND sold_at < ?")
            .bind(app_id)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }

    pub async fn load_market_history(
        &self,
        game: Game,
        from: DateTime<Utc>,
        through: DateTime<Utc>,
    ) -> Result<MarketHistory> {
        let app_id = i64::from(game.app_id());
        let snapshots = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM market_snapshots WHERE app_id = ? AND observed_at BETWEEN ? AND ? ORDER BY observed_at, payload",
        )
        .bind(app_id)
        .bind(to_millis(from))
        .bind(to_millis(through))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let sales = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM market_sales WHERE app_id = ? AND sold_at BETWEEN ? AND ? ORDER BY sold_at, platform, sale_id",
        )
        .bind(app_id)
        .bind(to_millis(from))
        .bind(to_millis(through))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut snapshots: Vec<PriceSnapshot> = snapshots
            .iter()
            .map(|payload| serde_json::from_str(payload).map_err(storage))
            .collect::<Result<_>>()?;
        let mut sales: Vec<Sale> = sales
            .iter()
            .map(|payload| serde_json::from_str(payload).map_err(storage))
            .collect::<Result<_>>()?;
        snapshots.retain(|quote| (from..=through).contains(&quote.observed_at));
        sales.retain(|sale| (from..=through).contains(&sale.sold_at));
        Ok(MarketHistory { snapshots, sales })
    }

    // ----------------------------------------------------------------- queue

    /// Enqueue for immediate delivery; returns the event id.
    pub async fn enqueue(&self, event: &MarketEvent) -> Result<i64> {
        Ok(self
            .enqueue_with(event, EnqueueOptions::default())
            .await?
            .id)
    }

    pub async fn enqueue_with(
        &self,
        event: &MarketEvent,
        options: EnqueueOptions,
    ) -> Result<Enqueued> {
        let now = self.clock.now();
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        insert_event(
            &mut conn,
            event,
            now,
            options.available_at.unwrap_or(now),
            options.dedupe_key.as_deref(),
        )
        .await
    }

    /// Claim the oldest ready event with the default lease.
    pub async fn claim(&self, consumer: &str) -> Result<Option<ClaimedEvent>> {
        self.claim_with_lease(consumer, self.config.default_lease)
            .await
    }

    /// Claim the oldest ready event: pending with `available_at <= now`, or
    /// claimed with an expired lease. Expired claims that already used
    /// `max_attempts` are dead-lettered instead.
    pub async fn claim_with_lease(
        &self,
        consumer: &str,
        lease: Duration,
    ) -> Result<Option<ClaimedEvent>> {
        let lease = delta(lease)?;
        loop {
            let now = self.clock.now();
            let now_ms = to_millis(now);
            let max_attempts = i64::from(self.config.max_attempts);
            sqlx::query(
                "UPDATE event_queue SET status = 'dead', \
                     last_error = COALESCE(last_error, 'lease expired after final attempt') \
                 WHERE status = 'claimed' AND lease_expires_at <= ? AND attempts >= ?",
            )
            .bind(now_ms)
            .bind(max_attempts)
            .execute(&self.pool)
            .await
            .map_err(storage)?;

            let lease_expires_at = add(now, lease)?;
            let row = sqlx::query(
                "UPDATE event_queue SET status = 'claimed', attempts = attempts + 1, \
                     claimed_by = ?, claimed_at = ?, lease_expires_at = ? \
                 WHERE id = ( \
                     SELECT id FROM event_queue \
                     WHERE ((status = 'pending' AND available_at <= ?) \
                         OR (status = 'claimed' AND lease_expires_at <= ?)) \
                       AND attempts < ? \
                     ORDER BY available_at, id LIMIT 1) \
                 RETURNING id, payload, attempts, enqueued_at, available_at, last_error",
            )
            .bind(consumer)
            .bind(now_ms)
            .bind(to_millis(lease_expires_at))
            .bind(now_ms)
            .bind(now_ms)
            .bind(max_attempts)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let id: i64 = row.try_get("id").map_err(storage)?;
            let text: String = row.try_get("payload").map_err(storage)?;
            match serde_json::from_str::<MarketEvent>(&text) {
                Ok(event) => {
                    let attempts: i64 = row.try_get("attempts").map_err(storage)?;
                    return Ok(Some(ClaimedEvent {
                        id,
                        attempt: u32::try_from(attempts).map_err(storage)?,
                        event,
                        enqueued_at: from_millis(row.try_get("enqueued_at").map_err(storage)?)?,
                        available_at: from_millis(row.try_get("available_at").map_err(storage)?)?,
                        lease_expires_at,
                        last_error: row.try_get("last_error").map_err(storage)?,
                    }));
                }
                Err(err) => {
                    tracing::error!(id, error = %err, "dead-lettering undecodable event");
                    sqlx::query(
                        "UPDATE event_queue SET status = 'dead', last_error = ? WHERE id = ?",
                    )
                    .bind(format!("undecodable payload: {err}"))
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(storage)?;
                }
            }
        }
    }

    /// Acknowledge a claim. Fails if the claim was superseded.
    pub async fn ack(&self, claim: &ClaimedEvent) -> Result<()> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        ack_fenced(&mut conn, claim, self.clock.now()).await
    }

    /// Return a claim to the queue after `retry_after`, or dead-letter it if
    /// it has used `max_attempts`.
    pub async fn nack(
        &self,
        claim: &ClaimedEvent,
        error: &str,
        retry_after: Duration,
    ) -> Result<NackOutcome> {
        let now = self.clock.now();
        let (status, available_at, outcome) = if claim.attempt >= self.config.max_attempts {
            ("dead", now, NackOutcome::DeadLettered)
        } else {
            let at = add(now, delta(retry_after)?)?;
            ("pending", at, NackOutcome::Requeued { available_at: at })
        };
        let result = sqlx::query(
            "UPDATE event_queue SET status = ?, available_at = ?, last_error = ?, \
                 claimed_by = NULL, lease_expires_at = NULL \
             WHERE id = ? AND status = 'claimed' AND attempts = ?",
        )
        .bind(status)
        .bind(to_millis(available_at))
        .bind(error)
        .bind(claim.id)
        .bind(i64::from(claim.attempt))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(stale_claim(claim));
        }
        Ok(outcome)
    }

    /// Extend a live claim's lease; returns the new expiry.
    pub async fn extend_lease(
        &self,
        claim: &ClaimedEvent,
        lease: Duration,
    ) -> Result<DateTime<Utc>> {
        let expires = add(self.clock.now(), delta(lease)?)?;
        let result = sqlx::query(
            "UPDATE event_queue SET lease_expires_at = ? \
             WHERE id = ? AND status = 'claimed' AND attempts = ?",
        )
        .bind(to_millis(expires))
        .bind(claim.id)
        .bind(i64::from(claim.attempt))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(stale_claim(claim));
        }
        Ok(expires)
    }

    /// Make every outstanding claim immediately reclaimable (their attempt
    /// count is kept). Only safe when no other process consumes this queue,
    /// e.g. at single-process startup.
    pub async fn release_claims(&self) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE event_queue SET status = 'pending', claimed_by = NULL, \
                 lease_expires_at = NULL WHERE status = 'claimed'",
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: i64) -> Result<Option<QueuedEvent>> {
        let row = sqlx::query(&format!(
            "SELECT {QUEUE_COLUMNS} FROM event_queue WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.as_ref().map(queued_from_row).transpose()
    }

    /// All rows with the given status, oldest first.
    pub async fn list(&self, status: EventStatus) -> Result<Vec<QueuedEvent>> {
        let rows = sqlx::query(&format!(
            "SELECT {QUEUE_COLUMNS} FROM event_queue WHERE status = ? ORDER BY available_at, id"
        ))
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.iter().map(queued_from_row).collect()
    }

    pub async fn dead_letters(&self) -> Result<Vec<QueuedEvent>> {
        self.list(EventStatus::Dead).await
    }

    pub async fn acknowledged_branch_updates_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<MarketEvent>> {
        let rows = sqlx::query(
            "SELECT payload FROM event_queue WHERE status = 'acked' AND kind = 'branch_updated' \
             AND acked_at >= ? ORDER BY id",
        )
        .bind(to_millis(since))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let event = serde_json::from_str(row.get("payload")).ok()?;
                match &event {
                    MarketEvent::BranchUpdated {
                        app_id: cs2_arb_core::CS2_APP_ID,
                        observed_at,
                        ..
                    } if *observed_at > since => Some(event),
                    _ => None,
                }
            })
            .collect())
    }

    /// Move a dead-lettered event back to pending with a fresh attempt budget.
    pub async fn requeue_dead(&self, id: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE event_queue SET status = 'pending', attempts = 0, available_at = ?, \
                 claimed_by = NULL, lease_expires_at = NULL \
             WHERE id = ? AND status = 'dead'",
        )
        .bind(to_millis(self.clock.now()))
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() == 1)
    }

    /// Delete acked events acknowledged before `before`.
    pub async fn purge_acked(&self, before: DateTime<Utc>) -> Result<u64> {
        let result = sqlx::query("DELETE FROM event_queue WHERE status = 'acked' AND acked_at < ?")
            .bind(to_millis(before))
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(result.rows_affected())
    }

    pub async fn stats(&self) -> Result<QueueStats> {
        let rows = sqlx::query("SELECT status, COUNT(*) AS n FROM event_queue GROUP BY status")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        let mut stats = QueueStats::default();
        for row in rows {
            let status: String = row.try_get("status").map_err(storage)?;
            let count = to_u64(row.try_get("n").map_err(storage)?)?;
            match EventStatus::parse(&status)? {
                EventStatus::Pending => stats.pending = count,
                EventStatus::Claimed => stats.claimed = count,
                EventStatus::Acked => stats.acked = count,
                EventStatus::Dead => stats.dead = count,
            }
        }
        Ok(stats)
    }

    // ------------------------------------------------------ watcher baseline

    /// The persisted baseline for `app_id`, if any poll has been recorded.
    pub async fn baseline(&self, app_id: u32) -> Result<Option<Baseline>> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        load_baseline(&mut conn, app_id).await
    }

    /// Detect changes against the persisted baseline, enqueue `BranchUpdated`
    /// and panic-window events, and advance the baseline, atomically.
    ///
    /// For each change with `opens_window`: if that branch's window is open at
    /// `observed_at` its scheduled close moves to `observed_at +
    /// window_duration`; otherwise `PanicWindowOpened` (now) and
    /// `PanicWindowClosed` (delayed to the close time) are enqueued.
    pub async fn record_poll(
        &self,
        snapshot: &AppBranchSnapshot,
        config: &WatcherConfig,
        observed_at: DateTime<Utc>,
    ) -> Result<PollOutcome> {
        config.validate()?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let baseline = load_baseline(&mut tx, snapshot.app_id).await?;
        let mut outcome = PollOutcome::default();
        match detect_changes(baseline.as_ref(), snapshot, config, observed_at) {
            Detection::Stale { baseline, observed } => {
                tracing::warn!(baseline, observed, "ignoring stale branch snapshot");
                outcome.stale = true;
                return Ok(outcome);
            }
            Detection::Baseline => outcome.baseline_initialized = true,
            Detection::Changes(changes) => {
                for change in &changes {
                    let enqueued =
                        insert_event(&mut tx, &change.to_event(), observed_at, observed_at, None)
                            .await?;
                    outcome.enqueued.push(enqueued.id);
                    if change.opens_window {
                        let action = open_or_extend_window(
                            &mut tx,
                            change,
                            config.window_duration(),
                            &mut outcome.enqueued,
                        )
                        .await?;
                        outcome.windows.push(action);
                    }
                }
                outcome.changes = changes;
            }
        }
        store_snapshot(&mut tx, snapshot, observed_at).await?;
        tx.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    pub async fn panic_windows(&self, app_id: u32) -> Result<Vec<PanicWindow>> {
        let rows = sqlx::query(
            "SELECT app_id, branch, opened_at, closes_at, trigger_change, open_event_id, \
                 close_event_id FROM panic_windows WHERE app_id = ? ORDER BY branch",
        )
        .bind(i64::from(app_id))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.iter()
            .map(|row| {
                Ok(PanicWindow {
                    app_id: u32::try_from(row.try_get::<i64, _>("app_id").map_err(storage)?)
                        .map_err(storage)?,
                    branch: row.try_get("branch").map_err(storage)?,
                    opened_at: from_millis(row.try_get("opened_at").map_err(storage)?)?,
                    closes_at: from_millis(row.try_get("closes_at").map_err(storage)?)?,
                    trigger_change: to_u64(row.try_get("trigger_change").map_err(storage)?)?,
                    open_event_id: row.try_get("open_event_id").map_err(storage)?,
                    close_event_id: row.try_get("close_event_id").map_err(storage)?,
                })
            })
            .collect()
    }

    /// Windows open at `at` (per branch).
    pub async fn open_windows_at(
        &self,
        app_id: u32,
        at: DateTime<Utc>,
    ) -> Result<Vec<PanicWindow>> {
        Ok(self
            .panic_windows(app_id)
            .await?
            .into_iter()
            .filter(|window| window.is_open_at(at))
            .collect())
    }

    // ---------------------------------------------------- strategy state log

    /// Append a transition if `from_state` equals the strategy's current
    /// state (compare-and-append); otherwise `Error::InvalidData`.
    pub async fn record_transition(
        &self,
        transition: &NewTransition,
    ) -> Result<StrategyTransition> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let recorded = append_transition(&mut tx, transition, self.clock.now()).await?;
        tx.commit().await.map_err(storage)?;
        Ok(recorded)
    }

    /// Ack `claim` and append `transition` in one transaction, so the state
    /// change is applied exactly once per acknowledged event.
    pub async fn ack_with_transition(
        &self,
        claim: &ClaimedEvent,
        transition: &NewTransition,
    ) -> Result<StrategyTransition> {
        let now = self.clock.now();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        ack_fenced(&mut tx, claim, now).await?;
        let recorded = append_transition(&mut tx, transition, now).await?;
        tx.commit().await.map_err(storage)?;
        Ok(recorded)
    }

    /// Full transition history for `strategy`, oldest first.
    pub async fn transitions(&self, strategy: &str) -> Result<Vec<StrategyTransition>> {
        let rows = sqlx::query(
            "SELECT id, strategy, from_state, to_state, reason, event_id, occurred_at \
             FROM strategy_transitions WHERE strategy = ? ORDER BY id",
        )
        .bind(strategy)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.iter().map(transition_from_row).collect()
    }

    pub async fn current_state(&self, strategy: &str) -> Result<Option<String>> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        current_state(&mut conn, strategy).await
    }
}

fn stale_claim(claim: &ClaimedEvent) -> Error {
    Error::Storage(format!(
        "claim of event {} (attempt {}) is no longer held",
        claim.id, claim.attempt
    ))
}

async fn insert_event(
    conn: &mut SqliteConnection,
    event: &MarketEvent,
    enqueued_at: DateTime<Utc>,
    available_at: DateTime<Utc>,
    dedupe_key: Option<&str>,
) -> Result<Enqueued> {
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO event_queue (kind, payload, dedupe_key, status, attempts, enqueued_at, \
             available_at) VALUES (?, ?, ?, 'pending', 0, ?, ?) \
         ON CONFLICT (dedupe_key) DO NOTHING RETURNING id",
    )
    .bind(event_kind(event))
    .bind(payload(event)?)
    .bind(dedupe_key)
    .bind(to_millis(enqueued_at))
    .bind(to_millis(available_at))
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    if let Some(id) = inserted {
        return Ok(Enqueued { id, inserted: true });
    }
    let id: i64 = sqlx::query_scalar("SELECT id FROM event_queue WHERE dedupe_key = ?")
        .bind(dedupe_key)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
    Ok(Enqueued {
        id,
        inserted: false,
    })
}

async fn ack_fenced(
    conn: &mut SqliteConnection,
    claim: &ClaimedEvent,
    now: DateTime<Utc>,
) -> Result<()> {
    let result = sqlx::query(
        "UPDATE event_queue SET status = 'acked', acked_at = ?, lease_expires_at = NULL \
         WHERE id = ? AND status = 'claimed' AND attempts = ?",
    )
    .bind(to_millis(now))
    .bind(claim.id)
    .bind(i64::from(claim.attempt))
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    if result.rows_affected() != 1 {
        return Err(stale_claim(claim));
    }
    Ok(())
}

async fn load_baseline(conn: &mut SqliteConnection, app_id: u32) -> Result<Option<Baseline>> {
    let app_row = sqlx::query("SELECT change_number FROM app_state WHERE app_id = ?")
        .bind(i64::from(app_id))
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    let Some(app_row) = app_row else {
        return Ok(None);
    };
    let change_number: Option<i64> = app_row.try_get("change_number").map_err(storage)?;
    let rows = sqlx::query("SELECT branch, buildid FROM branch_state WHERE app_id = ?")
        .bind(i64::from(app_id))
        .fetch_all(&mut *conn)
        .await
        .map_err(storage)?;
    let mut baseline = Baseline {
        change_number: change_number.map(to_u64).transpose()?,
        ..Baseline::default()
    };
    for row in rows {
        baseline.branches.insert(
            row.try_get("branch").map_err(storage)?,
            to_u64(row.try_get("buildid").map_err(storage)?)?,
        );
    }
    Ok(Some(baseline))
}

async fn store_snapshot(
    conn: &mut SqliteConnection,
    snapshot: &AppBranchSnapshot,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    let app_id = i64::from(snapshot.app_id);
    let observed = to_millis(observed_at);
    sqlx::query(
        "INSERT INTO app_state (app_id, change_number, observed_at) VALUES (?, ?, ?) \
         ON CONFLICT (app_id) DO UPDATE SET \
             change_number = COALESCE(excluded.change_number, app_state.change_number), \
             observed_at = excluded.observed_at",
    )
    .bind(app_id)
    .bind(snapshot.change_number.map(to_i64).transpose()?)
    .bind(observed)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    // Branches missing from a snapshot keep their last buildid, so a branch
    // that is hidden and reappears unchanged is not reported again.
    for (branch, info) in &snapshot.branches {
        sqlx::query(
            "INSERT INTO branch_state (app_id, branch, buildid, time_updated, observed_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (app_id, branch) DO UPDATE SET buildid = excluded.buildid, \
                 time_updated = excluded.time_updated, observed_at = excluded.observed_at",
        )
        .bind(app_id)
        .bind(branch)
        .bind(to_i64(info.buildid)?)
        .bind(info.time_updated.map(to_millis))
        .bind(observed)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
    }
    Ok(())
}

async fn open_or_extend_window(
    conn: &mut SqliteConnection,
    change: &BranchChange,
    duration: TimeDelta,
    enqueued: &mut Vec<i64>,
) -> Result<WindowAction> {
    let app_id = i64::from(change.app_id);
    let now = change.observed_at;
    let closes_at = add(now, duration)?;
    let close_event = MarketEvent::PanicWindowClosed {
        branch: change.branch.clone(),
        closed_at: closes_at,
    };
    let existing = sqlx::query(
        "SELECT closes_at, close_event_id FROM panic_windows WHERE app_id = ? AND branch = ?",
    )
    .bind(app_id)
    .bind(&change.branch)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;

    if let Some(row) = existing {
        let current_close = from_millis(row.try_get("closes_at").map_err(storage)?)?;
        if current_close > now {
            let closes_at = closes_at.max(current_close);
            let close_event = MarketEvent::PanicWindowClosed {
                branch: change.branch.clone(),
                closed_at: closes_at,
            };
            let close_id: Option<i64> = row.try_get("close_event_id").map_err(storage)?;
            let rescheduled = match close_id {
                Some(id) => {
                    sqlx::query(
                        "UPDATE event_queue SET payload = ?, available_at = ? \
                         WHERE id = ? AND status = 'pending'",
                    )
                    .bind(payload(&close_event)?)
                    .bind(to_millis(closes_at))
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .map_err(storage)?
                    .rows_affected()
                        == 1
                }
                None => false,
            };
            let close_event_id = match (rescheduled, close_id) {
                (true, Some(id)) => id,
                _ => {
                    let inserted = insert_event(conn, &close_event, now, closes_at, None).await?;
                    enqueued.push(inserted.id);
                    inserted.id
                }
            };
            sqlx::query(
                "UPDATE panic_windows SET closes_at = ?, trigger_change = ?, close_event_id = ? \
                 WHERE app_id = ? AND branch = ?",
            )
            .bind(to_millis(closes_at))
            .bind(to_i64(change.change_number)?)
            .bind(close_event_id)
            .bind(app_id)
            .bind(&change.branch)
            .execute(&mut *conn)
            .await
            .map_err(storage)?;
            return Ok(WindowAction::Extended {
                branch: change.branch.clone(),
                closes_at,
                close_event_id,
            });
        }
    }

    let open_event = MarketEvent::PanicWindowOpened {
        branch: change.branch.clone(),
        opened_at: now,
    };
    let open_event_id = insert_event(conn, &open_event, now, now, None).await?.id;
    let close_event_id = insert_event(conn, &close_event, now, closes_at, None)
        .await?
        .id;
    enqueued.push(open_event_id);
    enqueued.push(close_event_id);
    sqlx::query(
        "INSERT INTO panic_windows (app_id, branch, opened_at, closes_at, trigger_change, \
             open_event_id, close_event_id) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (app_id, branch) DO UPDATE SET opened_at = excluded.opened_at, \
             closes_at = excluded.closes_at, trigger_change = excluded.trigger_change, \
             open_event_id = excluded.open_event_id, close_event_id = excluded.close_event_id",
    )
    .bind(app_id)
    .bind(&change.branch)
    .bind(to_millis(now))
    .bind(to_millis(closes_at))
    .bind(to_i64(change.change_number)?)
    .bind(open_event_id)
    .bind(close_event_id)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(WindowAction::Opened {
        branch: change.branch.clone(),
        opened_at: now,
        closes_at,
        open_event_id,
        close_event_id,
    })
}

async fn current_state(conn: &mut SqliteConnection, strategy: &str) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT to_state FROM strategy_transitions WHERE strategy = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(strategy)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)
}

async fn append_transition(
    conn: &mut SqliteConnection,
    transition: &NewTransition,
    now: DateTime<Utc>,
) -> Result<StrategyTransition> {
    let current = current_state(conn, &transition.strategy).await?;
    if current != transition.from_state {
        return Err(Error::InvalidData(format!(
            "strategy {} is in state {:?}, not {:?}",
            transition.strategy, current, transition.from_state
        )));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO strategy_transitions (strategy, from_state, to_state, reason, event_id, \
             occurred_at) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&transition.strategy)
    .bind(&transition.from_state)
    .bind(&transition.to_state)
    .bind(&transition.reason)
    .bind(transition.event_id)
    .bind(to_millis(now))
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(StrategyTransition {
        id,
        strategy: transition.strategy.clone(),
        from_state: transition.from_state.clone(),
        to_state: transition.to_state.clone(),
        reason: transition.reason.clone(),
        event_id: transition.event_id,
        occurred_at: from_millis(to_millis(now))?,
    })
}

fn queued_from_row(row: &SqliteRow) -> Result<QueuedEvent> {
    let payload: String = row.try_get("payload").map_err(storage)?;
    let status: String = row.try_get("status").map_err(storage)?;
    let attempts: i64 = row.try_get("attempts").map_err(storage)?;
    Ok(QueuedEvent {
        id: row.try_get("id").map_err(storage)?,
        kind: row.try_get("kind").map_err(storage)?,
        event: serde_json::from_str(&payload).ok(),
        payload,
        dedupe_key: row.try_get("dedupe_key").map_err(storage)?,
        status: EventStatus::parse(&status)?,
        attempts: u32::try_from(attempts).map_err(storage)?,
        enqueued_at: from_millis(row.try_get("enqueued_at").map_err(storage)?)?,
        available_at: from_millis(row.try_get("available_at").map_err(storage)?)?,
        claimed_by: row.try_get("claimed_by").map_err(storage)?,
        lease_expires_at: opt_from_millis(row.try_get("lease_expires_at").map_err(storage)?)?,
        acked_at: opt_from_millis(row.try_get("acked_at").map_err(storage)?)?,
        last_error: row.try_get("last_error").map_err(storage)?,
    })
}

fn transition_from_row(row: &SqliteRow) -> Result<StrategyTransition> {
    Ok(StrategyTransition {
        id: row.try_get("id").map_err(storage)?,
        strategy: row.try_get("strategy").map_err(storage)?,
        from_state: row.try_get("from_state").map_err(storage)?,
        to_state: row.try_get("to_state").map_err(storage)?,
        reason: row.try_get("reason").map_err(storage)?,
        event_id: row.try_get("event_id").map_err(storage)?,
        occurred_at: from_millis(row.try_get("occurred_at").map_err(storage)?)?,
    })
}
