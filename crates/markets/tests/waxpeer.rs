mod common;

use std::sync::Arc;

use common::{clock, fees, t0};
use cs2_arb_core::{Game, ItemId, MarketClient, Money, WearTier};
use cs2_arb_markets::{NoopGate, WaxpeerClient};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
