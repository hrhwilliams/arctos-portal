//! `GET /api/schema` and `GET /api/taxa` — everything the form needs to render
//! itself, computed once at startup (spec 03).
//!
//! Two sources, and the difference matters. Code tables (`docs/data/code-tables`,
//! fetched from Arctos out of band) are **complete**: a controlled value with no
//! records still ships, because an option vanishing between snapshots silently
//! changes the meaning of a saved URL. Aggregations over the Parquet are
//! **observed**: they carry counts and only list what the data holds.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use duckdb::Connection;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{errors::AppError, translate::PER_PAGE};

/// `(id, label, parquet column)`, in the order the form renders them.
///
/// `scientific_name` is not a rank — it is a Latin binomial — but it shares the
/// (field, value) shape, rides in the same dropdown, and is listed first
/// because it is what users reach for.
pub const RANKS: &[(&str, &str, &str)] = &[
    ("scientific_name", "Scientific name", "scientific_name"),
    ("phylum", "Phylum", "phylum"),
    ("class", "Class", "phylclass"),
    ("order", "Order", "phylorder"),
    ("family", "Family", "family"),
    ("subfamily", "Subfamily", "subfamily"),
    ("genus", "Genus", "genus"),
    ("species", "Species", "species"),
];

/// Rows an export is capped at. Enforced in [`crate::state`] and served in
/// [`Limits`], so the client disabling the button and the service truncating
/// agree by construction.
pub const MAX_EXPORT_ROWS: usize = 100_000;

/// Keys that are metadata on a code-table row, never the controlled value.
/// Whatever is left is the value column, which every table names differently
/// (`examined_detected`, `sex_cde`, `caste`, …).
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

#[derive(Serialize, Debug)]
pub struct Schema {
    pub snapshot_date: String,
    pub ranks: Vec<Rank>,
    pub attribute_types: Vec<AttributeType>,
    pub vocabularies: BTreeMap<String, Vec<VocabValue>>,
    pub countries: Vec<Facet>,
    pub states: Vec<Facet>,
    pub relations: Vec<Relation>,
    pub guid_prefixes: Vec<GuidPrefix>,
    pub sorts: Vec<Sort>,
    pub limits: Limits,
    pub nonpublic_types_dropped: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct Rank {
    pub id: &'static str,
    pub label: &'static str,
    pub field: &'static str,
}

#[derive(Serialize, Debug)]
pub struct AttributeType {
    pub id: String,
    pub label: String,
    pub description: String,
    pub vocabulary: Option<String>,
    pub units_table: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct VocabValue {
    pub value: String,
    pub description: String,
    pub documentation_url: String,
}

#[derive(Serialize, Debug)]
pub struct Facet {
    pub value: String,
    pub count: u64,
}

#[derive(Serialize, Debug)]
pub struct Relation {
    pub value: String,
    pub description: String,
}

#[derive(Serialize, Debug)]
pub struct GuidPrefix {
    pub value: String,
    pub count: u64,
    pub institution: String,
    pub collection_cde: String,
}

#[derive(Serialize, Debug)]
pub struct Sort {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Serialize, Debug)]
pub struct Limits {
    /// What the pager divides by. The service fixes the page size and takes no
    /// `per_page`, so this — not `max_per_page` — is the number the client needs.
    pub page_size: usize,
    pub max_per_page: usize,
    pub max_result_window: usize,
    pub max_export_rows: usize,
}

#[derive(Serialize, Debug, Clone)]
pub struct Taxon {
    pub rank: &'static str,
    pub name: String,
    pub record_count: u64,
    pub parent_rank: Option<&'static str>,
    pub parent_name: Option<String>,
}

/// A code table as Arctos ships it: `{"data": [...]}` or a bare array.
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
        // Arctos returns an array for some url fields; the first element is the url
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

/// One entry per distinct `value_code_table` a public attribute type names.
/// A table that exists but is empty ships as `[]` rather than being skipped —
/// the client requires every non-null `vocabulary` to be a key here.
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
        // parents before children (`ectoparasite` before `ectoparasite: flea`),
        // which is what lets the form indent the hierarchy
        values.sort_by_key(|v| (v.value.matches(':').count(), v.value.to_lowercase()));
        vocab.insert(table, values);
    }
    Ok(vocab)
}

