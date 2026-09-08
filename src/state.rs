use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use duckdb::Connection;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
    errors::AppError,
    schema::{Schema, Taxon, build_taxa},
    search::SearchForm,
    translate::{
        PER_PAGE, SOURCE, attr_rows, export_query, relation_matches, taxon_rows, translate,
    },
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

/// This struct reports the search as a funnel.
///
/// There are `taxon` records of the taxa asked for, `context` of them match the
/// rest of the form, each attribute row carries the count in `attrs`, and
/// `matched` match the whole form.
///
/// A row count is measured against `context`, not against `matched`. The row
/// counts therefore do not sum to `matched`: a record carrying two of the
/// attributes is counted in both rows.
///
/// `/api/relations` reuses `context` and `matched` for its own two numbers and
/// leaves the rest out.
#[derive(Serialize, Debug, Default)]
pub struct Summary {
    /// The taxon rows alone, before any other filter narrows them. Absent when
    /// the form is not a taxon search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taxon: Option<u64>,
    pub context: u64,
    pub matched: u64,
    /// One entry per `attr` row of the request, in the order it was sent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<AttrCount>,
}

/// One attribute row, and how many of the `context` records carry it.
#[derive(Serialize, Debug)]
pub struct AttrCount {
    /// The row as the request sent it, so the client can line the count up with
    /// the row it drew.
    pub attr: String,
    pub count: u64,
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

/// A doc value is always a list. A single-valued field has one element.
fn first_guid(fields: &Value) -> Option<&str> {
    fields.get("guid")?.get(0)?.as_str()
}

#[derive(Deserialize)]
struct EsAggregations {
    summary: EsSummary,
}

#[derive(Deserialize)]
struct EsSummary {
    #[serde(default)]
    taxon: Option<DocCount>,
    context: DocCount,
    matched: DocCount,
    /// The count of the enclosing `global` aggregation: every record in the
    /// index. It is named here only so the flattened map below does not try to
    /// read it as a row count.
    #[serde(rename = "doc_count", default)]
    _doc_count: Option<u64>,
    /// The per-row counts, keyed `row_0`, `row_1`, and so on. The named fields
    /// above are taken first, so this map holds only the rows.
    #[serde(flatten)]
    rows: std::collections::BTreeMap<String, DocCount>,
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
                    taxon: a.summary.taxon.map(|t| t.doc_count),
                    context: a.summary.context.doc_count,
                    matched: a.summary.matched.doc_count,
                    // The rows are named here, not labelled: the caller pairs
                    // them with the `attr` values it sent.
                    attrs: a
                        .summary
                        .rows
                        .into_iter()
                        .map(|(key, count)| AttrCount {
                            attr: key,
                            count: count.doc_count,
                        })
                        .collect(),
                }),
            records: response.hits.hits.into_iter().map(|h| h.source).collect(),
        }
    }
}

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
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn prefixes_of(guids: &[String]) -> String {
    guids
        .iter()
        .filter_map(|g| g.rsplit_once(':').map(|(prefix, _)| prefix))
        .collect::<std::collections::BTreeSet<_>>()
        .iter()
        .map(|p| quote(p))
        .collect::<Vec<_>>()
        .join(", ")
}

/// This directory holds `DuckDB` spill files and the export's two temporary
/// files.
pub const TEMP_DIR: &str = ".tmp";

/// This list sets the default download columns. It does not use `SELECT *`.
/// The column count sets the cost of an export. See docs/04 for the
/// measurements. This list holds the fields the search page already shows, plus
/// two more fields a record needs, plus the coordinate pair, without which an
/// export cannot be mapped at all.
const EXPORT_COLUMNS: &[&str] = &[
    "guid",
    "scientific_name",
    "country",
    "state_prov",
    "dec_lat",
    "dec_long",
    "use_license_url",
    "attributedetail",
    "related_record_cache",
];

