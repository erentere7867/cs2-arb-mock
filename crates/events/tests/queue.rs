//! Durable queue semantics, persistence across reopen, and the transition log.

mod common;

use std::time::Duration;

use chrono::TimeDelta;
use common::{open, snapshot, t0};
use cs2_arb_core::{
    BuyOrder, BuyOrderPoll, EmptyListingPoll, Error, FloatBucket, Game, ItemId, MarketEvent, Money,
    Platform, PriceSnapshot, Sale, WearTier,
};
use cs2_arb_events::{
    Clock, EnqueueOptions, EventQueue, EventStatus, ManualClock, NackOutcome, NewTransition,
    QueueConfig, QueueStats, WatcherConfig,
};

fn branch_event(change_number: u64) -> MarketEvent {
    MarketEvent::BranchUpdated {
        app_id: 730,
        branch: "staging".into(),
        change_number,
        observed_at: t0(),
    }
}

fn config() -> QueueConfig {
    QueueConfig {
        max_attempts: 3,
        default_lease: Duration::from_secs(30),
    }
}

#[tokio::test]
async fn queue_creates_missing_database_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data/nested/events.db");
    EventQueue::open(path.to_str().unwrap())
        .await
        .unwrap()
        .close()
        .await;
    assert!(path.is_file());

    let path = dir.path().join("another/nested/events.db");
    let url = format!("sqlite://{}", path.display());
    EventQueue::open(&url).await.unwrap().close().await;
    assert!(path.is_file());
}

#[tokio::test]
async fn queue_reopens_with_an_unknown_applied_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let location = path.to_str().unwrap();
    EventQueue::open(location).await.unwrap().close().await;

    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (99999999999999, 'future migration', TRUE, X'', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    EventQueue::open(location).await.unwrap().close().await;
}

#[tokio::test]
async fn market_history_survives_reopen_and_prunes_by_game() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let now = t0() + TimeDelta::nanoseconds(123_456);
    let quote = PriceSnapshot {
        item: ItemId {
            market_hash_name: "Sample (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        },
        platform: Platform::DMarket,
        float_bucket: Some(FloatBucket::FieldTestedLow),
        price: Money(1234),
        observed_at: now - TimeDelta::days(7),
    };
    let old_quote = PriceSnapshot {
        observed_at: now - TimeDelta::days(11),
        ..quote.clone()
    };
    let sale = Sale {
        id: "same-id".into(),
        platform: quote.platform,
        item: quote.item.clone(),
        price: quote.price,
        float_value: Some(0.18),
        sold_at: quote.observed_at,
        verified: true,
    };
    let other_venue = Sale {
        platform: Platform::CsFloat,
        ..sale.clone()
    };
    let old_sale = Sale {
        id: "older".into(),
        sold_at: old_quote.observed_at,
        ..sale.clone()
    };
    let unverified = Sale {
        id: "aggregate".into(),
        verified: false,
        ..sale.clone()
    };
    let rust_quote = PriceSnapshot {
        item: ItemId {
            market_hash_name: "Alien Red".into(),
            wear_tier: WearTier::NotApplicable,
        },
        float_bucket: None,
        ..old_quote.clone()
    };
    let rust_sale = Sale {
        item: rust_quote.item.clone(),
        float_value: None,
        sold_at: rust_quote.observed_at,
        ..sale.clone()
    };
    let poll = BuyOrderPoll {
        at: quote.observed_at,
        platform: quote.platform,
        item: quote.item.clone(),
        bids: vec![BuyOrder {
            platform: quote.platform,
            item: quote.item.clone(),
            float_bucket: quote.float_bucket,
            price: quote.price,
            quantity: 2,
            observed_at: quote.observed_at,
        }],
    };
    let old_poll = BuyOrderPoll {
        at: old_quote.observed_at,
        bids: Vec::new(),
        ..poll.clone()
    };
    let empty = EmptyListingPoll {
        at: old_quote.observed_at,
        platform: quote.platform,
        item: quote.item.clone(),
    };
    let start = now - TimeDelta::days(12);
    let queue = EventQueue::open(path.to_str().unwrap()).await.unwrap();
    for _ in 0..2 {
        queue
            .record_market_history(
                Game::Cs2,
                &[quote.clone(), old_quote.clone()],
                &[
                    sale.clone(),
                    other_venue.clone(),
                    old_sale.clone(),
                    unverified.clone(),
                ],
                &[poll.clone(), old_poll.clone()],
                std::slice::from_ref(&empty),
                start,
            )
            .await
            .unwrap();
    }
    queue
        .record_market_history(
            Game::Rust,
            std::slice::from_ref(&rust_quote),
            std::slice::from_ref(&rust_sale),
            &[],
            &[],
            start,
        )
        .await
        .unwrap();
    queue.close().await;

    let queue = EventQueue::open(path.to_str().unwrap()).await.unwrap();
    let history = queue
        .load_market_history(Game::Cs2, start, now)
        .await
        .unwrap();
    assert!(history.bid_polls.is_empty() && history.empty_listing_polls.is_empty());
    let history = queue
        .load_market_replay(Game::Cs2, start, now)
        .await
        .unwrap();
    assert_eq!(history.empty_listing_polls, vec![empty]);
    assert_eq!(history.snapshots, vec![old_quote, quote.clone()]);
    assert_eq!(
        history.sales,
        vec![old_sale, other_venue.clone(), sale.clone()]
    );
    assert_eq!(history.bid_polls, vec![old_poll, poll.clone()]);
    let before = quote.observed_at - TimeDelta::nanoseconds(1);
    let history = queue
        .load_market_history(Game::Cs2, before, before)
        .await
        .unwrap();
    assert!(history.snapshots.is_empty() && history.sales.is_empty());
    queue
        .record_market_history(Game::Cs2, &[], &[], &[], &[], now - TimeDelta::days(10))
        .await
        .unwrap();
    let history = queue
        .load_market_replay(Game::Cs2, start, now)
        .await
        .unwrap();
    assert_eq!(history.snapshots, vec![quote]);
    assert_eq!(history.sales, vec![other_venue, sale]);
    assert_eq!(history.bid_polls, vec![poll.clone()]);
    queue.close().await;

    let reader = EventQueue::open_read_only(path.to_str().unwrap())
        .await
        .unwrap();
    let history = reader
        .load_market_replay(Game::Cs2, start, now)
        .await
        .unwrap();
    assert_eq!(history.bid_polls, vec![poll]);
    assert!(reader.enqueue(&branch_event(9)).await.is_err());
    reader.close().await;
    let queue = EventQueue::open(path.to_str().unwrap()).await.unwrap();
    let history = queue
        .load_market_history(Game::Rust, start, now)
        .await
        .unwrap();
    assert_eq!(history.snapshots, vec![rust_quote]);
    assert_eq!(history.sales, vec![rust_sale]);
}

