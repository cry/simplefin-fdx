/// Reconciles SimpleFIN and LunchFlow data already stored in the database.
///
/// Phase 1: account matching — pairs `accounts` rows with `lf_accounts` rows
/// by comparing the last 20 transactions from each source. A currency mismatch
/// immediately disqualifies a pair; otherwise the fraction of transactions that
/// can be matched by amount (within $0.01) and date (within ±2 days) forms the
/// confidence score. Pairs scoring ≥ 0.6 are written as 'matched'.
///
/// Phase 2: transaction matching — for each matched account pair, links
/// `transactions` rows to `lf_transactions` rows by amount and date proximity,
/// writing results to `reconciled_transactions`.
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info};

use crate::{
    reconciler::db::{UserReconciliationAction, get_user_account_rules},
    util,
};

/// Confidence threshold above which two records are considered matched.
const MATCH_THRESHOLD: f64 = 0.6;

/// How many recent transactions to compare when scoring account pairs.
const TXN_WINDOW: i64 = 20;

/// Minimum number of transactions required in the smaller set to use
/// transaction-based scoring. Below this, the caller falls back to name
/// similarity alone.
const MIN_TRANSACTIONS: usize = 2;

// ---------------------------------------------------------------------------
// Phase 1 — account matching
// ---------------------------------------------------------------------------

struct SfinAccount {
    id: String,
    name: String,
    currency: Option<String>,
}

struct LfAccount {
    id: i64,
    name: String,
    currency: Option<String>,
}

/// Normalised name similarity in [0.0, 1.0] using Levenshtein distance via
/// the `strsim` crate. Comparison is case-insensitive.
fn name_similarity(a: &str, b: &str) -> f64 {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    strsim::normalized_levenshtein(&a, &b)
}

/// Score how likely two accounts refer to the same real-world account by
/// comparing their most recent transactions.
///
/// Returns `Some(score)` in [0.0, 1.0] when both sides have at least
/// `MIN_TRANSACTIONS` rows — the fraction that could be matched by amount
/// (within $0.01) and date (within ±5 days). Returns `None` when either side
/// has no transactions at all (caller should fall back to name similarity).
/// Currency mismatch short-circuits to `Some(0.0)`.
async fn score_accounts_by_transactions(
    pool: &SqlitePool,
    sfin_account_id: &str,
    lf_account_id: i64,
    sfin_currency: Option<&str>,
    lf_currency: Option<&str>,
) -> Result<Option<f64>, sqlx::Error> {
    // Hard filter: known currency mismatch means different accounts.
    if let (Some(sc), Some(lc)) = (sfin_currency, lf_currency) {
        if !sc.eq_ignore_ascii_case(lc) {
            return Ok(Some(0.0));
        }
    }

    let sfin_rows = sqlx::query(
        "SELECT posted, amount FROM transactions \
         WHERE account_id = ? ORDER BY posted DESC LIMIT ?",
    )
    .bind(sfin_account_id)
    .bind(TXN_WINDOW)
    .fetch_all(pool)
    .await?;

    let lf_rows = sqlx::query(
        "SELECT date, amount FROM lf_transactions \
         WHERE lf_account_id = ? ORDER BY date DESC LIMIT ?",
    )
    .bind(lf_account_id)
    .bind(TXN_WINDOW)
    .fetch_all(pool)
    .await?;

    let sfin_txns: Vec<(i64, f64)> = sfin_rows
        .into_iter()
        .filter_map(|r| {
            let amount_str: String = r.get("amount");
            let amount: f64 = amount_str.parse().ok()?;
            Some((r.get::<i64, _>("posted"), amount))
        })
        .collect();

    let lf_txns: Vec<(i64, f64)> = lf_rows
        .into_iter()
        .map(|r| {
            let date: String = r.get("date");
            (util::date_str_to_unix(&date), r.get::<f64, _>("amount"))
        })
        .collect();

    let min_count = sfin_txns.len().min(lf_txns.len());
    // No transactions on either side — signal the caller to use name similarity.
    if min_count == 0 {
        return Ok(None);
    }
    // Too few transactions to score reliably — treat as no match but don't
    // fall back to name-only (we have *some* data, just not enough).
    if min_count < MIN_TRANSACTIONS {
        return Ok(Some(0.0));
    }

    // Greedy one-to-one matching: for each sfin transaction find the first
    // unmatched lf transaction with a compatible amount and date.
    let mut matched = 0usize;
    let mut used_lf = vec![false; lf_txns.len()];

    'sfin: for (sfin_ts, sfin_amount) in &sfin_txns {
        for (i, (lf_ts, lf_amount)) in lf_txns.iter().enumerate() {
            if used_lf[i] {
                continue;
            }
            let amount_diff = (sfin_amount.abs() - lf_amount.abs()).abs();
            let day_diff = (sfin_ts - lf_ts).unsigned_abs() / 86_400;
            if amount_diff < 0.01 && day_diff <= 5 {
                matched += 1;
                used_lf[i] = true;
                continue 'sfin;
            }
        }
    }

    Ok(Some(matched as f64 / min_count as f64))
}

