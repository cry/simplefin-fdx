use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::util;

// ---------------------------------------------------------------------------
// Unified Transaction Types
// ---------------------------------------------------------------------------

/// View of a SimpleFIN transaction for unified display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfinTransactionView {
    pub id: String,
    pub posted: i64,
    pub amount: String,
    pub description: String,
    pub payee: Option<String>,
    pub memo: Option<String>,
    pub pending: bool,
}

/// View of a LunchFlow transaction for unified display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LfTransactionView {
    pub id: String,
    pub amount: f64,
    pub currency: String,
    pub date: String,
    pub merchant: Option<String>,
    pub description: Option<String>,
    pub is_pending: bool,
}

/// Unified transaction with reconciliation status from both SimpleFIN and LunchFlow.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pool: &sqlx::SqlitePool,
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
// User Reconciliation Rules
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
    pool: &sqlx::SqlitePool,
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
    pool: &sqlx::SqlitePool,
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

pub async fn delete_user_account_rule(
    pool: &sqlx::SqlitePool,
    id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM user_account_reconciliation WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Account Match Summary (for the management UI)
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
// Name Preference
// ---------------------------------------------------------------------------

/// Returns a map of sfin_account_id → preferred_source for all rows where
/// preferred_source = 'lunchflow'. Rows defaulting to 'simplefin' are omitted
/// so callers only need to check for presence.
pub async fn get_lf_name_preferences(
    pool: &sqlx::SqlitePool,
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
    pool: &sqlx::SqlitePool,
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
pub async fn get_account_matches(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<AccountMatchRow>, sqlx::Error> {
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
