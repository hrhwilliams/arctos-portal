use std::{path::Path, sync::Arc};

use duckdb::Connection;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::{
    errors::AppError,
    schema::{MAX_EXPORT_ROWS, Schema, Taxon, build_taxa},
    search::SearchForm,
    translate::{export_query, translate},
};

/// This struct holds what a search returns.
///
/// This module decodes the Elasticsearch envelope here (`hits.hits[]._source`,
/// `aggregations`, `took`, `_shards`). No other module reads the wire format.
#[derive(Serialize, Debug)]
pub struct SearchResults {
    pub total: Total,
    pub summary: Summary,
    /// This field holds one `_source` object per hit. Each object carries the
    /// [`crate::translate::SOURCE`] fields. The value stays as JSON, because
    /// `events` and `relations` are nested arrays.
    pub records: Vec<Value>,
}

/// `track_total_hits` caps this value. `relation` is `eq` when `value` is
/// exact. `relation` is `gte` when the match count exceeds the cap. This lets
/// a caller render "10,000+" instead of a wrong number.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Total {
    pub value: u64,
    pub relation: String,
}

/// This struct reports: "Out of `context` records, `matched` carry the
/// attributes you asked for."
#[derive(Serialize, Debug, Default)]
pub struct Summary {
    pub context: u64,
    pub matched: u64,
}

/// This struct names the Elasticsearch envelope so serde can decode it. ES
/// answers under the `aggregations` key, not the `aggs` key the query uses.
#[derive(Deserialize)]
struct EsResponse {
    hits: EsHits,
    aggregations: Option<EsAggregations>,
}

#[derive(Deserialize)]
struct EsHits {
    #[serde(default)]
    total: Option<Total>,
    hits: Vec<EsHit>,
}

/// A search response carries `_source`. An export response carries `fields`
/// instead, because the export query asks for `docvalue_fields`. Both fields
/// are optional, so one struct decodes either response.
#[derive(Deserialize)]
struct EsHit {
    #[serde(rename = "_source", default)]
    source: Value,
    #[serde(default)]
    fields: Value,
}

#[derive(Deserialize)]
struct EsAggregations {
    summary: EsSummary,
}

#[derive(Deserialize)]
struct EsSummary {
    context: DocCount,
    matched: DocCount,
}

#[derive(Deserialize)]
struct DocCount {
    doc_count: u64,
}

impl From<EsResponse> for SearchResults {
    fn from(response: EsResponse) -> Self {
        Self {
            total: response.hits.total.unwrap_or_default(),
            // An empty form carries no aggregation. Zero is the correct value.
            summary: response
                .aggregations
                .map_or_else(Summary::default, |a| Summary {
                    context: a.summary.context.doc_count,
                    matched: a.summary.matched.doc_count,
                }),
            records: response.hits.hits.into_iter().map(|h| h.source).collect(),
        }
    }
}

/// A clone of this struct is cheap. `reqwest::Client` uses `Arc` internally,
/// so every handler shares one connection pool. The schema and the taxa
/// table are also shared through `Arc`. No request copies them.
#[derive(Clone)]
pub struct AppState {
    elasticsearch_url: String,
    client: reqwest::Client,
    schema: Arc<Schema>,
    taxa: Arc<Vec<Taxon>>,
}

/// This directory holds one Parquet file per `guid_prefix`. The dump does not
/// use one file for the whole dataset.
pub const PARQUET: &str = "arctos_parquet";

/// This function returns the `FROM` clause every reader uses.
///
/// The dump is split into one directory per `guid_prefix`. This split lets
/// the planner skip whole files by directory name. See docs/04 for the
/// measurements behind this design.
///
/// `hive_partitioning` reads the prefix back out of the directory path. The
/// files do not store the prefix.
fn dataset() -> String {
    format!("read_parquet('{PARQUET}/**/*.parquet', hive_partitioning = true)")
}