/// This list sets the columns a map needs, and nothing else. `?cols=map` names
/// it, and `/api/berkeleymapper.xml` describes exactly these columns.
///
/// The omissions are the point. `BerkeleyMapper` builds a balloon for every
/// marker up front rather than on click, so `attributedetail`, at about 2.3 KB
/// of JSON per record, is what stops ten thousand points from ever drawing.
const MAP_COLUMNS: &[&str] = &[
    "guid",
    "scientific_name",
    "country",
    "state_prov",
    "dec_lat",
    "dec_long",
    "coordinateuncertaintyinmeters",
];

/// The `?cols=` value that selects [`MAP_COLUMNS`]. The dump has no column of
/// this name for it to shadow.
const MAP_PRESET: &str = "map";

/// This function renders one column of an export's select list.
///
/// The function quotes the name, because a dump column name comes from the CSV
/// header and may not be a bare SQL name. `qualifier` is the table alias the
/// column is read from, `"p."` or nothing.
///
/// An export publishes each column under its dump name. A consumer that needs
/// another name says so in its own configuration: `BerkeleyMapper` reads the
/// coordinate pair through a `<concept datatype="darwin:decimallatitude">` in
/// `src/routes/berkeleymapper.xml`, not through a name this service invents.
fn select_column(column: &str, qualifier: &str) -> String {
    format!("{qualifier}\"{}\"", column.replace('"', "\"\""))
}

/// This function checks the columns a download asks for against the columns
/// the dump holds. `None` or an empty value returns [`EXPORT_COLUMNS`], and
/// `map` alone returns [`MAP_COLUMNS`].
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
    // The preset is a whole value, not a name inside a list. Mixing it with
    // column names would leave the map config describing a set it does not
    // know.
    if requested == MAP_PRESET {
        return Ok(MAP_COLUMNS.iter().map(|&c| c.to_owned()).collect());
    }

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

/// This function names, on each related record, the matched specimens it was
/// reached from.
///
/// The field is [`RELATED_GUID`], the same name and the same meaning the export
/// column carries. It holds an array here and a `"; "`-joined string there,
/// because a CSV cell holds one value.
///
/// A caller cannot work this out for itself. The record's own `relations` array
/// lists every link it has in Arctos, and a page of results never carries the
/// guids of the specimens the search matched.
fn attach_pairings(records: &mut [Value], pairings: &Pairings) {
    for record in records {
        if let Some(guid) = record.get("guid").and_then(Value::as_str)
            && let Some(specimens) = pairings.get(guid)
        {
            record[RELATED_GUID] = json!(specimens);
        }
    }
}