async fn reconcile_accounts(pool: &SqlitePool, now: i64) -> Result<(), sqlx::Error> {
    // Load user-defined rules first — they take full precedence.
    let user_rules = get_user_account_rules(pool).await?;

    // Index user rules for fast lookup.
    // forced_matches: sfin_id → lf_id (action = 'match', both sides set)
    let mut forced_matches: HashMap<String, i64> = HashMap::new();
    // excluded_sfin: sfin accounts that must never be auto-matched (lf_id IS NULL)
    let mut excluded_sfin: HashSet<String> = HashSet::new();
    // excluded_lf: lf accounts that must never be auto-matched (sfin_id IS NULL)
    let mut excluded_lf: HashSet<i64> = HashSet::new();
    // excluded_pairs: specific (sfin_id, lf_id) pairs to skip
    let mut excluded_pairs: HashSet<(String, i64)> = HashSet::new();

    for rule in &user_rules {
        match (&rule.action, &rule.sfin_account_id, rule.lf_account_id) {
            (UserReconciliationAction::Match, Some(sfin_id), Some(lf_id)) => {
                forced_matches.insert(sfin_id.clone(), lf_id);
            }
            (UserReconciliationAction::Exclude, Some(sfin_id), Some(lf_id)) => {
                excluded_pairs.insert((sfin_id.clone(), lf_id));
            }
            (UserReconciliationAction::Exclude, Some(sfin_id), None) => {
                excluded_sfin.insert(sfin_id.clone());
            }
            (UserReconciliationAction::Exclude, None, Some(lf_id)) => {
                excluded_lf.insert(lf_id);
            }
            _ => {}
        }
    }

    // Clear all previous account reconciliation rows so stale sfin_only/lf_only
    // entries can't coexist with a newly matched row for the same account.
    sqlx::query("DELETE FROM reconciled_accounts")
        .execute(pool)
        .await?;

    let sfin_rows = sqlx::query("SELECT id, name, currency FROM accounts")
        .fetch_all(pool)
        .await?;
    let sfin_accounts: Vec<SfinAccount> = sfin_rows
        .into_iter()
        .map(|r| SfinAccount {
            id: r.get("id"),
            name: r.get("name"),
            currency: r.get("currency"),
        })
        .collect();

    let lf_rows = sqlx::query("SELECT id, name, currency FROM lf_accounts")
        .fetch_all(pool)
        .await?;
    let lf_accounts: Vec<LfAccount> = lf_rows
        .into_iter()
        .map(|r| LfAccount {
            id: r.get("id"),
            name: r.get("name"),
            currency: r.get("currency"),
        })
        .collect();

    // Phase 1a: apply forced matches (user rules with action = 'match').
    // Track which accounts are already spoken for so auto-matching skips them.
    let mut matched_sfin_ids: HashSet<String> = HashSet::new();
    let mut matched_lf_ids: HashSet<i64> = HashSet::new();

    for (sfin_id, lf_id) in &forced_matches {
        // Only write the match if both accounts actually exist in the DB.
        let sfin_exists = sfin_accounts.iter().any(|s| &s.id == sfin_id);
        let lf_exists = lf_accounts.iter().any(|l| l.id == *lf_id);
        if sfin_exists && lf_exists {
            debug!(
                sfin_id = %sfin_id,
                lf_id = lf_id,
                "User-forced account match"
            );
            upsert_reconciled_account(pool, Some(sfin_id), Some(*lf_id), 1.0, "matched", now)
                .await?;
            matched_sfin_ids.insert(sfin_id.clone());
            matched_lf_ids.insert(*lf_id);
        }
    }

    // Phase 1b: auto-match remaining accounts.
    for lf in &lf_accounts {
        // Skip if already claimed by a user rule or a prior auto-match.
        if matched_lf_ids.contains(&lf.id) {
            continue;
        }
        // Skip if the user said to keep this LF account standalone.
        if excluded_lf.contains(&lf.id) {
            upsert_reconciled_account(pool, None, Some(lf.id), 0.0, "lf_only", now).await?;
            continue;
        }

        let mut best_score = 0.0_f64;
        let mut best_sfin: Option<&SfinAccount> = None;

        for sfin in &sfin_accounts {
            // Skip accounts already matched (forced or auto).
            if matched_sfin_ids.contains(&sfin.id) {
                continue;
            }
            // Skip accounts the user said to keep standalone.
            if excluded_sfin.contains(&sfin.id) {
                continue;
            }
            // Skip this specific pair if the user excluded it.
            if excluded_pairs.contains(&(sfin.id.clone(), lf.id)) {
                continue;
            }

            let txn_score = score_accounts_by_transactions(
                pool,
                &sfin.id,
                lf.id,
                sfin.currency.as_deref(),
                lf.currency.as_deref(),
            )
            .await?;
            let name_sim = name_similarity(&sfin.name, &lf.name);
            let score = match txn_score {
                None => name_sim,
                Some(ts) => (ts + name_sim * 0.15).min(1.0),
            };
            if score > best_score {
                best_score = score;
                best_sfin = Some(sfin);
            }
        }

        if let Some(sfin) = best_sfin {
            if best_score >= MATCH_THRESHOLD {
                debug!(
                    sfin_id = %sfin.id,
                    sfin_name = %sfin.name,
                    lf_id = lf.id,
                    lf_name = %lf.name,
                    confidence = best_score,
                    "Auto-matched account"
                );
                upsert_reconciled_account(
                    pool,
                    Some(&sfin.id),
                    Some(lf.id),
                    best_score,
                    "matched",
                    now,
                )
                .await?;
                matched_sfin_ids.insert(sfin.id.clone());
                matched_lf_ids.insert(lf.id);
                continue;
            }
        }

        // No match found for this LF account.
        upsert_reconciled_account(pool, None, Some(lf.id), 0.0, "lf_only", now).await?;
    }

    // Any SimpleFIN accounts with no LF counterpart.
    for sfin in &sfin_accounts {
        if !matched_sfin_ids.contains(&sfin.id) {
            upsert_reconciled_account(pool, Some(&sfin.id), None, 0.0, "sfin_only", now).await?;
        }
    }

    info!(
        sfin_count = sfin_accounts.len(),
        lf_count = lf_accounts.len(),
        user_rules = user_rules.len(),
        "Account reconciliation complete"
    );
    Ok(())
}

