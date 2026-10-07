use axum::{
    Json,
    body::Body,
    extract::State,
    http::header::{CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use axum_extra::extract::Form;
use serde_json::Value;
use time::{
    OffsetDateTime,
    format_description::well_known::{
        Iso8601,
        iso8601::{Config, TimePrecision},
    },
};
use tokio::sync::mpsc;

use crate::{
    csv::Csv,
    errors::AppError,
    search::{Format, SearchForm},
    state::{AppState, SearchResults, export_columns},
    summary::SearchSummary,
    translate::{SOURCE, Search},
};

const STAMP: Iso8601<
    {
        Config::DEFAULT
            .set_use_separators(false)
            .set_time_precision(TimePrecision::Second {
                decimal_digits: None,
            })
            .encode()
    },
> = Iso8601;

/// One page of the specimens the form matches, with the summary funnel.
///
/// # Errors
///
/// The function returns an error when the Elasticsearch request fails.
#[utoipa::path(
    get,
    path = "/api/search",
    params(SearchForm),
    responses(
        (status = 200, description = "One page of records and the summary", body = SearchResults),
        (status = 200, description = "The same page as CSV, when `format=csv`", content_type = "text/csv"),
        (status = 400, description = "A `taxon`, `attr`, or `part` row outside the grammar", content_type = "application/json")
    )
)]
#[tracing::instrument(skip(app_state))]
pub async fn search(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let search = Search::parse(&search_form, &app_state.schema().relations)?;
    let results = app_state.search(&search).await?;

    Ok(match search_form.format {
        Format::Json => Json(results).into_response(),
        Format::Csv => (
            [
                (CONTENT_TYPE, "text/csv;charset=utf-8".to_owned()),
                (
                    CONTENT_DISPOSITION,
                    format!("attachment;filename=\"search_{}.csv\"", timestamp()),
                ),
            ],
            to_csv(&results.records),
        )
            .into_response(),
    })
}

/// This function answers the second table: the records reached from the matched
/// specimens, by the relationships and Related taxa the `taxon` rows name.
///
/// The parameters are the search's own, plus a `page` of its own. Every filter
/// applies to the matched specimens; none of them constrain the related records.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] on a malformed `taxon` or
/// `attr`, the same as the search, and when the search reaches more related
/// records than one table can list. The function returns an error when the
/// Elasticsearch request fails.
#[utoipa::path(
    get,
    path = "/api/relations",
    params(SearchForm),
    responses(
        (status = 200, description = "One page of related records; `summary.context` is the matched specimens, `summary.matched` the distinct related records", body = SearchResults),
        (status = 400, description = "A row outside the grammar, an unknown `tab`, or more related records than one table can list", content_type = "application/json")
    )
)]
#[tracing::instrument(skip(app_state))]
pub async fn relations(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let search = Search::parse(&search_form, &app_state.schema().relations)?;
    Ok(Json(app_state.related(&search).await?).into_response())
}

/// This function answers the Summary tab: statistics over every record the
/// search matches and over the related records it reaches, in one response.
///
/// The parameters are the search's own. `page` and `tab` are ignored: one
/// response covers both tables.
///
/// # Errors
///
/// The function returns the same [`AppError::BadRequest`] as the search for a
/// malformed row, and an error when the records request fails. A related set
/// past the table's limit does not fail the summary.
#[utoipa::path(
    get,
    path = "/api/summary",
    params(SearchForm),
    responses(
        (status = 200, description = "Statistics over the matched records and, when the query names a relationship, over the related records", body = SearchSummary),
        (status = 400, description = "A row outside the grammar", content_type = "application/json")
    )
)]
#[tracing::instrument(skip(app_state))]
pub async fn summary(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let search = Search::parse(&search_form, &app_state.schema().relations)?;
    Ok(Json(app_state.summary(&search).await?).into_response())
}

/// This function exports the whole matching set of records, as a CSV sent
/// gzip-encoded.
///
/// `?cols=` sets the columns to export. Without `?cols=`, the export uses the
/// default column set. `?cols=map` uses the smaller set a map needs, which is
/// the one `/api/berkeleymapper.xml` describes.
///
/// `?tab=` exports the related records of that table instead of the specimens
/// the search matched, by the same two phases [`relations`] lists them with. The
/// columns are the same either way: a related record is a record of the same
/// dump.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] when `cols` names a column
/// that the dump does not have. The function returns an error when the
/// Elasticsearch request fails or when the system cannot read the Parquet
/// file.
#[utoipa::path(
    get,
    path = "/api/download",
    params(SearchForm),
    responses(
        (status = 200, description = "Every matching record in a CSV table", content_type = "text/csv"),
        (status = 400, description = "A row outside the grammar, or a `cols` name the dump does not have", content_type = "application/json")
    )
)]
#[tracing::instrument(skip(app_state))]
pub async fn download(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let columns = export_columns(search_form.cols.as_deref(), &app_state.schema().columns)?;
    let search = Search::parse(&search_form, &app_state.schema().relations)?;

    // A prefix-only filter needs no Elasticsearch round trip: the dump is
    // already partitioned by guid_prefix.
    let gz = if search.guid_prefix_only() {
        let prefixes = search.prefixes.clone();
        tokio::task::spawn_blocking(move || {
            AppState::export_csv_gz_by_prefixes(&prefixes, &columns)
        })
        .await
        .map_err(std::io::Error::other)??
    } else {
        let (guids, pairings) = app_state.download_guids(&search).await?;
        tokio::task::spawn_blocking(move || {
            AppState::export_csv_gz(&guids, pairings.as_ref(), &columns)
        })
        .await
        .map_err(std::io::Error::other)??
    };

    // The bytes on the wire are the same gzip stream either way. Declaring them
    // as a gzip-encoded CSV, rather than as a gzip file, is what lets a client
    // decompress them on the way in and hand its caller a CSV — which is what a
    // browser, an undici fetch, and `curl --compressed` all do. The name loses
    // its `.gz` to match: what lands on disk is the CSV.
    //
    // `Content-Encoding` also tells the compression layer that this response is
    // already encoded, so it does not gzip it a second time.
    //
    // ponytail: this is sent unconditionally, without reading `Accept-Encoding`.
    // A client that does not decode gzip — a bare `curl -o`, which decodes only
    // when asked — writes the gzip bytes into a file named `.csv`. Switch on the
    // request header if such a client ever needs serving.
    Ok((
        [
            (CONTENT_TYPE, "text/csv;charset=utf-8".to_owned()),
            (CONTENT_ENCODING, "gzip".to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("attachment;filename=\"arctos_{}.csv\"", timestamp()),
            ),
        ],
        gz,
    )
        .into_response())
}

