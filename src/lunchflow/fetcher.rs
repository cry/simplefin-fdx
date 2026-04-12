use lunchflow::{LunchFlowClient, TransactionParams, models::AccountStatus};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::{
    fetcher_loop::{FetcherLoop, run_loop},
    lunchflow::db as lf_db,
    reconciler,
    state::SharedState,
    util,
};

const LF_LAST_FETCHED_KEY: &str = "lf_last_fetched";

/// Fetch all accounts, balances, transactions, and holdings from LunchFlow,
/// then trigger reconciliation against the SimpleFIN data already in the DB.
async fn lf_fetch(
    client: &LunchFlowClient,
    pool: &SqlitePool,
    from_ts: i64,
    now: i64,
) -> Result<(), String> {
    let accounts = client.list_accounts().await.map_err(|e| e.to_string())?;

    let from_date = util::unix_to_date_str(from_ts);
    let to_date = util::unix_to_date_str(now);

    for account in &accounts {
        // Fetch balance separately and store alongside the account row.
        let balance = match client.get_balance(account.id).await {
            Ok(b) => Some(b),
            Err(e) => {
                warn!(account_id = account.id, error = %e, "Failed to fetch lunchflow balance");
                None
            }
        };

        lf_db::upsert_lf_account(pool, account, balance.as_ref(), now)
            .await
            .map_err(|e| e.to_string())?;

        // Only fetch transactions and holdings for active accounts.
        if account.status != AccountStatus::Active {
            info!(
                account_id = account.id,
                status = %account.status,
                "Skipping non-active lunchflow account"
            );
            continue;
        }

        let params = TransactionParams {
            include_pending: true,
            from: Some(from_date.clone()),
            to: Some(to_date.clone()),
        };

        match client.get_transactions(account.id, params).await {
            Ok(txns) => {
                info!(
                    account_id = account.id,
                    count = txns.len(),
                    "Fetched lunchflow transactions"
                );
                lf_db::upsert_lf_transactions(pool, account.id, &txns)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                warn!(account_id = account.id, error = %e, "Failed to fetch lunchflow transactions");
            }
        }

        match client.get_holdings(account.id).await {
            Ok(holdings) => {
                info!(
                    account_id = account.id,
                    count = holdings.holdings.len(),
                    "Fetched lunchflow holdings"
                );
                lf_db::replace_lf_holdings(pool, account.id, &holdings.holdings, now)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(lunchflow::Error::HoldingsNotSupported) => {
                // Many providers don't support holdings; skip silently.
            }
            Err(e) => {
                warn!(account_id = account.id, error = %e, "Failed to fetch lunchflow holdings");
            }
        }
    }

    Ok(())
}

struct LunchFlowFetcher {
    client: LunchFlowClient,
}

impl FetcherLoop for LunchFlowFetcher {
    fn name(&self) -> &'static str {
        "LunchFlow"
    }

    fn last_fetched_db_key(&self) -> &'static str {
        LF_LAST_FETCHED_KEY
    }

    // next_start_db_key left as default (None) — LunchFlow derives from_ts
    // from last_fetched directly.

    async fn do_fetch(
        &self,
        pool: &SqlitePool,
        _state: &SharedState,
        from_ts: i64,
        now: i64,
    ) -> Result<(), String> {
        lf_fetch(&self.client, pool, from_ts, now).await
    }

    async fn restore_last_fetched(&self, state: &SharedState, ts: i64) {
        state.write().await.lf_last_fetched = Some(ts);
    }

    async fn on_success(&self, state: &SharedState, now: i64) {
        let mut s = state.write().await;
        s.lf_last_fetched = Some(now);
        s.lf_fetch_error = None;
    }

    async fn on_failure(&self, state: &SharedState, error: String) {
        state.write().await.lf_fetch_error = Some(error);
    }

    async fn post_fetch_success(&self, pool: &SqlitePool, _state: &SharedState) {
        if let Err(e) = reconciler::run(pool).await {
            warn!("Reconciliation failed: {e}");
        }
    }
}

pub async fn run(
    pool: SqlitePool,
    shared: SharedState,
    api_key: String,
    fetch_interval_secs: u64,
    start_date_days_back: u64,
) {
    let client = match LunchFlowClient::new(api_key) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create LunchFlow client: {e}");
            return;
        }
    };

    run_loop(
        LunchFlowFetcher { client },
        pool,
        shared,
        fetch_interval_secs,
        start_date_days_back,
    )
    .await;
}
