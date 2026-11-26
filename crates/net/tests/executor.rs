//! End-to-end executor tests against a hand-rolled local HTTP server on
//! 127.0.0.1, driven by paused tokio time. No external network is touched.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cs2_arb_core::{Error, Platform};
use cs2_arb_net::{
    CooldownBehavior, CooldownState, HttpExecutor, RateLimit, RateLimiter, RetryPolicy,
    DEFAULT_ENDPOINT,
};
use rand::RngCore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

#[derive(Clone)]
enum Reply {
    Status {
        code: u16,
        headers: Vec<(&'static str, String)>,
        body: &'static str,
    },
    /// Read the request, then drop the connection without answering.
    Disconnect,
}

fn status(code: u16) -> Reply {
    Reply::Status {
        code,
        headers: Vec::new(),
        body: r#"{"ok":true}"#,
    }
}

fn with_retry_after(code: u16, value: &str) -> Reply {
    Reply::Status {
        code,
        headers: vec![("Retry-After", value.to_owned())],
        body: "{}",
    }
}

#[derive(Default)]
struct ServerState {
    script: VecDeque<Reply>,
    log: Vec<(String, String, Instant)>,
}

struct MockServer {
    base: String,
    state: Arc<Mutex<ServerState>>,
}

impl MockServer {
    async fn start(script: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ServerState {
            script: script.into_iter().collect(),
            log: Vec::new(),
        }));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let mut parts = head.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_owned();
                    let path = parts.next().unwrap_or_default().to_owned();
                    let reply = {
                        let mut state = state.lock().unwrap();
                        state.log.push((method, path, Instant::now()));
                        state.script.pop_front().unwrap_or_else(|| status(200))
                    };
                    match reply {
                        Reply::Disconnect => drop(socket),
                        Reply::Status {
                            code,
                            headers,
                            body,
                        } => {
                            let mut response = format!(
                                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n",
                                body.len()
                            );
                            for (name, value) in headers {
                                response.push_str(&format!("{name}: {value}\r\n"));
                            }
                            response.push_str("\r\n");
                            response.push_str(body);
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        }
                    }
                });
            }
        });
        Self { base, state }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn arrivals(&self) -> Vec<(String, Instant)> {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .map(|(_, path, at)| (path.clone(), *at))
            .collect()
    }

    fn count(&self) -> usize {
        self.state.lock().unwrap().log.len()
    }
}

/// RNG returning a constant: `1 << 63` makes every jittered delay exactly half
/// its ceiling.
struct Fixed(u64);

