use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Template rendering failed: {0}")]
    Askama(#[from] askama::Error),

    #[error("Elasticsearch error: {0}")]
    Elasticsearch(#[from] elasticsearch::Error),

    #[error("Failed to convert a value in response")]
    JsonConversion(String),

    #[error("Missing value in response")]
    JsonMissingValue(String),

    #[error("Not found")]
    NotFound,

    #[error("Internal server error")]
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("Application error: {:?}", self);

        let (status, message) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Askama(_)
            | AppError::Elasticsearch(_)
            | AppError::Internal
            | AppError::JsonMissingValue(_)
            | AppError::JsonConversion(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        let body = format!(
            "<!doctype html><html><body><h1>{}</h1><p>{}.</p></body></html>",
            status.as_u16(),
            message
        );

        (status, Html(body)).into_response()
    }
}
