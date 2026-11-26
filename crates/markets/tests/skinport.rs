mod common;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use cs2_arb_core::{Error, MarketClient, Money, Platform};
use cs2_arb_markets::skinport::{SkinportClient, SkinportConfig};
use cs2_arb_markets::{LimitPolicy, NoopGate, RequestGate, WindowLimit};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn items_body() -> serde_json::Value {
    json!([
        {
            "market_hash_name": "AK-47 | Redline (Field-Tested)",
            "currency": "USD",
            "suggested_price": 25.5,
            "item_page": "https://skinport.com/item/x",
            "market_page": "https://skinport.com/market/x",
            "min_price": 21.37,
            "max_price": 80.0,
            "mean_price": 30.1,
            "median_price": 27.0,
            "quantity": 42,
            "created_at": 1535988253,
            "updated_at": 1748779200
        },
        {
            "market_hash_name": "Operation Breakout Case",
            "currency": "USD",
            "suggested_price": null,
            "item_page": "",
            "market_page": "",
            "min_price": null,
            "max_price": null,
            "mean_price": null,
            "median_price": null,
            "quantity": 0,
            "created_at": 1,
            "updated_at": 2
        }
    ])
}

fn stats(median: f64, volume: u64) -> serde_json::Value {
    json!({"min": median - 1.0, "max": median + 1.0, "avg": median + 0.5, "median": median, "volume": volume})
}

fn sales_body() -> serde_json::Value {
    json!([
        {
            "market_hash_name": "AK-47 | Redline (Field-Tested)",
            "version": null,
            "currency": "USD",
            "item_page": "",
            "market_page": "",
            "last_24_hours": stats(22.0, 5),
            "last_7_days": stats(23.0, 40),
            "last_30_days": stats(24.0, 150),
            "last_90_days": stats(25.0, 400)
        }
    ])
}

fn config(server: &MockServer) -> SkinportConfig {
    SkinportConfig {
        base_url: server.uri(),
        request_timeout: None,
        ..SkinportConfig::default()
    }
}

fn client(config: SkinportConfig) -> SkinportClient {
    SkinportClient::with_config(fees(), config, Arc::new(NoopGate), clock()).unwrap()
}

async fn mount_items(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/items"))
        .and(header("accept-encoding", "br"))
        .and(query_param("app_id", "730"))
        .and(query_param("currency", "USD"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items_body()))
        .mount(server)
        .await;
}

async fn mount_sales(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/sales/history"))
        .and(header("accept-encoding", "br"))
        .and(query_param("app_id", "730"))
        .and(query_param("currency", "USD"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sales_body()))
        .mount(server)
        .await;
}

async fn hits(server: &MockServer, p: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == p)
        .count()
}

#[tokio::test]
async fn parses_items_into_listing_and_snapshot() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    let sp = client(config(&server));
    let item = redline();

    let listings = sp.list_listings(&item).await.unwrap();
    assert_eq!(listings.len(), 1);
    let l = &listings[0];
    assert_eq!(l.platform, Platform::Skinport);
    assert_eq!(l.price, Money(2137));
    assert_eq!(l.quantity, 42);
    assert_eq!(l.item, item);
    assert_eq!(l.observed_at, t0());
    assert_eq!(l.float_value, None);

    let snap = sp.price_snapshot(&item).await.unwrap();
    assert_eq!(snap.price, Money(2137));
    assert_eq!(snap.float_bucket, None);

    let case = cs2_arb_core::ItemId {
        market_hash_name: "Operation Breakout Case".into(),
        wear_tier: cs2_arb_core::WearTier::FactoryNew,
    };
    assert!(sp.list_listings(&case).await.unwrap().is_empty());
    assert!(sp.price_snapshot(&case).await.is_err());
    assert_eq!(sp.fees().await.unwrap(), fees());
    // Everything above was served by one upstream request.
    assert_eq!(hits(&server, "/v1/items").await, 1);
}

