use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use utoipa::{IntoParams, OpenApi};

use crate::{
    error::AppError,
    fdx::{
        ErrorResponse, FdxAccount, FdxAccountList, FdxHoldingList, FdxPage, FdxTransactionList,
        HealthResponse,
        mapping::{
            cached_account_to_fdx, holding_row_to_fdx, lf_account_to_fdx, lf_transaction_to_fdx,
            transaction_row_to_fdx, unified_transaction_to_fdx, unix_to_rfc3339,
        },
    },
    lunchflow::db::{get_all_lf_accounts, get_lf_account, get_lf_transactions_raw},
    reconciler::{
        AccountMatchRow, UserAccountRule, UserReconciliationAction, delete_user_account_rule,
        get_account_matches, get_lf_name_preferences, get_unified_transactions,
        get_user_account_rules, insert_user_account_rule, upsert_name_preference,
    },
    simplefin::db::{get_holdings, get_transactions},
    state::SharedState,
    util,
};

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedState,
    pub pool: SqlitePool,
    /// True if a SimpleFIN access URL is available (token set or previously claimed).
    pub simplefin_configured: bool,
    /// True if `LUNCHFLOW_API_KEY` is set.
    pub lunchflow_configured: bool,
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
        crate::fdx::LfTransactionExt,
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
        description = "FDX v6-compatible API backed by SimpleFIN and LunchFlow. \
                       Account IDs are prefixed: SIMPLEFIN- (SimpleFIN only), \
                       LUNCHFLOW- (LunchFlow only), REC- (reconciled pair). \
                       No authentication — deploy behind a reverse proxy.",
    )
)]
pub struct ApiDoc;

// ---------------------------------------------------------------------------
// Account source parsing
// ---------------------------------------------------------------------------

enum AccountSource {
    /// Original SimpleFIN account id (strip "SIMPLEFIN-" prefix).
    SimpleFin(String),
    /// LunchFlow account id (strip "LUNCHFLOW-" prefix, parse as i64).
    LunchFlow(i64),
    /// Reconciled pair, keyed by SimpleFIN id (strip "REC-" prefix).
    Reconciled(String),
}

