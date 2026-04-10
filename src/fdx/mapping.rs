use chrono::{DateTime, TimeZone, Utc};

use crate::{
    db::{HoldingRow, TransactionRow},
    fdx::{FdxAccount, FdxCurrency, FdxHolding, FdxTransaction},
    state::CachedAccount,
};

pub fn unix_to_rfc3339(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
}

fn parse_amount(s: &str) -> f64 {
    s.parse().unwrap_or(0.0)
}

fn parse_amount_opt(s: Option<&str>) -> Option<f64> {
    s.map(|v| v.parse().unwrap_or(0.0))
}

pub fn cached_account_to_fdx(account: &CachedAccount) -> FdxAccount {
    FdxAccount {
        account_id: account.id.clone(),
        account_type: "OTHER".to_string(),
        display_name: account.name.clone(),
        currency: FdxCurrency {
            currency_code: account.currency.clone(),
        },
        current_balance: parse_amount(&account.balance),
        available_balance: account
            .available_balance
            .as_deref()
            .map(parse_amount),
        balance_date: unix_to_rfc3339(account.balance_date),
    }
}

pub fn transaction_row_to_fdx(row: &TransactionRow) -> FdxTransaction {
    let amount = parse_amount(&row.amount);
    let debit_credit_memo = if amount < 0.0 { "DEBIT" } else { "CREDIT" };
    let status = if row.pending { "PENDING" } else { "POSTED" };

    FdxTransaction {
        transaction_id: row.id.clone(),
        posted_timestamp: unix_to_rfc3339(row.posted),
        transaction_timestamp: row.transacted_at.map(unix_to_rfc3339),
        amount,
        description: row.description.clone(),
        payee: row.payee.clone(),
        memo: row.memo.clone(),
        status,
        debit_credit_memo,
    }
}

pub fn holding_row_to_fdx(row: &HoldingRow) -> FdxHolding {
    FdxHolding {
        holding_id: row.id.clone(),
        currency: FdxCurrency {
            currency_code: row.currency.clone(),
        },
        position_date: unix_to_rfc3339(row.created),
        symbol: row.symbol.clone(),
        description: row.description.clone(),
        market_value: parse_amount_opt(row.market_value.as_deref()),
        cost_basis: parse_amount_opt(row.cost_basis.as_deref()),
        purchase_price: parse_amount_opt(row.purchase_price.as_deref()),
        units: parse_amount_opt(row.shares.as_deref()),
    }
}
