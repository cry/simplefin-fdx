mod config;
mod db;
mod error;
mod fdx;
mod lunchflow;
mod simplefin;
mod state;
mod util;

use axum::{
    Json, Router,
    http::header,
    response::IntoResponse,
    routing::{delete, get, post},
};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use utoipa::OpenApi;

use crate::fdx::routes::{
    ApiDoc, AppState, create_reconciliation_rule, delete_reconciliation_rule, get_account, health,
    list_account_matches, list_accounts, list_holdings, list_reconciliation_rules,
    list_transactions, run_reconciliation, set_name_preference,
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

    // SimpleFIN is considered configured if a setup token is present OR an access
    // URL was previously claimed and saved to the database.
    let sfin_access_url = db::get_config(&pool, "access_url").await.ok().flatten();
    let simplefin_configured = cfg.setup_token.is_some() || sfin_access_url.is_some();
    let lunchflow_configured = cfg.lunchflow_api_key.is_some();

    if !simplefin_configured && !lunchflow_configured {
        warn!("No data sources configured. Set SIMPLEFIN_SETUP_TOKEN and/or LUNCHFLOW_API_KEY.");
    }

    if simplefin_configured && lunchflow_configured {
        if let Err(e) = lunchflow::reconciler::run(&pool).await {
            warn!(error = %e, "Startup reconciliation failed");
        }
    }

    if simplefin_configured {
        tokio::spawn(simplefin::fetcher::run(
            pool.clone(),
            shared.clone(),
            cfg.setup_token,
            cfg.fetch_interval_secs,
            cfg.start_date_days_back,
        ));
    } else {
        info!("SIMPLEFIN_SETUP_TOKEN not set and no saved access URL — SimpleFIN fetcher disabled");
    }

    if let Some(api_key) = cfg.lunchflow_api_key {
        tokio::spawn(lunchflow::fetcher::run(
            pool.clone(),
            shared.clone(),
            api_key,
            cfg.fetch_interval_secs,
            cfg.start_date_days_back,
        ));
    } else {
        info!("LUNCHFLOW_API_KEY not set — LunchFlow fetcher disabled");
    }

    let app_state = AppState {
        shared,
        pool,
        simplefin_configured,
        lunchflow_configured,
    };

    // Serve the OpenAPI spec as a plain JSON endpoint.
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
        .route(
            "/api/reconciliation/account-matches",
            get(list_account_matches),
        )
        .route(
            "/api/reconciliation/account-rules",
            get(list_reconciliation_rules).post(create_reconciliation_rule),
        )
        .route(
            "/api/reconciliation/account-rules/{id}",
            delete(delete_reconciliation_rule),
        )
        .route("/api/reconciliation/run", post(run_reconciliation))
        .route(
            "/api/reconciliation/name-preference/{sfinAccountId}",
            post(set_name_preference),
        )
        .with_state(app_state);

    async fn ui() -> impl IntoResponse {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            include_str!("../static/index.html"),
        )
    }
    let ui_route = Router::new().route("/", get(ui));

    let app = Router::new()
        .merge(api)
        .merge(openapi_route)
        .merge(ui_route)
        .layer(TraceLayer::new_for_http());

    info!("Listening on {}", cfg.server_addr);
    info!("OpenAPI spec available at /openapi.json");
    let listener = tokio::net::TcpListener::bind(&cfg.server_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
