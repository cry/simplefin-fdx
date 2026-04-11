use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use tracing::warn;

use crate::{
    fdx::{
        FdxAccount, FdxCurrency, FdxHolding, FdxTransaction, LfTransactionExt, SfinTransactionExt,
    },
    lunchflow::db::{LfAccountFull, UnifiedTransaction},
    simplefin::db::{HoldingRow, TransactionRow},
    state::CachedAccount,
};

pub fn unix_to_rfc3339(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
}

fn date_to_rfc3339(date: &str) -> String {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().to_rfc3339())
        .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string())
}

fn parse_amount(s: &str) -> f64 {
    s.parse().unwrap_or_else(|_| {
        warn!(
            value = s,
            "Failed to parse amount string, defaulting to 0.0"
        );
        0.0
    })
}

fn parse_amount_opt(s: Option<&str>) -> Option<f64> {
    s.map(|v| {
        v.parse().unwrap_or_else(|_| {
            warn!(
                value = v,
                "Failed to parse optional amount string, defaulting to 0.0"
            );
            0.0
        })
    })
}

/// Map a SimpleFIN cached account to an FDX account. `prefix` is applied to
/// the account ID and must be one of `"SIMPLEFIN-"` or `"REC-"`.
pub fn cached_account_to_fdx(account: &CachedAccount, prefix: &str) -> FdxAccount {
    FdxAccount {
        account_id: format!("{}{}", prefix, account.id),
        account_type: "OTHER".to_string(),
        display_name: account.name.clone(),
        currency: FdxCurrency {
            currency_code: account.currency.clone(),
        },
        current_balance: parse_amount(&account.balance),
        available_balance: account.available_balance.as_deref().map(parse_amount),
        balance_date: unix_to_rfc3339(account.balance_date),
        simplefin_account_id: None,
        lunchflow_account_id: None,
    }
}

/// Map a LunchFlow account to an FDX account with a `LUNCHFLOW-` prefix.
pub fn lf_account_to_fdx(account: &LfAccountFull) -> FdxAccount {
    FdxAccount {
        account_id: format!("LUNCHFLOW-{}", account.id),
        account_type: "OTHER".to_string(),
        display_name: account.name.clone(),
        currency: FdxCurrency {
            currency_code: account
                .currency
                .clone()
                .unwrap_or_else(|| "USD".to_string()),
        },
        current_balance: account.balance.unwrap_or(0.0),
        available_balance: None,
        balance_date: unix_to_rfc3339(account.fetched_at),
        simplefin_account_id: None,
        lunchflow_account_id: None,
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
        reconciliation_status: None,
        match_confidence: None,
        simplefin_transaction: None,
        lunchflow_transaction: None,
    }
}

/// Map a LunchFlow transaction row to an FDX transaction (for `LUNCHFLOW-` accounts).
pub fn lf_transaction_to_fdx(
    id: &str,
    amount: f64,
    date: &str,
    merchant: Option<&str>,
    description: Option<&str>,
    is_pending: bool,
) -> FdxTransaction {
    let debit_credit_memo = if amount < 0.0 { "DEBIT" } else { "CREDIT" };
    let status = if is_pending { "PENDING" } else { "POSTED" };
    FdxTransaction {
        transaction_id: id.to_string(),
        posted_timestamp: date_to_rfc3339(date),
        transaction_timestamp: None,
        amount,
        description: merchant
            .or(description)
            .unwrap_or("(no description)")
            .to_string(),
        payee: merchant.map(str::to_string),
        memo: description.map(str::to_string),
        status,
        debit_credit_memo,
        reconciliation_status: None,
        match_confidence: None,
        simplefin_transaction: None,
        lunchflow_transaction: None,
    }
}

