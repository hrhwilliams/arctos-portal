use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("DuckDB error: {0}")]
    Duckdb(#[from] duckdb::Error),
    #[error("Arrow error: {0}")]
    Arrow(#[from] duckdb::arrow::error::ArrowError),
    #[error("Code table error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Code table error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    BadRequest(String),
}

/// Every error is the same JSON envelope, `{ "error", "message" }`, the shape
/// spec 03 names. The portal shows `message` where the failed call's result
/// would have gone, so a summary that fails leaves the previews working.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("Application error: {self:?}");

        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = json!({
            "error": status.canonical_reason().unwrap_or("error"),
            "message": self.to_string(),
        });

        (status, Json(body)).into_response()
    }
}
