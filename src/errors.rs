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

    #[error("Not found")]
    NotFound,

    #[error("Internal server error")]
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Askama(_) | AppError::Elasticsearch(_) | AppError::Internal => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };

        let body = format!(
            "<!doctype html><html><body><h1>{}</h1><p>{}.</p></body></html>",
            status.as_u16(),
            message
        );

        (status, Html(body)).into_response()
    }
}
