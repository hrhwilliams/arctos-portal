use axum::{
    Json,
    extract::{Query, State},
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{
    schema::{Schema, Taxon, matching_taxa},
    state::AppState,
};

/// Everything the search form needs to render itself. Computed at startup and
/// served from memory — it blocks the first render of the search page.
#[utoipa::path(
    get,
    path = "/api/schema",
    responses((status = 200, description = "The form's vocabularies, facets, and limits", body = Schema))
)]
#[tracing::instrument(skip(app_state))]
pub async fn schema(State(app_state): State<AppState>) -> Response {
    (
        [(CACHE_CONTROL, "public, max-age=900")],
        Json(app_state.schema()),
    )
        .into_response()
}

/// The `BerkeleyMapper` configuration for `/api/download`.
///
/// `BerkeleyMapper` fetches it itself, as `configfile=`. It names the columns
/// holding the coordinate pair, and the ones to keep out of the map balloon.
///
/// The file is compiled into the binary. It is one fixed document that has to
/// travel with the export it describes, so it needs no static file route and
/// cannot drift from the deployment serving it. It sits beside this module
/// rather than in `docs/`, because the image build copies `src/` alone and
/// [`include_str`] reads it at compile time.
#[utoipa::path(
    get,
    path = "/api/berkeleymapper.xml",
    responses((status = 200, description = "The BerkeleyMapper config for `/api/download?cols=map`", content_type = "application/xml"))
)]
#[tracing::instrument]
pub async fn berkeleymapper() -> Response {
    (
        [
            (CONTENT_TYPE, "application/xml;charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("berkeleymapper.xml"),
    )
        .into_response()
}

#[derive(Deserialize, Debug, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TaxaQuery {
    /// Absent means every rank, which is what an unfiltered combobox wants.
    pub rank: Option<String>,
    /// A prefix of the name, two characters or more. Shorter matches nothing.
    pub q: String,
    /// At most this many matches, capped at 20. Default 10.
    pub limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub struct TaxaResults<'a> {
    #[schema(value_type = Vec<Taxon>)]
    matches: Vec<&'a Taxon>,
}

/// Typeahead over the distinct `(rank, name)` table. `limit` is capped at 20
/// however large the caller asks for.
#[utoipa::path(
    get,
    path = "/api/taxa",
    params(TaxaQuery),
    responses((status = 200, description = "Matches, most records first", body = TaxaResults))
)]
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
