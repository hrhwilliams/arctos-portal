use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

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

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("Application error: {self:?}");

        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = format!(
            "<!doctype html><html><body><h1>{}</h1><p>{}.</p></body></html>",
            status.as_u16(),
            self
        );

        (status, Html(body)).into_response()
    }
}
