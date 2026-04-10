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
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotReady => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            AppError::AccountNotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