/// This function is [`download`] read from the index alone.
///
/// No guid list, no Parquet, no `DuckDB`. Same parameters, same `cols` and
/// `tab` semantics, plus a trailing `related_guids` backlink column on every
/// row.
///
/// The body is streamed one page at a time, so the export holds one page in
/// memory whatever its size, and the compression layer gzips it on the way out
/// for any client that sends `Accept-Encoding`.
///
/// # Errors
///
/// The function returns the same 400s as [`download`]. Every check that can
/// refuse the request runs before the status goes out: the form is translated
/// here, and a related-records export runs its phase 1 here. An Elasticsearch
/// failure after that truncates the body.
#[utoipa::path(
    get,
    path = "/api/es-download",
    params(SearchForm),
    responses(
        (status = 200, description = "Every matching record as CSV, streamed, with a trailing `related_guids` column; capped at `limits.max_export_rows`", content_type = "text/csv"),
        (status = 400, description = "A row outside the grammar, or a `cols` name the dump does not have", content_type = "application/json")
    )
)]
#[tracing::instrument(skip(app_state))]
pub async fn es_download(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let columns = export_columns(search_form.cols.as_deref(), &app_state.schema().columns)?;
    let search = Search::parse(&search_form, &app_state.schema().relations)?;
    let pairings = if search.tab.is_some() {
        Some(app_state.related_guids(&search).await?.0)
    } else {
        app_state.check_export_size(&search).await?;
        None
    };

    let (tx, mut rx) = mpsc::channel::<Result<String, std::io::Error>>(2);
    tokio::spawn(async move {
        if let Err(e) = app_state
            .es_export(&search, &columns, pairings.as_ref(), &tx)
            .await
        {
            tracing::error!("es-download failed after the header was sent: {e:?}");
            drop(tx.send(Err(std::io::Error::other(e.to_string()))).await);
        }
    });

    Ok((
        [
            (CONTENT_TYPE, "text/csv;charset=utf-8".to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("attachment;filename=\"arctos_es_{}.csv\"", timestamp()),
            ),
        ],
        Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))),
    )
        .into_response())
}

fn timestamp() -> String {
    OffsetDateTime::now_utc().format(&STAMP).unwrap_or_default()
}

/// The output has one row per record and one column per [`SOURCE`] field. A
/// field that is not a single value (`events`, `relations`) keeps its JSON
/// text in the cell.
fn to_csv(records: &[Value]) -> String {
    let shape = Csv::new(SOURCE.iter().map(|&c| c.to_owned()).collect());
    std::iter::once(shape.header())
        .chain(records.iter().map(|r| shape.line(r)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::string_slice)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stamp_is_basic_iso_8601_with_no_separators() {
        let s = timestamp();
        // Example: 20260728T234028Z. This form is safe for a filename.
        assert_eq!(s.len(), 16);
        assert_eq!(&s[8..9], "T");
        assert!(s.ends_with('Z'));
        assert!(s[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(s[9..15].chars().all(|c| c.is_ascii_digit()));
        assert!(!s.contains([':', '-']));
    }

    #[test]
    fn csv_quotes_only_what_needs_it_and_keeps_nested_values() {
        let records = vec![json!({
            "guid": "MSB:Mamm:1",
            "scientific_name": "Sorex \"cinereus\", sensu lato",
            "relations": [{ "relationship": "host of parasite" }],
            "country": "United States"
        })];
        let csv = to_csv(&records);
        let lines: Vec<&str> = csv.lines().collect();

        assert_eq!(lines[0], SOURCE.join(","));
        // A plain value has no quotes. A comma or a quote forces quotes. Each
        // inner quote doubles.
        assert!(lines[1].starts_with("MSB:Mamm:1,\"Sorex \"\"cinereus\"\", sensu lato\","));
        // A nested array stays as JSON text in one cell.
        assert!(lines[1].contains("\"[{\"\"relationship\"\":\"\"host of parasite\"\"}]\""));
        // A missing field is an empty cell.
        assert!(lines[1].ends_with(','));
        assert_eq!(lines.len(), 2);
    }
}