#[tokio::test]
async fn maps_sales_history_aggregates_to_unverified_sales() {
    let server = MockServer::start().await;
    mount_sales(&server).await;
    let sp = client(config(&server));
    let item = redline();

    let day = sp
        .recent_sales(&item, t0() - chrono::Duration::hours(6))
        .await
        .unwrap();
    assert_eq!(day.len(), 1);
    assert_eq!(day[0].price, Money(2200));
    assert!(!day[0].verified);
    assert!(day[0].id.contains(":24h:"));

    let month = sp
        .recent_sales(&item, t0() - chrono::Duration::days(20))
        .await
        .unwrap();
    assert_eq!(month[0].price, Money(2400));
    let far = sp
        .recent_sales(&item, t0() - chrono::Duration::days(365))
        .await
        .unwrap();
    assert_eq!(far[0].price, Money(2500));
    assert_eq!(hits(&server, "/v1/sales/history").await, 1);
}

#[tokio::test]
async fn decodes_brotli_responses() {
    let server = MockServer::start().await;
    let raw = serde_json::to_vec(&items_body()).unwrap();
    let mut compressed = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
        w.write_all(&raw).unwrap();
    }
    Mock::given(path("/v1/items"))
        .and(header("accept-encoding", "br"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "br")
                .insert_header("content-type", "application/json")
                .set_body_bytes(compressed),
        )
        .mount(&server)
        .await;
    let sp = client(config(&server));
    assert_eq!(
        sp.price_snapshot(&redline()).await.unwrap().price,
        Money(2137)
    );
}

#[tokio::test(start_paused = true)]
async fn serves_repeat_calls_from_cache_within_ttl() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    let sp = client(config(&server));
    for _ in 0..20 {
        sp.list_listings(&redline()).await.unwrap();
    }
    assert_eq!(hits(&server, "/v1/items").await, 1);
    tokio::time::advance(Duration::from_secs(299)).await;
    sp.list_listings(&redline()).await.unwrap();
    assert_eq!(hits(&server, "/v1/items").await, 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    sp.list_listings(&redline()).await.unwrap();
    assert_eq!(hits(&server, "/v1/items").await, 2);
}

