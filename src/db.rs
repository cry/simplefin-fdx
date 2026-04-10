use sqlx::{Row, SqlitePool};

use crate::state::CachedAccount;

/// A transaction row read back from SQLite.
pub struct TransactionRow {
    pub id: String,
    pub posted: i64,
    pub amount: String,
    pub description: String,
    pub payee: Option<String>,
    pub memo: Option<String>,
    pub transacted_at: Option<i64>,
    pub pending: bool,
}

/// A holding row read back from SQLite.
pub struct HoldingRow {
    pub id: String,
    pub created: i64,
    pub currency: String,
    pub cost_basis: Option<String>,
    pub description: Option<String>,
    pub market_value: Option<String>,
    pub purchase_price: Option<String>,
    pub shares: Option<String>,
    pub symbol: Option<String>,
}

pub async fn upsert_account(pool: &SqlitePool, account: &CachedAccount) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO accounts
            (id, name, currency, balance, balance_date, available_balance, conn_id)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name              = excluded.name,
            currency          = excluded.currency,
            balance           = excluded.balance,
            balance_date      = excluded.balance_date,
            available_balance = excluded.available_balance,
            conn_id           = excluded.conn_id
        "#,
    )
    .bind(&account.id)
    .bind(&account.name)
    .bind(&account.currency)
    .bind(&account.balance)
    .bind(account.balance_date)
    .bind(&account.available_balance)
    .bind(&account.conn_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_transaction(
    pool: &SqlitePool,
    account_id: &str,
    txn: &simplefin::models::Transaction,
) -> Result<(), sqlx::Error> {
    let extra = txn.extra.as_ref().map(|v| v.to_string());
    let is_pending = txn.pending.unwrap_or(txn.posted == 0);
    sqlx::query(
        r#"
        INSERT INTO transactions
            (id, account_id, posted, amount, description, payee, memo,
             transacted_at, pending, extra)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            posted        = excluded.posted,
            amount        = excluded.amount,
            description   = excluded.description,
            payee         = excluded.payee,
            memo          = excluded.memo,
            transacted_at = excluded.transacted_at,
            pending       = excluded.pending,
            extra         = excluded.extra
        "#,
    )
    .bind(&txn.id)
    .bind(account_id)
    .bind(txn.posted)
    .bind(&txn.amount)
    .bind(&txn.description)
    .bind(&txn.payee)
    .bind(&txn.memo)
    .bind(txn.transacted_at)
    .bind(is_pending as i64)
    .bind(extra)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_holding(
    pool: &SqlitePool,
    account_id: &str,
    holding: &simplefin::models::Holding,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO holdings
            (id, account_id, created, currency, cost_basis, description,
             market_value, purchase_price, shares, symbol)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            created        = excluded.created,
            currency       = excluded.currency,
            cost_basis     = excluded.cost_basis,
            description    = excluded.description,
            market_value   = excluded.market_value,
            purchase_price = excluded.purchase_price,
            shares         = excluded.shares,
            symbol         = excluded.symbol
        "#,
    )
    .bind(&holding.id)
    .bind(account_id)
    .bind(holding.created)
    .bind(&holding.currency)
    .bind(&holding.cost_basis)
    .bind(&holding.description)
    .bind(&holding.market_value)
    .bind(&holding.purchase_price)
    .bind(&holding.shares)
    .bind(&holding.symbol)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_transactions(
    pool: &SqlitePool,
    account_id: &str,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
) -> Result<Vec<TransactionRow>, sqlx::Error> {
    let start = start_ts.unwrap_or(0);
    let end = end_ts.unwrap_or(i64::MAX);

    let rows = sqlx::query(
        r#"
        SELECT id, posted, amount, description, payee, memo, transacted_at, pending
        FROM transactions
        WHERE account_id = ? AND (pending = 1 OR (posted >= ? AND posted <= ?))
        ORDER BY pending DESC, posted DESC
        "#,
    )
    .bind(account_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| TransactionRow {
            id: row.get("id"),
            posted: row.get("posted"),
            amount: row.get("amount"),
            description: row.get("description"),
            payee: row.get("payee"),
            memo: row.get("memo"),
            transacted_at: row.get("transacted_at"),
            pending: row.get::<i64, _>("pending") != 0,
        })
        .collect())
}

pub async fn get_holdings(
    pool: &SqlitePool,
    account_id: &str,
) -> Result<Vec<HoldingRow>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT id, account_id, created, currency, cost_basis, description,
               market_value, purchase_price, shares, symbol
        FROM holdings
        WHERE account_id = ?
        ORDER BY symbol ASC, id ASC
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| HoldingRow {
            id: row.get("id"),
            created: row.get("created"),
            currency: row.get("currency"),
            cost_basis: row.get("cost_basis"),
            description: row.get("description"),
            market_value: row.get("market_value"),
            purchase_price: row.get("purchase_price"),
            shares: row.get("shares"),
            symbol: row.get("symbol"),
        })
        .collect())
}

pub async fn load_accounts(pool: &SqlitePool) -> Result<Vec<CachedAccount>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, currency, balance, balance_date, available_balance, conn_id \
         FROM accounts",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CachedAccount {
            id: row.get("id"),
            name: row.get("name"),
            currency: row.get("currency"),
            balance: row.get("balance"),
            balance_date: row.get("balance_date"),
            available_balance: row.get("available_balance"),
            conn_id: row.get("conn_id"),
        })
        .collect())
}

pub async fn get_config(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM config WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("value")))
}

pub async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO config (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
