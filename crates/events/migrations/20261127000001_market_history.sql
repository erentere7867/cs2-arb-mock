CREATE TABLE market_snapshots (
    app_id      INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    payload     TEXT NOT NULL,
    PRIMARY KEY (app_id, observed_at, payload)
) WITHOUT ROWID;

CREATE TABLE market_sales (
    app_id   INTEGER NOT NULL,
    platform TEXT NOT NULL,
    sale_id  TEXT NOT NULL,
    sold_at  INTEGER NOT NULL,
    payload  TEXT NOT NULL,
    PRIMARY KEY (app_id, platform, sale_id)
) WITHOUT ROWID;

CREATE INDEX market_sales_by_time ON market_sales (app_id, sold_at);