#[tokio::test]
async fn queue_survives_reopen_with_at_least_once_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let clock = ManualClock::new(t0());

    let (acked_id, inflight_id, pending_id) = {
        let queue = open(&path, &clock, config()).await;
        let acked_id = queue.enqueue(&branch_event(1)).await.unwrap();
        let inflight_id = queue.enqueue(&branch_event(2)).await.unwrap();
        let pending_id = queue.enqueue(&branch_event(3)).await.unwrap();

        let first = queue.claim("worker-a").await.unwrap().unwrap();
        assert_eq!((first.id, first.attempt), (acked_id, 1));
        queue.ack(&first).await.unwrap();
        // Claimed but never acked: simulates a crash mid-processing.
        let second = queue.claim("worker-a").await.unwrap().unwrap();
        assert_eq!(second.id, inflight_id);
        queue.close().await;
        (acked_id, inflight_id, pending_id)
    };

    let queue = open(&path, &clock, config()).await;
    assert_eq!(
        queue.stats().await.unwrap(),
        QueueStats {
            pending: 1,
            claimed: 1,
            acked: 1,
            dead: 0
        }
    );
    // The in-flight lease is still held, so the pending event comes first.
    let claim = queue.claim("worker-b").await.unwrap().unwrap();
    assert_eq!(claim.id, pending_id);
    queue.ack(&claim).await.unwrap();
    assert!(queue.claim("worker-b").await.unwrap().is_none());

    // After the lease expires the un-acked event is redelivered.
    clock.advance(TimeDelta::seconds(31));
    let redelivered = queue.claim("worker-b").await.unwrap().unwrap();
    assert_eq!(redelivered.id, inflight_id);
    assert_eq!(redelivered.attempt, 2);
    assert_eq!(redelivered.event, branch_event(2));
    queue.ack(&redelivered).await.unwrap();

    assert!(queue.claim("worker-b").await.unwrap().is_none());
    let acked = queue.get(acked_id).await.unwrap().unwrap();
    assert_eq!(acked.status, EventStatus::Acked);
    assert_eq!(acked.attempts, 1);
    queue.close().await;

    // A third open sees everything acked; nothing is redelivered.
    let queue = open(&path, &clock, config()).await;
    assert_eq!(queue.stats().await.unwrap().acked, 3);
    clock.advance(TimeDelta::hours(1));
    assert!(queue.claim("worker-c").await.unwrap().is_none());
}

