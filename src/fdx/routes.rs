use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::DateTime;
use serde::Deserialize;
use sqlx::SqlitePool;
use utoipa::{IntoParams, OpenApi};

use crate::{
    db,
    error::AppError,
    fdx::{
        mapping::{cached_account_to_fdx, holding_row_to_fdx, transaction_row_to_fdx, unix_to_rfc3339},
        ErrorResponse, FdxAccount, FdxAccountList, FdxHoldingList, FdxPage, FdxTransactionList,
        HealthResponse,
    },
    state::SharedState,
};

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedState,
    pub pool: SqlitePool,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        list_accounts,
        get_account,
        list_transactions,
        list_holdings,
        health,
    ),
    components(schemas(
        crate::fdx::FdxAccountList,
        crate::fdx::FdxAccount,
        crate::fdx::FdxCurrency,
        crate::fdx::FdxPage,
        crate::fdx::FdxTransactionList,
        crate::fdx::FdxTransaction,
        crate::fdx::FdxHoldingList,
        crate::fdx::FdxHolding,
        crate::fdx::HealthResponse,
        crate::fdx::ErrorResponse,
    )),
    tags(
        (name = "accounts", description = "Account, transaction and holding data"),
        (name = "health", description = "Service health"),
    ),
    info(
        title = "SimpleFIN FDX Server",
        version = "0.1.0",
        description = "FDX v6-compatible API backed by a SimpleFIN bridge. \
                       No authentication — deploy behind a reverse proxy.",
    )
)]
pub struct ApiDoc;

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts",
    responses(
        (status = 200, description = "Account list", body = FdxAccountList),
        (status = 503, description = "No data fetched yet", body = ErrorResponse),
    ),
    tag = "accounts"
)]
pub async fn list_accounts(
    State(app): State<AppState>,
) -> Result<Json<FdxAccountList>, AppError> {
    let state = app.shared.read().await;
    if state.accounts.is_empty() && state.last_fetched.is_none() {
        return Err(AppError::NotReady);
    }
    let accounts: Vec<_> = state.accounts.iter().map(cached_account_to_fdx).collect();
    let total = accounts.len();
    Ok(Json(FdxAccountList {
        accounts,
        page: FdxPage { total },
    }))
}

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts/{accountId}",
    params(("accountId" = String, Path, description = "Account ID")),
    responses(
        (status = 200, description = "Account details", body = FdxAccount),
        (status = 404, description = "Account not found", body = ErrorResponse),
        (status = 503, description = "No data fetched yet", body = ErrorResponse),
    ),
    tag = "accounts"
)]
pub async fn get_account(
    State(app): State<AppState>,
    Path(account_id): Path<String>,
) -> Result<Json<FdxAccount>, AppError> {
    let state = app.shared.read().await;
    if state.accounts.is_empty() && state.last_fetched.is_none() {
        return Err(AppError::NotReady);
    }
    let account = state
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or(AppError::AccountNotFound)?;
    Ok(Json(cached_account_to_fdx(account)))
}

#[derive(Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct TransactionQuery {
    /// Include transactions posted on or after this time (RFC 3339).
    #[param(example = "2024-01-01T00:00:00Z")]
    pub start_time: Option<String>,
    /// Include transactions posted on or before this time (RFC 3339).
    #[param(example = "2024-12-31T23:59:59Z")]
    pub end_time: Option<String>,
}

fn parse_iso8601(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts/{accountId}/transactions",
    params(
        ("accountId" = String, Path, description = "Account ID"),
        TransactionQuery,
    ),
    responses(
        (status = 200, description = "Transaction list", body = FdxTransactionList),
        (status = 404, description = "Account not found", body = ErrorResponse),
        (status = 503, description = "No data fetched yet", body = ErrorResponse),
    ),
    tag = "accounts"
)]
pub async fn list_transactions(
    State(app): State<AppState>,
    Path(account_id): Path<String>,
    Query(query): Query<TransactionQuery>,
) -> Result<Json<FdxTransactionList>, AppError> {
    {
        let state = app.shared.read().await;
        if state.accounts.is_empty() && state.last_fetched.is_none() {
            return Err(AppError::NotReady);
        }
        if !state.accounts.iter().any(|a| a.id == account_id) {
            return Err(AppError::AccountNotFound);
        }
    }

    let start_ts = query.start_time.as_deref().and_then(parse_iso8601);
    let end_ts = query.end_time.as_deref().and_then(parse_iso8601);

    let rows = db::get_transactions(&app.pool, &account_id, start_ts, end_ts).await?;
    let transactions: Vec<_> = rows.iter().map(transaction_row_to_fdx).collect();
    let total = transactions.len();
    Ok(Json(FdxTransactionList {
        transactions,
        page: FdxPage { total },
    }))
}

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts/{accountId}/holdings",
    params(("accountId" = String, Path, description = "Account ID")),
    responses(
        (status = 200, description = "Holdings list", body = FdxHoldingList),
        (status = 404, description = "Account not found", body = ErrorResponse),
        (status = 503, description = "No data fetched yet", body = ErrorResponse),
    ),
    tag = "accounts"
)]
pub async fn list_holdings(
    State(app): State<AppState>,
    Path(account_id): Path<String>,
) -> Result<Json<FdxHoldingList>, AppError> {
    {
        let state = app.shared.read().await;
        if state.accounts.is_empty() && state.last_fetched.is_none() {
            return Err(AppError::NotReady);
        }
        if !state.accounts.iter().any(|a| a.id == account_id) {
            return Err(AppError::AccountNotFound);
        }
    }

    let rows = db::get_holdings(&app.pool, &account_id).await?;
    let holdings: Vec<_> = rows.iter().map(holding_row_to_fdx).collect();
    let total = holdings.len();
    Ok(Json(FdxHoldingList {
        holdings,
        page: FdxPage { total },
    }))
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service health", body = HealthResponse),
    ),
    tag = "health"
)]
pub async fn health(State(app): State<AppState>) -> Json<HealthResponse> {
    let state = app.shared.read().await;
    Json(HealthResponse {
        status: if state.fetch_error.is_none() { "ok" } else { "degraded" },
        last_fetched: state.last_fetched.map(unix_to_rfc3339),
        fetch_error: state.fetch_error.clone(),
    })
}