fn parse_account_source(id: &str) -> Result<AccountSource, AppError> {
    if let Some(rest) = id.strip_prefix("SIMPLEFIN-") {
        Ok(AccountSource::SimpleFin(rest.to_string()))
    } else if let Some(rest) = id.strip_prefix("LUNCHFLOW-") {
        rest.parse::<i64>()
            .map(AccountSource::LunchFlow)
            .map_err(|_| AppError::AccountNotFound)
    } else if let Some(rest) = id.strip_prefix("REC-") {
        Ok(AccountSource::Reconciled(rest.to_string()))
    } else {
        Err(AppError::AccountNotFound)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return matched (sfin_account_id, lf_account_id) pairs from the reconciliation table.
async fn matched_pairs(pool: &SqlitePool) -> Vec<(String, i64)> {
    sqlx::query(
        "SELECT sfin_account_id, lf_account_id FROM reconciled_accounts WHERE status = 'matched'",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| {
        let sfin: Option<String> = r.get("sfin_account_id");
        let lf: Option<i64> = r.get("lf_account_id");
        Some((sfin?, lf?))
    })
    .collect()
}

/// Returns `NotReady` only when every configured provider has yet to complete
/// its first fetch. At least one configured source must have data.
fn check_ready(
    state: &crate::state::CacheState,
    sfin_configured: bool,
    lf_configured: bool,
) -> Result<(), AppError> {
    let sfin_ready =
        sfin_configured && (state.last_fetched.is_some() || !state.accounts.is_empty());
    let lf_ready = lf_configured && state.lf_last_fetched.is_some();
    if sfin_ready || lf_ready {
        Ok(())
    } else {
        Err(AppError::NotReady)
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts",
    responses(
        (status = 200, description = "Account list", body = FdxAccountList),
        (status = 503, description = "No data fetched yet", body = ErrorResponse),
    ),
    tag = "accounts"
)]
pub async fn list_accounts(State(app): State<AppState>) -> Result<Json<FdxAccountList>, AppError> {
    let state = app.shared.read().await;
    check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;

    let both = app.simplefin_configured && app.lunchflow_configured;
    let mut accounts: Vec<FdxAccount> = Vec::new();

    // All SimpleFIN accounts always appear as SIMPLEFIN-.
    if app.simplefin_configured {
        accounts.extend(
            state
                .accounts
                .iter()
                .map(|a| cached_account_to_fdx(a, "SIMPLEFIN-")),
        );
    }

    // All LunchFlow accounts always appear as LUNCHFLOW-.
    let lf_accounts = if app.lunchflow_configured {
        let lf = get_all_lf_accounts(&app.pool).await.unwrap_or_default();
        accounts.extend(lf.iter().map(lf_account_to_fdx));
        lf
    } else {
        vec![]
    };

    // Reconciled pairs appear as additional REC- accounts referencing both sources.
    if both {
        let pairs = matched_pairs(&app.pool).await;
        let lf_name_prefs = get_lf_name_preferences(&app.pool).await.unwrap_or_default();
        for (sfin_id, lf_id) in pairs {
            if let Some(sfin) = state.accounts.iter().find(|a| a.id == sfin_id) {
                let mut rec = cached_account_to_fdx(sfin, "REC-");
                rec.simplefin_account_id = Some(format!("SIMPLEFIN-{sfin_id}"));
                rec.lunchflow_account_id = Some(format!("LUNCHFLOW-{lf_id}"));
                if lf_name_prefs.contains(&sfin_id) {
                    if let Some(lf) = lf_accounts.iter().find(|a| a.id == lf_id) {
                        rec.display_name = lf.name.clone();
                    }
                }
                accounts.push(rec);
            }
        }
    }

    let total = accounts.len();
    Ok(Json(FdxAccountList {
        accounts,
        page: FdxPage { total },
    }))
}

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts/{accountId}",
    params(("accountId" = String, Path, description = "Prefixed account ID (SIMPLEFIN- / LUNCHFLOW- / REC-)")),
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
    match parse_account_source(&account_id)? {
        AccountSource::SimpleFin(sfin_id) => {
            let state = app.shared.read().await;
            check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;
            let account = state
                .accounts
                .iter()
                .find(|a| a.id == sfin_id)
                .ok_or(AppError::AccountNotFound)?;
            Ok(Json(cached_account_to_fdx(account, "SIMPLEFIN-")))
        }
        AccountSource::LunchFlow(lf_id) => {
            let account = get_lf_account(&app.pool, lf_id)
                .await?
                .ok_or(AppError::AccountNotFound)?;
            Ok(Json(lf_account_to_fdx(&account)))
        }
        AccountSource::Reconciled(sfin_id) => {
            let state = app.shared.read().await;
            check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;
            let account = state
                .accounts
                .iter()
                .find(|a| a.id == sfin_id)
                .ok_or(AppError::AccountNotFound)?;
            let mut rec = cached_account_to_fdx(account, "REC-");

            // Fetch matched lf_account_id, lf name, and name preference in one query.
            let row = sqlx::query(
                "SELECT ra.lf_account_id, lfa.name AS lf_name, \
                        COALESCE(np.preferred_source, 'simplefin') AS preferred_source \
                 FROM reconciled_accounts ra \
                 LEFT JOIN lf_accounts lfa ON lfa.id = ra.lf_account_id \
                 LEFT JOIN user_account_name_preference np \
                        ON np.sfin_account_id = ra.sfin_account_id \
                 WHERE ra.sfin_account_id = ? AND ra.status = 'matched' \
                 LIMIT 1",
            )
            .bind(&sfin_id)
            .fetch_optional(&app.pool)
            .await
            .ok()
            .flatten();

            if let Some(r) = row {
                if let Some(lf_id) = r.get::<Option<i64>, _>("lf_account_id") {
                    rec.simplefin_account_id = Some(format!("SIMPLEFIN-{sfin_id}"));
                    rec.lunchflow_account_id = Some(format!("LUNCHFLOW-{lf_id}"));
                    let preferred: String = r.get("preferred_source");
                    if preferred == "lunchflow" {
                        if let Some(lf_name) = r.get::<Option<String>, _>("lf_name") {
                            rec.display_name = lf_name;
                        }
                    }
                }
            }

            Ok(Json(rec))
        }
    }
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
        ("accountId" = String, Path, description = "Prefixed account ID"),
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
    let start_ts = query.start_time.as_deref().and_then(parse_iso8601);
    let end_ts = query.end_time.as_deref().and_then(parse_iso8601);

    match parse_account_source(&account_id)? {
        AccountSource::SimpleFin(sfin_id) => {
            {
                let state = app.shared.read().await;
                check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;
                if !state.accounts.iter().any(|a| a.id == sfin_id) {
                    return Err(AppError::AccountNotFound);
                }
            }
            let rows = get_transactions(&app.pool, &sfin_id, start_ts, end_ts).await?;
            let transactions: Vec<_> = rows.iter().map(transaction_row_to_fdx).collect();
            let total = transactions.len();
            Ok(Json(FdxTransactionList {
                transactions,
                page: FdxPage { total },
            }))
        }
        AccountSource::LunchFlow(lf_id) => {
            let txn_rows = get_lf_transactions_raw(&app.pool, lf_id, start_ts, end_ts).await?;
            if txn_rows.is_empty() {
                // Verify the account actually exists before returning an empty list.
                if get_lf_account(&app.pool, lf_id).await?.is_none() {
                    return Err(AppError::AccountNotFound);
                }
            }
            let transactions: Vec<_> = txn_rows
                .iter()
                .map(|r| {
                    lf_transaction_to_fdx(
                        &r.id,
                        r.amount,
                        &r.date,
                        r.merchant.as_deref(),
                        r.description.as_deref(),
                        r.is_pending,
                    )
                })
                .collect();
            let total = transactions.len();
            Ok(Json(FdxTransactionList {
                transactions,
                page: FdxPage { total },
            }))
        }
        AccountSource::Reconciled(sfin_id) => {
            {
                let state = app.shared.read().await;
                check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;
                if !state.accounts.iter().any(|a| a.id == sfin_id) {
                    return Err(AppError::AccountNotFound);
                }
            }
            let unified = get_unified_transactions(&app.pool, &sfin_id, start_ts, end_ts).await?;
            let transactions: Vec<_> = unified.iter().map(unified_transaction_to_fdx).collect();
            let total = transactions.len();
            Ok(Json(FdxTransactionList {
                transactions,
                page: FdxPage { total },
            }))
        }
    }
}

#[utoipa::path(
    get,
    path = "/fdx/v6/accounts/{accountId}/holdings",
    params(("accountId" = String, Path, description = "Prefixed account ID")),
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
    // Holdings are sourced from SimpleFIN for both SIMPLEFIN- and REC- accounts.
    // LUNCHFLOW- accounts return an empty holdings list (lf holdings are less
    // structured and not mapped to FDX holdings format).
    let sfin_id = match parse_account_source(&account_id)? {
        AccountSource::SimpleFin(id) | AccountSource::Reconciled(id) => id,
        AccountSource::LunchFlow(lf_id) => {
            if get_lf_account(&app.pool, lf_id).await?.is_none() {
                return Err(AppError::AccountNotFound);
            }
            return Ok(Json(FdxHoldingList {
                holdings: vec![],
                page: FdxPage { total: 0 },
            }));
        }
    };

    {
        let state = app.shared.read().await;
        check_ready(&state, app.simplefin_configured, app.lunchflow_configured)?;
        if !state.accounts.iter().any(|a| a.id == sfin_id) {
            return Err(AppError::AccountNotFound);
        }
    }

    let rows = get_holdings(&app.pool, &sfin_id).await?;
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
    let has_error = state.fetch_error.is_some() || state.lf_fetch_error.is_some();
    Json(HealthResponse {
        status: if has_error { "degraded" } else { "ok" },
        last_fetched: if app.simplefin_configured {
            state.last_fetched.map(unix_to_rfc3339)
        } else {
            None
        },
        fetch_error: state.fetch_error.clone(),
        lf_last_fetched: if app.lunchflow_configured {
            state.lf_last_fetched.map(unix_to_rfc3339)
        } else {
            None
        },
        lf_fetch_error: state.lf_fetch_error.clone(),
    })
}

