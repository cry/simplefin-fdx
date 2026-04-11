use lunchflow::models::{Account, Balance, Holding, Transaction};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::util;

pub struct LfTransactionRow {
    pub id: String,
    pub amount: f64,
    pub date: String,
    pub merchant: Option<String>,
    pub description: Option<String>,
    pub is_pending: bool,
}

/// Upsert a lunchflow account and its current balance.
pub async fn upsert_lf_account(
    pool: &SqlitePool,
    account: &Account,
    balance: Option<&Balance>,
    fetched_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO lf_accounts
            (id, name, institution_name, provider, currency, status,
             balance, balance_currency, fetched_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name             = excluded.name,
            institution_name = excluded.institution_name,
            provider         = excluded.provider,
            currency         = excluded.currency,
            status           = excluded.status,
            balance          = excluded.balance,
            balance_currency = excluded.balance_currency,
            fetched_at       = excluded.fetched_at
        "#,
    )
    .bind(account.id as i64)
    .bind(&account.name)
    .bind(&account.institution_name)
    .bind(format!("{:?}", account.provider))
    .bind(&account.currency)
    .bind(account.status.to_string())
    .bind(balance.map(|b| b.amount))
    .bind(balance.map(|b| b.currency.as_str()))
    .bind(fetched_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Upsert a batch of transactions for a lunchflow account.
pub async fn upsert_lf_transactions(
    pool: &SqlitePool,
    account_id: u64,
    transactions: &[Transaction],
) -> Result<(), sqlx::Error> {
    for txn in transactions {
        // Pending transactions have no server-assigned id; synthesise one so
        // we can upsert without creating duplicates on repeated fetches.
        let id = match &txn.id {
            Some(id) => id.clone(),
            None => format!("lf_pending_{}_{}", account_id, txn.date),
        };

        sqlx::query(
            r#"
            INSERT INTO lf_transactions
                (id, lf_account_id, amount, currency, date, merchant, description, is_pending)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                amount      = excluded.amount,
                currency    = excluded.currency,
                date        = excluded.date,
                merchant    = excluded.merchant,
                description = excluded.description,
                is_pending  = excluded.is_pending
            "#,
        )
        .bind(&id)
        .bind(account_id as i64)
        .bind(txn.amount)
        .bind(&txn.currency)
        .bind(&txn.date)
        .bind(&txn.merchant)
        .bind(&txn.description)
        .bind(txn.is_pending as i64)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Replace all holdings for a lunchflow account. Holdings have no stable id
/// so we delete-and-reinsert on every fetch.
pub async fn replace_lf_holdings(
    pool: &SqlitePool,
    account_id: u64,
    holdings: &[Holding],
    fetched_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM lf_holdings WHERE lf_account_id = ?")
        .bind(account_id as i64)
        .execute(pool)
        .await?;

    for h in holdings {
        sqlx::query(
            r#"
            INSERT INTO lf_holdings
                (lf_account_id, security_name, ticker_symbol, isin,
                 quantity, price, value, cost_basis, currency, fetched_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(account_id as i64)
        .bind(&h.security.name)
        .bind(&h.security.ticker_symbol)
        .bind(&h.security.isin)
        .bind(h.quantity)
        .bind(h.price)
        .bind(h.value)
        .bind(h.cost_basis)
        .bind(&h.currency)
        .bind(fetched_at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Full LunchFlow account row, used for building FDX responses.
pub struct LfAccountFull {
    pub id: i64,
    pub name: String,
    pub currency: Option<String>,
    pub balance: Option<f64>,
    pub fetched_at: i64,
}

/// Load every LunchFlow account. Used in LunchFlow-only mode where
/// reconciliation never runs and all accounts should be exposed.
pub async fn get_all_lf_accounts(pool: &SqlitePool) -> Result<Vec<LfAccountFull>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, name, currency, balance, fetched_at FROM lf_accounts")
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| LfAccountFull {
            id: r.get("id"),
            name: r.get("name"),
            currency: r.get("currency"),
            balance: r.get("balance"),
            fetched_at: r.get("fetched_at"),
        })
        .collect())
}

/// Fetch a single LunchFlow account by its numeric id.
pub async fn get_lf_account(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<LfAccountFull>, sqlx::Error> {
    let row =
        sqlx::query("SELECT id, name, currency, balance, fetched_at FROM lf_accounts WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;

    Ok(row.map(|r| LfAccountFull {
        id: r.get("id"),
        name: r.get("name"),
        currency: r.get("currency"),
        balance: r.get("balance"),
        fetched_at: r.get("fetched_at"),
    }))
}

/// Fetch raw LunchFlow transactions for a given account, with an optional date
/// range expressed as unix timestamps (converted to YYYY-MM-DD for the query).
pub async fn get_lf_transactions_raw(
    pool: &SqlitePool,
    lf_account_id: i64,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
) -> Result<Vec<LfTransactionRow>, sqlx::Error> {
    let from = start_ts
        .map(util::unix_to_date_str)
        .unwrap_or_else(|| "0000-01-01".to_string());
    let to = end_ts
        .map(util::unix_to_date_str)
        .unwrap_or_else(|| "9999-12-31".to_string());

    let rows = sqlx::query(
        r#"
        SELECT id, amount, date, merchant, description, is_pending
        FROM lf_transactions
        WHERE lf_account_id = ?
          AND (is_pending = 1 OR (date >= ? AND date <= ?))
        ORDER BY is_pending DESC, date DESC
        "#,
    )
    .bind(lf_account_id)
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| LfTransactionRow {
            id: r.get("id"),
            amount: r.get("amount"),
            date: r.get("date"),
            merchant: r.get("merchant"),
            description: r.get("description"),
            is_pending: r.get::<i64, _>("is_pending") != 0,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Unified transaction view
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SfinTransactionView {
    pub id: String,
    pub posted: i64,
    pub amount: String,
    pub description: String,
    pub payee: Option<String>,
    pub memo: Option<String>,
    pub pending: bool,
}

#[derive(Serialize)]
pub struct LfTransactionView {
    pub id: String,
    pub amount: f64,
    pub currency: String,
    pub date: String,
    pub merchant: Option<String>,
    pub description: Option<String>,
    pub is_pending: bool,
}

/// A transaction entry showing data from one or both sources alongside its
/// reconciliation status.
#[derive(Serialize)]
pub struct UnifiedTransaction {
    /// `"matched"` | `"sfin_only"` | `"lf_only"` | `"unreconciled"`
    pub status: String,
    pub match_confidence: Option<f64>,
    /// `reconciled_transactions.id` — present when the row exists (matched/sfin_only/lf_only).
    /// Used to mint the `REC-{id}` transaction ID for matched transactions.
    #[serde(skip)]
    pub rec_id: Option<i64>,
    pub simplefin: Option<SfinTransactionView>,
    pub lunchflow: Option<LfTransactionView>,
    /// Sort key (unix seconds) used internally; not serialised.
    #[serde(skip)]
    pub sort_ts: i64,
}

/// Fetch all unified transactions for a SimpleFIN account, merging data from
/// both sources and joining in reconciliation status where available.
///
/// `start_ts` / `end_ts` are unix timestamps that filter the SimpleFIN side;
/// the equivalent YYYY-MM-DD range is derived and applied to the LunchFlow side.
pub async fn get_unified_transactions(
    pool: &SqlitePool,
    sfin_account_id: &str,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
) -> Result<Vec<UnifiedTransaction>, sqlx::Error> {
    let start = start_ts.unwrap_or(0);
    let end = end_ts.unwrap_or(i64::MAX);

    // Derive YYYY-MM-DD bounds for the LunchFlow side.
    let lf_from = start_ts
        .map(util::unix_to_date_str)
        .unwrap_or_else(|| "0000-01-01".to_string());
    let lf_to = end_ts
        .map(util::unix_to_date_str)
        .unwrap_or_else(|| "9999-12-31".to_string());

    // Find the matched LunchFlow account for this SimpleFIN account (if any).
    let lf_account_id: Option<i64> = sqlx::query(
        "SELECT lf_account_id FROM reconciled_accounts \
         WHERE sfin_account_id = ? AND status = 'matched' LIMIT 1",
    )
    .bind(sfin_account_id)
    .fetch_optional(pool)
    .await?
    .map(|r| r.get("lf_account_id"));

    // Query 1: All SimpleFIN transactions with reconciliation info joined in.
    let sfin_rows = sqlx::query(
        r#"
        SELECT
            COALESCE(rt.status, 'unreconciled') AS recon_status,
            rt.match_confidence,
            rt.id         AS rec_id,
            t.id          AS sfin_id,
            t.posted      AS sfin_posted,
            t.amount      AS sfin_amount,
            t.description AS sfin_description,
            t.payee,
            t.memo,
            t.pending     AS sfin_pending,
            lft.id          AS lf_id,
            lft.amount      AS lf_amount,
            lft.currency    AS lf_currency,
            lft.date        AS lf_date,
            lft.merchant,
            lft.description AS lf_description,
            lft.is_pending  AS lf_is_pending
        FROM transactions t
        LEFT JOIN reconciled_transactions rt ON rt.sfin_txn_id = t.id
        LEFT JOIN lf_transactions lft ON lft.id = rt.lf_txn_id
        WHERE t.account_id = ?
          AND (t.pending = 1 OR (t.posted >= ? AND t.posted <= ?))
        ORDER BY t.pending DESC, t.posted DESC
        "#,
    )
    .bind(sfin_account_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await?;

    let mut results: Vec<UnifiedTransaction> = sfin_rows
        .into_iter()
        .map(|r| {
            let lf_id: Option<String> = r.get("lf_id");
            let sfin_posted: i64 = r.get("sfin_posted");
            UnifiedTransaction {
                status: r.get("recon_status"),
                match_confidence: r.get("match_confidence"),
                rec_id: r.get("rec_id"),
                simplefin: Some(SfinTransactionView {
                    id: r.get("sfin_id"),
                    posted: sfin_posted,
                    amount: r.get("sfin_amount"),
                    description: r.get("sfin_description"),
                    payee: r.get("payee"),
                    memo: r.get("memo"),
                    pending: r.get::<i64, _>("sfin_pending") != 0,
                }),
                lunchflow: lf_id.map(|id| LfTransactionView {
                    id,
                    amount: r.get("lf_amount"),
                    currency: r.get("lf_currency"),
                    date: r.get("lf_date"),
                    merchant: r.get("merchant"),
                    description: r.get("lf_description"),
                    is_pending: r.get::<i64, _>("lf_is_pending") != 0,
                }),
                sort_ts: sfin_posted,
            }
        })
        .collect();

    // Query 2: LunchFlow-only transactions from the matched lf account.
    if let Some(lf_id) = lf_account_id {
        let lf_only_rows = sqlx::query(
            r#"
            SELECT
                rt.id           AS rec_id,
                rt.match_confidence,
                lft.id          AS lf_id,
                lft.amount      AS lf_amount,
                lft.currency    AS lf_currency,
                lft.date        AS lf_date,
                lft.merchant,
                lft.description AS lf_description,
                lft.is_pending  AS lf_is_pending
            FROM lf_transactions lft
            JOIN reconciled_transactions rt
                ON rt.lf_txn_id = lft.id AND rt.status = 'lf_only'
            WHERE lft.lf_account_id = ?
              AND (lft.is_pending = 1 OR (lft.date >= ? AND lft.date <= ?))
            "#,
        )
        .bind(lf_id)
        .bind(&lf_from)
        .bind(&lf_to)
        .fetch_all(pool)
        .await?;

        for r in lf_only_rows {
            let date: String = r.get("lf_date");
            let sort_ts = util::date_str_to_unix(&date);
            results.push(UnifiedTransaction {
                status: "lf_only".to_string(),
                match_confidence: r.get("match_confidence"),
                rec_id: r.get("rec_id"),
                simplefin: None,
                lunchflow: Some(LfTransactionView {
                    id: r.get("lf_id"),
                    amount: r.get("lf_amount"),
                    currency: r.get("lf_currency"),
                    date,
                    merchant: r.get("merchant"),
                    description: r.get("lf_description"),
                    is_pending: r.get::<i64, _>("lf_is_pending") != 0,
                }),
                sort_ts,
            });
        }

        // Re-sort: pending first, then descending by timestamp.
        results.sort_by(|a, b| {
            let a_pending = a.simplefin.as_ref().map_or(false, |s| s.pending)
                || a.lunchflow.as_ref().map_or(false, |l| l.is_pending);
            let b_pending = b.simplefin.as_ref().map_or(false, |s| s.pending)
                || b.lunchflow.as_ref().map_or(false, |l| l.is_pending);
            b_pending
                .cmp(&a_pending)
                .then_with(|| b.sort_ts.cmp(&a.sort_ts))
        });
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// User-defined reconciliation rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserReconciliationAction {
    Match,
    Exclude,
}

impl UserReconciliationAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Exclude => "exclude",
        }
    }
}

impl std::fmt::Display for UserReconciliationAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for UserReconciliationAction {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "match" => Ok(Self::Match),
            "exclude" => Ok(Self::Exclude),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserAccountRule {
    pub id: i64,
    pub sfin_account_id: Option<String>,
    pub lf_account_id: Option<i64>,
    pub action: UserReconciliationAction,
    pub created_at: i64,
}

pub async fn get_user_account_rules(
    pool: &SqlitePool,
) -> Result<Vec<UserAccountRule>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, sfin_account_id, lf_account_id, action, created_at \
         FROM user_account_reconciliation ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let action_str: String = r.get("action");
            let action = action_str.parse().ok()?;
            Some(UserAccountRule {
                id: r.get("id"),
                sfin_account_id: r.get("sfin_account_id"),
                lf_account_id: r.get("lf_account_id"),
                action,
                created_at: r.get("created_at"),
            })
        })
        .collect())
}

pub async fn insert_user_account_rule(
    pool: &SqlitePool,
    sfin_account_id: Option<&str>,
    lf_account_id: Option<i64>,
    action: &UserReconciliationAction,
    created_at: i64,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO user_account_reconciliation \
             (sfin_account_id, lf_account_id, action, created_at) \
         VALUES (?, ?, ?, ?) \
         RETURNING id",
    )
    .bind(sfin_account_id)
    .bind(lf_account_id)
    .bind(action.as_str())
    .bind(created_at)
    .fetch_one(pool)
    .await?;

    Ok(row.get("id"))
}

pub async fn delete_user_account_rule(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM user_account_reconciliation WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Account match summary (for the management UI)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AccountMatchRow {
    pub id: i64,
    pub status: String,
    pub match_confidence: Option<f64>,
    pub sfin_account_id: Option<String>,
    pub sfin_name: Option<String>,
    pub lf_account_id: Option<i64>,
    pub lf_name: Option<String>,
    /// `"simplefin"` (default) or `"lunchflow"`. Only meaningful for matched rows.
    pub preferred_name_source: String,
}

// ---------------------------------------------------------------------------
// Name preference
// ---------------------------------------------------------------------------

/// Returns a map of sfin_account_id → preferred_source for all rows where
/// preferred_source = 'lunchflow'. Rows defaulting to 'simplefin' are omitted
/// so callers only need to check for presence.
pub async fn get_lf_name_preferences(
    pool: &SqlitePool,
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT sfin_account_id FROM user_account_name_preference \
         WHERE preferred_source = 'lunchflow'",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| r.get("sfin_account_id")).collect())
}

pub async fn upsert_name_preference(
    pool: &SqlitePool,
    sfin_account_id: &str,
    preferred_source: &str, // 'simplefin' | 'lunchflow'
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_account_name_preference (sfin_account_id, preferred_source) \
         VALUES (?, ?) \
         ON CONFLICT(sfin_account_id) DO UPDATE SET preferred_source = excluded.preferred_source",
    )
    .bind(sfin_account_id)
    .bind(preferred_source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return all rows from `reconciled_accounts`, joined with account names and
/// the current name preference (if any).
pub async fn get_account_matches(pool: &SqlitePool) -> Result<Vec<AccountMatchRow>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT
            ra.id,
            ra.status,
            ra.match_confidence,
            ra.sfin_account_id,
            a.name  AS sfin_name,
            ra.lf_account_id,
            lfa.name AS lf_name,
            COALESCE(np.preferred_source, 'simplefin') AS preferred_name_source
        FROM reconciled_accounts ra
        LEFT JOIN accounts a ON a.id = ra.sfin_account_id
        LEFT JOIN lf_accounts lfa ON lfa.id = ra.lf_account_id
        LEFT JOIN user_account_name_preference np ON np.sfin_account_id = ra.sfin_account_id
        ORDER BY ra.status DESC, ra.id
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| AccountMatchRow {
            id: r.get("id"),
            status: r.get("status"),
            match_confidence: r.get("match_confidence"),
            sfin_account_id: r.get("sfin_account_id"),
            sfin_name: r.get("sfin_name"),
            lf_account_id: r.get("lf_account_id"),
            lf_name: r.get("lf_name"),
            preferred_name_source: r.get("preferred_name_source"),
        })
        .collect())
}
