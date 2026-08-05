use axum::{
    Json,
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

use crate::{
    errors::AppError,
    search::{Format, SearchForm},
    state::AppState,
    translate::SOURCE,
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

/// # Errors
///
/// Returns an error if the Elasticsearch request fails.
#[tracing::instrument(skip(app_state))]
pub async fn search(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let format = search_form.format;
    let results = app_state.search(search_form).await?;

    Ok(match format {
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

/// The whole matching set, not a page: Elasticsearch answers which records
/// match and `DuckDB` reads their full rows out of the Parquet.
///
/// # Errors
///
/// Returns an error if the Elasticsearch request fails or the Parquet cannot be
/// read.
#[tracing::instrument(skip(app_state))]
pub async fn download(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let guids = app_state.export_guids(&search_form).await?;
    // DuckDB is synchronous and this is minutes of work at the row cap
    let gz = tokio::task::spawn_blocking(move || AppState::export_csv_gz(&guids))
        .await
        .map_err(std::io::Error::other)??;

    Ok((
        [
            (CONTENT_TYPE, "application/gzip".to_owned()),
            // already gzip: says so, and stops the compression layer wrapping it again
            (CONTENT_ENCODING, "identity".to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("attachment;filename=\"arctos_{}.csv.gz\"", timestamp()),
            ),
        ],
        gz,
    )
        .into_response())
}

fn timestamp() -> String {
    OffsetDateTime::now_utc().format(&STAMP).unwrap_or_default()
}

/// One row per record, one column per [`SOURCE`] field. A field that is not a
/// scalar (`events`, `relations`) keeps its JSON in the cell rather than being
/// flattened, so nothing is silently dropped.
fn to_csv(records: &[Value]) -> String {
    let mut out = SOURCE.join(",");
    for record in records {
        out.push('\n');
        let row: Vec<String> = SOURCE
            .iter()
            .map(|field| match &record[field] {
                Value::Null => String::new(),
                Value::String(s) => escape(s),
                other => escape(&other.to_string()),
            })
            .collect();
        out.push_str(&row.join(","));
    }
    out.push('\n');
    out
}

/// RFC 4180: wrap in quotes if the value carries a delimiter, and double any
/// quote inside it.
fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stamp_is_basic_iso_8601_with_no_separators() {
        let s = timestamp();
        // 20260728T234028Z — nothing a filesystem or a shell would object to
        assert_eq!(s.len(), 16);
        assert_eq!(&s[8..9], "T");
        assert!(s.ends_with('Z'));
        assert!(s[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(s[9..15].chars().all(|c| c.is_ascii_digit()));
        assert!(!s.contains([':', '-']));
    }

    #[test]
    fn csv_quotes_only_what_needs_it_and_keeps_nested_values() {
        // a record, not an Elasticsearch envelope — the wire format stops at the
        // seam, so this test no longer has to know it
        let records = vec![json!({
            "guid": "MSB:Mamm:1",
            "scientific_name": "Sorex \"cinereus\", sensu lato",
            "relations": [{ "relationship": "host of parasite" }],
            "country": "United States"
        })];
        let csv = to_csv(&records);
        let lines: Vec<&str> = csv.lines().collect();

        assert_eq!(lines[0], SOURCE.join(","));
        // plain value unquoted; comma and embedded quote force quoting, and the
        // inner `"` doubles
        assert!(lines[1].starts_with("MSB:Mamm:1,\"Sorex \"\"cinereus\"\", sensu lato\","));
        // nested arrays survive as JSON in one cell, so the comma inside quotes
        assert!(lines[1].contains("\"[{\"\"relationship\"\":\"\"host of parasite\"\"}]\""));
        // absent fields are empty cells, so the missing trailing one is bare
        assert!(lines[1].ends_with(','));
        assert_eq!(lines.len(), 2);
    }
}
