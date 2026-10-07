//! `GET /api/schema` and `GET /api/taxa` supply everything the form needs to
//! render itself. The system computes this data once at startup (spec 03).
//!
//! This module uses two data sources.
//!
//! - Code tables (`docs/data/code-tables`, fetched from Arctos apart from this
//!   service) are **complete**. A controlled value with no records still
//!   ships. An option that vanishes between snapshots would change the
//!   meaning of a saved URL.
//! - Aggregations over the Parquet file are **observed**. They carry counts.
//!   They list only the values the data holds.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use duckdb::Connection;
use serde::Serialize;
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::{errors::AppError, ranks, translate::PER_PAGE};

/// Each tuple holds `(id, label, parquet column)`. The list order sets the
/// form render order.
///
/// `scientific_name` is not a rank. It is a Latin binomial. It shares the
/// same (field, value) shape as a rank, so it sits in the same dropdown. It
/// is listed first because users select it first. The ranks themselves come
/// from `src/ranks.json`, through [`crate::ranks`].
fn all_ranks() -> Vec<(&'static str, &'static str, &'static str)> {
    std::iter::once(("scientific_name", "Scientific name", "scientific_name"))
        .chain(ranks::ranks().iter().map(|r| (r.id, r.label, r.column)))
        .collect()
}

/// This constant caps the row count of an export. [`crate::state`] enforces
/// this cap. [`Limits`] serves this cap to the client.
pub const MAX_EXPORT_ROWS: usize = 100_000;

/// This list holds keys that are metadata on a code-table row. None of these
/// keys hold the controlled value. The remaining key is the value column.
/// Each table names its value column differently, for example
/// `examined_detected`, `sex_cde`, or `caste`.
const NON_VALUE_KEYS: &[&str] = &[
    "description",
    "issue_url",
    "documentation_url",
    "collections",
    "recommend_for_collection_type",
    "search_terms",
    "value_code_table",
    "unit_code_table",
    "public",
    "definition",
    "collection_type",
];

