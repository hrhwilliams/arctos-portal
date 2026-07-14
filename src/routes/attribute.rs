use askama::Template;
use axum::{
    extract::State,
    response::{Html, IntoResponse},
};

use crate::{errors::AppError, state::AppState};

#[derive(Template)]
#[template(path = "attribute.html")]
struct Attribute<'a> {
    pub types: &'a [String],
}

pub async fn attribute(State(app_state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let template = Attribute {
        types: &app_state.types(),
    };
    Ok(Html(template.render()?))
}
