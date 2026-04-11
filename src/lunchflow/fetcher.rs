use lunchflow::{LunchFlowClient, TransactionParams, models::AccountStatus};
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::{db as lf_db, reconciler};
use crate::{db, state::SharedState, util};

const LF_LAST_FETCHED_KEY: &str = "lf_last_fetched";

/// Fetch all accounts, balances, transactions, and holdings from LunchFlow,
/// then trigger reconciliation against the SimpleFIN data already in the DB.
async fn do_fetch(
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

    // Restore last fetch time into SharedState so /health is accurate immediately.
    let last_fetched: Option<i64> = db::get_config(&pool, LF_LAST_FETCHED_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());

    if let Some(ts) = last_fetched {
        shared.write().await.lf_last_fetched = Some(ts);
    }

    let mut from_ts: i64 = last_fetched
        .map(|ts| ts - 86_400) // 24 h overlap to catch late-arriving transactions
        .unwrap_or_else(|| util::now_unix() - (start_date_days_back as i64 * 86_400));

    // If we restarted before the next interval, wait out the remainder.
    if let Some(ts) = last_fetched {
        let elapsed = (util::now_unix() - ts).max(0) as u64;
        if elapsed < fetch_interval_secs {
            let wait = fetch_interval_secs - elapsed;
            info!(
                wait_secs = wait,
                "LunchFlow fetcher: waiting until next scheduled fetch"
            );
            sleep(Duration::from_secs(wait)).await;
        }
    }

    loop {
        let now = util::now_unix();
        info!(from = from_ts, to = now, "LunchFlow fetch cycle starting");

        match do_fetch(&client, &pool, from_ts, now).await {
            Ok(()) => {
                info!("LunchFlow fetch cycle complete");
                {
                    let mut s = shared.write().await;
                    s.lf_last_fetched = Some(now);
                    s.lf_fetch_error = None;
                }
                from_ts = now - 86_400;

                if let Err(e) = db::set_config(&pool, LF_LAST_FETCHED_KEY, &now.to_string()).await {
                    warn!("Failed to persist lf_last_fetched: {e}");
                }

                // Run reconciliation now that both data sources are up to date.
                if let Err(e) = reconciler::run(&pool).await {
                    warn!("Reconciliation failed: {e}");
                }
            }
            Err(e) => {
                error!("LunchFlow fetch cycle failed: {e}");
                shared.write().await.lf_fetch_error = Some(e);
                // from_ts unchanged so we retry the same window next cycle.
            }
        }

        sleep(Duration::from_secs(fetch_interval_secs)).await;
    }
}
