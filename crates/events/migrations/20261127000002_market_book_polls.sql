CREATE TABLE market_bid_polls (
    app_id      INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    payload     TEXT NOT NULL,
    PRIMARY KEY (app_id, observed_at, payload)
) WITHOUT ROWID;

CREATE TABLE market_empty_listing_polls (
    app_id      INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    payload     TEXT NOT NULL,
    PRIMARY KEY (app_id, observed_at, payload)
) WITHOUT ROWID;
