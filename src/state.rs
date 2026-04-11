use std::sync::Arc;
use tokio::sync::RwLock;

/// A snapshot of a single account, derived from SimpleFIN's Account model.
/// Held in memory so account-list endpoints don't need a DB round-trip.
#[derive(Clone, Debug)]
pub struct CachedAccount {
    pub id: String,
    pub name: String,
    pub currency: String,
    pub balance: String,
    pub balance_date: i64,
    pub available_balance: Option<String>,
    pub conn_id: Option<String>,
}

#[derive(Default)]
pub struct CacheState {
    /// SimpleFIN accounts — empty when SimpleFIN is not configured.
    pub accounts: Vec<CachedAccount>,
    /// Unix timestamp of the last successful SimpleFIN fetch.
    pub last_fetched: Option<i64>,
    /// Error from the most recent failed SimpleFIN fetch.
    pub fetch_error: Option<String>,
    /// Unix timestamp of the last successful LunchFlow fetch.
    pub lf_last_fetched: Option<i64>,
    /// Error from the most recent failed LunchFlow fetch.
    pub lf_fetch_error: Option<String>,
}

pub type SharedState = Arc<RwLock<CacheState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(CacheState::default()))
}
