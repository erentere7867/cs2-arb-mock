use cs2_arb_core::{Error, MarketEvent, Result, CS2_APP_ID};
use cs2_arb_events::{EventQueue, NewTransition, SteamCmdApiSource, Watcher, WatcherConfig};

pub async fn run(config: &str, database: &str, once: bool) -> Result<()> {
    let config = WatcherConfig::from_toml(config)?;
    if config.app_id != CS2_APP_ID {
        return Err(Error::InvalidConfig(
            "watch only supports CS2 (App ID 730)".into(),
        ));
    }
    let queue = EventQueue::open(database).await?;
    let watcher = Watcher::new(SteamCmdApiSource::new()?, queue, config)?;
    if once {
        let outcome = watcher.poll_once().await?;
        println!(
            "Recorded {} branch changes and queued {} events",
            outcome.changes.len(),
            outcome.enqueued.len()
        );
        return Ok(());
    }
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let poller = tokio::spawn(async move { watcher.run(receiver).await });
    let signal = tokio::signal::ctrl_c().await;
    let _ = stop.send(true);
    poller
        .await
        .map_err(|err| Error::Storage(format!("watcher task failed: {err}")))?;
    signal.map_err(|err| Error::Storage(format!("could not listen for Ctrl-C: {err}")))
}

pub async fn drain(queue: &EventQueue, events: &mut Vec<MarketEvent>) -> Result<()> {
    let strategy = "event-dip";
    while let Some(claim) = queue.claim(strategy).await? {
        let state = if queue
            .open_windows_at(CS2_APP_ID, queue.clock().now())
            .await?
            .is_empty()
        {
            "monitoring"
        } else {
            "panic"
        };
        let prior = queue.current_state(strategy).await?;
        if prior.as_deref() != Some(state) {
            queue
                .ack_with_transition(
                    &claim,
                    &NewTransition {
                        strategy: strategy.into(),
                        from_state: prior,
                        to_state: state.into(),
                        reason: format!("CS2 market event {}", claim.id),
                        event_id: Some(claim.id),
                    },
                )
                .await?;
        } else {
            queue.ack(&claim).await?;
        }
        events.push(claim.event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, TimeDelta};
    use cs2_arb_core::ManualClock;
    use cs2_arb_events::{QueueConfig, ScriptedSource};

    use super::*;

    #[tokio::test]
    async fn watcher_events_are_delivered_and_record_strategy_state_transitions() {
        let clock = Arc::new(ManualClock::new(
            DateTime::from_timestamp(1_740_000_000, 0).unwrap(),
        ));
        let queue = EventQueue::open_in_memory(QueueConfig::default(), clock.clone())
            .await
            .unwrap();
        let source = ScriptedSource::from_snapshots([
            cs2_arb_events::parse_steamcmd_info(
                include_str!(
                    "../../../crates/events/tests/fixtures/steamcmd_730_synthetic_1_baseline.json"
                ),
                CS2_APP_ID,
            )
            .unwrap(),
            cs2_arb_events::parse_steamcmd_info(
                include_str!(
                "../../../crates/events/tests/fixtures/steamcmd_730_synthetic_2_staging_bump.json"
            ),
                CS2_APP_ID,
            )
            .unwrap(),
        ]);
        let config = WatcherConfig::default();
        let watcher = Watcher::new(source, queue.clone(), config.clone()).unwrap();
        watcher.poll_once().await.unwrap();
        clock.advance(TimeDelta::minutes(2));
        watcher.poll_once().await.unwrap();

        let mut events = Vec::new();
        drain(&queue, &mut events).await.unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, MarketEvent::PanicWindowOpened { .. })));
        assert_eq!(
            queue.current_state("event-dip").await.unwrap().as_deref(),
            Some("panic")
        );

        clock.advance(config.window_duration());
        drain(&queue, &mut events).await.unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, MarketEvent::PanicWindowClosed { .. })));
        assert_eq!(
            queue.current_state("event-dip").await.unwrap().as_deref(),
            Some("monitoring")
        );
        assert_eq!(queue.stats().await.unwrap().pending, 0);
    }
}