/// Map a unified (reconciled) transaction to an FDX transaction, populated with
/// reconciliation metadata.
///
/// Matched transactions get a `REC-{id}` transaction ID and carry both source
/// transactions nested under `simpleFinTransaction` / `lunchflowTransaction`.
/// Unmatched (sfin_only / lf_only) transactions keep their source ID and do not
/// include the nested source objects.
pub fn unified_transaction_to_fdx(txn: &UnifiedTransaction) -> FdxTransaction {
    let matched = txn.status == "matched";

    if let Some(sfin) = &txn.simplefin {
        let amount = parse_amount(&sfin.amount);
        let debit_credit_memo = if amount < 0.0 { "DEBIT" } else { "CREDIT" };
        let status = if sfin.pending { "PENDING" } else { "POSTED" };

        // For matched transactions: REC-{id} as the canonical ID and nest both
        // source transactions. For sfin_only: keep the SimpleFIN ID as-is.
        let (transaction_id, simplefin_transaction, lunchflow_transaction) = if matched {
            let rec_id = txn
                .rec_id
                .map(|id| format!("REC-{id}"))
                .unwrap_or_else(|| sfin.id.clone());
            let sfin_ext = SfinTransactionExt {
                id: sfin.id.clone(),
                posted_timestamp: unix_to_rfc3339(sfin.posted),
                amount,
                description: Some(sfin.description.clone()).filter(|s| !s.is_empty()),
                payee: sfin.payee.clone(),
                memo: sfin.memo.clone().filter(|s| !s.is_empty()),
            };
            let lf_ext = txn.lunchflow.as_ref().map(|lf| LfTransactionExt {
                id: lf.id.clone(),
                amount: lf.amount,
                currency: lf.currency.clone(),
                date: lf.date.clone(),
                merchant: lf.merchant.clone(),
                description: lf.description.clone(),
                is_pending: lf.is_pending,
            });
            (rec_id, Some(sfin_ext), lf_ext)
        } else {
            (sfin.id.clone(), None, None)
        };

        FdxTransaction {
            transaction_id,
            posted_timestamp: unix_to_rfc3339(sfin.posted),
            transaction_timestamp: None,
            amount,
            description: sfin.description.clone(),
            payee: sfin.payee.clone(),
            memo: sfin.memo.clone(),
            status,
            debit_credit_memo,
            reconciliation_status: Some(txn.status.clone()),
            match_confidence: txn.match_confidence,
            simplefin_transaction,
            lunchflow_transaction,
        }
    } else if let Some(lf) = &txn.lunchflow {
        // LunchFlow-only transaction in a REC account — keep the LF source ID.
        let debit_credit_memo = if lf.amount < 0.0 { "DEBIT" } else { "CREDIT" };
        let status = if lf.is_pending { "PENDING" } else { "POSTED" };
        FdxTransaction {
            transaction_id: lf.id.clone(),
            posted_timestamp: date_to_rfc3339(&lf.date),
            transaction_timestamp: None,
            amount: lf.amount,
            description: lf
                .merchant
                .as_deref()
                .or(lf.description.as_deref())
                .unwrap_or("(no description)")
                .to_string(),
            payee: lf.merchant.clone(),
            memo: lf.description.clone(),
            status,
            debit_credit_memo,
            reconciliation_status: Some(txn.status.clone()),
            match_confidence: txn.match_confidence,
            simplefin_transaction: None,
            lunchflow_transaction: None,
        }
    } else {
        // Should never happen, but return a safe empty transaction.
        FdxTransaction {
            transaction_id: String::new(),
            posted_timestamp: unix_to_rfc3339(0),
            transaction_timestamp: None,
            amount: 0.0,
            description: String::new(),
            payee: None,
            memo: None,
            status: "POSTED",
            debit_credit_memo: "CREDIT",
            reconciliation_status: Some(txn.status.clone()),
            match_confidence: None,
            simplefin_transaction: None,
            lunchflow_transaction: None,
        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lunchflow::db::{LfTransactionView, SfinTransactionView, UnifiedTransaction};

    fn sfin_view(
        id: &str,
        posted: i64,
        amount: &str,
        description: &str,
        pending: bool,
    ) -> SfinTransactionView {
        SfinTransactionView {
            id: id.to_string(),
            posted,
            amount: amount.to_string(),
            description: description.to_string(),
            payee: None,
            memo: None,
            pending,
        }
    }

    fn lf_view(id: &str, amount: f64, date: &str, merchant: Option<&str>) -> LfTransactionView {
        LfTransactionView {
            id: id.to_string(),
            amount,
            currency: "USD".to_string(),
            date: date.to_string(),
            merchant: merchant.map(str::to_string),
            description: None,
            is_pending: false,
        }
    }

    // -----------------------------------------------------------------------
    // unified_transaction_to_fdx — matched path
    // -----------------------------------------------------------------------

    #[test]
    fn matched_transaction_gets_rec_id() {
        let txn = UnifiedTransaction {
            status: "matched".to_string(),
            match_confidence: Some(0.85),
            rec_id: Some(42),
            simplefin: Some(sfin_view(
                "TRN-001",
                1_705_276_800,
                "-311.92",
                "AUTOPAY PAYMENT",
                false,
            )),
            lunchflow: Some(lf_view("lf-001", -311.92, "2024-01-15", Some("Payment"))),
            sort_ts: 1_705_276_800,
        };
        let fdx = unified_transaction_to_fdx(&txn);

        assert_eq!(fdx.transaction_id, "REC-42");
        assert_eq!(fdx.amount, -311.92);
        assert_eq!(fdx.status, "POSTED");
        assert_eq!(fdx.debit_credit_memo, "DEBIT");
        assert_eq!(fdx.reconciliation_status.as_deref(), Some("matched"));
        assert_eq!(fdx.match_confidence, Some(0.85));

        let sfin_ext = fdx
            .simplefin_transaction
            .expect("simplefin_transaction should be present");
        assert_eq!(sfin_ext.id, "TRN-001");
        assert_eq!(sfin_ext.amount, -311.92);

        let lf_ext = fdx
            .lunchflow_transaction
            .expect("lunchflow_transaction should be present");
        assert_eq!(lf_ext.id, "lf-001");
        assert_eq!(lf_ext.date, "2024-01-15");
    }

    #[test]
    fn matched_transaction_without_rec_id_falls_back_to_sfin_id() {
        let txn = UnifiedTransaction {
            status: "matched".to_string(),
            match_confidence: Some(1.0),
            rec_id: None, // edge case: row exists but id not populated
            simplefin: Some(sfin_view(
                "TRN-002",
                1_705_276_800,
                "-28.11",
                "CLOUD WORKSPACE",
                false,
            )),
            lunchflow: Some(lf_view(
                "lf-002",
                -28.11,
                "2024-01-14",
                Some("Cloud Workspace"),
            )),
            sort_ts: 1_705_276_800,
        };
        let fdx = unified_transaction_to_fdx(&txn);

        // Should fall back to the SimpleFIN ID when rec_id is None.
        assert_eq!(fdx.transaction_id, "TRN-002");
        assert!(fdx.simplefin_transaction.is_some());
    }

    // -----------------------------------------------------------------------
    // unified_transaction_to_fdx — sfin_only path
    // -----------------------------------------------------------------------

    #[test]
    fn sfin_only_keeps_source_id_and_no_nested_refs() {
        let txn = UnifiedTransaction {
            status: "sfin_only".to_string(),
            match_confidence: None,
            rec_id: Some(99),
            simplefin: Some(sfin_view(
                "TRN-LULU",
                1_705_276_800,
                "72.00",
                "LULULEMON RETURN",
                false,
            )),
            lunchflow: None,
            sort_ts: 1_705_276_800,
        };
        let fdx = unified_transaction_to_fdx(&txn);

        assert_eq!(fdx.transaction_id, "TRN-LULU");
        assert_eq!(fdx.amount, 72.00);
        assert_eq!(fdx.debit_credit_memo, "CREDIT");
        assert_eq!(fdx.reconciliation_status.as_deref(), Some("sfin_only"));
        assert!(
            fdx.simplefin_transaction.is_none(),
            "no nested ref for sfin_only"
        );
        assert!(fdx.lunchflow_transaction.is_none());
    }

    // -----------------------------------------------------------------------
    // unified_transaction_to_fdx — lf_only path
    // -----------------------------------------------------------------------

    #[test]
    fn lf_only_keeps_source_id_and_no_nested_refs() {
        let txn = UnifiedTransaction {
            status: "lf_only".to_string(),
            match_confidence: None,
            rec_id: Some(77),
            simplefin: None,
            lunchflow: Some(LfTransactionView {
                id: "lf-jal-001".to_string(),
                amount: -242.20,
                currency: "USD".to_string(),
                date: "2024-01-10".to_string(),
                merchant: Some("Japan Airlines".to_string()),
                description: None,
                is_pending: false,
            }),
            sort_ts: 1_704_844_800,
        };
        let fdx = unified_transaction_to_fdx(&txn);

        assert_eq!(fdx.transaction_id, "lf-jal-001");
        assert_eq!(fdx.amount, -242.20);
        assert_eq!(fdx.debit_credit_memo, "DEBIT");
        assert_eq!(fdx.description, "Japan Airlines");
        assert_eq!(fdx.reconciliation_status.as_deref(), Some("lf_only"));
        assert!(fdx.simplefin_transaction.is_none());
        assert!(fdx.lunchflow_transaction.is_none());
    }

    // -----------------------------------------------------------------------
    // unified_transaction_to_fdx — pending transaction
    // -----------------------------------------------------------------------

    #[test]
    fn pending_sfin_transaction_maps_to_pending_status() {
        let txn = UnifiedTransaction {
            status: "matched".to_string(),
            match_confidence: Some(0.85),
            rec_id: Some(55),
            simplefin: Some(sfin_view("TRN-KAGI", 0, "-10.89", "RCH*KAGI.COM", true)),
            lunchflow: Some(LfTransactionView {
                id: "lf-pending-kagi".to_string(),
                amount: -10.89,
                currency: "USD".to_string(),
                date: "2024-01-15".to_string(),
                merchant: Some("Kagi".to_string()),
                description: None,
                is_pending: true,
            }),
            sort_ts: 0,
        };
        let fdx = unified_transaction_to_fdx(&txn);

        assert_eq!(fdx.status, "PENDING");
        assert_eq!(fdx.transaction_id, "REC-55");
        assert_eq!(fdx.amount, -10.89);
    }

    // -----------------------------------------------------------------------
    // debit_credit_memo correctness
    // -----------------------------------------------------------------------

    #[test]
    fn positive_amount_is_credit() {
        let txn = UnifiedTransaction {
            status: "sfin_only".to_string(),
            match_confidence: None,
            rec_id: None,
            simplefin: Some(sfin_view(
                "TRN-REF",
                1_705_276_800,
                "499.00",
                "BENEFIT REIMBURSEMENT",
                false,
            )),
            lunchflow: None,
            sort_ts: 1_705_276_800,
        };
        let fdx = unified_transaction_to_fdx(&txn);
        assert_eq!(fdx.debit_credit_memo, "CREDIT");
        assert_eq!(fdx.amount, 499.00);
    }
}