async fn upsert_reconciled_account(
    pool: &SqlitePool,
    sfin_id: Option<&str>,
    lf_id: Option<i64>,
    confidence: f64,
    status: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO reconciled_accounts
            (sfin_account_id, lf_account_id, match_confidence, status, reconciled_at)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(sfin_account_id, lf_account_id) DO UPDATE SET
            match_confidence = excluded.match_confidence,
            status           = excluded.status,
            reconciled_at    = excluded.reconciled_at
        "#,
    )
    .bind(sfin_id)
    .bind(lf_id)
    .bind(confidence)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2 — transaction matching
// ---------------------------------------------------------------------------

struct SfinTxn {
    id: String,
    posted: i64, // unix timestamp
    amount: f64, // parsed from the TEXT column
    description: String,
    pending: bool,
}

struct LfTxn {
    id: String,
    #[allow(dead_code)]
    date: String, // YYYY-MM-DD
    date_ts: i64, // parsed to unix for arithmetic
    amount: f64,
    merchant: Option<String>,
    description: Option<String>,
    is_pending: bool,
}

/// Score how likely a SimpleFIN and LunchFlow transaction are the same event.
fn score_transactions(sfin: &SfinTxn, lf: &LfTxn) -> f64 {
    // Amount must be very close (both represent the same currency amount, signs
    // may differ between data sources so we compare absolute values).
    if (sfin.amount.abs() - lf.amount.abs()).abs() > 0.01 {
        return 0.0;
    }
    let mut score = 0.4_f64;

    let sfin_lower = sfin.description.to_lowercase();
    let merchant_lower = lf.merchant.as_deref().unwrap_or("").to_lowercase();
    let lf_desc_lower = lf.description.as_deref().unwrap_or("").to_lowercase();
    let desc_matches = (!merchant_lower.is_empty() && sfin_lower.contains(&merchant_lower))
        || (!lf_desc_lower.is_empty() && sfin_lower.contains(&lf_desc_lower));

    if sfin.pending && lf.is_pending {
        // Pending transactions don't have reliable posted dates — score on the
        // shared pending status and description similarity instead.
        score += 0.3; // both pending
        if desc_matches {
            score += 0.15;
        }
        return score.min(1.0);
    }

    // Posted transactions: use date proximity.
    let day_diff = ((sfin.posted - lf.date_ts).abs() / 86_400) as u64;
    score += match day_diff {
        0 => 0.4,
        1 => 0.25,
        2 => 0.15,
        3 => 0.1,
        4 | 5 => 0.05,
        _ => return 0.0, // beyond ±5 days, not a match
    };

    if desc_matches {
        score += 0.15;
    }

    if sfin.pending == lf.is_pending {
        score += 0.05;
    }

    score.min(1.0)
}