#[tokio::test(start_paused = true)]
async fn enforces_eight_requests_per_five_minutes_per_endpoint() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    mount_sales(&server).await;
    // TTL 0 and no stale fallback: every call is a cache miss that needs a request slot.
    let cfg = SkinportConfig {
        cache_ttl: Duration::ZERO,
        serve_stale_on_limit: false,
        ..config(&server)
    };
    let sp = client(cfg);
    let item = redline();
    for _ in 0..8 {
        sp.list_listings(&item).await.unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
    }
    let err = sp.list_listings(&item).await.unwrap_err();
    let Error::RateLimited { retry_after } = err else {
        panic!("expected RateLimited, got {err:?}");
    };
    // First request was at t=0, now t=80s: slot frees at t=300s.
    assert_eq!(retry_after, Duration::from_secs(220));
    assert_eq!(hits(&server, "/v1/items").await, 8);

    // The sales-history endpoint has its own independent budget.
    for _ in 0..8 {
        sp.recent_sales(&item, common::t0()).await.unwrap();
    }
    assert!(matches!(
        sp.recent_sales(&item, common::t0()).await,
        Err(Error::RateLimited { .. })
    ));
    assert_eq!(hits(&server, "/v1/sales/history").await, 8);

    tokio::time::advance(retry_after).await;
    sp.list_listings(&item).await.unwrap();
    assert_eq!(hits(&server, "/v1/items").await, 9);
    // Only the oldest slot expired, so the next call is limited again.
    assert!(matches!(
        sp.list_listings(&item).await,
        Err(Error::RateLimited { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn wait_policy_sleeps_until_a_slot_frees() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    let cfg = SkinportConfig {
        cache_ttl: Duration::ZERO,
        on_limit: LimitPolicy::Wait,
        ..config(&server)
    };
    let sp = client(cfg);
    let start = tokio::time::Instant::now();
    for _ in 0..9 {
        sp.list_listings(&redline()).await.unwrap();
    }
    assert!(start.elapsed() >= Duration::from_secs(300));
    assert_eq!(hits(&server, "/v1/items").await, 9);
}

#[tokio::test(start_paused = true)]
async fn serves_stale_cache_when_rate_limited() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    let cfg = SkinportConfig {
        cache_ttl: Duration::from_secs(10),
        items_limit: WindowLimit {
            max_requests: 1,
            window: Duration::from_secs(300),
        },
        ..config(&server)
    };
    let sp = client(cfg);
    let fresh = sp.items().await.unwrap();
    assert!(fresh.fresh);
    tokio::time::advance(Duration::from_secs(11)).await;
    let stale = sp.items().await.unwrap();
    assert!(!stale.fresh);
    assert_eq!(stale.fetched_at, fresh.fetched_at);
    assert_eq!(hits(&server, "/v1/items").await, 1);
}

#[tokio::test(start_paused = true)]
async fn server_429_penalizes_endpoint_and_reports_to_gate() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/items"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_items(&server).await;
    let gate = Arc::new(RecordingGate::default());
    let sp = SkinportClient::with_config(
        fees(),
        config(&server),
        gate.clone() as Arc<dyn RequestGate>,
        clock(),
    )
    .unwrap();
    let err = sp.items().await.unwrap_err();
    assert!(
        matches!(err, Error::RateLimited { retry_after } if retry_after == Duration::from_secs(120))
    );
    assert_eq!(
        gate.responses.lock().unwrap().as_slice(),
        &[("skinport.items", 429, Some(Duration::from_secs(120)))]
    );
    // Local limiter now blocks the endpoint for the server's window without a request.
    let err = sp.items().await.unwrap_err();
    assert!(
        matches!(err, Error::RateLimited { retry_after } if retry_after == Duration::from_secs(120))
    );
    assert_eq!(hits(&server, "/v1/items").await, 1);
    tokio::time::advance(Duration::from_secs(120)).await;
    sp.items().await.unwrap();
    assert_eq!(hits(&server, "/v1/items").await, 2);
    assert_eq!(gate.acquired.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn disk_cache_survives_client_restart() {
    let server = MockServer::start().await;
    mount_items(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = SkinportConfig {
        cache_dir: Some(dir.path().to_path_buf()),
        ..config(&server)
    };
    let shared_clock = clock();
    let first = SkinportClient::with_config(
        fees(),
        cfg.clone(),
        Arc::new(NoopGate),
        shared_clock.clone(),
    )
    .unwrap();
    first.list_listings(&redline()).await.unwrap();
    assert_eq!(hits(&server, "/v1/items").await, 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    // A new client within the TTL (wall clock +2 min) is served from disk.
    shared_clock.advance(chrono::Duration::minutes(2));
    let second = SkinportClient::with_config(
        fees(),
        cfg.clone(),
        Arc::new(NoopGate),
        shared_clock.clone(),
    )
    .unwrap();
    let listings = second.list_listings(&redline()).await.unwrap();
    assert_eq!(listings[0].price, Money(2137));
    assert_eq!(listings[0].observed_at, common::t0());
    assert_eq!(hits(&server, "/v1/items").await, 1);

    // Past the TTL the disk entry is stale and a fresh request is made.
    shared_clock.advance(chrono::Duration::minutes(4));
    let third =
        SkinportClient::with_config(fees(), cfg, Arc::new(NoopGate), shared_clock.clone()).unwrap();
    let listings = third.list_listings(&redline()).await.unwrap();
    assert_eq!(
        listings[0].observed_at,
        common::t0() + chrono::Duration::minutes(6)
    );
    assert_eq!(hits(&server, "/v1/items").await, 2);
}

#[tokio::test]
async fn rejects_non_usd_and_mismatched_items() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"market_hash_name": "AK-47 | Redline (Field-Tested)", "currency": "EUR", "min_price": 1.0, "quantity": 1}
        ])))
        .mount(&server)
        .await;
    let sp = client(config(&server));
    assert!(matches!(
        sp.list_listings(&redline()).await,
        Err(Error::InvalidData(_))
    ));
    let wrong = cs2_arb_core::ItemId {
        wear_tier: cs2_arb_core::WearTier::FactoryNew,
        ..redline()
    };
    assert!(matches!(
        sp.list_listings(&wrong).await,
        Err(Error::InvalidData(_))
    ));
}
