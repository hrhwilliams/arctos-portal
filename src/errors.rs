use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

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
            Self::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Self::Reqwest(_)
            | Self::Internal
            | Self::JsonMissingValue(_)
            | Self::JsonConversion(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        let body = format!(
            "<!doctype html><html><body><h1>{}</h1><p>{}.</p></body></html>",
            status.as_u16(),
            message
        );

        (status, Html(body)).into_response()
    }
}
