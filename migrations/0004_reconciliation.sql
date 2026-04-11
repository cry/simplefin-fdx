-- Account-level links between SimpleFIN and LunchFlow.
-- A row with only sfin_account_id set (lf_account_id NULL) means the account
-- was seen only in SimpleFIN, and vice-versa.
CREATE TABLE IF NOT EXISTS reconciled_accounts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    sfin_account_id  TEXT REFERENCES accounts(id),
    lf_account_id    INTEGER REFERENCES lf_accounts(id),
    match_confidence REAL NOT NULL DEFAULT 0.0,   -- 0.0–1.0
    status           TEXT NOT NULL,               -- 'matched' | 'sfin_only' | 'lf_only'
    reconciled_at    INTEGER NOT NULL,
    UNIQUE(sfin_account_id, lf_account_id)
);

-- Transaction-level links between SimpleFIN and LunchFlow.
-- A row with only one FK set means the transaction appeared in only one source.
CREATE TABLE IF NOT EXISTS reconciled_transactions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    sfin_txn_id      TEXT REFERENCES transactions(id),
    lf_txn_id        TEXT REFERENCES lf_transactions(id),
    match_confidence REAL NOT NULL DEFAULT 0.0,   -- 0.0–1.0
    status           TEXT NOT NULL,               -- 'matched' | 'sfin_only' | 'lf_only'
    reconciled_at    INTEGER NOT NULL,
    notes            TEXT,
    UNIQUE(sfin_txn_id, lf_txn_id)
);

CREATE INDEX IF NOT EXISTS idx_recon_txn_sfin ON reconciled_transactions (sfin_txn_id);
CREATE INDEX IF NOT EXISTS idx_recon_txn_lf   ON reconciled_transactions (lf_txn_id);
