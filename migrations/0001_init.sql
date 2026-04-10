-- config: persists the claimed access URL so a setup token is only consumed once
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- accounts: snapshot of latest account state, upserted on each fetch
CREATE TABLE IF NOT EXISTS accounts (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    currency          TEXT NOT NULL,
    balance           TEXT NOT NULL,
    balance_date      INTEGER NOT NULL,
    available_balance TEXT,
    conn_id           TEXT,
    extra             TEXT
);

-- transactions: append-only log, upserted by id to handle overlapping fetches
CREATE TABLE IF NOT EXISTS transactions (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id),
    posted        INTEGER NOT NULL,
    amount        TEXT NOT NULL,
    description   TEXT NOT NULL,
    transacted_at INTEGER,
    pending       INTEGER NOT NULL DEFAULT 0,
    extra         TEXT
);

CREATE INDEX IF NOT EXISTS idx_txn_account_posted
    ON transactions (account_id, posted);
