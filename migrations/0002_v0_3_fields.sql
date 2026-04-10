-- Transactions: new fields from SimpleFIN 0.3
ALTER TABLE transactions ADD COLUMN payee TEXT;
ALTER TABLE transactions ADD COLUMN memo  TEXT;

-- Holdings: new in SimpleFIN 0.3; one row per holding, upserted by id each fetch
CREATE TABLE IF NOT EXISTS holdings (
    id             TEXT PRIMARY KEY,
    account_id     TEXT NOT NULL REFERENCES accounts(id),
    created        INTEGER NOT NULL,
    currency       TEXT NOT NULL,
    cost_basis     TEXT,
    description    TEXT,
    market_value   TEXT,
    purchase_price TEXT,
    shares         TEXT,
    symbol         TEXT
);

CREATE INDEX IF NOT EXISTS idx_holding_account
    ON holdings (account_id);