#[tokio::test]
async fn release_claims_makes_inflight_events_available_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let clock = ManualClock::new(t0());
    let queue = open(&path, &clock, config()).await;
    let id = queue.enqueue(&branch_event(1)).await.unwrap();
    queue.claim("old-process").await.unwrap().unwrap();
    queue.close().await;

    let queue = open(&path, &clock, config()).await;
    assert!(queue.claim("new-process").await.unwrap().is_none());
    assert_eq!(queue.release_claims().await.unwrap(), 1);
    let claim = queue.claim("new-process").await.unwrap().unwrap();
    assert_eq!((claim.id, claim.attempt), (id, 2));
}

#[tokio::test]
async fn watcher_baseline_and_windows_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let clock = ManualClock::new(t0());
    let watcher_config = WatcherConfig::default();
    let first = snapshot("steamcmd_730_synthetic_1_baseline.json");
    let second = snapshot("steamcmd_730_synthetic_2_staging_bump.json");

    {
        let queue = open(&path, &clock, config()).await;
        queue
            .record_poll(&first, &watcher_config, clock.now())
            .await
            .unwrap();
        let outcome = queue
            .record_poll(&second, &watcher_config, clock.now())
            .await
            .unwrap();
        assert_eq!(outcome.enqueued.len(), 3);
        queue.close().await;
    }

    let queue = open(&path, &clock, config()).await;
    // Same snapshot after restart: no duplicate detection.
    let outcome = queue
        .record_poll(&second, &watcher_config, clock.now())
        .await
        .unwrap();
    assert!(!outcome.baseline_initialized);
    assert!(outcome.changes.is_empty());
    assert_eq!(queue.stats().await.unwrap().pending, 3);
    let windows = queue.open_windows_at(730, clock.now()).await.unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].branch, "staging");
    assert_eq!(windows[0].trigger_change, 26_900_123);
    assert_eq!(
        windows[0].closes_at,
        clock.now() + watcher_config.window_duration()
    );
}

#[tokio::test]
async fn nack_retries_then_dead_letters() {
    let clock = ManualClock::new(t0());
    let queue = EventQueue::open_in_memory(config(), std::sync::Arc::new(clock.clone()))
        .await
        .unwrap();
    let id = queue.enqueue(&branch_event(7)).await.unwrap();

    for attempt in 1..=2 {
        let claim = queue.claim("w").await.unwrap().unwrap();
        assert_eq!(claim.attempt, attempt);
        let outcome = queue
            .nack(&claim, "downstream unavailable", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NackOutcome::Requeued {
                available_at: clock.now() + TimeDelta::seconds(10)
            }
        );
        assert!(
            queue.claim("w").await.unwrap().is_none(),
            "retry delay respected"
        );
        clock.advance(TimeDelta::seconds(10));
    }
    let claim = queue.claim("w").await.unwrap().unwrap();
    assert_eq!(claim.attempt, 3);
    assert_eq!(claim.last_error.as_deref(), Some("downstream unavailable"));
    assert_eq!(
        queue
            .nack(&claim, "still failing", Duration::ZERO)
            .await
            .unwrap(),
        NackOutcome::DeadLettered
    );
    assert!(queue.claim("w").await.unwrap().is_none());
    let dead = queue.dead_letters().await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, id);
    assert_eq!(dead[0].last_error.as_deref(), Some("still failing"));

    assert!(queue.requeue_dead(id).await.unwrap());
    let claim = queue.claim("w").await.unwrap().unwrap();
    assert_eq!(claim.attempt, 1);
}

#[tokio::test]
async fn expired_final_lease_is_dead_lettered() {
    let clock = ManualClock::new(t0());
    let queue = EventQueue::open_in_memory(
        QueueConfig {
            max_attempts: 2,
            default_lease: Duration::from_secs(5),
        },
        std::sync::Arc::new(clock.clone()),
    )
    .await
    .unwrap();
    queue.enqueue(&branch_event(1)).await.unwrap();
    queue.claim("w").await.unwrap().unwrap();
    clock.advance(TimeDelta::seconds(5));
    assert_eq!(queue.claim("w").await.unwrap().unwrap().attempt, 2);
    clock.advance(TimeDelta::seconds(5));
    assert!(queue.claim("w").await.unwrap().is_none());
    assert_eq!(queue.stats().await.unwrap().dead, 1);
}

