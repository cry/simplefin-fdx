use simplefin::client::{AccountsRequest, SimpleFINClient};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::{
    db::{get_config, set_config},
    fetcher_loop::{FetcherLoop, run_loop},
    simplefin::db::{load_accounts, upsert_account, upsert_holding, upsert_transaction},
    state::{CachedAccount, SharedState},
};

const ACCESS_URL_KEY: &str = "access_url";
const LAST_FETCHED_KEY: &str = "last_fetched";
const NEXT_START_KEY: &str = "next_start";
/// SimpleFIN bridges may reject or return incomplete data for windows longer than 90 days.
const MAX_WINDOW_SECS: i64 = 90 * 86_400;

/// Split `[start, end)` into a sequence of non-overlapping windows each ≤ 90 days.
fn batch_windows(start: i64, end: i64) -> Vec<(i64, i64)> {
    let mut windows = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let batch_end = (cursor + MAX_WINDOW_SECS).min(end);
        windows.push((cursor, batch_end));
        cursor = batch_end;
    }
    windows
}

/// Resolve the SimpleFIN access URL: read from DB, or claim using the setup token.
async fn resolve_access_url(
    pool: &SqlitePool,
    setup_token: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(url) = get_config(pool, ACCESS_URL_KEY).await? {
        return Ok(url);
    }

    let token = setup_token.ok_or_else(|| {
        anyhow::anyhow!(
            "No access URL in database and SIMPLEFIN_SETUP_TOKEN not set. \
             Set SIMPLEFIN_SETUP_TOKEN to the token from your SimpleFIN bridge."
        )
    })?;

    info!("Claiming access URL from setup token");
    let client = SimpleFINClient::claim(token).await?;
    let url = client.access_url_str().to_string();
    set_config(pool, ACCESS_URL_KEY, &url).await?;
    info!("Access URL claimed and persisted");
    Ok(url)
}

fn sfin_account_to_cached(account: &simplefin::models::Account) -> CachedAccount {
    CachedAccount {
        id: account.id.clone(),
        name: account.name.clone(),
        currency: account.currency.clone(),
        balance: account.balance.clone(),
        balance_date: account.balance_date,
        available_balance: account.available_balance.clone(),
        conn_id: account.conn_id.clone(),
    }
}

