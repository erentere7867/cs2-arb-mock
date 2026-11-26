mod common;

use std::sync::Arc;

use common::*;
use cs2_arb_core::{MarketClient, Money, Platform};
use cs2_arb_markets::dmarket::{string_to_sign, DMarketClient, DMarketConfig, DMarketCredentials};
use cs2_arb_markets::NoopGate;
use ed25519_dalek::{Signature, SigningKey, Verifier};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[test]
fn last_sale_identity_survives_a_newer_row() {
    use cs2_arb_markets::dmarket::parse_last_sales;

    let older = json!({"price": "1.00", "date": "1748779198"});
    let newer = json!({"price": "2.00", "date": "1748779199"});
    let since = t0() - chrono::Duration::days(1);
    let before = parse_last_sales(
        json!({"sales": [older.clone()]}).to_string().as_bytes(),
        &redline(),
        since,
    )
    .unwrap();
    let after = parse_last_sales(
        json!({"sales": [newer, older]}).to_string().as_bytes(),
        &redline(),
        since,
    )
    .unwrap();
    assert_eq!(before[0].id, after[0].id);
    assert_ne!(after[0].id, after[1].id);
    let mut another_item = redline();
    another_item.market_hash_name = "A different Field-Tested skin".into();
    let same_sale_shape = json!({"sales": [{"price": "1.00", "date": "1748779198"}]});
    let another =
        parse_last_sales(same_sale_shape.to_string().as_bytes(), &another_item, since).unwrap();
    assert_ne!(before[0].id, another[0].id);
}

const FIXTURE_SEED: [u8; 32] = [42; 32];

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn client(server: &MockServer) -> DMarketClient {
    client_with_gate(server, Arc::new(NoopGate), clock())
}

fn client_with_gate(
    server: &MockServer,
    gate: Arc<dyn cs2_arb_markets::RequestGate>,
    clock: Arc<cs2_arb_markets::ManualClock>,
) -> DMarketClient {
    let signing_key = SigningKey::from_bytes(&FIXTURE_SEED);
    let public = hex_encode(&signing_key.verifying_key().to_bytes());
    let private = hex_encode(&FIXTURE_SEED);
    let creds = DMarketCredentials::from_hex(&public, &format!("{private}{public}")).unwrap();
    let cfg = DMarketConfig {
        base_url: server.uri(),
        request_timeout: None,
        ..DMarketConfig::default()
    };
    DMarketClient::with_config(fees(), cfg, creds, gate, clock).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Verifies the request the way DMarket does: decoded path + raw query + body + date.
fn assert_signed(req: &Request) {
    assert_signed_at(req, t0().timestamp());
}

fn assert_signed_at(req: &Request, timestamp: i64) {
    let key = req.headers.get("x-api-key").unwrap().to_str().unwrap();
    let vk = SigningKey::from_bytes(&FIXTURE_SEED).verifying_key();
    assert_eq!(key, hex_encode(&vk.to_bytes()));
    let date = req.headers.get("x-sign-date").unwrap().to_str().unwrap();
    assert_eq!(date, timestamp.to_string());
    let sign = req.headers.get("x-request-sign").unwrap().to_str().unwrap();
    let sig_hex = sign.strip_prefix("dmar ed25519 ").expect("prefix");
    let message = string_to_sign(
        req.method.as_str(),
        req.url.path(),
        req.url.query().unwrap_or(""),
        std::str::from_utf8(&req.body).unwrap(),
        date.parse().unwrap(),
    );
    let sig = Signature::from_bytes(&hex(sig_hex).try_into().unwrap());
    vk.verify(message.as_bytes(), &sig)
        .expect("signature verifies");
}

#[tokio::test]
async fn signs_after_quota_admission() {
    use cs2_arb_markets::{Endpoint, ManualClock, RequestGate};
    struct WaitingGate(Arc<ManualClock>);

    #[async_trait::async_trait]
    impl RequestGate for WaitingGate {
        async fn acquire(&self, _: &Endpoint) -> cs2_arb_core::Result<()> {
            self.0.advance(chrono::Duration::minutes(3));
            Ok(())
        }

        fn on_response(&self, _: &Endpoint, _: u16, _: Option<std::time::Duration>) {}
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trade-aggregator/v1/last-sales"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sales": []})))
        .mount(&server)
        .await;
    let clock = clock();
    let client = client_with_gate(&server, Arc::new(WaitingGate(clock.clone())), clock);
    client.recent_sales(&redline(), t0()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_signed_at(&requests[0], t0().timestamp() + 180);
}

fn offer(
    id: &str,
    cents: &str,
    title: &str,
    float: &str,
    locked: bool,
    unlock: &str,
) -> serde_json::Value {
    json!({
        "attributes": {
            "title": title,
            "tradable": true,
            "tradeLockDays": 0,
            "unlockDate": unlock,
            "cs2": {"exterior": "EXTERIOR_FIELD_TESTED", "float": float}
        },
        "priceCents": cents,
        "createdAt": "2025-05-30T10:30:00Z",
        "locked": locked,
        "offerId": id,
        "discountPercent": 0,
        "offerType": "dmarket"
    })
}

