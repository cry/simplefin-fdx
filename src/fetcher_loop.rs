use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::{
    db::{get_config, set_config},
    error::AppError,
    state::SharedState, 
    util,
};

/// Strategy trait implemented by each provider's fetcher.
///
/// `run_loop` calls the methods below in the order described.  Implementors
/// only need to supply the provider-specific pieces; the polling loop,
/// interval-wait logic, and DB persistence are handled generically.
pub trait FetcherLoop {
    // ── Required ─────────────────────────────────────────────────────────────

    /// Short label used in log messages, e.g. "SimpleFIN" or "LunchFlow".
    fn name(&self) -> &'static str;

    /// Config DB key used to persist and restore `last_fetched`.
    fn last_fetched_db_key(&self) -> &'static str;

    /// Perform a single fetch covering the half-open range `[from_ts, now)`. 
    ///
    /// `state` is provided so implementations that need to update in-memory
    /// account lists mid-fetch (SimpleFIN) can do so.
    async fn do_fetch(
        &self,
        pool: &SqlitePool,
        state: &SharedState,
        from_ts: i64,
        now: i64,
    ) -> Result<(), AppError>;

    /// Write the restored `last_fetched` timestamp into `SharedState` on
    /// startup so `/health` reflects prior state before the first new fetch.
    async fn restore_last_fetched(&self, state: &SharedState, ts: i64);

    /// Mark a successful fetch in `SharedState`: set the provider's
    /// `last_fetched` field to `now` and clear its `fetch_error` field.
    async fn on_success(&self, state: &SharedState, now: i64);

    /// Mark a failed fetch in `SharedState`: set the provider's `fetch_error`
    /// field to `error`.
    async fn on_failure(&self, state: &SharedState, error: AppError);

    // ── Optional (default = no-op / None) ────────────────────────────────────

    /// Called once at the very start of `run_loop`, before the interval-wait
    /// or the main loop.  Use this for one-time startup work such as
    /// pre-loading cached data from the DB.
    ///
    /// Default implementation: no-op.
    async fn on_startup(&self, _pool: &SqlitePool, _state: &SharedState) {}

    /// Called after every *successful* fetch cycle, after state and DB have
    /// been updated but before sleeping.  Use this to trigger downstream work
    /// such as reconciliation.
    ///
    /// Default implementation: no-op.
    async fn post_fetch_success(&self, _pool: &SqlitePool, _state: &SharedState) {}

    /// If the provider persists the next window-start separately from
    /// `last_fetched`, return its DB key here.  `run_loop` will load `from_ts`
    /// from this key at startup and persist the new value after each successful
    /// fetch.
    ///
    /// Return `None` (default) to derive `from_ts` from `last_fetched - 86_400`
    /// instead (or from `start_date_days_back` on the very first run).
    fn next_start_db_key(&self) -> Option<&'static str> {
        None
    }
}

/// Generic polling loop shared by all fetcher implementations.
///
/// Call this from a provider's `run` function after constructing the API client
/// and building the strategy struct.
pub async fn run_loop<F>(
    fetcher: F,
    pool: SqlitePool,
    state: SharedState,
    fetch_interval_secs: u64,
    start_date_days_back: u64,
) where
    F: FetcherLoop,
{
    // ── Startup hook ─────────────────────────────────────────────────────────
    fetcher.on_startup(&pool, &state).await;

    // ── Restore persisted state ──────────────────────────────────────────────
    let last_fetched: Option<i64> = get_config(&pool, fetcher.last_fetched_db_key())
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());

    if let Some(ts) = last_fetched {
        fetcher.restore_last_fetched(&state, ts).await;
    }

    // Compute the initial from_ts.
    //
    // If the provider uses a separate `next_start` key, load from there.
    // Otherwise derive from `last_fetched - 86_400` (24 h overlap) or fall
    // back to `start_date_days_back` days ago for a first run.
    let mut from_ts: i64 = if let Some(key) = fetcher.next_start_db_key() {
        get_config(&pool, key)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| util::now_unix() - (start_date_days_back as i64 * 86_400))
    } else {
        last_fetched
            .map(|ts| ts - 86_400)
            .unwrap_or_else(|| util::now_unix() - (start_date_days_back as i64 * 86_400))
    };

    // ── Wait out the remainder of the current interval after a restart ────────
    if let Some(ts) = last_fetched {
        let elapsed = (util::now_unix() - ts).max(0) as u64;
        if elapsed < fetch_interval_secs {
            let wait = fetch_interval_secs - elapsed;
            info!(
                wait_secs = wait,
                fetcher = fetcher.name(),
                "Resuming after restart — waiting until next scheduled fetch"
            );
            sleep(Duration::from_secs(wait)).await;
        }
    }

    // ── Main polling loop ─────────────────────────────────────────────────────
    loop {
        let now = util::now_unix();
        info!(
            from = from_ts,
            to = now,
            fetcher = fetcher.name(),
            "Fetch cycle starting"
        );

        match fetcher.do_fetch(&pool, &state, from_ts, now).await {
            Ok(()) => {
                info!(fetcher = fetcher.name(), "Fetch cycle complete");

                fetcher.on_success(&state, now).await;

                // 24 h overlap so late-arriving transactions are not missed.
                from_ts = now - 86_400;

                // Persist last_fetched.
                if let Err(e) =
                    set_config(&pool, fetcher.last_fetched_db_key(), &now.to_string()).await
                {
                    warn!(fetcher = fetcher.name(), "Failed to persist last_fetched: {e}");
                }

                // Persist from_ts under the provider-specific next_start key if present.
                if let Some(key) = fetcher.next_start_db_key() {
                    if let Err(e) = set_config(&pool, key, &from_ts.to_string()).await {
                        warn!(fetcher = fetcher.name(), "Failed to persist next_start: {e}");
                    }
                }

                // Provider-specific post-success work (e.g. reconciliation).
                fetcher.post_fetch_success(&pool, &state).await;
            }
            Err(e) => {
                error!(fetcher = fetcher.name(), "Fetch cycle failed: {e}");
                fetcher.on_failure(&state, e).await;
                // from_ts is intentionally unchanged so the same window is retried.
            }
        }

        sleep(Duration::from_secs(fetch_interval_secs)).await;
    }
}