/// This function writes the guid list `DuckDB` joins the dump against.
///
/// Without `pairings` it is one guid per line. With them each line is
/// `guid<TAB>specimen`, so the export can name the specimen the row was reached
/// from. The delimiter is a tab, not a comma: a guid arrives from the index as
/// free text, and a comma inside one would split the line silently. Several
/// specimens join with `"; "`, the separator the dump uses for its own
/// multi-valued columns.
fn guid_file_body(guids: &[String], pairings: Option<&Pairings>) -> String {
    let Some(pairings) = pairings else {
        return guids.join("\n");
    };
    guids
        .iter()
        .map(|guid| {
            let specimens = pairings
                .get(guid)
                .map(|s| s.iter().cloned().collect::<Vec<_>>().join("; "))
                .unwrap_or_default();
            format!("{guid}\t{specimens}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// This function swaps each `row_N` aggregation name for the `attr` row it
/// counted.
///
/// The aggregation names a row by its position, because an Elasticsearch
/// aggregation name cannot hold an arbitrary attribute string. Pairing them back
/// up by position also fixes the order: `row_10` sorts before `row_2` as text.
fn label_rows(search_form: &SearchForm, counts: &[AttrCount]) -> Vec<AttrCount> {
    attr_rows(search_form)
        .into_iter()
        .enumerate()
        .filter_map(|(i, attr)| {
            counts
                .iter()
                .find(|c| c.attr == format!("row_{i}"))
                .map(|c| AttrCount {
                    attr,
                    count: c.count,
                })
        })
        .collect()
}

/// This constant sets the guid count per Elasticsearch page during an export.
/// This page is larger than a search page, because an export renders nothing
/// from a page. `index.max_result_window` does not limit `search_after`, but
/// 10,000 is the usual page size.
const EXPORT_PAGE: usize = 10_000;

/// This constant caps the related records `/api/relations` will list. It is
/// `index.max_terms_count`, the bound on the `terms` filter phase 2 builds.
const MAX_RELATED: usize = 65_536;

/// Each related record's guid, and the matched specimens that named it.
///
/// One related record is commonly named by several of them: a search for the
/// cestodes of `genus|Sorex` matches 229 relations that collapse to 188 records,
/// because an animal catalogued twice — once as a `:Host` record and once as a
/// `:Mamm` record — points at the same parasite lot from both halves.
type Pairings = BTreeMap<String, BTreeSet<String>>;

/// This constant names the export column holding the specimen on the other end
/// of the relation. The dump has no column of this name to collide with.
const RELATED_GUID: &str = "related_guid";

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
            "schema: {} attribute types, {} vocabularies, {} countries, {} states, {} \
             collectors, {} prefixes, {} taxa in {:.1}s",
            schema.attribute_types.len(),
            schema.vocabularies.len(),
            schema.countries.len(),
            schema.states.len(),
            schema.collectors.len(),
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
    /// The function returns [`AppError::BadRequest`] when a `taxon` or `attr`
    /// value does not fit its grammar. The function returns an error when the
    /// Elasticsearch request fails, when Elasticsearch returns a non-success
    /// status, or when the response body does not match the expected envelope.
    #[tracing::instrument(skip(self))]
    pub async fn search(&self, search_form: SearchForm) -> Result<SearchResults, AppError> {
        let query = translate(&search_form, &self.schema.relations)?;
        tracing::info!("{query:#}");

        let mut results: SearchResults = self.ask(&query).await?.into();
        results.summary.attrs = label_rows(&search_form, &results.summary.attrs);
        Ok(results)
    }

    /// This function runs one query against the index and decodes the envelope.
    async fn ask(&self, query: &Value) -> Result<EsResponse, AppError> {
        Ok(self
            .client
            .post(format!("{}/arctos/_search", self.elasticsearch_url))
            .header(CONTENT_TYPE, "application/json")
            .json(query)
            .send()
            .await?
            .error_for_status()?
            .json::<EsResponse>()
            .await?)
    }

    /// This function is phase 1: every record the matched specimens reach, by
    /// the relationships and Related taxa the form names, paired with the
    /// specimens that named it, plus the count of specimens scanned.
    ///
    /// The pairing is kept because it exists nowhere else. A related record's
    /// own row in the dump lists every specimen it is linked to across Arctos,
    /// not the ones this search matched. Both ends are in hand here and nowhere
    /// later, so keeping them costs nothing.
    ///
    /// `tab` narrows the relations to one relationship — the table the user is
    /// looking at. Both the listing and the download run this same function, so
    /// an export cannot disagree with the table it came from.
    ///
    /// The result is sorted and distinct: one related record is often named by
    /// several specimens, and by both catalogued halves of the same animal.
    ///
    /// ponytail: this pages the whole matched set through the service. A nested
    /// composite aggregation over `related_guid` would collect the same set
    /// inside the index; swap it in if a broad search makes this the expensive
    /// half.
    ///
    /// # Errors
    ///
    /// The function returns [`AppError::BadRequest`] when a `taxon` or `attr`
    /// value does not fit its grammar, when `tab` is not a relationship the
    /// schema lists, and when the search reaches more related records than one
    /// `terms` filter holds.
    #[tracing::instrument(skip(self))]
    async fn related_guids(
        &self,
        search_form: &SearchForm,
    ) -> Result<(Pairings, u64), AppError> {
        let rows = taxon_rows(search_form, &self.schema.relations)?;
        let tab = search_form.tab();
        if let Some(tab) = tab
            && !self.schema.relations.iter().any(|r| r.value == tab)
        {
            return Err(AppError::BadRequest(format!(
                "no relationship named `{tab}`"
            )));
        }
        let now = Instant::now();

        let mut guids: Pairings = Pairings::new();
        let mut specimens: u64 = 0;
        let mut after: Option<String> = None;
        loop {
            // This scan reads `relations` and nothing else.
            let mut query = export_query(
                search_form,
                &self.schema.relations,
                after.as_deref(),
                EXPORT_PAGE,
            )?;
            crate::translate::set(&mut query, "_source", json!(["relations"]));
            let response = self.ask(&query).await?;

            let page = response.hits.hits.len();
            for hit in &response.hits.hits {
                specimens = specimens.saturating_add(1);
                // The specimen on this side of the relation. It is read here
                // anyway, to page the scan.
                let specimen = first_guid(&hit.fields).unwrap_or_default();
                for relation in hit
                    .source
                    .get("relations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    // A relation that points outside Arctos carries no
                    // `related_guid`. It names no document to fetch, so this
                    // table cannot show it.
                    if relation_matches(relation, &rows, tab)
                        && let Some(guid) = relation.get("related_guid").and_then(Value::as_str)
                    {
                        guids
                            .entry(guid.to_owned())
                            .or_default()
                            .insert(specimen.to_owned());
                    }
                }
            }
            after = response
                .hits
                .hits
                .last()
                .and_then(|h| first_guid(&h.fields).map(ToOwned::to_owned));
            if page < EXPORT_PAGE || after.is_none() {
                break;
            }
        }

        // A `terms` filter is bounded by `index.max_terms_count`. A truncated
        // guid set would return a plausible, wrong table, so this fails instead.
        if guids.len() > MAX_RELATED {
            return Err(AppError::BadRequest(format!(
                "this search reaches {} related records, more than the {MAX_RELATED} this \
                 table can list — narrow the search",
                guids.len()
            )));
        }
        tracing::info!(
            "{} related records from {specimens} specimens in {:.1}s",
            guids.len(),
            now.elapsed().as_secs_f64()
        );
        Ok((guids, specimens))
    }

    /// This function answers `/api/relations`: the records reached from the
    /// matched specimens, rather than the matched specimens themselves.
    ///
    /// The two tables are independent. This one cannot be built from a page of
    /// [`Self::search`]: relations arrive inside records, and one response
    /// carries one page of them.
    ///
    /// Phase 1 runs the same query the search runs, over the whole matched set,
    /// and keeps the `related_guid` of every relation the form asked for. Phase 2
    /// is an ordinary record query over those guids.
    ///
    /// The predicate belongs to phase 1 alone. A link reads `host of parasite`
    /// from the shrew's side and `parasite of` from the tapeworm's, so testing it
    /// while still on the shrew means phase 2 never reasons about inverses. Every
    /// other filter — locality, date, collection, attributes — constrains the
    /// matched specimens only, which it does by being part of that same phase 1
    /// query. None of them reach the related records.
    ///
    /// # Errors
    ///
    /// The function returns [`AppError::BadRequest`] when a `taxon` or `attr`
    /// value does not fit its grammar, and when the search reaches more related
    /// records than one `terms` filter holds. The function returns an error when
    /// any Elasticsearch request fails.
    #[tracing::instrument(skip(self))]
    pub async fn related(&self, search_form: &SearchForm) -> Result<SearchResults, AppError> {
        let (pairings, specimens) = self.related_guids(search_form).await?;

        // `context` is the specimens the form matched, `matched` the distinct
        // related records they reach. Neither number bounds the other: a host
        // carries several parasites, and one parasite lot is often named by two
        // catalogued halves of the same animal.
        let summary = Summary {
            context: specimens,
            matched: u64::try_from(pairings.len()).unwrap_or(u64::MAX),
            ..Summary::default()
        };
        let guids: Vec<String> = pairings.keys().cloned().collect();
        if guids.is_empty() {
            return Ok(SearchResults {
                total: Total {
                    value: 0,
                    relation: "eq".to_owned(),
                },
                summary,
                records: Vec::new(),
            });
        }

        // Phase 2 is a lookup. The guid set is already the answer.
        let page = search_form.page.unwrap_or(1).max(1);
        let query = json!({
            "track_total_hits": true,
            "from": page.saturating_sub(1).saturating_mul(PER_PAGE),
            "size": PER_PAGE,
            "sort": [{ "guid": "asc" }],
            "_source": SOURCE,
            "query": { "terms": { "guid": guids } }
        });
        let mut results: SearchResults = self.ask(&query).await?.into();
        // `total` counts the related records the index holds. It falls short of
        // `matched` when a related record is not in this snapshot.
        results.summary = summary;
        attach_pairings(&mut results.records, &pairings);
        Ok(results)
    }

    /// This function returns the guids a download covers, in guid order, and for
    /// a related-records download the specimens each of them was reached from.
    ///
    /// A `tab` exports the related records of that table, not the specimens the
    /// search matched — the user downloads the table in front of them. Those
    /// rows carry no trace of which specimen reached them, so the pairing
    /// travels with them to become the `related_guid` column.
    ///
    /// # Errors
    ///
    /// The function returns the errors of [`Self::related_guids`] and
    /// [`Self::export_guids`].
    pub async fn download_guids(
        &self,
        search_form: &SearchForm,
    ) -> Result<(Vec<String>, Option<Pairings>), AppError> {
        if search_form.tab().is_some() {
            let (pairings, _) = self.related_guids(search_form).await?;
            return Ok((pairings.keys().cloned().collect(), Some(pairings)));
        }
        Ok((self.export_guids(search_form).await?, None))
    }

    /// This function returns every guid the form matches, in guid order.
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
            let query = export_query(
                search_form,
                &self.schema.relations,
                after.as_deref(),
                EXPORT_PAGE,
            )?;
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
                .filter_map(|h| first_guid(&h.fields).map(ToString::to_string))
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
            if page < EXPORT_PAGE || after.is_none() {
                break;
            }
        }

        tracing::info!(
            "{} guids from elasticsearch in {:.1}s",
            guids.len(),
            now.elapsed().as_secs_f64()
        );
        Ok(guids)
    }

    /// This function returns the `columns` of the Parquet rows under the
    /// given `guid_prefix` values, as gzipped CSV.
    ///
    /// The dump is partitioned by `guid_prefix`, so a prefix-only filter
    /// needs no Elasticsearch round trip and no guid list: `DuckDB` reads
    /// the matching partitions directly.
    ///
    /// This function blocks the calling thread, because `DuckDB` runs on one
    /// thread. Call this function from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// The function returns an error when it cannot write or read the
    /// temporary file, or when it cannot query the Parquet file.
    #[tracing::instrument]
    pub fn export_csv_gz_by_prefixes(
        prefixes: &[String],
        columns: &[String],
    ) -> Result<Vec<u8>, AppError> {
        std::fs::create_dir_all(TEMP_DIR)?;
        let csv_file = format!("{TEMP_DIR}/export_{:?}.csv.gz", std::thread::current().id());

        let conn = Connection::open_in_memory()?;
        conn.execute_batch(&format!("SET temp_directory = '{TEMP_DIR}';"))?;
        let list = prefixes
            .iter()
            .map(|p| quote(p))
            .collect::<Vec<_>>()
            .join(", ");
        let columns = columns
            .iter()
            .map(|c| select_column(c, ""))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = format!(
            "SELECT {columns} FROM {} WHERE guid_prefix IN ({list}) ORDER BY guid",
            dataset()
        );
        let now = Instant::now();
        conn.execute(
            &format!("COPY ({rows}) TO '{csv_file}' (FORMAT csv, COMPRESSION gzip)"),
            [],
        )?;

        let bytes = std::fs::read(&csv_file)?;
        tracing::info!(
            "{} bytes gzipped from {} prefixes in {:.1}s",
            bytes.len(),
            prefixes.len(),
            now.elapsed().as_secs_f64()
        );
        drop(std::fs::remove_file(&csv_file));
        Ok(bytes)
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
    #[tracing::instrument(skip(guids, pairings))]
    pub fn export_csv_gz(
        guids: &[String],
        pairings: Option<&Pairings>,
        columns: &[String],
    ) -> Result<Vec<u8>, AppError> {
        std::fs::create_dir_all(TEMP_DIR)?;
        let stem = format!("{TEMP_DIR}/export_{:?}", std::thread::current().id());
        let (guid_file, csv_file) = (format!("{stem}.guids"), format!("{stem}.csv.gz"));
        std::fs::write(&guid_file, guid_file_body(guids, pairings))?;

        let conn = Connection::open_in_memory()?;
        conn.execute_batch(&format!("SET temp_directory = '{TEMP_DIR}';"))?;
        let prefixes = prefixes_of(guids);
        let columns = columns
            .iter()
            .map(|c| select_column(c, "p."))
            .collect::<Vec<_>>()
            .join(", ");

        // A related-records export carries the specimen each row was reached
        // from. That pairing is not in the dump, so it rides along in the guid
        // file and joins back on, as one more column.
        let (lookup, join, select, empty_select) = if pairings.is_some() {
            (
                format!(
                    "read_csv('{guid_file}', header = false, delim = '\\t', \
                     columns = {{'guid': 'VARCHAR', '{RELATED_GUID}': 'VARCHAR'}}) g"
                ),
                "JOIN",
                format!("g.\"{RELATED_GUID}\", "),
                format!("NULL::VARCHAR AS \"{RELATED_GUID}\", "),
            )
        } else {
            (
                format!("read_csv('{guid_file}', header = false, columns = {{'guid': 'VARCHAR'}}) g"),
                "SEMI JOIN",
                String::new(),
                String::new(),
            )
        };

        // An empty match set is a valid export. It has headers and no rows.
        let rows = if guids.is_empty() {
            format!(
                "SELECT {empty_select}{columns} FROM {} p LIMIT 0",
                dataset()
            )
        } else {
            // The prefix filter picks which files DuckDB opens. The join
            // then picks the matching rows from those files. Each guid appears
            // once in the file, so the join cannot multiply rows.
            format!(
                "SELECT {select}{columns} FROM {} p {join} {lookup}
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

        // `map` is a preset, not a column name. It resolves whatever the dump
        // holds, because the map config describes exactly this set.
        assert_eq!(cols(Some("map")).unwrap(), MAP_COLUMNS);
        assert_eq!(cols(Some(" map ")).unwrap(), MAP_COLUMNS);
        // It is one whole value. A list that merely contains it is a list of
        // column names, and `map` is not one.
        assert!(cols(Some("map,sex")).is_err());
        assert!(cols(Some("MAP")).is_err());

        // Each column is exported under its own dump name. A quote inside one
        // is escaped, not passed through.
        assert_eq!(select_column("dec_lat", "p."), "p.\"dec_lat\"");
        assert_eq!(select_column("guid", ""), "\"guid\"");
        assert_eq!(select_column("a\"b", ""), "\"a\"\"b\"");

        // The function refuses any name the dump does not have.
        assert!(cols(Some("guid,nope")).is_err());
        assert!(cols(Some("guid\" FROM x; --")).is_err());
        assert!(
            cols(Some("GUID")).is_err(),
            "match is exact, not case-folded"
        );
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
                "taxon": { "doc_count": 2441 },
                "context": { "doc_count": 40 },
                "matched": { "doc_count": 7 },
                "row_0": { "doc_count": 31 },
                "row_1": { "doc_count": 5 }
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

        assert_eq!(results.summary.taxon, Some(2441));
        assert_eq!(results.summary.context, 40);
        assert_eq!(results.summary.matched, 7);
        // The per-row counts arrive keyed by position. The `doc_count` of the
        // enclosing global agg is not one of them.
        assert_eq!(results.summary.attrs.len(), 2);
        assert_eq!(results.summary.attrs[0].attr, "row_0");
        assert_eq!(results.summary.attrs[0].count, 31);
    }

    #[test]
    fn each_related_record_names_the_specimens_it_was_reached_from() {
        let pairings: Pairings = [
            (
                "DMNS:Para:113".to_owned(),
                ["MSB:Host:9", "DMNS:Mamm:8891"]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
            (
                "DMNS:Para:114".to_owned(),
                std::iter::once("DMNS:Mamm:77".to_owned()).collect(),
            ),
        ]
        .into_iter()
        .collect();

        let mut records = vec![
            json!({ "guid": "DMNS:Para:113", "scientific_name": "Cestoda" }),
            json!({ "guid": "DMNS:Para:114" }),
            // A record the pairing does not hold, and one with no guid at all.
            json!({ "guid": "DMNS:Para:999" }),
            json!({ "scientific_name": "Cestoda" }),
        ];
        attach_pairings(&mut records, &pairings);

        // The field holds an array, sorted, and does not disturb the record.
        assert_eq!(records[0]["related_guid"][0], "DMNS:Mamm:8891");
        assert_eq!(records[0]["related_guid"][1], "MSB:Host:9");
        assert_eq!(records[0]["scientific_name"], "Cestoda");
        assert_eq!(records[1]["related_guid"].as_array().unwrap().len(), 1);
        // A record with no pairing gains no field, rather than an empty one.
        assert!(records[2]["related_guid"].is_null());
        assert!(records[3]["related_guid"].is_null());
    }

    #[test]
    fn the_guid_file_carries_the_specimen_each_related_record_was_reached_from() {
        let guids: Vec<String> = ["DMNS:Para:1", "DMNS:Para:2", "DMNS:Para:3"]
            .iter()
            .map(ToString::to_string)
            .collect();

        // Without pairings the file is the plain guid list it has always been.
        let plain = guid_file_body(&guids, None);
        assert_eq!(plain, "DMNS:Para:1\nDMNS:Para:2\nDMNS:Para:3");
        assert!(!plain.contains('\t'));

        let pairings: Pairings = [
            // One parasite lot named by both catalogued halves of one animal.
            (
                "DMNS:Para:1".to_owned(),
                ["MSB:Host:9", "MSB:Mamm:9"]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
            (
                "DMNS:Para:2".to_owned(),
                std::iter::once("DMNS:Mamm:8891".to_owned()).collect(),
            ),
        ]
        .into_iter()
        .collect();

        let body = guid_file_body(&guids, Some(&pairings));
        let lines: Vec<&str> = body.lines().collect();
        // Several specimens join with the separator the dump uses, in sorted
        // order, on one line — the row is not repeated.
        assert_eq!(lines[0], "DMNS:Para:1\tMSB:Host:9; MSB:Mamm:9");
        assert_eq!(lines[1], "DMNS:Para:2\tDMNS:Mamm:8891");
        // A guid the map does not hold still gets its line, so the join keeps
        // the row rather than dropping it.
        assert_eq!(lines[2], "DMNS:Para:3\t");
        assert_eq!(lines.len(), guids.len());
    }

    #[test]
    fn each_row_count_is_labelled_with_the_attr_row_that_asked_for_it() {
        let counts = |keys: &[(&str, u64)]| -> Vec<AttrCount> {
            keys.iter()
                .map(|(attr, count)| AttrCount {
                    attr: (*attr).to_string(),
                    count: *count,
                })
                .collect()
        };
        let form = |attr: &[&str]| SearchForm {
            attr: Some(attr.iter().map(ToString::to_string).collect()),
            taxon: None,
            part: None,
            prefix: None,
            country: None,
            state: None,
            from: None,
            to: None,
            attr_op: crate::search::AttrOp::And,
            locality: None,
            collector: None,
            tab: None,
            page: None,
            format: crate::search::Format::Json,
            cols: None,
        };

        // The label is the row as sent, and the order is the order it was sent
        // in — not the aggregation's own text order, where `row_10` sorts
        // before `row_2`.
        let f = form(&["sex|male", "detected|ectoparasite", "!examined for"]);
        let labelled = label_rows(&f, &counts(&[("row_2", 3), ("row_0", 31), ("row_1", 5)]));
        assert_eq!(
            labelled
                .iter()
                .map(|c| (c.attr.as_str(), c.count))
                .collect::<Vec<_>>(),
            [("sex|male", 31), ("detected|ectoparasite", 5), ("!examined for", 3)]
        );

        // A blank row is not sent to the index, so it has no count to pair with.
        let f = form(&["sex|male", "  "]);
        assert_eq!(label_rows(&f, &counts(&[("row_0", 31)])).len(), 1);
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

    /// This test runs the real export against the real dump, so it needs the
    /// Parquet files on disk. `cargo test -- --ignored` runs it.
    #[test]
    #[ignore = "reads the Parquet dump"]
    fn the_related_export_carries_the_specimen_column() {
        let guids: Vec<String> = ["DMNS:Para:113", "DMNS:Para:114", "DMNS:Para:9999999"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let columns: Vec<String> = ["guid", "scientific_name", "dec_lat", "dec_long"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let pairings: Pairings = [
            (
                "DMNS:Para:113".to_owned(),
                ["MSB:Host:9", "DMNS:Mamm:8891"]
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
            (
                "DMNS:Para:114".to_owned(),
                std::iter::once("DMNS:Mamm:77".to_owned()).collect(),
            ),
        ]
        .into_iter()
        .collect();

        let read = |gz: Vec<u8>| -> String {
            let path = format!("{TEMP_DIR}/verify_{:?}.csv.gz", std::thread::current().id());
            std::fs::write(&path, gz).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            let out: String = conn
                .query_row(
                    &format!("SELECT string_agg(x, '\n') FROM (SELECT * FROM read_csv('{path}', header = false, columns = {{'x': 'VARCHAR'}}))"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            drop(std::fs::remove_file(&path));
            out
        };

        let paired = read(AppState::export_csv_gz(&guids, Some(&pairings), &columns).unwrap());
        println!("--- PAIRED ---\n{paired}");
        let plain = read(AppState::export_csv_gz(&guids, None, &columns).unwrap());
        println!("--- PLAIN ---\n{plain}");
        let empty = read(AppState::export_csv_gz(&[], Some(&pairings), &columns).unwrap());
        println!("--- EMPTY ---\n{empty}");

        // The paired export leads with the specimen column; the plain one has
        // no such column at all. Every column keeps its dump name.
        let header = "related_guid,guid,scientific_name,dec_lat,dec_long";
        assert!(paired.starts_with(header), "{paired}");
        assert!(paired.contains("DMNS:Mamm:8891; MSB:Host:9,DMNS:Para:113"));
        assert!(paired.contains("DMNS:Mamm:77,DMNS:Para:114"));
        assert!(plain.starts_with("guid,scientific_name,dec_lat,dec_long"));
        // A guid the dump does not hold drops out, as it does today.
        assert!(!paired.contains("9999999"));
        // An empty export still carries every column in its header.
        assert_eq!(empty.trim(), header);
    }
}
