use std::env;

pub struct Config {
    /// One-time setup token from the SimpleFIN bridge. Only needed on first run.
    pub setup_token: Option<String>,
    /// Seconds between polls of the SimpleFIN bridge.
    pub fetch_interval_secs: u64,
    /// TCP address to listen on.
    pub server_addr: String,
    /// SQLite connection string, e.g. `sqlite://simplefin.db`.
    pub database_url: String,
    /// Days of transaction history to fetch on the very first run.
    pub start_date_days_back: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            setup_token: env::var("SIMPLEFIN_SETUP_TOKEN").ok(),
            fetch_interval_secs: env::var("FETCH_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
            server_addr: env::var("SERVER_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://simplefin.db".to_string()),
            start_date_days_back: env::var("START_DATE_DAYS_BACK")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(90),
        }
    }
}
