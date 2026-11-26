//! TTL response cache: parsed values in memory, raw bodies optionally mirrored to disk as
//! JSON so a restarted process does not burn rate-limit budget.
//!
//! Memory freshness uses `tokio::time::Instant` (follows paused tokio time); disk freshness
//! uses the injected wall clock, since instants do not survive a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// A cached value plus when it was fetched from the marketplace.
#[derive(Debug, Clone)]
pub struct Cached<T> {
    pub value: T,
    /// Wall-clock fetch time; use it as `observed_at` so staleness stays visible.
    pub fetched_at: DateTime<Utc>,
    /// False when served past its TTL (e.g. stale fallback while rate limited).
    pub fresh: bool,
}

struct Entry<T> {
    value: T,
    fetched_at: DateTime<Utc>,
    expires_at: Instant,
}

#[derive(Serialize, Deserialize)]
struct DiskEntry {
    version: u32,
    key: String,
    fetched_at: DateTime<Utc>,
    body: String,
}

const DISK_VERSION: u32 = 1;

pub struct TtlCache<T> {
    ttl: Duration,
    entries: Mutex<HashMap<String, Entry<T>>>,
    disk_dir: Option<PathBuf>,
    namespace: &'static str,
}

impl<T: Clone> TtlCache<T> {
    pub fn new(namespace: &'static str, ttl: Duration, disk_dir: Option<PathBuf>) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
            disk_dir,
            namespace,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn lookup(&self, key: &str, allow_stale: bool) -> Option<Cached<T>> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        let fresh = Instant::now() < entry.expires_at;
        (fresh || allow_stale).then(|| Cached {
            value: entry.value.clone(),
            fetched_at: entry.fetched_at,
            fresh,
        })
    }

    pub fn get_fresh(&self, key: &str) -> Option<Cached<T>> {
        self.lookup(key, false)
    }

    pub fn get_any(&self, key: &str) -> Option<Cached<T>> {
        self.lookup(key, true)
    }

    /// Inserts a value fetched `age` ago (zero for a value fetched just now).
    pub fn insert(
        &self,
        key: &str,
        value: T,
        fetched_at: DateTime<Utc>,
        age: Duration,
    ) -> Cached<T> {
        let remaining = self.ttl.saturating_sub(age);
        let fresh = !remaining.is_zero();
        let entry = Entry {
            value: value.clone(),
            fetched_at,
            expires_at: Instant::now() + remaining,
        };
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), entry);
        Cached {
            value,
            fetched_at,
            fresh,
        }
    }

    fn disk_path(&self, key: &str) -> Option<PathBuf> {
        let dir = self.disk_dir.as_ref()?;
        let safe: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        // FNV-1a disambiguates keys that sanitize to the same name.
        let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        Some(dir.join(format!(
            "{}-{}-{hash:016x}.json",
            self.namespace,
            truncate(&safe, 80)
        )))
    }

    /// Raw body and fetch time from disk, regardless of age.
    pub async fn load_disk(&self, key: &str) -> Option<(DateTime<Utc>, Vec<u8>)> {
        let path = self.disk_path(key)?;
        let raw = tokio::fs::read(&path).await.ok()?;
        match serde_json::from_slice::<DiskEntry>(&raw) {
            Ok(entry) if entry.version == DISK_VERSION && entry.key == key => {
                Some((entry.fetched_at, entry.body.into_bytes()))
            }
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "ignoring unreadable cache file");
                None
            }
        }
    }

    /// Best-effort atomic write (temp file + rename); failures are logged, not fatal.
    pub async fn store_disk(&self, key: &str, fetched_at: DateTime<Utc>, body: &[u8]) {
        let Some(path) = self.disk_path(key) else {
            return;
        };
        let Ok(body) = String::from_utf8(body.to_vec()) else {
            return;
        };
        let entry = DiskEntry {
            version: DISK_VERSION,
            key: key.to_owned(),
            fetched_at,
            body,
        };
        if let Err(err) = write_atomic(&path, &entry).await {
            tracing::warn!(path = %path.display(), %err, "failed to write cache file");
        }
    }
}

async fn write_atomic(path: &Path, entry: &DiskEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(entry)?).await?;
    tokio::fs::rename(&tmp, path).await
}

fn truncate(s: &str, max: usize) -> &str {
    &s[..s.len().min(max)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test(start_paused = true)]
    async fn expires_and_round_trips_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache: TtlCache<u32> =
            TtlCache::new("t", Duration::from_secs(60), Some(dir.path().to_path_buf()));
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        assert!(cache.insert("k", 7, t0, Duration::ZERO).fresh);
        assert_eq!(cache.get_fresh("k").unwrap().value, 7);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(cache.get_fresh("k").is_none());
        let stale = cache.get_any("k").unwrap();
        assert!(!stale.fresh);
        assert!(!cache.insert("old", 1, t0, Duration::from_secs(61)).fresh);

        cache.store_disk("k?a=1", t0, br#"{"x":1}"#).await;
        let (at, body) = cache.load_disk("k?a=1").await.unwrap();
        assert_eq!(at, t0);
        assert_eq!(body, br#"{"x":1}"#);
        assert!(cache.load_disk("k?a=2").await.is_none());
    }
}