/// Fetch a single window (guaranteed ≤ 90 days). Writes accounts and transactions to
/// the DB and updates the in-memory account list. Does NOT update `last_fetched`.
async fn fetch_window(
    client: &SimpleFINClient,
    pool: &SqlitePool,
    state: &SharedState,
    start_ts: i64,
    end_ts: i64,
) -> Result<(), String> {
    let params = AccountsRequest {
        start_date: Some(start_ts),
        end_date: Some(end_ts),
        pending: true,
        accounts: vec![],
        balances_only: false,
    };

    let account_set = client
        .get_accounts(params)
        .await
        .map_err(|e| e.to_string())?;

    if !account_set.errors.is_empty() {
        for err in &account_set.errors {
            warn!(code = %err.code, message = %err.message, "SimpleFIN error in response");
        }
    }

    for msg in &account_set.api_messages {
        info!(message = %msg, "SimpleFIN API message");
    }

    for account in &account_set.accounts {
        let cached = sfin_account_to_cached(account);
        upsert_account(pool, &cached)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(transactions) = &account.transactions {
            for txn in transactions {
                upsert_transaction(pool, &account.id, txn)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            info!(
                account_id = %account.id,
                txn_count = transactions.len(),
                "Upserted transactions"
            );
        }

        for holding in &account.holdings {
            upsert_holding(pool, &account.id, holding)
                .await
                .map_err(|e| e.to_string())?;
        }
        if !account.holdings.is_empty() {
            info!(
                account_id = %account.id,
                holding_count = account.holdings.len(),
                "Upserted holdings"
            );
        }
    }

    // Always reflect the latest account balances from SimpleFIN (same across all windows).
    let cached_accounts: Vec<CachedAccount> = account_set
        .accounts
        .iter()
        .map(sfin_account_to_cached)
        .collect();
    state.write().await.accounts = cached_accounts;

    Ok(())
}

/// Fetch `[start_ts, now)`, splitting into ≤ 90-day batches. Returns the timestamp
/// used as `now` so the caller can record it as `last_fetched`.
async fn fetch_range(
    client: &SimpleFINClient,
    pool: &SqlitePool,
    state: &SharedState,
    start_ts: i64,
    end_ts: i64,
) -> Result<(), String> {
    let windows = batch_windows(start_ts, end_ts);
    let total = windows.len();
    for (i, (win_start, win_end)) in windows.into_iter().enumerate() {
        info!(
            batch = i + 1,
            of = total,
            start = win_start,
            end = win_end,
            "Fetching batch"
        );
        fetch_window(client, pool, state, win_start, win_end).await?;
    }
    Ok(())
}

struct SimpleFINFetcher {
    client: SimpleFINClient,
}

impl FetcherLoop for SimpleFINFetcher {
    fn name(&self) -> &'static str {
        "SimpleFIN"
    }

    fn last_fetched_db_key(&self) -> &'static str {
        LAST_FETCHED_KEY
    }

    fn next_start_db_key(&self) -> Option<&'static str> {
        Some(NEXT_START_KEY)
    }

    async fn do_fetch(
        &self,
        pool: &SqlitePool,
        state: &SharedState,
        from_ts: i64,
        now: i64,
    ) -> Result<(), String> {
        fetch_range(&self.client, pool, state, from_ts, now).await
    }

    async fn restore_last_fetched(&self, state: &SharedState, ts: i64) {
        state.write().await.last_fetched = Some(ts);
    }

    async fn on_success(&self, state: &SharedState, now: i64) {
        let mut s = state.write().await;
        s.last_fetched = Some(now);
        s.fetch_error = None;
    }

    async fn on_failure(&self, state: &SharedState, error: String) {
        state.write().await.fetch_error = Some(error);
    }

    async fn on_startup(&self, pool: &SqlitePool, state: &SharedState) {
        match load_accounts(pool).await {
            Ok(accounts) if !accounts.is_empty() => {
                state.write().await.accounts = accounts;
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to pre-load accounts from DB: {e}"),
        }
    }
}

pub async fn run(
    pool: SqlitePool,
    state: SharedState,
    setup_token: Option<String>,
    fetch_interval_secs: u64,
    start_date_days_back: u64,
) {
    let access_url = match resolve_access_url(&pool, setup_token.as_deref()).await {
        Ok(url) => url,
        Err(e) => {
            error!("Failed to resolve SimpleFIN access URL: {e}");
            return;
        }
    };

    let client = match SimpleFINClient::from_access_url(&access_url) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create SimpleFIN client: {e}");
            return;
        }
    };

    run_loop(
        SimpleFINFetcher { client },
        pool,
        state,
        fetch_interval_secs,
        start_date_days_back,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_windows_single() {
        // Range shorter than 90 days → one window
        let start = 0;
        let end = 30 * 86_400;
        let windows = batch_windows(start, end);
        assert_eq!(windows, vec![(0, end)]);
    }

    #[test]
    fn batch_windows_exact_multiple() {
        // Exactly 180 days → two 90-day windows
        let start = 0;
        let end = 180 * 86_400;
        let windows = batch_windows(start, end);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], (0, 90 * 86_400));
        assert_eq!(windows[1], (90 * 86_400, 180 * 86_400));
    }

    #[test]
    fn batch_windows_remainder() {
        // 100 days → one 90-day window + one 10-day window
        let start = 0;
        let end = 100 * 86_400;
        let windows = batch_windows(start, end);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], (0, 90 * 86_400));
        assert_eq!(windows[1], (90 * 86_400, 100 * 86_400));
    }

    #[test]
    fn batch_windows_empty() {
        // start == end → no windows
        assert!(batch_windows(100, 100).is_empty());
    }
}
