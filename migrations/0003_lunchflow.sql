-- Raw lunchflow accounts, keyed by lunchflow's u64 account id
CREATE TABLE IF NOT EXISTS lf_accounts (
    id                INTEGER PRIMARY KEY,   -- lunchflow account id (u64)
    name              TEXT NOT NULL,
    institution_name  TEXT NOT NULL,
    provider          TEXT NOT NULL,
    currency          TEXT,
    status            TEXT NOT NULL,
    balance           REAL,
    balance_currency  TEXT,
    fetched_at        INTEGER NOT NULL
);

-- Raw lunchflow transactions. Pending rows have no server-assigned id; we
-- generate a synthetic key "lf_pending_{account_id}_{date}" so they can be
-- upserted without duplication.
CREATE TABLE IF NOT EXISTS lf_transactions (
    id            TEXT PRIMARY KEY,
    lf_account_id INTEGER NOT NULL REFERENCES lf_accounts(id),
    amount        REAL NOT NULL,
    currency      TEXT NOT NULL,
    date          TEXT NOT NULL,   -- YYYY-MM-DD
    merchant      TEXT,
    description   TEXT,
    is_pending    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_lf_txn_account_date
    ON lf_transactions (lf_account_id, date);

-- Raw lunchflow holdings. Holdings have no stable server id, so we delete and
-- reinsert per account on each fetch cycle.
CREATE TABLE IF NOT EXISTS lf_holdings (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    lf_account_id   INTEGER NOT NULL REFERENCES lf_accounts(id),
    security_name   TEXT NOT NULL,
    ticker_symbol   TEXT,
    isin            TEXT,
    quantity        REAL NOT NULL,
    price           REAL NOT NULL,
    value           REAL NOT NULL,
    cost_basis      REAL,
    currency        TEXT NOT NULL,
    fetched_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_lf_holding_account
    ON lf_holdings (lf_account_id);
