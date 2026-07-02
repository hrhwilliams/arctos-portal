use axum::{
    Form,
    extract::State,
    response::{Html, IntoResponse},
};
use serde::Deserialize;

use crate::{errors::AppError, state::AppState};

#[derive(Deserialize)]
pub struct SearchForm {
    attribute_type: String,
    attribute_value: String,
}

pub async fn search(
    State(app_state): State<AppState>,
    Form(SearchForm {
        attribute_type,
        attribute_value,
    }): Form<SearchForm>,
) -> Result<impl IntoResponse, AppError> {
    app_state.search(attribute_type, attribute_value).await?;
    Ok(Html("<h1>Searched</h1>"))
}