#[tokio::test]
async fn stale_claims_are_fenced() {
    let clock = ManualClock::new(t0());
    let queue = EventQueue::open_in_memory(config(), std::sync::Arc::new(clock.clone()))
        .await
        .unwrap();
    queue.enqueue(&branch_event(1)).await.unwrap();
    let old = queue
        .claim_with_lease("slow", Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    clock.advance(TimeDelta::seconds(2));
    let new = queue.claim("fast").await.unwrap().unwrap();
    assert_eq!(new.attempt, 2);
    assert!(matches!(queue.ack(&old).await, Err(Error::Storage(_))));
    assert!(queue
        .extend_lease(&old, Duration::from_secs(60))
        .await
        .is_err());
    assert!(queue.nack(&old, "x", Duration::ZERO).await.is_err());
    queue
        .extend_lease(&new, Duration::from_secs(60))
        .await
        .unwrap();
    queue.ack(&new).await.unwrap();
    assert!(queue.ack(&new).await.is_err(), "double ack rejected");
}

#[tokio::test]
async fn dedupe_delay_and_purge() {
    let clock = ManualClock::new(t0());
    let queue = EventQueue::open_in_memory(config(), std::sync::Arc::new(clock.clone()))
        .await
        .unwrap();
    let options = EnqueueOptions {
        available_at: Some(t0() + TimeDelta::minutes(5)),
        dedupe_key: Some("staging:1".into()),
    };
    let first = queue
        .enqueue_with(&branch_event(1), options.clone())
        .await
        .unwrap();
    let again = queue.enqueue_with(&branch_event(1), options).await.unwrap();
    assert!(first.inserted);
    assert!(!again.inserted);
    assert_eq!(first.id, again.id);

    assert!(queue.claim("w").await.unwrap().is_none());
    clock.advance(TimeDelta::minutes(5));
    let claim = queue.claim("w").await.unwrap().unwrap();
    queue.ack(&claim).await.unwrap();
    assert_eq!(queue.list(EventStatus::Acked).await.unwrap().len(), 1);
    assert_eq!(queue.purge_acked(clock.now()).await.unwrap(), 0);
    clock.advance(TimeDelta::seconds(1));
    assert_eq!(queue.purge_acked(clock.now()).await.unwrap(), 1);
    assert!(queue.get(claim.id).await.unwrap().is_none());
}

#[tokio::test]
async fn strategy_transition_log_is_compare_and_append_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let clock = ManualClock::new(t0());
    {
        let queue = open(&path, &clock, config()).await;
        let first = queue
            .record_transition(&NewTransition {
                strategy: "dip".into(),
                from_state: None,
                to_state: "idle".into(),
                reason: "startup".into(),
                event_id: None,
            })
            .await
            .unwrap();
        assert_eq!(first.occurred_at, t0());

        let event_id = queue
            .enqueue(&MarketEvent::PanicWindowOpened {
                branch: "staging".into(),
                opened_at: t0(),
            })
            .await
            .unwrap();
        let claim = queue.claim("dip").await.unwrap().unwrap();
        clock.advance(TimeDelta::seconds(1));
        queue
            .ack_with_transition(
                &claim,
                &NewTransition {
                    strategy: "dip".into(),
                    from_state: Some("idle".into()),
                    to_state: "watching".into(),
                    reason: "panic window opened".into(),
                    event_id: Some(event_id),
                },
            )
            .await
            .unwrap();

        // Wrong from_state is rejected and leaves the log unchanged.
        let rejected = queue
            .record_transition(&NewTransition {
                strategy: "dip".into(),
                from_state: Some("idle".into()),
                to_state: "buying".into(),
                reason: "stale view".into(),
                event_id: None,
            })
            .await;
        assert!(matches!(rejected, Err(Error::InvalidData(_))));

        // A failed ack rolls back the transition too.
        let failed = queue
            .ack_with_transition(
                &claim,
                &NewTransition {
                    strategy: "dip".into(),
                    from_state: Some("watching".into()),
                    to_state: "buying".into(),
                    reason: "duplicate delivery".into(),
                    event_id: Some(event_id),
                },
            )
            .await;
        assert!(failed.is_err());
        queue.close().await;
    }

    let queue = open(&path, &clock, config()).await;
    assert_eq!(
        queue.current_state("dip").await.unwrap().as_deref(),
        Some("watching")
    );
    assert_eq!(queue.current_state("arb").await.unwrap(), None);
    let log = queue.transitions("dip").await.unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].from_state.as_deref(), Some("idle"));
    assert_eq!(log[1].event_id, Some(1));
    assert_eq!(log[1].occurred_at, t0() + TimeDelta::seconds(1));
}