/// Everything the search form needs to render itself. Code-table lists are
/// complete and carry no counts. Aggregated lists are observed and carry
/// counts.
#[derive(Serialize, Debug, ToSchema)]
pub struct Schema {
    pub snapshot_date: String,
    /// This field lists every column of the dump, in dump order. The client
    /// can build a column picker from this list. The service also checks
    /// `?cols=` against this list before it builds the export SQL.
    pub columns: Vec<String>,
    pub ranks: Vec<Rank>,
    pub attribute_types: Vec<AttributeType>,
    pub vocabularies: BTreeMap<String, Vec<VocabValue>>,
    pub countries: Vec<Facet>,
    pub states: Vec<Facet>,
    pub collectors: Vec<Facet>,
    pub relations: Vec<Relation>,
    pub guid_prefixes: Vec<GuidPrefix>,
    pub sorts: Vec<Sort>,
    pub limits: Limits,
    pub nonpublic_types_dropped: Vec<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Rank {
    pub id: &'static str,
    pub label: &'static str,
    pub field: &'static str,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct AttributeType {
    pub id: String,
    pub label: String,
    pub description: String,
    /// The name of the code table holding the values, a key of `vocabularies`.
    pub vocabulary: Option<String>,
    pub units_table: Option<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct VocabValue {
    pub value: String,
    pub description: String,
    pub documentation_url: String,
}

/// An observed value and how many records carry it.
#[derive(Serialize, Debug, ToSchema)]
pub struct Facet {
    pub value: String,
    pub count: u64,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Relation {
    pub value: String,
    pub description: String,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct GuidPrefix {
    pub value: String,
    pub count: u64,
    pub institution: String,
    pub collection_cde: String,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Sort {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Limits {
    /// The pager divides by this value. The service fixes the page size. The
    /// service does not accept a `per_page` value. The client must use
    /// `page_size`, not `max_per_page`.
    pub page_size: usize,
    pub max_per_page: usize,
    pub max_result_window: usize,
    pub max_export_rows: usize,
}

/// One distinct `(rank, name)` pair of the snapshot, with its record count and
/// its parent in the rank chain.
#[derive(Serialize, Debug, Clone, ToSchema)]
pub struct Taxon {
    pub rank: &'static str,
    pub name: String,
    pub record_count: u64,
    pub parent_rank: Option<&'static str>,
    pub parent_name: Option<String>,
}

/// Arctos ships a code table as `{"data": [...]}` or as a bare array.
fn load_table(dir: &Path, name: &str) -> Result<Vec<Map<String, Value>>, AppError> {
    let path: PathBuf = dir.join(format!("{name}.json"));
    let doc: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let rows = doc.get("data").unwrap_or(&doc);
    Ok(rows
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_object().cloned())
                .collect()
        })
        .unwrap_or_default())
}

fn str_field(row: &Map<String, Value>, key: &str) -> String {
    match row.get(key) {
        // Arctos returns an array for some url fields. The first element is the url.
        Some(Value::Array(a)) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn is_public(row: &Map<String, Value>) -> bool {
    match row.get("public") {
        Some(Value::Number(n)) => n.as_i64() != Some(0),
        Some(Value::String(s)) => s != "0",
        _ => true,
    }
}

/// The single column holding the controlled value.
fn value_key(rows: &[Map<String, Value>]) -> Option<String> {
    rows.first()?
        .keys()
        .find(|k| !NON_VALUE_KEYS.contains(&k.as_str()))
        .cloned()
}

/// This function builds one entry per distinct `value_code_table` name that a
/// public attribute type names. A table that exists but holds no rows ships
/// as `[]`. The function does not skip an empty table. The client requires
/// every non-null `vocabulary` value to be a key in this map.
fn build_vocabularies(
    dir: &Path,
    types: &[Map<String, Value>],
) -> Result<BTreeMap<String, Vec<VocabValue>>, AppError> {
    let mut vocab = BTreeMap::new();
    for table in types
        .iter()
        .map(|t| str_field(t, "value_code_table"))
        .filter(|t| !t.is_empty())
    {
        if vocab.contains_key(&table) {
            continue;
        }
        let rows = load_table(dir, &table)?;
        let mut values: Vec<VocabValue> = value_key(&rows)
            .map(|key| {
                rows.iter()
                    .map(|row| VocabValue {
                        value: str_field(row, &key),
                        description: str_field(row, "description"),
                        documentation_url: str_field(row, "documentation_url"),
                    })
                    .filter(|v| !v.value.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        // This sort puts a parent value before its child values, for example
        // `ectoparasite` before `ectoparasite: flea`. The form uses this order
        // to indent the hierarchy.
        values.sort_by_key(|v| (v.value.matches(':').count(), v.value.to_lowercase()));
        vocab.insert(table, values);
    }
    Ok(vocab)
}

/// This function converts `MSB:Mamm` into `("MSB", "Mammalogy")`.
/// `ctcollection_cde` lists only the full labels. The function matches the
/// abbreviation as a prefix of a full label.
///
// ponytail: this is a prefix match, not a lookup table. Swap it for a real
// code-to-label column if Arctos ever ships one, or if two collections ever
// share a prefix.
fn split_prefix(prefix: &str, collections: &[String]) -> (String, String) {
    let Some((institution, code)) = prefix.split_once(':') else {
        return (String::new(), String::new());
    };
    let label = collections
        .iter()
        .find(|c| c.to_lowercase().starts_with(&code.to_lowercase()))
        .cloned()
        .unwrap_or_default();
    (institution.to_string(), label)
}

/// This function runs `SELECT value, count(*)` over one column of the dump.
/// The result list holds the most-used value first. The function lists only
/// observed values.
///
/// `dataset` holds the whole `FROM` clause, not a file path. See
/// [`crate::state`].
fn facets(conn: &Connection, dataset: &str, column: &str) -> Result<Vec<Facet>, AppError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT trim({column}) AS value, count(*) AS n
         FROM {dataset}
         WHERE trim({column}) <> '' GROUP BY value ORDER BY n DESC, value"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Facet {
                value: row.get(0)?,
                count: row.get::<_, i64>(1)?.unsigned_abs(),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// This function returns each distinct collector `agent_name` with a record
/// count, restricted to agents whose `agent_role` is `collector` — the same
/// role [`crate::translate::translate`] filters on. `collector_agents` is a
/// JSON array column, so this is [`facets`] plus one `json_each` unnest; the
/// raw dump carries no separate `;`-joined rollup of it the way ranks do.
fn collector_facets(conn: &Connection, dataset: &str) -> Result<Vec<Facet>, AppError> {
    // The output alias must not be `value` — `json_each` already names its own
    // column `value`, and a `GROUP BY value` under that collision binds to the
    // raw per-row JSON object instead of the extracted name, which silently
    // stops same-named collectors from merging.
    let mut stmt = conn.prepare(&format!(
        "SELECT trim(je.value ->> 'agent_name') AS name, count(*) AS n
         FROM {dataset}, json_each(collector_agents) AS je
         WHERE trim(collector_agents) <> ''
           AND lower(trim(je.value ->> 'agent_role')) = 'collector'
           AND trim(je.value ->> 'agent_name') <> ''
         GROUP BY name
         ORDER BY n DESC, name"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Facet {
                value: row.get(0)?,
                count: row.get::<_, i64>(1)?.unsigned_abs(),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// This function lists the dump's column names, in order. `LIMIT 0` reads
/// only the Parquet footers. The function decodes no row. The list includes
/// `guid_prefix`, which lives in the directory name, not in the files.
fn columns(conn: &Connection, dataset: &str) -> Result<Vec<String>, AppError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT column_name FROM (DESCRIBE SELECT * FROM {dataset} LIMIT 0)"
    ))?;
    let rows = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// This function returns each distinct `(rank, name)` pair with a record
/// count. The list order is count-descending.
///
/// This function scans each rank column once. The dump joins several names
/// with `;` when a record carries several determinations. This function
/// splits each column before it counts the names. The whole table stays in
/// memory (about 340,000 rows). `/api/taxa` filters this table in memory.
///
/// # Errors
///
/// The function returns an error when it cannot read the Parquet file.
pub fn build_taxa(conn: &Connection, dataset: &str) -> Result<Vec<Taxon>, AppError> {
    let ranks = all_ranks();
    let branches: Vec<String> = ranks
        .iter()
        .enumerate()
        .map(|(i, (id, _, column))| {
            // The parent chain follows the taxonomic ranks. scientific_name
            // is not part of this chain.
            let parent = match ranks.get(i.wrapping_sub(1)).filter(|_| i > 1) {
                Some((.., column)) => {
                    format!("any_value(nullif(trim(split_part({column}, ';', 1)), ''))")
                }
                None => "NULL::VARCHAR".to_string(),
            };
            format!(
                "SELECT '{id}' AS rank, trim(x) AS name, count(*) AS n, {parent} AS parent
                 FROM {dataset}, unnest(string_split({column}, ';')) AS u(x)
                 WHERE trim(x) <> '' GROUP BY name"
            )
        })
        .collect();

    let mut stmt = conn.prepare(&format!(
        "{} ORDER BY n DESC, name",
        branches.join(" UNION ALL ")
    ))?;
    let rows = stmt
        .query_map([], |row| {
            let rank: String = row.get(0)?;
            let i = ranks.iter().position(|(id, ..)| *id == rank).unwrap_or(0);
            // A parent rank with no parent name does not help a client tell
            // taxa apart, so the function keeps both fields or neither.
            let parent_name: Option<String> = row.get(3)?;
            Ok(Taxon {
                rank: ranks.get(i).map_or("", |r| r.0),
                name: row.get(1)?,
                record_count: row.get::<_, i64>(2)?.unsigned_abs(),
                parent_rank: parent_name
                    .as_ref()
                    .and_then(|_| ranks.get(i.wrapping_sub(1)).filter(|_| i > 0))
                    .map(|r| r.0),
                parent_name,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

impl Schema {
    /// # Errors
    ///
    /// The function returns an error when a code table is missing, when it
    /// cannot parse a code table, or when a Parquet aggregation fails. This
    /// function never returns a partial schema.
    #[tracing::instrument(skip(conn))]
    pub fn build(
        conn: &Connection,
        dataset: &str,
        code_tables: &Path,
        snapshot_date: &str,
        taxa: &[Taxon],
    ) -> Result<Self, AppError> {
        let types = load_table(code_tables, "ctattribute_type")?;
        // D35: a type that Arctos marks non-public never reaches the client.
        // The ETL also drops these types. This check is the second gate.
        let (public, nonpublic): (Vec<_>, Vec<_>) = types.into_iter().partition(is_public);

        let mut attribute_types: Vec<AttributeType> = public
            .iter()
            .map(|t| AttributeType {
                id: str_field(t, "attribute_type"),
                label: str_field(t, "attribute_type"),
                description: str_field(t, "description"),
                vocabulary: Some(str_field(t, "value_code_table")).filter(|s| !s.is_empty()),
                units_table: Some(str_field(t, "unit_code_table")).filter(|s| !s.is_empty()),
            })
            .collect();
        attribute_types.sort_by_key(|t| t.id.to_lowercase());

        let mut relations: Vec<Relation> = load_table(code_tables, "ctid_references")?
            .iter()
            .map(|r| Relation {
                value: str_field(r, "id_references"),
                description: str_field(r, "description"),
            })
            .collect();
        relations.sort_by_key(|r| r.value.to_lowercase());

        let collections: Vec<String> = load_table(code_tables, "ctcollection_cde")?
            .iter()
            .map(|r| str_field(r, "collection_cde"))
            .collect();
        let guid_prefixes = facets(conn, dataset, "guid_prefix")?
            .into_iter()
            .map(|f| {
                let (institution, collection_cde) = split_prefix(&f.value, &collections);
                GuidPrefix {
                    value: f.value,
                    count: f.count,
                    institution,
                    collection_cde,
                }
            })
            .collect();

        // A rank with no values in the snapshot is not offered.
        // scientific_name is always offered.
        let ranks = all_ranks()
            .iter()
            .filter(|(id, ..)| *id == "scientific_name" || taxa.iter().any(|t| t.rank == *id))
            .map(|(id, label, field)| Rank {
                id,
                label,
                field,
            })
            .collect();

        Ok(Self {
            snapshot_date: snapshot_date.to_string(),
            columns: columns(conn, dataset)?,
            ranks,
            vocabularies: build_vocabularies(code_tables, &public)?,
            attribute_types,
            countries: facets(conn, dataset, "country")?,
            states: facets(conn, dataset, "state_prov")?,
            collectors: collector_facets(conn, dataset)?,
            relations,
            guid_prefixes,
            sorts: vec![
                Sort {
                    id: "guid_asc",
                    label: "Catalog number",
                },
                Sort {
                    id: "date_desc",
                    label: "Newest collection date",
                },
                Sort {
                    id: "date_asc",
                    label: "Oldest collection date",
                },
            ],
            limits: Limits {
                page_size: PER_PAGE,
                max_per_page: 200,
                max_result_window: 10_000,
                max_export_rows: MAX_EXPORT_ROWS,
            },
            nonpublic_types_dropped: nonpublic
                .iter()
                .map(|t| str_field(t, "attribute_type"))
                .collect(),
        })
    }
}

/// This function matches a prefix against the whole name. It filters by
/// rank. It returns matches in count-descending order.
///
/// The input list arrives already sorted, so the filter step keeps that
/// order. A query under two characters matches nothing. A shorter query
/// would return only the head of the list, which is not a useful suggestion.
#[must_use]
pub fn matching_taxa<'a>(
    taxa: &'a [Taxon],
    rank: Option<&str>,
    q: &str,
    limit: usize,
) -> Vec<&'a Taxon> {
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Vec::new();
    }
    taxa.iter()
        .filter(|t| rank.is_none_or(|r| t.rank == r) && t.name.to_lowercase().starts_with(&q))
        .take(limit)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(v: &Value) -> Vec<Map<String, Value>> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_object().unwrap().clone())
            .collect()
    }

    #[test]
    fn a_row_is_public_unless_arctos_says_otherwise() {
        assert!(is_public(&Map::new()));
        assert!(is_public(rows(&json!([{ "public": 1 }])).first().unwrap()));
        assert!(!is_public(rows(&json!([{ "public": 0 }])).first().unwrap()));
        // The dump holds a quoted flag in some places. "0" is not a true value.
        assert!(!is_public(rows(&json!([{ "public": "0" }])).first().unwrap()));
    }

    #[test]
    fn the_value_column_is_whatever_the_table_calls_it() {
        let table = rows(&json!([{
            "examined_detected": "ectoparasite: flea",
            "description": "...",
            "documentation_url": "",
        }]));
        assert_eq!(value_key(&table).unwrap(), "examined_detected");
    }

    #[test]
    fn vocabulary_urls_survive_arctos_returning_an_array() {
        let row = rows(&json!([{ "documentation_url": ["https://handbook", "second"] }]));
        assert_eq!(str_field(&row[0], "documentation_url"), "https://handbook");
        assert_eq!(str_field(&row[0], "issue_url"), "");
    }

    #[test]
    fn collector_facets_unnest_the_json_column_and_keep_only_the_collector_role() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE t (collector_agents VARCHAR);
               INSERT INTO t VALUES
                 ('[{"agent_name": "Amber L. Hobbes", "agent_role": "collector"},
                    {"agent_name": "Aren A. Eddingsaas", "agent_role": "preparator"}]'),
                 ('[{"agent_name": "Amber L. Hobbes", "agent_role": "COLLECTOR"}]'),
                 (''),
                 (NULL);"#,
        )
        .unwrap();

        let facets = collector_facets(&conn, "t").unwrap();
        // The preparator is dropped. The role match is case-insensitive, so
        // both Hobbes rows roll into one count. An empty or null column does
        // not crash the `json_each` unnest.
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].value, "Amber L. Hobbes");
        assert_eq!(facets[0].count, 2);
    }

    #[test]
    fn a_collection_code_expands_to_its_label() {
        let collections = vec!["Mammalogy".to_string(), "Parasite".to_string()];
        assert_eq!(
            split_prefix("MSB:Mamm", &collections),
            ("MSB".to_string(), "Mammalogy".to_string())
        );
        // An unresolvable label is "", not null. A prefix without a colon does
        // not crash the function.
        assert_eq!(
            split_prefix("MSB:Nope", &collections),
            ("MSB".to_string(), String::new())
        );
        assert_eq!(split_prefix("MSB", &collections), (String::new(), String::new()));
    }

    fn taxon(rank: &'static str, name: &str, n: u64) -> Taxon {
        Taxon {
            rank,
            name: name.to_string(),
            record_count: n,
            parent_rank: None,
            parent_name: None,
        }
    }

    #[test]
    fn taxa_match_on_prefix_within_a_rank_and_keep_count_order() {
        let taxa = vec![
            taxon("genus", "Sorex", 82_491),
            taxon("genus", "Sorella", 3),
            taxon("species", "sonomae", 900),
            taxon("genus", "Peromyscus", 228_756),
        ];

        let hits = matching_taxa(&taxa, Some("genus"), "sor", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "Sorex");
        assert_eq!(hits[1].name, "Sorella");

        // The match is case-insensitive. The match is rank-filtered. The
        // match list is capped.
        assert_eq!(matching_taxa(&taxa, Some("genus"), "SOR", 1).len(), 1);
        assert_eq!(matching_taxa(&taxa, None, "so", 10).len(), 3);

        // A one-character query returns no matches.
        assert!(matching_taxa(&taxa, Some("genus"), "s", 10).is_empty());
        // A substring is not a prefix.
        assert!(matching_taxa(&taxa, Some("genus"), "orex", 10).is_empty());
    }
}