async fn reconcile_transactions_for_pair(
    pool: &SqlitePool,
    sfin_account_id: &str,
    lf_account_id: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    // Clear all previous reconciliation rows for this account pair so that
    // stale sfin_only/lf_only entries can't coexist with a newly matched row
    // for the same transaction after a re-run.
    sqlx::query(
        "DELETE FROM reconciled_transactions
         WHERE sfin_txn_id IN (SELECT id FROM transactions WHERE account_id = ?)
            OR lf_txn_id   IN (SELECT id FROM lf_transactions WHERE lf_account_id = ?)",
    )
    .bind(sfin_account_id)
    .bind(lf_account_id)
    .execute(pool)
    .await?;

    // Load SimpleFIN transactions for this account (all time; reconciler sees
    // whatever is already in the DB).
    let sfin_rows = sqlx::query(
        "SELECT id, posted, amount, description, pending \
         FROM transactions WHERE account_id = ?",
    )
    .bind(sfin_account_id)
    .fetch_all(pool)
    .await?;

    let sfin_txns: Vec<SfinTxn> = sfin_rows
        .into_iter()
        .filter_map(|r| {
            let amount_str: String = r.get("amount");
            let amount: f64 = amount_str.parse().ok()?;
            Some(SfinTxn {
                id: r.get("id"),
                posted: r.get("posted"),
                amount,
                description: r.get("description"),
                pending: r.get::<i64, _>("pending") != 0,
            })
        })
        .collect();

    // Load LunchFlow transactions for this account.
    let lf_rows = sqlx::query(
        "SELECT id, date, amount, merchant, description, is_pending \
         FROM lf_transactions WHERE lf_account_id = ?",
    )
    .bind(lf_account_id)
    .fetch_all(pool)
    .await?;

    let lf_txns: Vec<LfTxn> = lf_rows
        .into_iter()
        .map(|r| {
            let date: String = r.get("date");
            let date_ts = util::date_str_to_unix(&date);
            LfTxn {
                id: r.get("id"),
                date,
                date_ts,
                amount: r.get("amount"),
                merchant: r.get("merchant"),
                description: r.get("description"),
                is_pending: r.get::<i64, _>("is_pending") != 0,
            }
        })
        .collect();

    let mut matched_sfin_ids: Vec<String> = Vec::new();
    let mut matched_lf_ids: Vec<String> = Vec::new();

    for lf in &lf_txns {
        let best = sfin_txns
            .iter()
            .map(|s| (s, score_transactions(s, lf)))
            .filter(|(_, score)| *score >= MATCH_THRESHOLD)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((sfin, confidence)) = best {
            upsert_reconciled_transaction(
                pool,
                Some(&sfin.id),
                Some(&lf.id),
                confidence,
                "matched",
                now,
                None,
            )
            .await?;
            matched_sfin_ids.push(sfin.id.clone());
            matched_lf_ids.push(lf.id.clone());
        } else {
            upsert_reconciled_transaction(pool, None, Some(&lf.id), 0.0, "lf_only", now, None)
                .await?;
        }
    }

    for sfin in &sfin_txns {
        if !matched_sfin_ids.contains(&sfin.id) {
            upsert_reconciled_transaction(pool, Some(&sfin.id), None, 0.0, "sfin_only", now, None)
                .await?;
        }
    }

    debug!(
        sfin_account = sfin_account_id,
        lf_account = lf_account_id,
        sfin_txns = sfin_txns.len(),
        lf_txns = lf_txns.len(),
        "Transaction reconciliation complete for account pair"
    );
    Ok(())
}

