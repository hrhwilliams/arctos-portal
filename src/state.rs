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

/// What a search returns.
///
/// Elasticsearch's envelope — `hits.hits[]._source`, `aggregations`, `took`,
/// `_shards` — is decoded here and goes no further, so nothing downstream has
/// to know the wire format to read a record.
#[derive(Serialize, Debug)]
pub struct SearchResults {
    pub total: Total,
    pub summary: Summary,
    /// One `_source` object per hit, carrying the [`crate::translate::SOURCE`]
    /// fields. Left as JSON: `events` and `relations` are nested arrays, and
    /// `SOURCE` stays the single list the CSV columns are also built from.
    pub records: Vec<Value>,
}

/// Capped by `track_total_hits`: `relation` is `eq` when `value` is exact and
/// `gte` when there are more matches than the cap, which is what lets a caller
/// render "10,000+" rather than a wrong number.
#[derive(Serialize, Deserialize, Debug)]
pub struct Total {
    pub value: u64,
    pub relation: String,
}

/// "Out of `context` records, `matched` carry the attributes you asked for."
#[derive(Serialize, Debug, Default)]
pub struct Summary {
    pub context: u64,
    pub matched: u64,
}

/// The envelope, named only so serde can take it apart. ES answers under
/// `aggregations`, not the `aggs` key the query is written with.
#[derive(Deserialize)]
struct EsResponse {
    hits: EsHits,
    aggregations: Option<EsAggregations>,
}

#[derive(Deserialize)]
struct EsHits {
    total: Total,
    hits: Vec<EsHit>,
}

/// A search carries `_source`; an export asks for `docvalue_fields` instead and
/// gets `fields`. Both are optional so the one envelope decodes either.
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
            total: response.hits.total,
            // An empty form carries no aggregation; zeroes are the honest answer.
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

/// Cheap to clone: `reqwest::Client` is `Arc`-backed, so every handler shares
/// the one connection pool, and the schema and taxa table are shared by `Arc`
/// rather than copied per request.
#[derive(Clone)]
pub struct AppState {
    elasticsearch_url: String,
    client: reqwest::Client,
    schema: Arc<Schema>,
    taxa: Arc<Vec<Taxon>>,
}

/// Where [`AppState::save_ducky`] writes: a directory holding one Parquet file
/// per `guid_prefix`, not one file for the whole dump.
pub const PARQUET: &str = "arctos_parquet";

/// The `FROM` clause every reader uses.
///
/// `DuckDB` pushes a single `=` into the Parquet scan but not a disjunction, so
/// `guid IN (…)` — what an export is — materialises all 154 columns of every
/// row before filtering, 10s whether it wants thirty rows or thirty thousand.
/// Compression is not the cost (uncompressed reads no faster) and neither is an
/// index (10ms per guid, which never amortises). Reading fewer rows is the only
/// lever, so the dump is split by `guid_prefix` and the planner skips whole
/// files by their directory name: 1.0s for a one-collection export.
///
/// `hive_partitioning` is what reads the prefix back out of the path — it is
/// not stored inside the files — and what makes that skipping possible.
fn dataset() -> String {
    format!("read_parquet('{PARQUET}/**/*.parquet', hive_partitioning = true)")
}

/// The distinct `guid_prefix`es a set of guids covers, as a SQL list.
///
/// `MSB:Mamm:12345` carries its own collection, so an export never has to ask
/// the index which partitions to read: it is in the guids already.
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

/// Scratch space for `DuckDB` spills and the export's two temporary files.
pub const TEMP_DIR: &str = ".tmp";

/// What a download contains.
///
/// Not `SELECT *`: reading 154 columns for 50,000 scattered rows means reading
/// essentially every byte of the collections they sit in, and it is the whole
/// cost of an export — 40s against 5s for a handful of columns, measured. These
/// are the fields the search page already shows, plus the two the dump holds
/// that a record is not much use without.
const EXPORT_COLUMNS: &[&str] = &[
    "guid",
    "scientific_name",
    "country",
    "state_prov",
    "use_license_url",
    "attributedetail",
    "related_record_cache",
];

/// Guids per Elasticsearch page while exporting. Larger than a search page
/// because nothing is rendered from it; ES's own `index.max_result_window`
/// does not apply to `search_after`, but 10,000 is the customary ceiling.
const EXPORT_PAGE: usize = 10_000;

impl AppState {
    /// Builds the schema and the taxon table from the Parquet and the code
    /// tables, which is the whole of what `/api/schema` and `/api/taxa` serve.
    /// Both are immutable afterwards — the snapshot on disk does not change
    /// under a running process, so there is no refresh loop to fail.
    ///
    /// # Errors
    ///
    /// Returns an error if the Parquet is unreadable or a code table is
    /// missing. There is no partial schema; the service does not start.
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
    /// Returns an error if the Elasticsearch request fails, returns a
    /// non-success status, or answers with a body that is not the expected
    /// envelope.
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

