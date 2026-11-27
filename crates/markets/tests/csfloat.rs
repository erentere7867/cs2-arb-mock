mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use cs2_arb_core::{Error, MarketClient, Money, Platform};
use cs2_arb_markets::csfloat::{CsFloatClient, CsFloatConfig, CsFloatCredentials};
use cs2_arb_markets::{NetGate, NoopGate, RequestGate, ResilientMarket};
use cs2_arb_net::{NetConfig, TokioClock};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(
    server: &MockServer,
    creds: Option<CsFloatCredentials>,
    gate: Arc<dyn RequestGate>,
) -> CsFloatClient {
    let cfg = CsFloatConfig {
        base_url: server.uri(),
        request_timeout: None,
        ..CsFloatConfig::default()
    };
    CsFloatClient::with_config(fees(), cfg, creds, gate, clock()).unwrap()
}

fn listing(id: u64, price: i64, float: f64, name: &str, kind: &str) -> serde_json::Value {
    json!({
        "id": id.to_string(),
        "created_at": "2025-06-01T10:00:00.000Z",
        "type": kind,
        "price": price,
        "state": "listed",
        "item": {"asset_id": "1", "market_hash_name": name, "float_value": float, "paint_seed": 1},
        "seller": {"steam_id": "x"}
    })
}

#[tokio::test]
async fn parses_wrapped_listings_with_auth_and_filters() {
    let server = MockServer::start().await;
    let name = "AK-47 | Redline (Field-Tested)";
    Mock::given(method("GET"))
        .and(path("/api/v1/listings"))
        .and(header("authorization", "test-key"))
        .and(query_param("market_hash_name", name))
        .and(query_param("sort_by", "lowest_price"))
        .and(query_param("type", "buy_now"))
        .and(query_param("limit", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                listing(2, 2399, 0.30, name, "buy_now"),
                listing(1, 2250, 0.21, name, "buy_now"),
                listing(3, 100, 0.21, name, "auction"),
                listing(4, 50, 0.21, "AK-47 | Redline (Minimal Wear)", "buy_now"),
                listing(5, 10, 0.05, name, "buy_now")
            ],
            "cursor": "abc"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let creds = CsFloatCredentials::new("test-key").unwrap();
    assert!(!format!("{creds:?}").contains("test-key"));
    let cf = client(&server, Some(creds), Arc::new(NoopGate));
    let listings = cf.list_listings(&redline()).await.unwrap();
    assert_eq!(
        listings
            .iter()
            .map(|l| (l.id.as_str(), l.price))
            .collect::<Vec<_>>(),
        vec![("csfloat:1", Money(2250)), ("csfloat:2", Money(2399))]
    );
    assert_eq!(listings[0].float_value, Some(0.21));
    assert_eq!(listings[0].platform, Platform::CsFloat);
    assert_eq!(listings[0].observed_at, t0());
    let snap = cf.price_snapshot(&redline()).await.unwrap();
    assert_eq!(snap.price, Money(2250));
    assert!(!format!("{cf:?}").contains("test-key"));
}

#[tokio::test]
async fn accepts_bare_array_listings() {
    let server = MockServer::start().await;
    let name = "AK-47 | Redline (Field-Tested)";
    Mock::given(path("/api/v1/listings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([listing(9, 1999, 0.2, name, "buy_now")])),
        )
        .mount(&server)
        .await;
    let cf = client(&server, None, Arc::new(NoopGate));
    let listings = cf.list_listings(&redline()).await.unwrap();
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].price, Money(1999));
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("authorization").is_none());
}

#[tokio::test]
async fn parses_sales_history_since_filter() {
    let server = MockServer::start().await;
    // Path segment is percent-encoded: spaces, '|' and parentheses.
    Mock::given(method("GET"))
        .and(path(
            "/api/v1/history/AK-47%20%7C%20Redline%20%28Field-Tested%29/sales",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "s1", "price": 2300, "sold_at": "2025-06-01T11:00:00Z", "state": "sold",
             "item": {"market_hash_name": "AK-47 | Redline (Field-Tested)", "float_value": 0.22}},
            {"price": 2100, "sold_at": "2025-05-31T09:00:00Z",
             "item": {"market_hash_name": "AK-47 | Redline (Field-Tested)", "float_value": 0.31}},
            {"id": "old", "price": 1000, "sold_at": "2025-01-01T00:00:00Z",
             "item": {"market_hash_name": "AK-47 | Redline (Field-Tested)", "float_value": 0.2}}
        ])))
        .mount(&server)
        .await;
    let cf = client(&server, None, Arc::new(NoopGate));
    let sales = cf
        .recent_sales(&redline(), t0() - chrono::Duration::days(2))
        .await
        .unwrap();
    assert_eq!(sales.len(), 2);
    assert_eq!(sales[0].price, Money(2100));
    assert!(sales[0].id.starts_with("csfloat:sale:"));
    assert_eq!(sales[1].id, "csfloat:s1");
    assert!(sales
        .iter()
        .all(|s| s.verified && s.platform == Platform::CsFloat));
}