// ---------------------------------------------------------------------------
// Reconciliation management API
// ---------------------------------------------------------------------------

pub async fn list_account_matches(
    State(app): State<AppState>,
) -> Result<Json<Vec<AccountMatchRow>>, AppError> {
    let rows = get_account_matches(&app.pool).await?;
    Ok(Json(rows))
}

pub async fn list_reconciliation_rules(
    State(app): State<AppState>,
) -> Result<Json<Vec<UserAccountRule>>, AppError> {
    let rules = get_user_account_rules(&app.pool).await?;
    Ok(Json(rules))
}

#[derive(Deserialize)]
pub struct CreateRuleRequest {
    pub sfin_account_id: Option<String>,
    pub lf_account_id: Option<i64>,
    pub action: UserReconciliationAction,
}

#[derive(Serialize)]
pub struct CreateRuleResponse {
    pub id: i64,
}

pub async fn create_reconciliation_rule(
    State(app): State<AppState>,
    Json(body): Json<CreateRuleRequest>,
) -> Result<(StatusCode, Json<CreateRuleResponse>), AppError> {
    // Validate: at least one side must be set.
    if body.sfin_account_id.is_none() && body.lf_account_id.is_none() {
        return Err(AppError::BadRequest(
            "At least one of sfin_account_id or lf_account_id must be provided".into(),
        ));
    }
    // Validate: 'match' requires both sides.
    if body.action == UserReconciliationAction::Match
        && (body.sfin_account_id.is_none() || body.lf_account_id.is_none())
    {
        return Err(AppError::BadRequest(
            "action 'match' requires both sfin_account_id and lf_account_id".into(),
        ));
    }

    let id = insert_user_account_rule(
        &app.pool,
        body.sfin_account_id.as_deref(),
        body.lf_account_id,
        &body.action,
        util::now_unix(),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(CreateRuleResponse { id })))
}

pub async fn delete_reconciliation_rule(
    State(app): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let deleted = delete_user_account_rule(&app.pool, id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::AccountNotFound)
    }
}

pub async fn run_reconciliation(State(app): State<AppState>) -> Result<StatusCode, AppError> {
    crate::reconciler::run(&app.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SetNamePreferenceRequest {
    /// `"simplefin"` or `"lunchflow"`
    pub preferred_source: String,
}

pub async fn set_name_preference(
    State(app): State<AppState>,
    Path(sfin_account_id): Path<String>,
    Json(body): Json<SetNamePreferenceRequest>,
) -> Result<StatusCode, AppError> {
    if body.preferred_source != "simplefin" && body.preferred_source != "lunchflow" {
        return Err(AppError::BadRequest(
            "preferred_source must be 'simplefin' or 'lunchflow'".into(),
        ));
    }
    upsert_name_preference(&app.pool, &sfin_account_id, &body.preferred_source).await?;
    Ok(StatusCode::NO_CONTENT)
}