    /// Every guid the form matches, in guid order, capped at
    /// [`MAX_EXPORT_ROWS`].
    ///
    /// `search_after` rather than `from`/`size`: an export routinely runs past
    /// the 10,000-document result window that paging is bounded by.
    ///
    /// # Errors
    ///
    /// Returns an error if any page of the Elasticsearch request fails or
    /// answers with something that is not the search envelope. A partial export
    /// is worse than none — a caller cannot tell it apart from a small result.
    #[tracing::instrument(skip(self))]
    pub async fn export_guids(&self, search_form: &SearchForm) -> Result<Vec<String>, AppError> {
        let mut guids: Vec<String> = Vec::new();
        let mut after: Option<String> = None;

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
            // doc values are always a list, one element for a single-valued field
            guids.extend(
                response
                    .hits
                    .hits
                    .iter()
                    .filter_map(|h| h.fields["guid"][0].as_str().map(ToString::to_string)),
            );

            // the index sorts by guid, so the last one is where the next page starts
            after = guids.last().cloned();
            if page < EXPORT_PAGE || guids.len() >= MAX_EXPORT_ROWS || after.is_none() {
                break;
            }
        }

        guids.truncate(MAX_EXPORT_ROWS);
        tracing::info!("exporting {} records", guids.len());
        Ok(guids)
    }

    /// The full Parquet rows for `guids`, as gzipped CSV.
    ///
    /// Elasticsearch answers *which* records match; the Parquet holds *what*
    /// they are, every column of the dump rather than the handful the search
    /// page renders. The guids go to disk and are semi-joined rather than
    /// inlined as a 100,000-term `IN` list, and `DuckDB` writes the gzip itself.
    ///
    /// Blocking: `DuckDB` is synchronous, so call this from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary files cannot be written or read, or if
    /// the Parquet cannot be queried.
    #[tracing::instrument(skip(guids))]
    pub fn export_csv_gz(guids: &[String]) -> Result<Vec<u8>, AppError> {
        std::fs::create_dir_all(TEMP_DIR)?;
        let stem = format!("{TEMP_DIR}/export_{:?}", std::thread::current().id());
        let (guid_file, csv_file) = (format!("{stem}.guids"), format!("{stem}.csv.gz"));
        std::fs::write(&guid_file, guids.join("\n"))?;

        let conn = Connection::open_in_memory()?;
        conn.execute_batch(&format!("SET temp_directory = '{TEMP_DIR}';"))?;
        let prefixes = prefixes_of(guids);
        let columns = EXPORT_COLUMNS
            .iter()
            .map(|c| format!("p.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        // an empty match set is still a valid export: headers, no rows
        let rows = if guids.is_empty() {
            format!("SELECT {columns} FROM {} p LIMIT 0", dataset())
        } else {
            // The prefix filter is the point of the partitioning: it decides
            // which files are opened at all, so an export of one collection
            // never touches the other 293. The join then picks the rows.
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

        // ponytail: the whole gzip is buffered before the first byte is sent —
        // ~30 MB at the row cap. Stream the file if that stops being acceptable.
        let bytes = std::fs::read(&csv_file)?;
        // The collection count is the cost: pruning leaves the matched
        // partitions to be read whole, so a search spanning most of them costs
        // what reading the whole dump costs.
        tracing::info!(
            "{} rows across {} collections to {} bytes gzipped in {:.1}s",
            guids.len(),
            prefixes.split(',').filter(|p| !p.is_empty()).count(),
            bytes.len(),
            now.elapsed().as_secs_f64()
        );

        // best-effort: a leftover temp file is not worth failing a good export
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

        // distinct, sorted, and one entry however many guids share a collection
        assert_eq!(
            prefixes_of(&guids(&["MSB:Mamm:1", "MSB:Mamm:2", "UAM:Arc:9"])),
            "'MSB:Mamm', 'UAM:Arc'"
        );
        // the catalog number is the last segment, never part of the prefix
        assert_eq!(prefixes_of(&guids(&["MVZ:Bird:12:3"])), "'MVZ:Bird:12'");
        // a guid with no prefix at all is skipped, not turned into a bad filter
        assert_eq!(prefixes_of(&guids(&["nonsense"])), "");
        assert_eq!(prefixes_of(&[]), "");
        // and a quote is escaped rather than closing the string
        assert_eq!(prefixes_of(&guids(&["O'X:Mamm:1"])), "'O''X:Mamm'");
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
            // ES answers under `aggregations`, though the query asks with `aggs`
            "aggregations": { "summary": {
                "doc_count": 99,
                "context": { "doc_count": 40 },
                "matched": { "doc_count": 7 }
            } }
        }));

        // records are the `_source` objects, with `_index`/`_id`/`_score` dropped
        assert_eq!(results.records.len(), 2);
        assert_eq!(results.records[0]["guid"], "MSB:Mamm:1");
        assert!(results.records[0].get("_index").is_none());

        // `gte` survives, or a caller cannot tell a capped count from an exact one
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
        // a silent zero here would read as "no matches" to every caller
        let body = json!({ "error": { "type": "search_phase_execution_exception" } });
        assert!(serde_json::from_value::<EsResponse>(body).is_err());
    }
}
