mod common;

use std::sync::Arc;

use common::{clock, fees, t0};
use cs2_arb_core::{Game, ItemId, MarketClient, Money, WearTier};
use cs2_arb_markets::{NoopGate, WaxpeerClient};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn detailed_bids_page_past_filtered_depth_on_get() {
    let server = MockServer::start().await;
    let clock = clock();
    let filtered = json!({"name":"Alien Red","game":"rust","price":14000,"amount":1,"filled":0,"filters":{"float_min":0.1},"by":"filtered"});
    let idless =
        json!({"name":"Alien Red","game":"rust","price":12500,"amount":5,"filled":0,"by":"owner"});
    let first_page: Vec<_> = [
        json!({"id":1,"name":"Alien Red","game":"rust","price":13000,"amount":1,"filled":0,"by":"earlier"}),
        json!({"id":2,"name":"Alien Red","game":"rust","price":12900,"amount":1,"filled":0,"by":"owner"}),
        json!({"id":3,"name":"Alien Red","game":"rust","price":12800,"amount":1,"filled":0,"by":"owner"}),
        idless.clone(),
    ].into_iter().chain(std::iter::repeat_n(filtered.clone(), 96)).collect();
    for (skip, rows) in [
        ("0", first_page),
        (
            "100",
            [
                idless,
                json!({"name":"Alien Red","game":"rust","price":12500,"amount":6,"filled":0,"by":"owner"}),
                json!({"id":3,"name":"Alien Red","game":"rust","price":12800,"amount":1,"filled":0,"by":"owner"}),
            ].into_iter().chain(std::iter::repeat_n(filtered.clone(), 97)).collect(),
        ),
        ("200", vec![filtered]),
    ] {
        let reply = ResponseTemplate::new(200).set_body_json(json!({"success":true,"orders":rows}));
        let response_clock = Arc::clone(&clock);
        Mock::given(method("GET"))
            .and(path("/v2/buy-orders"))
            .and(query_param("name", "Alien Red"))
            .and(query_param("game", "rust"))
            .and(query_param("skip", skip))
            .respond_with(move |_: &wiremock::Request| {
                response_clock.advance(chrono::TimeDelta::seconds(3));
                reply.clone()
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let client =
        WaxpeerClient::with_base_url(fees(), Game::Rust, &server.uri(), Arc::new(NoopGate), clock)
            .unwrap()
            .with_detailed_buy_orders(Some("fixture-api-key".into()));
    let item = ItemId {
        market_hash_name: "Alien Red".into(),
        wear_tier: WearTier::NotApplicable,
    };
    let (bids, observed_at) = client.poll_buy_orders(&item).await.unwrap();
    assert_eq!(observed_at, Some(t0() + chrono::TimeDelta::seconds(9)));
    assert_eq!(bids.len(), 5);
    assert_eq!(bids[0].price, Money(1300));
    assert_eq!(bids[0].observed_at, t0() + chrono::TimeDelta::seconds(3));
    assert_eq!(bids[1].price, Money(1290));
    assert_eq!(bids[1].quantity, 1);
    assert_eq!(bids[1].observed_at, t0() + chrono::TimeDelta::seconds(3));
    assert_eq!(bids[2].price, Money(1280));
    assert_eq!(bids[2].observed_at, t0() + chrono::TimeDelta::seconds(6));
    assert_eq!(bids[3].price, Money(1250));
    assert_eq!(bids[3].observed_at, t0() + chrono::TimeDelta::seconds(6));
    assert_eq!(bids[4].price, Money(1250));
    assert_eq!(bids[4].observed_at, t0() + chrono::TimeDelta::seconds(3));
}

#[tokio::test]
async fn snapshots_are_get_only_cached_and_game_specific() {
    let server = MockServer::start().await;
    for (game, code, name, tier) in [
        (
            Game::Cs2,
            "csgo",
            "AK-47 | Redline (Field-Tested)",
            WearTier::FieldTested,
        ),
        (Game::Rust, "rust", "Alien Red", WearTier::NotApplicable),
    ] {
        Mock::given(method("GET"))
            .and(path("/v1/prices/snapshot"))
            .and(query_param("game", code))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "timestamp": t0().timestamp() - 1,
                "items": [
                    {"name": name, "item_id": "1", "price": 10000, "unlock_at": "2035-01-01T00:00:00Z"},
                    {"name": name, "item_id": "2", "price": "10101"},
                    {"name": "Other item", "item_id": "3", "price": 1}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/buy-orders/snapshot"))
            .and(query_param("game", code))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "offers": [{"name": name, "max": 10006}, {"name": name, "max": "10019"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            WaxpeerClient::with_base_url(fees(), game, &server.uri(), Arc::new(NoopGate), clock())
                .unwrap();
        let item = ItemId {
            market_hash_name: name.into(),
            wear_tier: tier,
        };
        let listings = client.list_listings(&item).await.unwrap();
        assert_eq!(listings.len(), 2);
        assert_eq!(listings[0].observed_at.timestamp(), t0().timestamp() - 1);
        let quote = client.price_snapshot(&item).await.unwrap();
        assert_eq!(quote.price, Money(1011));
        assert_eq!(quote.float_bucket, None);
        let bids = client.top_buy_orders(&item).await.unwrap();
        assert_eq!(bids[0].price, Money(1001));
        assert_eq!(bids[0].quantity, 1);
        assert_eq!(client.top_buy_orders(&item).await.unwrap(), bids);
        assert!(client.recent_sales(&item, t0()).await.unwrap().is_empty());
    }
}
