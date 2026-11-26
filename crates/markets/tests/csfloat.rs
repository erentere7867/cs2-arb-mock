mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use cs2_arb_core::{Error, MarketClient, Money, Platform};
use cs2_arb_markets::csfloat::{CsFloatClient, CsFloatConfig, CsFloatCredentials};
use cs2_arb_markets::{NoopGate, RequestGate};
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

#[tokio::test]
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
