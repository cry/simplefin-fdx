use lunchflow::models::{Account, Balance, Holding, Transaction};
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

// End of LunchFlow database access functions
