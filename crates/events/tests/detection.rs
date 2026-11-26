//! Fixture-driven change detection and panic-window scheduling.

mod common;

use chrono::{DateTime, TimeDelta};
use common::{fixture, open, snapshot, t0};
use cs2_arb_core::{MarketEvent, CS2_APP_ID};
use cs2_arb_events::{
    detect_changes, parse_steamdb_rss, Baseline, BranchChangeKind, Clock, Detection, ManualClock,
    QueueConfig, ScriptedSource, Watcher, WatcherConfig, WindowAction,
};

#[test]
fn parses_captured_steamcmd_response() {
    let snap = snapshot("steamcmd_730_captured_2026-11-26.json");
    assert_eq!(snap.app_id, CS2_APP_ID);
    assert_eq!(snap.change_number, Some(41_480_785));
    assert!(snap.has_private_branches);
    let public = &snap.branches["public"];
    assert_eq!(public.buildid, 26_879_581);
    assert_eq!(
        public.time_updated,
        DateTime::from_timestamp(1_795_474_817, 0)
    );
    assert_eq!(
        public.time_build_updated,
        DateTime::from_timestamp(1_795_474_308, 0)
    );
    // The latest versioned branch mirrors public; staging is not publicly visible.
    assert_eq!(snap.branches["1.42.0.1"].buildid, public.buildid);
    assert!(!snap.branches.contains_key("staging"));
    assert!(!snap.branches.contains_key("prerelease"));
}

#[test]
fn parses_captured_steamdb_rss() {
    let snap = parse_steamdb_rss(&fixture("steamdb_rss_730_captured_2026-11-26.xml"), 730).unwrap();
    let public = &snap.branches["public"];
    assert_eq!(public.buildid, 26_879_581);
    assert_eq!(
        public.time_updated,
        DateTime::from_timestamp(1_795_474_820, 0)
    );
    assert!(parse_steamdb_rss("<rss><channel></channel></rss>", 730).is_err());
}

#[test]
fn detects_fixture_changes() {
    let config = WatcherConfig::default();
    let first = snapshot("steamcmd_730_synthetic_1_baseline.json");
    let second = snapshot("steamcmd_730_synthetic_2_staging_bump.json");
    let third = snapshot("steamcmd_730_synthetic_3_prerelease_appears.json");
    let stale = snapshot("steamcmd_730_synthetic_4_stale.json");

    assert_eq!(
        detect_changes(None, &first, &config, t0()),
        Detection::Baseline
    );

    let baseline = Baseline {
        change_number: first.change_number,
        branches: first
            .branches
            .iter()
            .map(|(name, info)| (name.clone(), info.buildid))
            .collect(),
    };
    assert_eq!(
        detect_changes(Some(&baseline), &first, &config, t0()),
        Detection::Changes(vec![])
    );

    let Detection::Changes(changes) = detect_changes(Some(&baseline), &second, &config, t0())
    else {
        panic!("expected changes");
    };
    // `csgo_legacy` is new but not watched; only staging is reported.
    assert_eq!(changes.len(), 1);
    let staging = &changes[0];
    assert_eq!(staging.branch, "staging");
    assert_eq!(staging.kind, BranchChangeKind::Buildid);
    assert_eq!(staging.previous, Some(26_900_000));
    assert_eq!(staging.change_number, 26_900_123);
    assert!(staging.opens_window);
    assert_eq!(
        staging.steam_time_updated,
        DateTime::from_timestamp(1_795_510_000, 0)
    );
    assert_eq!(
        staging.to_event(),
        MarketEvent::BranchUpdated {
            app_id: 730,
            branch: "staging".into(),
            change_number: 26_900_123,
            observed_at: t0(),
        }
    );

    let Detection::Changes(changes) = detect_changes(Some(&baseline), &third, &config, t0()) else {
        panic!("expected changes");
    };
    let names: Vec<_> = changes
        .iter()
        .map(|c| (c.branch.as_str(), c.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("prerelease", BranchChangeKind::NewBranch),
            ("staging", BranchChangeKind::Buildid)
        ]
    );

    assert_eq!(
        detect_changes(Some(&baseline), &stale, &config, t0()),
        Detection::Stale {
            baseline: 41_480_785,
            observed: 41_480_700
        }
    );

    let all = WatcherConfig {
        watched_branches: Default::default(),
        ..WatcherConfig::default()
    };
    let Detection::Changes(changes) = detect_changes(Some(&baseline), &second, &all, t0()) else {
        panic!("expected changes");
    };
    assert_eq!(changes.len(), 2);
    assert!(changes
        .iter()
        .any(|c| c.branch == "csgo_legacy" && !c.opens_window));
}

