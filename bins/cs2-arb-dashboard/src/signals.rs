use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Row};

#[derive(Serialize)]
pub struct Watcher {
    observed_at: Option<DateTime<Utc>>,
    windows: Vec<Window>,
    events: Vec<WatcherEvent>,
}

#[derive(Serialize)]
struct Window {
    branch: String,
    closes_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct WatcherEvent {
    kind: String,
    branch: Option<String>,
    at: Option<DateTime<Utc>>,
}

pub async fn watcher(path: &Path) -> Option<Watcher> {
    tokio::time::timeout(Duration::from_secs(2), read_watcher(path))
        .await
        .ok()?
        .ok()
}

async fn read_watcher(path: &Path) -> Result<Watcher, sqlx::Error> {
    let mut db = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_millis(250))
        .connect()
        .await?;
    let observed_at: Option<i64> =
        sqlx::query_scalar("SELECT observed_at FROM app_state WHERE app_id = 730")
            .fetch_optional(&mut db)
            .await?;
    let windows = sqlx::query(
        "SELECT branch, closes_at FROM panic_windows
         WHERE app_id = 730 AND opened_at <= ?1 AND closes_at > ?1
         ORDER BY closes_at LIMIT 12",
    )
    .bind(Utc::now().timestamp_millis())
    .fetch_all(&mut db)
    .await?
    .into_iter()
    .map(|row| Window {
        branch: row.get("branch"),
        closes_at: DateTime::from_timestamp_millis(row.get("closes_at")),
    })
    .collect();
    let events = sqlx::query(
        "SELECT kind, json_extract(payload, '$.branch') AS branch, enqueued_at
         FROM event_queue
         WHERE kind IN ('branch_updated', 'panic_window_opened', 'panic_window_closed')
         ORDER BY id DESC LIMIT 8",
    )
    .fetch_all(&mut db)
    .await?
    .into_iter()
    .map(|row| WatcherEvent {
        kind: row.get("kind"),
        branch: row.get("branch"),
        at: DateTime::from_timestamp_millis(row.get("enqueued_at")),
    })
    .collect();
    Ok(Watcher {
        observed_at: observed_at.and_then(DateTime::from_timestamp_millis),
        windows,
        events,
    })
}

#[derive(Default, Serialize)]
pub struct MarketHealth {
    readable: bool,
    sampled_entries: usize,
    skipped_reads: usize,
    csfloat_skipped_reads: usize,
    csfloat_http_429s: usize,
    csfloat_throttle_signals: usize,
    last_csfloat_signal: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct JournalEntry {
    #[serde(rename = "MESSAGE")]
    message: String,
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    timestamp: String,
}

pub async fn health(game: &str) -> MarketHealth {
    let mut command = tokio::process::Command::new("/usr/bin/journalctl");
    command.kill_on_drop(true).args([
        "--no-pager",
        "--output=json",
        "--output-fields=MESSAGE",
        "--since=1 hour ago",
        "--lines=300",
        "-u",
        &format!("cs2-arb-mock@{game}.service"),
    ]);
    match tokio::time::timeout(Duration::from_secs(2), command.output()).await {
        Ok(Ok(output)) if output.status.success() => parse_health(&output.stdout),
        _ => MarketHealth::default(),
    }
}

fn parse_health(output: &[u8]) -> MarketHealth {
    let mut health = MarketHealth::default();
    for line in output.split(|b| *b == b'\n') {
        let Ok(entry) = serde_json::from_slice::<JournalEntry>(line) else {
            continue;
        };
        health.readable = true;
        health.sampled_entries += 1;
        let message = entry.message.to_ascii_lowercase();
        let skipped = [
            "read-only listing request skipped",
            "read-only sales request skipped",
            "read-only buy-orders request skipped",
        ]
        .iter()
        .any(|pattern| message.contains(pattern));
        if skipped {
            health.skipped_reads += 1;
        }
        if !message.contains("csfloat") {
            continue;
        }
        health.csfloat_skipped_reads += usize::from(skipped);
        let explicit = message
            .contains("http 429 received; endpoint silenced for the full penalty window")
            || message.contains("http 429 received; pausing market host requests");
        let throttle =
            message.contains("rate limited") || message.contains("429 too many requests");
        if explicit || throttle {
            health.csfloat_http_429s += usize::from(explicit);
            health.csfloat_throttle_signals += usize::from(!explicit);
            if let Some(at) = entry
                .timestamp
                .parse::<i64>()
                .ok()
                .and_then(DateTime::from_timestamp_micros)
            {
                health.last_csfloat_signal = health.last_csfloat_signal.max(Some(at));
            }
        }
    }
    health
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_explicit_429s_from_throttled_reads_and_other_venues() {
        let sample = br#"
{"MESSAGE":"2026-11-27T12:00:00.000000Z  WARN cs2_arb_net::limiter: HTTP 429 received; endpoint silenced for the full penalty window endpoint=CsFloat:/api/v1/listings cooldown_secs=60.0","__REALTIME_TIMESTAMP":"1795779000000000"}
{"MESSAGE":"HTTP 429 received; pausing market host requests endpoint=\"csfloat.listings\"","__REALTIME_TIMESTAMP":"1795780000000000"}
{"MESSAGE":"read-only listing request skipped platform=CsFloat error=rate limited","__REALTIME_TIMESTAMP":"1795781000000000"}
{"MESSAGE":"read-only listing request skipped platform=Skinport error=rate limited","__REALTIME_TIMESTAMP":"1795782000000000"}
{"MESSAGE":"read-only sales request skipped platform=CsFloat error=network timeout","__REALTIME_TIMESTAMP":"1795783000000000"}
"#;
        let health = parse_health(sample);
        assert!(health.readable);
        assert_eq!(health.sampled_entries, 5);
        assert_eq!(health.skipped_reads, 3);
        assert_eq!(health.csfloat_skipped_reads, 2);
        assert_eq!(health.csfloat_http_429s, 2);
        assert_eq!(health.csfloat_throttle_signals, 1);
        assert_eq!(health.last_csfloat_signal.unwrap().timestamp(), 1795781000);
        assert!(!parse_health(b"").readable);
    }
}
