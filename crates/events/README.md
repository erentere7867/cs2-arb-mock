# Workstream C — Event watcher

Watches CS2 (Steam App ID 730) branches for staging and prerelease build or
changelist changes. Detected changes go into a SQLite queue that the
simulated strategy reads. The watcher only sends HTTP GETs for public Steam
metadata. It has no trading or order code.

## Pieces

| Module | Contents |
| --- | --- |
| `source` | `BranchInfoSource` trait; `SteamCmdApiSource` (PICS via `api.steamcmd.net`); `SteamDbRssSource` (public builds only); `ScriptedSource` for tests and replays |
| `watcher` | `WatcherConfig` (reads the `[events]` TOML table); the pure function `detect_changes`; `Watcher::poll_once` and `Watcher::run` |
| `queue` | `EventQueue`: a durable queue with at-least-once delivery, the watcher baseline, panic windows, and the strategy-state transition log |
| `clock` | `Clock`, `SystemClock`, `ManualClock` |

## Events

Each poll runs in one SQLite transaction (`EventQueue::record_poll`). It
detects changes against the stored baseline, enqueues events, and moves the
baseline forward. A crash can't lose a change or report it twice.

* The first poll records the baseline and emits no events.
* Each changed or newly appearing watched branch emits
  `MarketEvent::BranchUpdated { change_number = buildid }`.
* A change on a trigger branch (default `staging` and `prerelease`) enqueues
  `PanicWindowOpened` at the observation time. It also enqueues
  `PanicWindowClosed` with delayed delivery at `opened + window_duration_secs`
  (default 4 days, an illustrative value that matches `data/fixtures`). If
  another trigger change arrives while the window is open, the scheduled close
  moves later. No second open event is sent.
* If a snapshot's app `_change_number` is lower than the baseline's, the
  snapshot counts as stale and is ignored.
* With `track_app_changelist`, a rise in `_change_number` that no visible
  branch explains emits `BranchUpdated { branch: "_changelist" }` when the app
  reports private branches. This is a heuristic. Add `_changelist` to
  `trigger_branches` if it should open windows.

## Queue semantics

Delivery is at least once. `claim` leases the oldest ready event and increments
`attempts`. If the lease runs out before an `ack`, the event can be claimed
again, including after a restart. `ack`, `nack` and `extend_lease` are fenced
by the attempt number. After `max_attempts` the event goes to the dead letters.
Order holds only when nothing fails, so consumers must be idempotent per event
id. `ack_with_transition` acknowledges an event and appends a strategy
transition atomically. Transitions are compare-and-append on `from_state`.

## Source verification (observed 2026-11-26)

* `api.steamcmd.net/v1/info/730` is verified. The shape is
  `data."730".depots.branches.<name>.{buildid,timeupdated,timebuildupdated,description}`,
  with values as strings, plus an integer `data."730"._change_number` and
  `depots.privatebranches = "1"`.
* UNVERIFIED: only `public` and versioned branches were visible. Staging and
  prerelease did not appear and may stay hidden from anonymous PICS. Proxy
  rate limits are unknown, and so is the `pwdrequired` field name.
* `steamdb.info/api/PatchnotesRSS/?appid=730` is RSS with
  `<guid>build#ID</guid>` and covers public builds only. Whether automated
  polling is allowed is UNVERIFIED.

Captured responses are in `tests/fixtures/*captured*`. The `*synthetic*`
fixtures add a staging branch and are not real data.
