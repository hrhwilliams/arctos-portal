use askama::Template;
use axum::response::{Html, IntoResponse};

use crate::errors::AppError;

#[derive(Template)]
#[template(path = "index.html")]
struct Index;

pub async fn index() -> Result<impl IntoResponse, AppError> {
    Ok(Html(Index.render()?))
}