impl RngCore for Fixed {
    fn next_u32(&mut self) -> u32 {
        self.0 as u32
    }
    fn next_u64(&mut self) -> u64 {
        self.0
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        dst.fill(self.0 as u8);
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// Plain-HTTP client without timeouts or idle pooling, so paused tokio time
/// has no client-side timers to auto-advance into.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

fn skinport_limiter() -> Arc<RateLimiter> {
    let mut limiter = RateLimiter::default();
    let limit = RateLimit::new(8, secs(300)).unwrap();
    limiter
        .register_shared(Platform::Skinport, &["items", "/v1/items"], limit)
        .unwrap();
    limiter
        .register_shared(
            Platform::Skinport,
            &["sales_history", "/v1/sales/history"],
            limit,
        )
        .unwrap();
    Arc::new(limiter)
}

fn executor(limiter: &Arc<RateLimiter>, retry: RetryPolicy) -> HttpExecutor {
    HttpExecutor::with_client(
        Platform::Skinport,
        Arc::clone(limiter),
        retry,
        test_client(),
    )
    .with_rng(Fixed(1 << 63))
}

#[tokio::test(start_paused = true)]
async fn a_429_blocks_every_request_for_the_whole_window() {
    let server = MockServer::start([with_retry_after(429, "10")]).await;
    let limiter = skinport_limiter();
    let exec = Arc::new(executor(&limiter, RetryPolicy::default()));

    let t429 = Instant::now();
    match exec.execute(exec.get(server.url("/v1/items"))).await {
        Err(Error::RateLimited { retry_after }) => assert_eq!(retry_after, secs(300)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(matches!(
        exec.cooldown_state("/v1/items").unwrap(),
        CooldownState::Cooldown { .. }
    ));

    // Fail-fast callers get an error and nothing is sent.
    let fail_fast = executor(&limiter, RetryPolicy::default())
        .with_cooldown_behavior(CooldownBehavior::FailFast);
    for _ in 0..3 {
        assert!(matches!(
            fail_fast
                .execute(fail_fast.get(server.url("/v1/items")))
                .await,
            Err(Error::RateLimited { .. })
        ));
        assert!(fail_fast.try_acquire("items").is_err());
    }
    assert_eq!(server.count(), 1);

    // Blocking callers queue up and none of them is sent before the window ends.
    let mut waiters = Vec::new();
    for _ in 0..5 {
        let exec = Arc::clone(&exec);
        let url = server.url("/v1/items");
        waiters.push(tokio::spawn(async move {
            exec.execute(exec.get(url)).await.map(|r| r.status())
        }));
    }
    tokio::time::sleep(secs(299)).await;
    assert_eq!(server.count(), 1, "request sent during cooldown");
    assert!(waiters.iter().all(|w| !w.is_finished()));

    // A different endpoint has its own bucket and cooldown.
    let other = exec
        .execute(exec.get(server.url("/v1/sales/history")))
        .await
        .unwrap();
    assert_eq!(other.status(), 200);

    for waiter in waiters {
        assert_eq!(waiter.await.unwrap().unwrap(), 200);
    }
    let arrivals = server.arrivals();
    let items: Vec<Instant> = arrivals
        .iter()
        .filter(|(path, _)| path == "/v1/items")
        .map(|(_, at)| *at)
        .collect();
    assert_eq!(items.len(), 6);
    for at in &items[1..] {
        assert!(
            *at >= t429 + secs(300),
            "sent {:?} into cooldown",
            *at - t429
        );
    }
    assert_eq!(exec.cooldown_state("items").unwrap(), CooldownState::Normal);
}

#[tokio::test(start_paused = true)]
async fn a_longer_retry_after_extends_the_silence() {
    let server = MockServer::start([with_retry_after(429, "900")]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    let t0 = Instant::now();
    assert!(matches!(
        exec.execute(exec.get(server.url("/v1/items"))).await,
        Err(Error::RateLimited { retry_after }) if retry_after == secs(900)
    ));
    exec.execute(exec.get(server.url("/v1/items")))
        .await
        .unwrap();
    let arrivals = server.arrivals();
    assert_eq!(arrivals.len(), 2);
    assert!(arrivals[1].1 >= t0 + secs(900));
}

#[tokio::test(start_paused = true)]
async fn server_errors_back_off_exponentially_then_succeed() {
    let server = MockServer::start([status(503), status(502), status(500), status(200)]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    let t0 = Instant::now();
    let body: serde_json::Value = exec
        .execute_json(exec.get(server.url("/v1/items")))
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let at: Vec<Duration> = server.arrivals().iter().map(|(_, t)| *t - t0).collect();
    // Half-ceiling jitter: 0.5 s, 1 s, 2 s.
    let ms = Duration::from_millis;
    assert_eq!(at, [ms(0), ms(500), ms(1500), ms(3500)]);
    // Every attempt consumed a token.
    assert_eq!(
        limiter
            .resolve(Platform::Skinport, "items")
            .unwrap()
            .available_tokens(),
        4
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_on_503_is_honoured_as_a_floor() {
    let server = MockServer::start([with_retry_after(503, "7"), status(200)]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    let t0 = Instant::now();
    exec.execute(exec.get(server.url("/v1/items")))
        .await
        .unwrap();
    let arrivals = server.arrivals();
    assert_eq!(arrivals[1].1 - t0, secs(7));
}

#[tokio::test(start_paused = true)]
async fn disconnects_are_retried() {
    let server = MockServer::start([Reply::Disconnect, Reply::Disconnect, status(200)]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    let response = exec
        .execute(exec.get(server.url("/v1/items")))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(server.count(), 3);
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_max_attempts() {
    let server = MockServer::start(std::iter::repeat_n(status(500), 10)).await;
    let limiter = skinport_limiter();
    let retry = RetryPolicy::new(secs(1), secs(300), 3).unwrap();
    let exec = executor(&limiter, retry);
    match exec.execute(exec.get(server.url("/v1/items"))).await {
        Err(Error::Network(message)) => assert!(message.contains("500"), "{message}"),
        other => panic!("expected Network error, got {other:?}"),
    }
    assert_eq!(server.count(), 3);
}

#[tokio::test(start_paused = true)]
async fn token_bucket_paces_requests_and_refills() {
    let server = MockServer::start([]).await;
    let mut limiter = RateLimiter::default();
    limiter
        .register(
            Platform::CsFloat,
            DEFAULT_ENDPOINT,
            RateLimit::new(2, secs(60)).unwrap(),
        )
        .unwrap();
    let limiter = Arc::new(limiter);
    let exec = HttpExecutor::with_client(
        Platform::CsFloat,
        Arc::clone(&limiter),
        RetryPolicy::default(),
        test_client(),
    );
    let t0 = Instant::now();
    for _ in 0..5 {
        exec.execute(exec.get(server.url("/api/v1/listings?limit=50")))
            .await
            .unwrap();
    }
    let at: Vec<Duration> = server.arrivals().iter().map(|(_, t)| *t - t0).collect();
    assert_eq!(at, [secs(0), secs(0), secs(60), secs(60), secs(120)]);
}

#[tokio::test(start_paused = true)]
async fn manual_acquire_and_record_response_share_state_with_execute() {
    let server = MockServer::start([]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    exec.acquire("items").await.unwrap();
    exec.record_response("items", reqwest::StatusCode::TOO_MANY_REQUESTS, None)
        .unwrap();
    let fail_fast = executor(&limiter, RetryPolicy::default())
        .with_cooldown_behavior(CooldownBehavior::FailFast);
    assert!(matches!(
        fail_fast
            .execute(fail_fast.get(server.url("/v1/items")))
            .await,
        Err(Error::RateLimited { .. })
    ));
    assert_eq!(server.count(), 0);
}

#[tokio::test(start_paused = true)]
async fn client_errors_are_not_retried() {
    let server = MockServer::start([status(404), status(404)]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    let response = exec
        .execute(exec.get(server.url("/v1/items")))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(matches!(
        exec.execute_json::<serde_json::Value>(exec.get(server.url("/v1/items")))
            .await,
        Err(Error::Network(_))
    ));
    assert_eq!(server.count(), 2);
}

#[tokio::test(start_paused = true)]
async fn refuses_mutating_methods_and_unknown_endpoints_without_sending() {
    let server = MockServer::start([]).await;
    let limiter = skinport_limiter();
    let exec = executor(&limiter, RetryPolicy::default());
    for request in [
        exec.client().post(server.url("/v1/items")),
        exec.client().put(server.url("/v1/items")),
        exec.client().delete(server.url("/v1/items")),
    ] {
        assert!(matches!(
            exec.execute(request).await,
            Err(Error::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        exec.execute(exec.get(server.url("/v1/unconfigured"))).await,
        Err(Error::InvalidConfig(_))
    ));
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn network_errors_do_not_leak_query_strings() {
    // Port 9 on localhost is (almost certainly) closed: connection refused.
    let mut limiter = RateLimiter::default();
    limiter
        .register(
            Platform::DMarket,
            DEFAULT_ENDPOINT,
            RateLimit::new(10, secs(1)).unwrap(),
        )
        .unwrap();
    let exec = HttpExecutor::with_client(
        Platform::DMarket,
        Arc::new(limiter),
        RetryPolicy::new(Duration::from_millis(1), Duration::from_millis(2), 2).unwrap(),
        test_client(),
    );
    let err = exec
        .execute(exec.get("http://127.0.0.1:9/x?api_key=SECRET123"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Network(_)));
    assert!(!err.to_string().contains("SECRET123"), "{err}");
}
