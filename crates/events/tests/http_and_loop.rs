//! HTTP source against a local hand-rolled server (no real network), and the
//! polling loop under paused tokio time.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{fixture, snapshot, t0};
use cs2_arb_core::{Error, Result};
use cs2_arb_events::{
    AppBranchSnapshot, BranchInfoSource, Clock, ManualClock, PollOutcome, PollSink,
    SteamCmdApiSource, SteamDbRssSource, Watcher, WatcherConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
}

/// Serves the scripted replies in order, one per connection, and records
/// each request line.
async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            log.lock().unwrap().push(request);
            let mut response = format!(
                "HTTP/1.1 {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                reply.status,
                reply.body.len()
            );
            for (name, value) in reply.headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&reply.body);
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn steamcmd_source_fetches_parses_and_maps_errors() {
    let (base, seen) = serve(vec![
        Reply {
            status: "200 OK",
            headers: vec![("content-type", "application/json".into())],
            body: fixture("steamcmd_730_captured_2026-11-26.json"),
        },
        Reply {
            status: "429 Too Many Requests",
            headers: vec![("retry-after", "42".into())],
            body: String::new(),
        },
        Reply {
            status: "503 Service Unavailable",
            headers: vec![],
            body: String::new(),
        },
        Reply {
            status: "200 OK",
            headers: vec![],
            body: r#"{"data": {"730": {}}, "status": "success"}"#.into(),
        },
    ])
    .await;
    let source = SteamCmdApiSource::with_base_url(format!("{base}/v1/info/"), 730).unwrap();
    assert_eq!(source.url(), format!("{base}/v1/info/730"));

    let snap = source.fetch().await.unwrap();
    assert_eq!(snap.branches["public"].buildid, 26_879_581);
    assert!(matches!(
        source.fetch().await,
        Err(Error::RateLimited { retry_after }) if retry_after == Duration::from_secs(42)
    ));
    assert!(matches!(source.fetch().await, Err(Error::Network(_))));
    assert!(matches!(source.fetch().await, Err(Error::InvalidData(_))));

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].starts_with("GET /v1/info/730 HTTP/1.1"));
    assert!(requests[0]
        .to_ascii_lowercase()
        .contains("user-agent: cs2-arb-mock/0.1"));
}

#[tokio::test]
async fn steamdb_rss_source_fetches_public_build() {
    let (base, _) = serve(vec![Reply {
        status: "200 OK",
        headers: vec![("content-type", "application/rss+xml".into())],
        body: fixture("steamdb_rss_730_captured_2026-11-26.xml"),
    }])
    .await;
    let source =
        SteamDbRssSource::with_url(format!("{base}/api/PatchnotesRSS/?appid=730"), 730).unwrap();
    let snap = source.fetch().await.unwrap();
    assert_eq!(snap.branches.len(), 1);
    assert_eq!(snap.branches["public"].buildid, 26_879_581);
}

/// Records the (virtual) instant of each fetch and stops the loop when done.
struct TimedSource {
    script: Mutex<VecDeque<Result<AppBranchSnapshot>>>,
    calls: Mutex<Vec<tokio::time::Instant>>,
    stop: watch::Sender<bool>,
}

#[async_trait]
impl BranchInfoSource for TimedSource {
    fn name(&self) -> &str {
        "timed"
    }

    async fn fetch(&self) -> Result<AppBranchSnapshot> {
        self.calls.lock().unwrap().push(tokio::time::Instant::now());
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(result) => result,
            None => {
                self.stop.send_replace(true);
                Err(Error::Network("done".into()))
            }
        }
    }
}

/// In-memory sink: SQLite's worker thread does not cooperate with paused
/// tokio time, so the timing test records polls in memory instead.
struct MemorySink {
    clock: ManualClock,
    polls: Mutex<Vec<AppBranchSnapshot>>,
}

#[async_trait]
impl PollSink for MemorySink {
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.clock.clone())
    }

    async fn record_poll(
        &self,
        snapshot: &AppBranchSnapshot,
        _config: &WatcherConfig,
        _observed_at: DateTime<Utc>,
    ) -> Result<PollOutcome> {
        self.polls.lock().unwrap().push(snapshot.clone());
        Ok(PollOutcome::default())
    }
}

#[tokio::test(start_paused = true)]
async fn run_loop_polls_on_interval_and_backs_off_on_rate_limit() {
    let sink = MemorySink {
        clock: ManualClock::new(t0()),
        polls: Mutex::new(Vec::new()),
    };
    let (stop, stop_rx) = watch::channel(false);
    let source = TimedSource {
        script: Mutex::new(VecDeque::from([
            Ok(snapshot("steamcmd_730_synthetic_1_baseline.json")),
            Err(Error::Network("connection reset".into())),
            Err(Error::RateLimited {
                retry_after: Duration::from_secs(600),
            }),
            Err(Error::RateLimited {
                retry_after: Duration::from_secs(3600),
            }),
            Ok(snapshot("steamcmd_730_synthetic_2_staging_bump.json")),
        ])),
        calls: Mutex::new(Vec::new()),
        stop,
    };
    let config = WatcherConfig {
        poll_interval_secs: 120,
        max_backoff_secs: 900,
        ..WatcherConfig::default()
    };
    let watcher = Watcher::new(source, sink, config).unwrap();
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(24 * 3600), watcher.run(stop_rx))
        .await
        .expect("loop stops when the script is exhausted");

    let calls = watcher.source().calls.lock().unwrap().clone();
    assert_eq!(calls[0], started, "first poll is immediate");
    let gaps: Vec<u64> = calls
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_secs())
        .collect();
    // ok, network error, 600s retry-after, 3600s capped to 900s, ok, exhausted
    assert_eq!(gaps, [120, 120, 600, 900, 120]);
    assert_eq!(watcher.sink().polls.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn run_loop_stops_promptly_on_shutdown() {
    let sink = MemorySink {
        clock: ManualClock::new(t0()),
        polls: Mutex::new(Vec::new()),
    };
    let (stop, stop_rx) = watch::channel(false);
    let (unused, _) = watch::channel(false);
    let source = TimedSource {
        script: Mutex::new(VecDeque::from([Ok(snapshot(
            "steamcmd_730_synthetic_1_baseline.json",
        ))])),
        calls: Mutex::new(Vec::new()),
        stop: unused,
    };
    let watcher = Watcher::new(source, sink, WatcherConfig::default()).unwrap();
    let started = tokio::time::Instant::now();
    tokio::join!(watcher.run(stop_rx), async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        stop.send_replace(true);
    });
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    assert_eq!(watcher.source().calls.lock().unwrap().len(), 1);
}
