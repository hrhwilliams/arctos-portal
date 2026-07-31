use axum::{Json, extract::State, response::IntoResponse};
use axum_extra::extract::Form;

use crate::{errors::AppError, search::SearchForm, state::AppState};

#[tracing::instrument(skip(app_state))]
pub async fn search(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<impl IntoResponse, AppError> {
    let results = app_state.search(search_form).await?;
    Ok(Json(results))
}