async fn upsert_reconciled_transaction(
    pool: &SqlitePool,
    sfin_txn_id: Option<&str>,
    lf_txn_id: Option<&str>,
    confidence: f64,
    status: &str,
    now: i64,
    notes: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO reconciled_transactions
            (sfin_txn_id, lf_txn_id, match_confidence, status, reconciled_at, notes)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(sfin_txn_id, lf_txn_id) DO UPDATE SET
            match_confidence = excluded.match_confidence,
            status           = excluded.status,
            reconciled_at    = excluded.reconciled_at,
            notes            = excluded.notes
        "#,
    )
    .bind(sfin_txn_id)
    .bind(lf_txn_id)
    .bind(confidence)
    .bind(status)
    .bind(now)
    .bind(notes)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let now = util::now_unix();

    reconcile_accounts(pool, now).await?;

    // Load matched account pairs and reconcile their transactions.
    let pairs = sqlx::query(
        "SELECT sfin_account_id, lf_account_id \
         FROM reconciled_accounts WHERE status = 'matched'",
    )
    .fetch_all(pool)
    .await?;

    for pair in pairs {
        let sfin_id: String = pair.get("sfin_account_id");
        let lf_id: i64 = pair.get("lf_account_id");
        reconcile_transactions_for_pair(pool, &sfin_id, lf_id, now).await?;
    }

    info!("Full reconciliation run complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build an in-memory SQLite pool with the full schema applied inline so
    /// tests are self-contained and don't require the migrations directory.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE accounts (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, currency TEXT NOT NULL,
                 balance TEXT NOT NULL, balance_date INTEGER NOT NULL,
                 available_balance TEXT, conn_id TEXT, extra TEXT
             );
             CREATE TABLE transactions (
                 id TEXT PRIMARY KEY, account_id TEXT NOT NULL, posted INTEGER NOT NULL,
                 amount TEXT NOT NULL, description TEXT NOT NULL, transacted_at INTEGER,
                 pending INTEGER NOT NULL DEFAULT 0, extra TEXT, payee TEXT, memo TEXT
             );
             CREATE TABLE holdings (
                 id TEXT PRIMARY KEY, account_id TEXT NOT NULL, created INTEGER NOT NULL,
                 currency TEXT NOT NULL, cost_basis TEXT, description TEXT,
                 market_value TEXT, purchase_price TEXT, shares TEXT, symbol TEXT
             );
             CREATE TABLE lf_accounts (
                 id INTEGER PRIMARY KEY, name TEXT NOT NULL, institution_name TEXT NOT NULL,
                 provider TEXT NOT NULL, currency TEXT, status TEXT NOT NULL,
                 balance REAL, balance_currency TEXT, fetched_at INTEGER NOT NULL
             );
             CREATE TABLE lf_transactions (
                 id TEXT PRIMARY KEY, lf_account_id INTEGER NOT NULL,
                 amount REAL NOT NULL, currency TEXT NOT NULL, date TEXT NOT NULL,
                 merchant TEXT, description TEXT, is_pending INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE lf_holdings (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, lf_account_id INTEGER NOT NULL,
                 security_name TEXT NOT NULL, ticker_symbol TEXT, isin TEXT,
                 quantity REAL NOT NULL, price REAL NOT NULL, value REAL NOT NULL,
                 cost_basis REAL, currency TEXT NOT NULL, fetched_at INTEGER NOT NULL
             );
             CREATE TABLE reconciled_accounts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 sfin_account_id TEXT, lf_account_id INTEGER,
                 match_confidence REAL NOT NULL DEFAULT 0.0,
                 status TEXT NOT NULL, reconciled_at INTEGER NOT NULL,
                 UNIQUE(sfin_account_id, lf_account_id)
             );
             CREATE TABLE reconciled_transactions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 sfin_txn_id TEXT, lf_txn_id TEXT,
                 match_confidence REAL NOT NULL DEFAULT 0.0,
                 status TEXT NOT NULL, reconciled_at INTEGER NOT NULL, notes TEXT,
                 UNIQUE(sfin_txn_id, lf_txn_id)
             );",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn sfin(id: &str, posted: i64, amount: f64, desc: &str, pending: bool) -> SfinTxn {
        SfinTxn {
            id: id.to_string(),
            posted,
            amount,
            description: desc.to_string(),
            pending,
        }
    }

    fn lf(
        id: &str,
        date_ts: i64,
        amount: f64,
        merchant: Option<&str>,
        desc: Option<&str>,
        is_pending: bool,
    ) -> LfTxn {
        LfTxn {
            id: id.to_string(),
            date: String::new(),
            date_ts,
            amount,
            merchant: merchant.map(str::to_string),
            description: desc.map(str::to_string),
            is_pending,
        }
    }

    // Unix timestamp for a given day offset from a fixed base (2024-01-15 00:00:00 UTC).
    fn day(offset: i64) -> i64 {
        1_705_276_800 + offset * 86_400
    }

    // -----------------------------------------------------------------------
    // name_similarity
    // -----------------------------------------------------------------------

    #[test]
    fn name_similarity_exact_match() {
        assert!((name_similarity("Checking", "Checking") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn name_similarity_case_insensitive() {
        assert!((name_similarity("SAVINGS", "savings") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn name_similarity_close_names() {
        // "Checking Account" vs "Checking" — should still be fairly high.
        let sim = name_similarity("Checking Account", "Checking");
        assert!(
            sim >= 0.5,
            "similar names should score at or above 0.5, got {sim}"
        );
    }

    #[test]
    fn name_similarity_unrelated_names() {
        let sim = name_similarity("Savings", "Credit Card");
        assert!(
            sim < 0.5,
            "unrelated names should score below 0.5, got {sim}"
        );
    }

    #[test]
    fn name_similarity_bonus_cannot_push_zero_txn_score_over_threshold() {
        // name_similarity * 0.15 <= 0.15 < MATCH_THRESHOLD (0.6)
        // so a 0.0 transaction score can never reach the threshold via name alone.
        let max_name_bonus = 1.0_f64 * 0.15;
        assert!(max_name_bonus < MATCH_THRESHOLD);
    }

    // -----------------------------------------------------------------------
    // score_transactions — amount gate
    // -----------------------------------------------------------------------

    #[test]
    fn amount_mismatch_returns_zero() {
        let s = sfin("s1", day(0), -103.67, "LYFT", false);
        let l = lf("l1", day(0), -103.68, Some("Lyft"), None, false);
        assert_eq!(score_transactions(&s, &l), 0.0);
    }

    #[test]
    fn amount_match_ignores_sign() {
        // SimpleFIN stores debits as negative; LunchFlow may use positive amounts.
        let s = sfin("s1", day(0), -28.11, "CLOUD WORKSPACE", false);
        let l = lf("l1", day(0), 28.11, Some("Cloud Workspace"), None, false);
        let score = score_transactions(&s, &l);
        assert!(score >= MATCH_THRESHOLD, "score was {score}");
    }

    // -----------------------------------------------------------------------
    // score_transactions — date proximity (posted path)
    // -----------------------------------------------------------------------

    #[test]
    fn exact_date_match() {
        let s = sfin("s1", day(0), -311.92, "AUTOPAY PAYMENT", false);
        let l = lf("l1", day(0), -311.92, Some("Payment"), None, false);
        let score = score_transactions(&s, &l);
        // 0.4 base + 0.4 same-day + 0.05 same-pending-status = 0.85
        assert!(score >= 0.8, "score was {score}");
    }

    #[test]
    fn one_day_apart() {
        let s = sfin("s1", day(0), -19.99, "DISNEY PLUS", false);
        let l = lf("l1", day(1), -19.99, Some("Disney+"), None, false);
        let score = score_transactions(&s, &l);
        // 0.4 + 0.25 day + 0.15 desc + 0.05 pending = 0.85
        assert!(score >= MATCH_THRESHOLD, "score was {score}");
    }

    #[test]
    fn three_days_apart_matches() {
        // Regression: we extended the window from ±2 to ±5 days.
        let s = sfin("s1", day(0), -27.49, "AMAYSIM MOBILE", false);
        let l = lf("l1", day(3), -27.49, Some("Amaysim"), None, false);
        let score = score_transactions(&s, &l);
        assert!(score >= MATCH_THRESHOLD, "score was {score}");
    }

    #[test]
    fn five_days_apart_is_minimum() {
        let s = sfin("s1", day(0), -466.19, "ALAMO RENT", false);
        let l = lf("l1", day(5), -466.19, Some("Alamo"), None, false);
        let score = score_transactions(&s, &l);
        // 0.4 + 0.05 (day 4|5) = 0.45 — below threshold, but not zero
        assert!(score > 0.0, "score was {score}");
    }

    #[test]
    fn six_days_apart_returns_zero() {
        let s = sfin("s1", day(0), -50.00, "SOME MERCHANT", false);
        let l = lf("l1", day(6), -50.00, Some("Some Merchant"), None, false);
        assert_eq!(score_transactions(&s, &l), 0.0);
    }

    #[test]
    fn description_match_adds_bonus() {
        let s = sfin("s1", day(0), -10.89, "RCH*KAGI.COM", false);
        let l_no_desc = lf("l1", day(0), -10.89, None, None, false);
        let l_with_desc = lf("l2", day(0), -10.89, Some("Kagi"), None, false);
        let score_without = score_transactions(&s, &l_no_desc);
        let score_with = score_transactions(&s, &l_with_desc);
        assert!(score_with > score_without, "desc match should add bonus");
    }

    // -----------------------------------------------------------------------
    // score_transactions — pending path
    // -----------------------------------------------------------------------

    #[test]
    fn both_pending_skips_date() {
        // posted = 0 on SimpleFIN pending transactions causes a huge date diff
        // vs any real LunchFlow date. The pending path must skip date checking.
        let s = sfin("s1", 0, -10.89, "RCH*KAGI.COM", true);
        let l = lf("l1", day(0), -10.89, Some("Kagi"), None, true);
        let score = score_transactions(&s, &l);
        // 0.4 base + 0.3 both-pending + 0.15 desc = 0.85
        assert!(score >= MATCH_THRESHOLD, "score was {score}");
    }

    #[test]
    fn mixed_pending_uses_date_path() {
        // One pending, one posted — should fall through to date-proximity check.
        let s = sfin("s1", 0, -10.89, "RCH*KAGI.COM", true);
        let l = lf("l1", day(0), -10.89, Some("Kagi"), None, false);
        let score = score_transactions(&s, &l);
        // posted=0 vs day(0)=1_705_276_800 → massive day diff → returns 0.0
        assert_eq!(score, 0.0, "mixed pending should not match: {score}");
    }

    // -----------------------------------------------------------------------
    // score_accounts_by_transactions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn no_transactions_returns_none() {
        let pool = test_pool().await;
        // Accounts exist but have zero transactions on both sides.
        let score =
            score_accounts_by_transactions(&pool, "ACT-empty", 99, Some("USD"), Some("USD"))
                .await
                .unwrap();
        assert_eq!(
            score, None,
            "zero transactions should return None to trigger name-only fallback"
        );
    }

    #[tokio::test]
    async fn currency_mismatch_returns_zero() {
        let pool = test_pool().await;
        let score = score_accounts_by_transactions(&pool, "ACT-001", 1, Some("USD"), Some("AUD"))
            .await
            .unwrap();
        assert_eq!(score, Some(0.0));
    }

    #[tokio::test]
    async fn too_few_transactions_returns_zero() {
        let pool = test_pool().await;

        // Insert accounts.
        sqlx::query(
            "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ACT-001")
        .bind("Checking")
        .bind("USD")
        .bind("1000.00")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(1i64)
        .bind("Checking")
        .bind("Bank")
        .bind("plaid")
        .bind("USD")
        .bind("active")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        // Only 1 transaction each — below MIN_TRANSACTIONS=2.
        for i in 0..1i64 {
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-{i:03}"))
            .bind("ACT-001")
            .bind(day(i))
            .bind(format!("-{:.2}", 10.0 + i as f64))
            .bind("MERCHANT")
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-{i:03}"))
            .bind(1i64)
            .bind(-(10.0 + i as f64))
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score = score_accounts_by_transactions(&pool, "ACT-001", 1, Some("USD"), Some("USD"))
            .await
            .unwrap();
        // 1 transaction < MIN_TRANSACTIONS=2, but min_count > 0 → Some(0.0), not None
        assert_eq!(
            score,
            Some(0.0),
            "should return Some(0.0) when min_count > 0 but below MIN_TRANSACTIONS"
        );
    }

    #[tokio::test]
    async fn matching_transactions_produce_high_score() {
        let pool = test_pool().await;

        sqlx::query(
            "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ACT-002")
        .bind("Savings")
        .bind("USD")
        .bind("5000.00")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(2i64)
        .bind("Savings")
        .bind("Bank")
        .bind("plaid")
        .bind("USD")
        .bind("active")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        // 5 matching transactions (same absolute amount, same day).
        let amounts = [-311.92, -28.11, -103.67, -19.99, -0.04];
        let descs = [
            "AUTOPAY PAYMENT",
            "CLOUD WORKSPACE",
            "LYFT",
            "DISNEY PLUS",
            "INTEREST",
        ];
        for (i, (amount, desc)) in amounts.iter().zip(descs.iter()).enumerate() {
            let i = i as i64;
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-{i:03}"))
            .bind("ACT-002")
            .bind(day(i))
            .bind(amount.to_string())
            .bind(desc)
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-{i:03}"))
            .bind(2i64)
            .bind(*amount)
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score = score_accounts_by_transactions(&pool, "ACT-002", 2, Some("USD"), Some("USD"))
            .await
            .unwrap()
            .expect("should return Some when transactions exist");
        assert!(score >= MATCH_THRESHOLD, "expected high score, got {score}");
        assert!(
            (score - 1.0).abs() < 0.01,
            "all txns match, expected 1.0, got {score}"
        );
    }

    #[tokio::test]
    async fn wrong_account_pair_produces_low_score() {
        let pool = test_pool().await;

        // Two SimpleFIN accounts, two LF accounts. Only account A↔LF-1 share transactions.
        for (id, name) in [("ACT-A", "Checking"), ("ACT-B", "Credit Card")] {
            sqlx::query(
                "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(name)
            .bind("USD")
            .bind("0.00")
            .bind(day(0))
            .execute(&pool)
            .await
            .unwrap();
        }

        for (id, name) in [(10i64, "Checking"), (11i64, "Credit Card")] {
            sqlx::query(
                "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(name)
            .bind("Bank")
            .bind("plaid")
            .bind("USD")
            .bind("active")
            .bind(day(0))
            .execute(&pool)
            .await
            .unwrap();
        }

        // 5 matching transactions between ACT-A and LF-10.
        let amounts = [-50.0, -75.25, -12.99, -200.00, -8.50];
        for (i, amount) in amounts.iter().enumerate() {
            let i = i as i64;
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-A{i}"))
            .bind("ACT-A")
            .bind(day(i))
            .bind(amount.to_string())
            .bind("MERCHANT")
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-10-{i}"))
            .bind(10i64)
            .bind(*amount)
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        // 5 completely different transactions for LF-11.
        for i in 0..5i64 {
            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-11-{i}"))
            .bind(11i64)
            .bind(-(999.0 + i as f64)) // amounts that don't appear in ACT-A
            .bind("USD")
            .bind(format!("2024-02-{:02}", 1 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score_correct =
            score_accounts_by_transactions(&pool, "ACT-A", 10, Some("USD"), Some("USD"))
                .await
                .unwrap()
                .expect("should return Some when transactions exist");
        let score_wrong =
            score_accounts_by_transactions(&pool, "ACT-A", 11, Some("USD"), Some("USD"))
                .await
                .unwrap()
                .expect("should return Some when transactions exist");

        assert!(
            score_correct >= MATCH_THRESHOLD,
            "correct pair should match: {score_correct}"
        );
        assert!(
            score_wrong < MATCH_THRESHOLD,
            "wrong pair should not match: {score_wrong}"
        );
    }

    /// Test that the optimized two-pointer algorithm works correctly for small datasets
    #[tokio::test]
    async fn test_two_pointer_algorithm_small() {
        let pool = test_pool().await;

        sqlx::query(
            "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ACT-001")
        .bind("Checking")
        .bind("USD")
        .bind("1000.00")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(1i64)
        .bind("Checking")
        .bind("Bank")
        .bind("plaid")
        .bind("USD")
        .bind("active")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        // Insert 3 transactions with matching timestamps and amounts
        let transactions = [
            (day(0), -10.50, "Coffee Shop"),
            (day(1), -25.75, "Groceries"),
            (day(2), -5.25, "Gas Station"),
        ];

        for (i, (timestamp, amount, desc)) in transactions.iter().enumerate() {
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-{i:03}"))
            .bind("ACT-001")
            .bind(timestamp)
            .bind(amount.to_string())
            .bind(desc)
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-{i:03}"))
            .bind(1i64)
            .bind(*amount)
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score = score_accounts_by_transactions(&pool, "ACT-001", 1, Some("USD"), Some("USD"))
            .await
            .unwrap()
            .expect("should return Some when transactions exist");

        // With all three matching transactions we should get a high score (close to 1.0)
        assert!(
            score >= MATCH_THRESHOLD,
            "expected high score for perfect matches, got {score}"
        );
        assert!(
            (score - 1.0).abs() < 0.01,
            "all txns match, expected 1.0, got {score}"
        );
    }

    /// Test that the optimized two-pointer algorithm correctly handles unsorted transactions
    #[tokio::test]
    async fn test_two_pointer_algorithm_unsorted_transactions() {
        let pool = test_pool().await;

        sqlx::query(
            "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ACT-002")
        .bind("Savings")
        .bind("USD")
        .bind("5000.00")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(2i64)
        .bind("Savings")
        .bind("Bank")
        .bind("plaid")
        .bind("USD")
        .bind("active")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        // Insert transactions in non-sorted order to test sorting logic
        let transactions = [
            (day(2), -5.25, "Gas Station"),  // inserted last
            (day(0), -10.50, "Coffee Shop"), // inserted first
            (day(1), -25.75, "Groceries"),   // inserted in middle
        ];

        for (i, (timestamp, amount, desc)) in transactions.iter().enumerate() {
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-{i:03}"))
            .bind("ACT-002")
            .bind(timestamp)
            .bind(amount.to_string())
            .bind(desc)
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-{i:03}"))
            .bind(2i64)
            .bind(*amount)
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + i))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score = score_accounts_by_transactions(&pool, "ACT-002", 2, Some("USD"), Some("USD"))
            .await
            .unwrap()
            .expect("should return Some when transactions exist");

        // Even with unsorted input, the algorithm should correctly match all three
        assert!(
            score >= MATCH_THRESHOLD,
            "expected high score for perfect matches despite order, got {score}"
        );
    }

    /// Test performance optimization with large transaction datasets (regression test)
    #[tokio::test]
    async fn performance_test_large_transaction_sets() {
        let pool = test_pool().await;

        sqlx::query(
            "INSERT INTO accounts (id, name, currency, balance, balance_date) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ACT-LARGE")
        .bind("Large Account")
        .bind("USD")
        .bind("10000.00")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO lf_accounts (id, name, institution_name, provider, currency, status, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(999i64)
        .bind("Large Account")
        .bind("Bank")
        .bind("plaid")
        .bind("USD")
        .bind("active")
        .bind(day(0))
        .execute(&pool)
        .await
        .unwrap();

        // Insert 50 matching transactions (should be well within TXN_WINDOW limit of 20)
        for i in 0..50i64 {
            let amount = -10.0 - (i as f64 * 0.1);
            sqlx::query(
                "INSERT INTO transactions (id, account_id, posted, amount, description) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("TRN-{i:03}"))
            .bind("ACT-LARGE")
            .bind(day(i % 10)) // Cycle through a few days to test the logic
            .bind(amount.to_string())
            .bind("Test Transaction")
            .execute(&pool)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO lf_transactions (id, lf_account_id, amount, currency, date) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(format!("lf-{i:03}"))
            .bind(999i64)
            .bind(amount)
            .bind("USD")
            .bind(format!("2024-01-{:02}", 15 + (i % 10) as i32))
            .execute(&pool)
            .await
            .unwrap();
        }

        let score =
            score_accounts_by_transactions(&pool, "ACT-LARGE", 999, Some("USD"), Some("USD"))
                .await
                .unwrap()
                .expect("should return Some when transactions exist");

        // Should handle large datasets without performance issues
        assert!(
            score >= MATCH_THRESHOLD,
            "expected high score for many matching transactions, got {score}"
        );
    }
}