/// This function returns the distinct `guid_prefix` values a set of guids
/// covers, as a SQL list.
///
/// Each guid carries its own collection prefix, for example
/// `MSB:Mamm:12345`. An export reads the partitions to open from the guids.
/// It does not query the index for this list.
fn prefixes_of(guids: &[String]) -> String {
    guids
        .iter()
        .filter_map(|g| g.rsplit_once(':').map(|(prefix, _)| prefix))
        .collect::<std::collections::BTreeSet<_>>()
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// This directory holds `DuckDB` spill files and the export's two temporary
/// files.
pub const TEMP_DIR: &str = ".tmp";

/// This list sets the default download columns. It does not use `SELECT *`.
/// The column count sets the cost of an export. See docs/04 for the
/// measurements. This list holds the fields the search page already shows,
/// plus two more fields a record needs.
const EXPORT_COLUMNS: &[&str] = &[
    "guid",
    "scientific_name",
    "country",
    "state_prov",
    "use_license_url",
    "attributedetail",
    "related_record_cache",
];

/// This function checks the columns a download asks for against the columns
/// the dump holds. `None` or an empty value returns [`EXPORT_COLUMNS`].
///
/// The function clones each returned name from the allowlist entry. It never
/// clones the caller's string. A name that is not a dump column returns a
/// 400 error. A name that is a dump column carries only itself into the SQL.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] and names the first column
/// the dump does not have. The function also returns this error when the
/// column list resolves to nothing.
pub fn export_columns(requested: Option<&str>, known: &[String]) -> Result<Vec<String>, AppError> {
    let Some(requested) = requested.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(EXPORT_COLUMNS.iter().map(|&c| c.to_owned()).collect());
    };

    let columns = requested
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            known
                .iter()
                .find(|k| k.as_str() == c)
                .cloned()
                .ok_or_else(|| AppError::BadRequest(format!("no column named `{c}`")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if columns.is_empty() {
        return Err(AppError::BadRequest("`cols` names no columns".to_owned()));
    }
    Ok(columns)
}

/// This constant sets the guid count per Elasticsearch page during an export.
/// This page is larger than a search page, because an export renders nothing
/// from a page. `index.max_result_window` does not limit `search_after`, but
/// 10,000 is the usual page size.
const EXPORT_PAGE: usize = 10_000;

impl AppState {
    /// This function builds the schema and the taxon table from the Parquet
    /// file and the code tables. `/api/schema` and `/api/taxa` serve this
    /// data. Neither the schema nor the taxon table changes after this
    /// function returns. The snapshot on disk does not change while the
    /// process runs, so this service has no refresh loop.
    ///
    /// # Errors
    ///
    /// The function returns an error when it cannot read the Parquet file or
    /// when a code table is missing. In this case, the service does not
    /// start.
    #[tracing::instrument]
    pub fn new(
        elasticsearch_url: &str,
        code_tables: &Path,
        snapshot_date: &str,
    ) -> Result<Self, AppError> {
        let now = Instant::now();
        let conn = Connection::open_in_memory()?;
        let taxa = build_taxa(&conn, &dataset())?;
        let schema = Schema::build(&conn, &dataset(), code_tables, snapshot_date, &taxa)?;
        tracing::info!(
            "schema: {} attribute types, {} vocabularies, {} countries, {} states, {} prefixes, \
             {} taxa in {:.1}s",
            schema.attribute_types.len(),
            schema.vocabularies.len(),
            schema.countries.len(),
            schema.states.len(),
            schema.guid_prefixes.len(),
            taxa.len(),
            now.elapsed().as_secs_f64(),
        );

        Ok(Self {
            elasticsearch_url: elasticsearch_url.into(),
            client: reqwest::Client::new(),
            schema: Arc::new(schema),
            taxa: Arc::new(taxa),
        })
    }

    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    #[must_use]
    pub fn taxa(&self) -> &[Taxon] {
        &self.taxa
    }

    /// # Errors
    ///
    /// The function returns an error when the Elasticsearch request fails,
    /// when Elasticsearch returns a non-success status, or when the response
    /// body does not match the expected envelope.
    #[tracing::instrument(skip(self))]
    pub async fn search(&self, search_form: SearchForm) -> Result<SearchResults, AppError> {
        let query = translate(&search_form);
        tracing::info!("{query:#}");

        let response = self
            .client
            .post(format!("{}/arctos/_search", self.elasticsearch_url))
            .header(CONTENT_TYPE, "application/json")
            .json(&query)
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json::<EsResponse>().await?.into())
    }

    /// This function returns every guid the form matches, in guid order. The
    /// result is capped at [`MAX_EXPORT_ROWS`].
    ///
    /// This function uses `search_after`, not `from`/`size`. An export often
    /// runs past the 10,000-document result window that page-based paging is
    /// bounded by.
    ///
    /// # Errors
    ///
    /// The function returns an error when any page of the Elasticsearch
    /// request fails or when a response does not match the search envelope.
    /// The function never returns a partial export as a success, because a
    /// caller could not tell a partial export apart from a small result.
    #[tracing::instrument(skip(self))]
    pub async fn export_guids(&self, search_form: &SearchForm) -> Result<Vec<String>, AppError> {
        let mut guids: Vec<String> = Vec::new();
        let mut after: Option<String> = None;
        let now = Instant::now();

        loop {
            let query = export_query(search_form, after.as_deref(), EXPORT_PAGE);
            let response = self
                .client
                .post(format!("{}/arctos/_search", self.elasticsearch_url))
                .header(CONTENT_TYPE, "application/json")
                .json(&query)
                .send()
                .await?
                .error_for_status()?
                .json::<EsResponse>()
                .await?;

            let page = response.hits.hits.len();
            // A doc value is always a list. A single-valued field has one element.
            let page_guids: Vec<String> = response
                .hits
                .hits
                .iter()
                .filter_map(|h| h.fields["guid"][0].as_str().map(ToString::to_string))
                .collect();
            // Hits without guids mean a malformed envelope. Reading `after`
            // from the accumulator instead would re-request this page forever.
            if page > 0 && page_guids.is_empty() {
                return Err(AppError::from(std::io::Error::other(
                    "export page carried hits but no guid doc values",
                )));
            }

            // The index sorts by guid. The next page starts at the last guid.
            after = page_guids.last().cloned();
            guids.extend(page_guids);
            if page < EXPORT_PAGE || guids.len() >= MAX_EXPORT_ROWS || after.is_none() {
                break;
            }
        }

        guids.truncate(MAX_EXPORT_ROWS);
        tracing::info!(
            "{} guids from elasticsearch in {:.1}s",
            guids.len(),
            now.elapsed().as_secs_f64()
        );
        Ok(guids)
    }

    /// This function returns the `columns` of the Parquet rows for `guids`,
    /// as gzipped CSV.
    ///
    /// Elasticsearch answers which records match. The Parquet file holds the
    /// record data. This function writes the guids to disk and joins them
    /// against the Parquet rows. `DuckDB` writes the gzip file.
    ///
    /// The caller must build `columns` with [`export_columns`]. That function
    /// checks each column name against the dump before this function inserts
    /// the name into the query.
    ///
    /// This function blocks the calling thread, because `DuckDB` runs on one
    /// thread. Call this function from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// The function returns an error when it cannot write or read the
    /// temporary files, or when it cannot query the Parquet file.
    #[tracing::instrument(skip(guids))]
    pub fn export_csv_gz(guids: &[String], columns: &[String]) -> Result<Vec<u8>, AppError> {
        std::fs::create_dir_all(TEMP_DIR)?;
        let stem = format!("{TEMP_DIR}/export_{:?}", std::thread::current().id());
        let (guid_file, csv_file) = (format!("{stem}.guids"), format!("{stem}.csv.gz"));
        std::fs::write(&guid_file, guids.join("\n"))?;

        let conn = Connection::open_in_memory()?;
        conn.execute_batch(&format!("SET temp_directory = '{TEMP_DIR}';"))?;
        let prefixes = prefixes_of(guids);
        let columns = columns
            .iter()
            // The function quotes each column name, because a dump column
            // name comes from the CSV header and may not be a bare SQL name.
            .map(|c| format!("p.\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        // An empty match set is a valid export. It has headers and no rows.
        let rows = if guids.is_empty() {
            format!("SELECT {columns} FROM {} p LIMIT 0", dataset())
        } else {
            // The prefix filter picks which files DuckDB opens. The join
            // then picks the matching rows from those files.
            format!(
                "SELECT {columns} FROM {} p SEMI JOIN
                 read_csv('{guid_file}', header = false, columns = {{'guid': 'VARCHAR'}}) g
                 USING (guid)
                 WHERE p.guid_prefix IN ({prefixes})",
                dataset()
            )
        };
        let now = Instant::now();
        conn.execute(
            &format!("COPY ({rows}) TO '{csv_file}' (FORMAT csv, COMPRESSION gzip)"),
            [],
        )?;

        // ponytail: this function buffers the whole gzip file before it sends
        // the first byte, about 30 MB at the row cap. Stream the file if that
        // stops being acceptable.
        let bytes = std::fs::read(&csv_file)?;
        tracing::info!(
            "{} rows across {} collections to {} bytes gzipped in {:.1}s",
            guids.len(),
            prefixes.split(',').filter(|p| !p.is_empty()).count(),
            bytes.len(),
            now.elapsed().as_secs_f64()
        );

        // This cleanup is best-effort. A leftover temp file does not fail a
        // good export.
        drop(std::fs::remove_file(&guid_file));
        drop(std::fs::remove_file(&csv_file));
        Ok(bytes)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_partitions_an_export_reads_come_out_of_the_guids() {
        let guids = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();

        // The result list is distinct and sorted. One collection with several
        // guids produces one entry.
        assert_eq!(
            prefixes_of(&guids(&["MSB:Mamm:1", "MSB:Mamm:2", "UAM:Arc:9"])),
            "'MSB:Mamm', 'UAM:Arc'"
        );
        // The catalog number is the last segment. It is not part of the prefix.
        assert_eq!(prefixes_of(&guids(&["MVZ:Bird:12:3"])), "'MVZ:Bird:12'");
        // A guid with no prefix is skipped. It does not become a bad filter.
        assert_eq!(prefixes_of(&guids(&["nonsense"])), "");
        assert_eq!(prefixes_of(&[]), "");
        // A quote inside a guid is escaped. It does not close the SQL string.
        assert_eq!(prefixes_of(&guids(&["O'X:Mamm:1"])), "'O''X:Mamm'");
    }

    #[test]
    fn cols_is_resolved_against_the_dump_or_refused() {
        let known: Vec<String> = ["guid", "country", "sex"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cols = |c: Option<&str>| export_columns(c, &known);

        assert_eq!(cols(Some("country,guid")).unwrap(), ["country", "guid"]);
        // An absent value, a blank value, and a whitespace value all mean the
        // default set.
        assert_eq!(cols(None).unwrap(), EXPORT_COLUMNS);
        assert_eq!(cols(Some("  ")).unwrap(), EXPORT_COLUMNS);
        // The function trims spaces around each name.
        assert_eq!(cols(Some(" sex , guid ")).unwrap(), ["sex", "guid"]);

        // The function refuses any name the dump does not have.
        assert!(cols(Some("guid,nope")).is_err());
        assert!(cols(Some("guid\" FROM x; --")).is_err());
        assert!(cols(Some("GUID")).is_err(), "match is exact, not case-folded");
        // A list that resolves to nothing is refused.
        assert!(cols(Some(",,")).is_err());
    }

    fn decode(body: &Value) -> SearchResults {
        serde_json::from_value::<EsResponse>(body.clone())
            .unwrap()
            .into()
    }

    #[test]
    fn the_envelope_is_unwrapped_to_records_and_a_summary() {
        let results = decode(&json!({
            "took": 5,
            "_shards": { "total": 1 },
            "hits": {
                "total": { "value": 10_000, "relation": "gte" },
                "max_score": null,
                "hits": [
                    { "_index": "arctos", "_id": "x", "_source": { "guid": "MSB:Mamm:1" } },
                    { "_index": "arctos", "_id": "y", "_source": { "guid": "MSB:Mamm:2" } }
                ]
            },
            // ES answers under the `aggregations` key, though the query asks
            // under the `aggs` key.
            "aggregations": { "summary": {
                "doc_count": 99,
                "context": { "doc_count": 40 },
                "matched": { "doc_count": 7 }
            } }
        }));

        // Each record is a `_source` object. The result drops `_index`,
        // `_id`, and `_score`.
        assert_eq!(results.records.len(), 2);
        assert_eq!(results.records[0]["guid"], "MSB:Mamm:1");
        assert!(results.records[0].get("_index").is_none());

        // The `gte` relation must survive. Without it, a caller cannot tell a
        // capped count from an exact count.
        assert_eq!(results.total.value, 10_000);
        assert_eq!(results.total.relation, "gte");

        assert_eq!(results.summary.context, 40);
        assert_eq!(results.summary.matched, 7);
    }

    #[test]
    fn a_response_without_aggregations_summarises_as_zero() {
        let results = decode(&json!({
            "hits": { "total": { "value": 0, "relation": "eq" }, "hits": [] }
        }));
        assert!(results.records.is_empty());
        assert_eq!(results.summary.matched, 0);
    }

    #[test]
    fn a_body_that_is_not_the_envelope_is_an_error_not_an_empty_result() {
        // A silent zero result here would read as "no matches" to every caller.
        let body = json!({ "error": { "type": "search_phase_execution_exception" } });
        assert!(serde_json::from_value::<EsResponse>(body).is_err());
    }
}
