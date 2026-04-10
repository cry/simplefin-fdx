mod config;
mod db;
mod error;
mod fdx;
mod fetcher;
mod state;

use axum::{Json, Router, routing::get};
use tower_http::trace::TraceLayer;
use tracing::info;
use utoipa::OpenApi;

use crate::fdx::routes::{
    ApiDoc, AppState, get_account, health, list_accounts, list_holdings, list_transactions,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "simplefin_server=info,tower_http=info".into()),
        )
        .init();

    let cfg = config::Config::from_env();

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            cfg.database_url
                .parse::<sqlx::sqlite::SqliteConnectOptions>()?
                .create_if_missing(true),
        )
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    let shared = state::new_shared_state();

    tokio::spawn(fetcher::run(
        pool.clone(),
        shared.clone(),
        cfg.setup_token,
        cfg.fetch_interval_secs,
        cfg.start_date_days_back,
    ));

    let app_state = AppState { shared, pool };

    // Serve the OpenAPI spec as a plain JSON endpoint. Point any OpenAPI viewer
    // (Swagger UI, Redoc, Stoplight, Postman) at /openapi.json to explore the API.
    let spec = ApiDoc::openapi();
    let openapi_route = Router::new().route("/openapi.json", get(|| async move { Json(spec) }));

    let api = Router::new()
        .route("/health", get(health))
        .route("/fdx/v6/accounts", get(list_accounts))
        .route("/fdx/v6/accounts/{accountId}", get(get_account))
        .route(
            "/fdx/v6/accounts/{accountId}/transactions",
            get(list_transactions),
        )
        .route("/fdx/v6/accounts/{accountId}/holdings", get(list_holdings))
        .with_state(app_state);

    let app = Router::new()
        .merge(api)
        .merge(openapi_route)
        .layer(TraceLayer::new_for_http());

    info!("Listening on {}", cfg.server_addr);
    info!("OpenAPI spec available at /openapi.json");
    let listener = tokio::net::TcpListener::bind(&cfg.server_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
