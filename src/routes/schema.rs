use axum::{
    Json,
    extract::{Query, State},
    http::header::CACHE_CONTROL,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    schema::{Taxon, matching_taxa},
    state::AppState,
};

/// Everything the search form needs to render itself. Computed at startup and
/// served from memory — it blocks the first render of the search page.
#[tracing::instrument(skip(app_state))]
pub async fn schema(State(app_state): State<AppState>) -> Response {
    (
        [(CACHE_CONTROL, "public, max-age=900")],
        Json(app_state.schema()),
    )
        .into_response()
}

#[derive(Deserialize, Debug)]
pub struct TaxaQuery {
    /// Absent means every rank, which is what an unfiltered combobox wants.
    pub rank: Option<String>,
    pub q: String,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct TaxaResults<'a> {
    matches: Vec<&'a Taxon>,
}

/// Typeahead over the distinct `(rank, name)` table. `limit` is capped at 20
/// however large the caller asks for.
#[tracing::instrument(skip(app_state))]
pub async fn taxa(State(app_state): State<AppState>, Query(query): Query<TaxaQuery>) -> Response {
    let matches = matching_taxa(
        app_state.taxa(),
        query.rank.as_deref(),
        &query.q,
        query.limit.unwrap_or(10).min(20),
    );

    (
        [(CACHE_CONTROL, "public, max-age=3600")],
        Json(TaxaResults { matches }),
    )
        .into_response()
}
