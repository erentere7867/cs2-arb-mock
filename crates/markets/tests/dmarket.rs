mod common;

use std::sync::Arc;

use common::*;
use cs2_arb_core::{MarketClient, Money, Platform};
use cs2_arb_markets::dmarket::{string_to_sign, DMarketClient, DMarketConfig, DMarketCredentials};
use cs2_arb_markets::NoopGate;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SEED_HEX: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";
const PUBLIC_HEX: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";

fn client(server: &MockServer) -> DMarketClient {
    let creds =
        DMarketCredentials::from_hex(PUBLIC_HEX, &format!("{SEED_HEX}{PUBLIC_HEX}")).unwrap();
    let cfg = DMarketConfig {
        base_url: server.uri(),
        request_timeout: None,
        ..DMarketConfig::default()
    };
    DMarketClient::with_config(fees(), cfg, creds, Arc::new(NoopGate), clock()).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Verifies the request the way DMarket does: decoded path + raw query + body + date.
fn assert_signed(req: &Request) {
    let key = req.headers.get("x-api-key").unwrap().to_str().unwrap();
    assert_eq!(key, PUBLIC_HEX);
    let date = req.headers.get("x-sign-date").unwrap().to_str().unwrap();
    assert_eq!(date, t0().timestamp().to_string());
    let sign = req.headers.get("x-request-sign").unwrap().to_str().unwrap();
    let sig_hex = sign.strip_prefix("dmar ed25519 ").expect("prefix");
    let message = string_to_sign(
        req.method.as_str(),
        req.url.path(),
        req.url.query().unwrap_or(""),
        std::str::from_utf8(&req.body).unwrap(),
        date.parse().unwrap(),
    );
    let vk = VerifyingKey::from_bytes(&hex(PUBLIC_HEX).try_into().unwrap()).unwrap();
    let sig = Signature::from_bytes(&hex(sig_hex).try_into().unwrap());
    vk.verify(message.as_bytes(), &sig)
        .expect("signature verifies");
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
