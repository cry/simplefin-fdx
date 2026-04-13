use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("no data available yet — initial fetch in progress")]
    NotReady,
    #[error("account not found")]
    AccountNotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Database(sqlx::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        // Surface UNIQUE constraint violations as 409 Conflict.
        if let sqlx::Error::Database(ref db_err) = e {
            if db_err.is_unique_violation() {
                return AppError::Conflict("a rule for this account pair already exists".into());
            }
        }
        AppError::Database(e)
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Internal(e.to_string())
    }
}

// Handle specific external library errors that can occur in fetchers
impl From<lunchflow::Error> for AppError {
    fn from(e: lunchflow::Error) -> Self {
        AppError::Internal(format!("LunchFlow error: {}", e))
    }
}

impl From<simplefin::Error> for AppError {
    fn from(e: simplefin::Error) -> Self {
        AppError::Internal(format!("SimpleFIN error: {}", e))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotReady => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            AppError::AccountNotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::Conflict(_) => (StatusCode::CONFLICT, self.to_string()),
            AppError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            AppError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
