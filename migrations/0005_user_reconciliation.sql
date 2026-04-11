-- User-defined account reconciliation rules.
-- These take precedence over the automatic reconciler.
--
-- action = 'match'   — force these two accounts to be treated as reconciled
--                      (requires both sfin_account_id and lf_account_id)
-- action = 'exclude' — prevent reconciliation:
--                      both IDs set  → never match this specific pair
--                      only sfin_id  → keep this SimpleFIN account as standalone
--                      only lf_id    → keep this LunchFlow account as standalone
CREATE TABLE user_account_reconciliation (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    sfin_account_id  TEXT,
    lf_account_id    INTEGER,
    action           TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    UNIQUE(sfin_account_id, lf_account_id),
    CHECK (sfin_account_id IS NOT NULL OR lf_account_id IS NOT NULL),
    CHECK (action IN ('match', 'exclude')),
    CHECK (action != 'match' OR (sfin_account_id IS NOT NULL AND lf_account_id IS NOT NULL))
);