/// `MSB:Mamm` → `("MSB", "Mammalogy")`. `ctcollection_cde` lists only the full
/// labels, so the abbreviation is matched as a prefix of one.
///
// ponytail: prefix match, not a lookup — swap for a real code→label column if
// Arctos ever ships one, or if two collections ever share a prefix.
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

/// `SELECT value, count(*)` over one column of the dump, most-used first.
/// Observed values only — nothing enumerates a place name with no records.
///
/// `dataset` is the whole `FROM` clause, not a path: the dump is a partitioned
/// Parquet directory and reading it takes options (`crate::state`).
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

/// Distinct `(rank, name)` with record counts, count-descending.
///
/// One scan per rank column. Names are `;`-joined in the dump when a record
/// carries several determinations, so each column is split before counting.
/// The whole table lives in memory (~340k rows) and `/api/taxa` filters it
/// there: the alternative is a Parquet scan per keystroke.
///
/// # Errors
///
/// Returns an error if the Parquet cannot be read.
pub fn build_taxa(conn: &Connection, dataset: &str) -> Result<Vec<Taxon>, AppError> {
    let branches: Vec<String> = RANKS
        .iter()
        .enumerate()
        .map(|(i, (id, _, column))| {
            // the parent chain is taxonomic; scientific_name is outside it
            let parent = if i > 1 {
                format!("any_value(nullif(trim(split_part({}, ';', 1)), ''))", RANKS[i - 1].2)
            } else {
                "NULL::VARCHAR".to_string()
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
            let i = RANKS.iter().position(|(id, ..)| *id == rank).unwrap_or(0);
            // a parent rank with no name behind it disambiguates nothing, so
            // both go or neither does
            let parent_name: Option<String> = row.get(3)?;
            Ok(Taxon {
                rank: RANKS[i].0,
                name: row.get(1)?,
                record_count: row.get::<_, i64>(2)?.unsigned_abs(),
                parent_rank: parent_name
                    .as_ref()
                    .and_then(|_| (i > 0).then(|| RANKS[i - 1].0)),
                parent_name,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

impl Schema {
    /// # Errors
    ///
    /// Returns an error if a code table is missing or unparseable, or if the
    /// Parquet aggregations fail. There is no partial schema.
    #[tracing::instrument(skip(conn))]
    pub fn build(
        conn: &Connection,
        dataset: &str,
        code_tables: &Path,
        snapshot_date: &str,
        taxa: &[Taxon],
    ) -> Result<Self, AppError> {
        let types = load_table(code_tables, "ctattribute_type")?;
        // D35 — types Arctos marks non-public never reach the client at all.
        // The ETL drops them too; this is the second of two gates.
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

        // a rank the snapshot has no values for is not offered; scientific_name
        // always is
        let ranks = RANKS
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
            ranks,
            vocabularies: build_vocabularies(code_tables, &public)?,
            attribute_types,
            countries: facets(conn, dataset, "country")?,
            states: facets(conn, dataset, "state_prov")?,
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

/// Prefix match on the whole name, rank-filtered, count-descending.
///
/// The input list is already sorted, so filtering preserves the order. Under
/// two characters matches nothing: it would return the head of the list, which
/// is noise, not a suggestion.
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
        // the dump has quoted flags in places, and "0" is not truthy
        assert!(!is_public(rows(&json!([{ "public": "0" }])).first().unwrap()));
    }

    #[test]
    fn the_value_column_is_whatever_the_table_calls_it() {
        let table = rows(&json!([{
            "examined_detected": "ectoparasite: flea",
            "description": "…",
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
    fn a_collection_code_expands_to_its_label() {
        let collections = vec!["Mammalogy".to_string(), "Parasite".to_string()];
        assert_eq!(
            split_prefix("MSB:Mamm", &collections),
            ("MSB".to_string(), "Mammalogy".to_string())
        );
        // unresolvable is "", never null, and a prefix without a colon is not a crash
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

        // case-insensitive, rank-filtered, and capped
        assert_eq!(matching_taxa(&taxa, Some("genus"), "SOR", 1).len(), 1);
        assert_eq!(matching_taxa(&taxa, None, "so", 10).len(), 3);

        // a one-character query is the head of the list, not a suggestion
        assert!(matching_taxa(&taxa, Some("genus"), "s", 10).is_empty());
        // and a substring is not a prefix
        assert!(matching_taxa(&taxa, Some("genus"), "orex", 10).is_empty());
    }
}
