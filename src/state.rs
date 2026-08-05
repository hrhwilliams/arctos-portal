use duckdb::{Connection, arrow::util::pretty::pretty_format_batches, params};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::{errors::AppError, search::SearchForm, translate::translate};

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

#[derive(Deserialize)]
struct EsHit {
    #[serde(rename = "_source")]
    source: Value,
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
/// the one connection pool.
#[derive(Clone)]
pub struct AppState {
    elasticsearch_url: String,
    client: reqwest::Client,
}

impl AppState {
    #[must_use]
    pub fn new(elasticsearch_url: &str) -> Self {
        Self {
            elasticsearch_url: elasticsearch_url.into(),
            client: reqwest::Client::new(),
        }
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

    /// Rewrite a CSV as Parquet. `DuckDB` streams `COPY`, so neither the CSV nor
    /// an intermediate table is ever held whole — the in-memory connection is
    /// just a query engine here, and only the Parquet file is written.
    ///
    /// <https://duckdb.org/docs/current/clients/rust>
    ///
    /// # Errors
    ///
    /// Returns an error if the CSV cannot be read or parsed, or if the Parquet
    /// file cannot be written.
    #[tracing::instrument(skip(self))]
    pub fn save_ducky(&self, csv_file: &str) -> Result<(), AppError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "SET preserve_insertion_order = false;
             SET temp_directory = '.tmp';
             SET memory_limit = '16GB';",
        )?;

        let exists: bool = conn.query_row(
            "SELECT count(*) > 0 FROM glob('arctos.parquet')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            tracing::info!("arctos.parquet already exists, skipping conversion");
            return Ok(());
        }

        let now = Instant::now();
        conn.execute(
            "COPY (SELECT * FROM read_csv(?, sample_size = -1, store_rejects = true))
             TO 'arctos.parquet' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 9)",
            params![csv_file],
        )?;
        tracing::info!("took {} seconds", now.elapsed().as_secs_f64());

        let mut stmt = conn.prepare("SELECT * FROM read_parquet('arctos.parquet') LIMIT 10")?;
        let batches: Vec<_> = stmt.query_arrow([])?.collect();
        tracing::debug!("{}", pretty_format_batches(&batches)?);

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

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