#[tokio::test]
async fn parses_signed_offers() {
    let server = MockServer::start().await;
    let name = "AK-47 | Redline (Field-Tested)";
    Mock::given(method("GET"))
        .and(path("/marketplace-api/v2/offers"))
        .and(query_param("gameId", "a8db"))
        .and(query_param("title", name))
        .and(query_param("orderBy", "price"))
        .and(query_param("orderDir", "asc"))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                offer("o2", "2450", name, "0.2356003224849701", false, "2025-06-08T00:00:00Z"),
                offer("o1", "2299", name, "0.1601", false, "2025-05-01T00:00:00Z"),
                offer("locked", "100", name, "0.2", true, ""),
                offer("prefix", "50", "AK-47 | Redline (Field-Tested) Souvenir", "0.2", false, ""),
                offer("badfloat", "60", name, "0.01", false, "")
            ],
            "total": "5",
            "cursor": ""
        })))
        .mount(&server)
        .await;
    let dm = client(&server);
    let listings = dm.list_listings(&redline()).await.unwrap();
    assert_eq!(
        listings
            .iter()
            .map(|l| (l.id.as_str(), l.price))
            .collect::<Vec<_>>(),
        vec![("dmarket:o1", Money(2299)), ("dmarket:o2", Money(2450))]
    );
    assert_eq!(listings[0].trade_locked_until, None);
    assert_eq!(
        listings[1].trade_locked_until,
        Some("2025-06-08T00:00:00Z".parse().unwrap())
    );
    assert_eq!(listings[1].float_value, Some(0.2356003224849701));
    assert_eq!(listings[0].platform, Platform::DMarket);
    let snap = dm.price_snapshot(&redline()).await.unwrap();
    assert_eq!(snap.price, Money(2299));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for req in &requests {
        assert_eq!(req.method.as_str(), "GET");
        assert!(req
            .url
            .query()
            .unwrap()
            .contains("title=AK-47%20%7C%20Redline%20%28Field-Tested%29"));
        assert_signed(req);
    }
}

#[tokio::test]
async fn parses_signed_last_sales() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trade-aggregator/v1/last-sales"))
        .and(query_param("gameId", "a8db"))
        .and(query_param("title", "AK-47 | Redline (Field-Tested)"))
        .and(query_param("limit", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sales": [
                {"price": "23.10", "date": (t0().timestamp() - 3600).to_string(), "txOperationType": "Offer",
                 "offerAttributes": {"floatValue": 0.25, "paintSeed": 1}, "orderAttributes": {}},
                {"price": "22.99", "date": (t0().timestamp() - 7200).to_string(), "txOperationType": "Target",
                 "offerAttributes": {"floatValue": null}, "orderAttributes": {"isAdvanced": false}},
                {"price": "5.00", "date": (t0().timestamp() - 86400 * 30).to_string(), "txOperationType": "Offer",
                 "offerAttributes": {"floatValue": 0.2}}
            ]
        })))
        .mount(&server)
        .await;
    let dm = client(&server);
    let sales = dm
        .recent_sales(&redline(), t0() - chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(
        sales.iter().map(|s| s.price).collect::<Vec<_>>(),
        vec![Money(2299), Money(2310)]
    );
    assert_eq!(sales[1].float_value, Some(0.25));
    assert_eq!(sales[0].float_value, None);
    assert!(sales.iter().all(|s| s.verified));
    assert_ne!(sales[0].id, sales[1].id);
    assert_signed(&server.received_requests().await.unwrap()[0]);
    assert_eq!(dm.fees().await.unwrap(), fees());
}

#[tokio::test]
async fn pages_full_sales_results_until_the_cutoff() {
    let server = MockServer::start().await;
    let since = t0() - chrono::Duration::days(7);
    for offset in [0, 20, 40, 60] {
        let query = if offset == 0 {
            query_param("limit", "20")
        } else {
            query_param("offset", offset.to_string())
        };
        let sales: Vec<_> = (offset..offset + 20)
            .map(|i| {
                json!({"price": "1.00", "date": if i < 71 {
                    (t0().timestamp() - i).to_string()
                } else {
                    (since.timestamp() - i).to_string()
                }})
            })
            .collect();
        let mut request = Mock::given(method("GET"))
            .and(path("/trade-aggregator/v1/last-sales"))
            .and(query);
        if offset == 0 {
            request =
                request.and(|req: &Request| !req.url.query_pairs().any(|(key, _)| key == "offset"));
        }
        request
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sales": sales})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let sales = client(&server)
        .recent_sales(&redline(), since)
        .await
        .unwrap();
    assert_eq!(sales.len(), 71);
    for request in server.received_requests().await.unwrap() {
        assert_signed(&request);
    }
}
