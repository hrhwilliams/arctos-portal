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
    state::{AppState, export_columns},
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
/// The function returns an error when the Elasticsearch request fails.
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

/// This function exports the whole matching set of records.
///
/// `?cols=` sets the columns to export. Without `?cols=`, the export uses the
/// default column set.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] when `cols` names a column
/// that the dump does not have. The function returns an error when the
/// Elasticsearch request fails or when the system cannot read the Parquet
/// file.
#[tracing::instrument(skip(app_state))]
pub async fn download(
    State(app_state): State<AppState>,
    Form(search_form): Form<SearchForm>,
) -> Result<Response, AppError> {
    let columns = export_columns(
        search_form.cols.as_deref(),
        &app_state.schema().columns,
    )?;
    let guids = app_state.export_guids(&search_form).await?;
    let gz = tokio::task::spawn_blocking(move || AppState::export_csv_gz(&guids, &columns))
        .await
        .map_err(std::io::Error::other)??;

    Ok((
        [
            (CONTENT_TYPE, "application/gzip".to_owned()),
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

/// The output has one row per record and one column per [`SOURCE`] field. A
/// field that is not a single value (`events`, `relations`) keeps its JSON
/// text in the cell.
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

/// This function follows RFC 4180. It wraps the value in quotes when the
/// value has a delimiter. It doubles each quote inside the value.
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
