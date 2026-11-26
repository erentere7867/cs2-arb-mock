-- Durable event queue, watcher baseline, panic windows and strategy-state log.
-- All timestamps are UTC unix milliseconds.

CREATE TABLE IF NOT EXISTS event_queue (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    kind             TEXT    NOT NULL,
    payload          TEXT    NOT NULL,
    dedupe_key       TEXT    UNIQUE,
    status           TEXT    NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'claimed', 'acked', 'dead')),
    attempts         INTEGER NOT NULL DEFAULT 0,
    enqueued_at      INTEGER NOT NULL,
    available_at     INTEGER NOT NULL,
    claimed_by       TEXT,
    claimed_at       INTEGER,
    lease_expires_at INTEGER,
    acked_at         INTEGER,
    last_error       TEXT
);

CREATE INDEX IF NOT EXISTS event_queue_ready
    ON event_queue (status, available_at, id);

CREATE TABLE IF NOT EXISTS app_state (
    app_id        INTEGER PRIMARY KEY,
    change_number INTEGER,
    observed_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS branch_state (
    app_id       INTEGER NOT NULL,
    branch       TEXT    NOT NULL,
    buildid      INTEGER NOT NULL,
    time_updated INTEGER,
    observed_at  INTEGER NOT NULL,
    PRIMARY KEY (app_id, branch)
);

CREATE TABLE IF NOT EXISTS panic_windows (
    app_id          INTEGER NOT NULL,
    branch          TEXT    NOT NULL,
    opened_at       INTEGER NOT NULL,
    closes_at       INTEGER NOT NULL,
    trigger_change  INTEGER NOT NULL,
    open_event_id   INTEGER,
    close_event_id  INTEGER,
    PRIMARY KEY (app_id, branch)
);

CREATE TABLE IF NOT EXISTS strategy_transitions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    strategy    TEXT    NOT NULL,
    from_state  TEXT,
    to_state    TEXT    NOT NULL,
    reason      TEXT    NOT NULL DEFAULT '',
    event_id    INTEGER,
    occurred_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS strategy_transitions_by_strategy
    ON strategy_transitions (strategy, id);
