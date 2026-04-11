-- Per-account display name preference for reconciled (REC-) accounts.
-- Keyed by sfin_account_id since that is the canonical key for REC- accounts.
-- When preferred_source = 'lunchflow', the LunchFlow account name is used
-- as displayName in FDX responses instead of the SimpleFIN name.
CREATE TABLE user_account_name_preference (
    sfin_account_id  TEXT PRIMARY KEY,
    preferred_source TEXT NOT NULL DEFAULT 'simplefin',
    CHECK (preferred_source IN ('simplefin', 'lunchflow'))
);