#[test]
fn anonymous_sale_identity_survives_a_newer_row() {
    use cs2_arb_markets::csfloat::parse_sales;

    let older = json!({"price": 2100, "sold_at": "2025-05-31T09:00:00Z",
        "item": {"market_hash_name": redline().market_hash_name, "float_value": 0.31}});
    let mut newer = older.clone();
    newer["sold_at"] = json!("2025-06-01T11:00:00Z");
    let since = t0() - chrono::Duration::days(2);
    let before = parse_sales(
        json!([older.clone()]).to_string().as_bytes(),
        &redline(),
        since,
    )
    .unwrap();
    let after = parse_sales(
        json!([newer, older]).to_string().as_bytes(),
        &redline(),
        since,
    )
    .unwrap();
    assert_eq!(before[0].id, after[0].id);
    assert_ne!(after[0].id, after[1].id);
}

#[tokio::test(start_paused = true)]
async fn maps_429_with_ratelimit_reset_and_http_errors() {
    let server = MockServer::start().await;
    let reset = (t0().timestamp() + 45).to_string();
    Mock::given(path("/api/v1/listings"))
        .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", reset.as_str()))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/history/x/sales"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let gate = Arc::new(RecordingGate::default());
    let cf = client(&server, None, gate.clone());
    let err = cf.list_listings(&redline()).await.unwrap_err();
    assert!(
        matches!(err, Error::RateLimited { retry_after } if retry_after == Duration::from_secs(45))
    );
    let item = cs2_arb_core::ItemId {
        market_hash_name: "x".into(),
        wear_tier: cs2_arb_core::WearTier::FactoryNew,
    };
    tokio::time::advance(Duration::from_secs(45)).await;
    assert!(
        matches!(cf.recent_sales(&item, t0()).await, Err(Error::Network(m)) if m.contains("500"))
    );
    assert_eq!(
        gate.responses.lock().unwrap().as_slice(),
        &[
            ("csfloat.listings", 429, Some(Duration::from_secs(45))),
            ("csfloat.sales_history", 500, None)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_silences_all_csfloat_routes_until_expiry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/listings"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "60")
                .set_body_string("retry after 90s"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/buy-orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    let cf = client(&server, None, Arc::new(NoopGate));
    assert!(matches!(
        cf.list_listings(&redline()).await,
        Err(Error::RateLimited { retry_after }) if retry_after == Duration::from_secs(60)
    ));
    for (advance, elapsed) in [(0, 0), (1, 1), (58, 59)] {
        tokio::time::advance(Duration::from_secs(advance)).await;
        assert!(matches!(
            cf.list_listings(&redline()).await,
            Err(Error::RateLimited { retry_after }) if retry_after == Duration::from_secs(60 - elapsed)
        ));
        assert!(matches!(
            cf.top_buy_orders(&redline()).await,
            Err(Error::RateLimited { .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(cf.top_buy_orders(&redline()).await.unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn missing_retry_after_uses_body_then_default() {
    for (body, secs) in [
        (r#"{"message":"Too many requests, retry after 75s"}"#, 75),
        ("no hint", 60),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/listings"))
            .respond_with(ResponseTemplate::new(429).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let gate = Arc::new(RecordingGate::default());
        let cf = client(&server, None, gate.clone());
        let delay = Duration::from_secs(secs);
        assert!(matches!(
            cf.list_listings(&redline()).await,
            Err(Error::RateLimited { retry_after }) if retry_after == delay
        ));
        assert_eq!(
            gate.responses.lock().unwrap().as_slice(),
            &[("csfloat.listings", 429, Some(delay))]
        );
        assert!(matches!(
            cf.list_listings(&redline()).await,
            Err(Error::RateLimited { .. })
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn retries_5xx_but_records_and_does_not_retry_429() {
    let server = MockServer::start().await;
    let attempts = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/api/v1/listings"))
        .respond_with(
            move |_: &wiremock::Request| match attempts.fetch_add(1, Ordering::Relaxed) {
                0 => ResponseTemplate::new(503),
                _ => ResponseTemplate::new(429).insert_header("Retry-After", "0"),
            },
        )
        .expect(2)
        .mount(&server)
        .await;
    let config = NetConfig::from_toml(
        "[rate_limits.csfloat.default]\nmax_requests = 10\nwindow_seconds = 60",
    )
    .unwrap();
    let limiter = Arc::new(config.build_limiter(Arc::new(TokioClock)).unwrap());
    let market = ResilientMarket::new(
        client(&server, None, Arc::new(NetGate(limiter.clone()))),
        cs2_arb_net::RetryPolicy::new(Duration::from_millis(1), Duration::from_millis(1), 4)
            .unwrap(),
    );
    assert!(matches!(
        market.list_listings(&redline()).await,
        Err(Error::RateLimited { .. })
    ));
    assert!(matches!(
        limiter.try_acquire(Platform::CsFloat, "/api/v1/listings"),
        Err(Error::RateLimited { retry_after }) if retry_after > Duration::from_secs(55)
    ));
    server.verify().await;
}

#[tokio::test]
async fn parses_read_only_top_buy_orders_sorted_highest_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/buy-orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                { "market_hash_name": "AK-47 | Redline (Field-Tested)", "price": 2100, "qty": 2 },
                { "market_hash_name": "AK-47 | Redline (Field-Tested)", "price": 2250, "qty": 1 },
                { "market_hash_name": "AWP | Asiimov (Field-Tested)", "price": 9900, "qty": 1 }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let gate = Arc::new(RecordingGate::default());
    let cf = client(&server, None, gate);
    let orders = cf.top_buy_orders(&redline()).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].price, Money(2250));
    assert_eq!(orders[0].quantity, 1);
    assert_eq!(orders[1].price, Money(2100));
    assert_eq!(orders[1].quantity, 2);
    server.verify().await;
}
