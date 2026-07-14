use askama::Template;
use axum::{
    Form,
    extract::State,
    response::{Html, IntoResponse},
};
use serde::Deserialize;

use crate::{
    errors::AppError,
    state::{AppState, SearchResult},
};

#[derive(Deserialize)]
pub struct SearchForm {
    scientific_name: Option<String>,
    attribute_type: String,
    attribute_value: String,
}

#[derive(Template)]
#[template(path = "htmx/search_results.html")]
pub struct SearchResults {
    n_results: usize,
    results: Vec<SearchResult>,
}

#[tracing::instrument(skip(app_state))]
pub async fn search(
    State(app_state): State<AppState>,
    Form(SearchForm {
        scientific_name,
        attribute_type,
        attribute_value,
    }): Form<SearchForm>,
) -> Result<impl IntoResponse, AppError> {
    let results = app_state
        .search(scientific_name, attribute_type, attribute_value)
        .await?;

    let template = SearchResults {
        n_results: results.len(),
        results,
    };

    Ok(Html(template.render()?))
}