#[tokio::test]
async fn watcher_emits_branch_update_and_scheduled_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let clock = ManualClock::new(t0());
    let queue = open(&path, &clock, QueueConfig::default()).await;
    let config = WatcherConfig {
        window_duration_secs: 6 * 3600,
        ..WatcherConfig::default()
    };
    let source = ScriptedSource::from_snapshots([
        snapshot("steamcmd_730_synthetic_1_baseline.json"),
        snapshot("steamcmd_730_synthetic_2_staging_bump.json"),
        snapshot("steamcmd_730_synthetic_4_stale.json"),
        snapshot("steamcmd_730_synthetic_3_prerelease_appears.json"),
    ]);
    let watcher = Watcher::new(source, queue.clone(), config).unwrap();

    let outcome = watcher.poll_once().await.unwrap();
    assert!(outcome.baseline_initialized);
    assert!(outcome.enqueued.is_empty());
    assert!(queue.claim("strategy").await.unwrap().is_none());

    clock.advance(TimeDelta::minutes(2));
    let opened_at = clock.now();
    let outcome = watcher.poll_once().await.unwrap();
    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(outcome.enqueued.len(), 3);
    let closes_at = opened_at + TimeDelta::hours(6);
    assert!(matches!(
        &outcome.windows[..],
        [WindowAction::Opened { branch, closes_at: c, .. }] if branch == "staging" && *c == closes_at
    ));

    let mut delivered = Vec::new();
    while let Some(claim) = queue.claim("strategy").await.unwrap() {
        delivered.push(claim.event.clone());
        queue.ack(&claim).await.unwrap();
    }
    assert_eq!(
        delivered,
        [
            MarketEvent::BranchUpdated {
                app_id: 730,
                branch: "staging".into(),
                change_number: 26_900_123,
                observed_at: opened_at,
            },
            MarketEvent::PanicWindowOpened {
                branch: "staging".into(),
                opened_at,
            },
        ]
    );
    assert_eq!(
        queue.open_windows_at(730, opened_at).await.unwrap().len(),
        1
    );

    // A stale proxy response is ignored and does not move the baseline.
    clock.advance(TimeDelta::minutes(2));
    let outcome = watcher.poll_once().await.unwrap();
    assert!(outcome.stale);
    assert_eq!(
        queue.baseline(730).await.unwrap().unwrap().branches["staging"],
        26_900_123
    );

    // Prerelease appearing opens its own window; staging is unchanged.
    clock.advance(TimeDelta::minutes(2));
    let pre_at = clock.now();
    let outcome = watcher.poll_once().await.unwrap();
    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(outcome.changes[0].branch, "prerelease");
    let claim = queue.claim("strategy").await.unwrap().unwrap();
    assert!(
        matches!(claim.event, MarketEvent::BranchUpdated { ref branch, .. } if branch == "prerelease")
    );
    queue.ack(&claim).await.unwrap();
    let claim = queue.claim("strategy").await.unwrap().unwrap();
    assert_eq!(
        claim.event,
        MarketEvent::PanicWindowOpened {
            branch: "prerelease".into(),
            opened_at: pre_at
        }
    );
    queue.ack(&claim).await.unwrap();

    // Close events are delivered only once their time arrives.
    assert!(queue.claim("strategy").await.unwrap().is_none());
    clock.set(closes_at);
    let claim = queue.claim("strategy").await.unwrap().unwrap();
    assert_eq!(
        claim.event,
        MarketEvent::PanicWindowClosed {
            branch: "staging".into(),
            closed_at: closes_at
        }
    );
    queue.ack(&claim).await.unwrap();
    assert!(queue.claim("strategy").await.unwrap().is_none());
    clock.set(pre_at + TimeDelta::hours(6));
    let claim = queue.claim("strategy").await.unwrap().unwrap();
    assert!(
        matches!(claim.event, MarketEvent::PanicWindowClosed { ref branch, .. } if branch == "prerelease")
    );
}

#[tokio::test]
async fn repeated_trigger_extends_open_window() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(t0());
    let queue = open(&dir.path().join("q.db"), &clock, QueueConfig::default()).await;
    let config = WatcherConfig {
        window_duration_secs: 3600,
        ..WatcherConfig::default()
    };
    let mut first = snapshot("steamcmd_730_synthetic_1_baseline.json");
    queue
        .record_poll(&first, &config, clock.now())
        .await
        .unwrap();

    let bump = |snap: &mut cs2_arb_events::AppBranchSnapshot| {
        snap.change_number = snap.change_number.map(|n| n + 1);
        snap.branches.get_mut("staging").unwrap().buildid += 1;
    };
    bump(&mut first);
    let opened = queue
        .record_poll(&first, &config, clock.now())
        .await
        .unwrap();
    let [WindowAction::Opened { close_event_id, .. }] = opened.windows[..] else {
        panic!("expected open");
    };

    clock.advance(TimeDelta::minutes(30));
    bump(&mut first);
    let extended = queue
        .record_poll(&first, &config, clock.now())
        .await
        .unwrap();
    let new_close = clock.now() + TimeDelta::hours(1);
    assert_eq!(
        extended.windows,
        [WindowAction::Extended {
            branch: "staging".into(),
            closes_at: new_close,
            close_event_id,
        }]
    );
    // Only the BranchUpdated was added; the close was rescheduled in place.
    assert_eq!(extended.enqueued.len(), 1);
    let close = queue.get(close_event_id).await.unwrap().unwrap();
    assert_eq!(close.available_at, new_close);
    assert_eq!(
        close.event,
        Some(MarketEvent::PanicWindowClosed {
            branch: "staging".into(),
            closed_at: new_close
        })
    );

    // After the window closes, the next change opens a fresh window.
    clock.set(new_close + TimeDelta::minutes(1));
    bump(&mut first);
    let reopened = queue
        .record_poll(&first, &config, clock.now())
        .await
        .unwrap();
    assert!(matches!(
        reopened.windows[..],
        [WindowAction::Opened { .. }]
    ));
    let windows = queue.panic_windows(730).await.unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].opened_at, clock.now());
}
