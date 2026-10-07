//! `GET /api/summary`: statistics over every record a search matches, and over
//! the related records it reaches, in one response.
//!
//! The index answers all of it from one request with `size: 0`: `cardinality`
//! for the distinct counts, `min`/`max` on the event dates, `terms` for the
//! facets. This module holds the aggregation block and the reader that turns
//! the response into a [`Stats`]. `state.rs` runs the requests.

use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use crate::schema::Facet;

/// Each facet lists at most this many values.
const FACET_SIZE: usize = 10;

/// The whole summary.
#[derive(Serialize, Debug, ToSchema)]
pub struct SearchSummary {
    /// Statistics over the set `/api/search` matches.
    pub records: Stats,
    /// One entry per relationship, in name order — the same split the tabs of
    /// `/api/relations` show. Every relationship the query named is here, with
    /// zeros when the matched records carry none of it, plus any relationship a
    /// Related-taxon row reached without naming. `null` when the query names no
    /// relationship at all, and when the related set is past the `terms` limit
    /// that `/api/relations` itself refuses.
    pub related: Option<Vec<RelationStats>>,
    /// One sentence restating the query. `null` for an empty form.
    pub description: Option<String>,
}

/// One relationship, and the records reached by it.
#[derive(Serialize, Debug, ToSchema)]
pub struct RelationStats {
    /// The relationship as the records carry it, for example `host of
    /// parasite`.
    pub relation: String,
    /// The distinct (matched record, related record) links of this
    /// relationship, before the related records collapse to distinct guids.
    /// It is at least `matched`, and larger wherever one related record was
    /// reached from several matched ones.
    pub pairings: u64,
    /// `matched`, `distinct`, `years` and `facets`, the same shape the records
    /// block carries.
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Stats {
    /// The exact record count, the same number the page reports as
    /// `summary.matched`.
    pub matched: u64,
    pub distinct: Distinct,
    /// Min and max collecting year. `null` when no record carries a date;
    /// undated records do not pull `min` down.
    pub years: Option<Years>,
    pub facets: Facets,
}

/// Distinct counts. Approximate above a few thousand values, which is what
/// `cardinality` gives; the numbers are for orientation, not citation.
#[derive(Serialize, Debug, ToSchema)]
pub struct Distinct {
    /// Distinct lowest identified taxa, by `scientific_name`.
    pub taxa: u64,
    /// Distinct localities, by the event's locality id.
    pub localities: u64,
    /// Distinct collector names, the field `collector=` filters on.
    pub collectors: u64,
}

impl Stats {
    /// The statistics of a set with no records in it. A relationship the
    /// search named and reached nothing by reads this way, rather than going
    /// missing from the list.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            matched: 0,
            distinct: Distinct {
                taxa: 0,
                localities: 0,
                collectors: 0,
            },
            years: None,
            facets: Facets {
                taxa: Vec::new(),
                countries: Vec::new(),
                states: Vec::new(),
                collections: Vec::new(),
            },
        }
    }
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Years {
    pub min: i32,
    pub max: i32,
}

/// The top values by record count, descending. A record holding several
/// values counts once under each. An empty list means none recorded.
#[derive(Serialize, Debug, ToSchema)]
pub struct Facets {
    pub taxa: Vec<Facet>,
    pub countries: Vec<Facet>,
    pub states: Vec<Facet>,
    pub collections: Vec<Facet>,
}

/// The aggregation block. Both the records query and the related query carry
/// this same block, so the two [`Stats`] cannot differ in shape.
///
/// `country` and `state_prov` are indexed through the `lc` normalizer, so
/// their buckets come back lowercased; [`stats`] restores the display case
/// from the schema's own facet lists.
#[must_use]
pub fn aggs() -> Value {
    json!({
        "taxa": { "cardinality": { "field": "scientific_name.keyword" } },
        "localities": {
            "nested": { "path": "events" },
            "aggs": { "n": { "cardinality": { "field": "events.locality_id" } } }
        },
        "collectors": {
            "nested": { "path": "agents" },
            "aggs": { "role": {
                "filter": { "term": { "agents.agent_role": "collector" } },
                "aggs": { "n": { "cardinality": { "field": "agents.agent_name.keyword" } } }
            } }
        },
        "year_min": { "min": { "field": "event_date_min", "format": "yyyy" } },
        "year_max": { "max": { "field": "event_date_max", "format": "yyyy" } },
        "facet_taxa": { "terms": { "field": "scientific_name.keyword", "size": FACET_SIZE } },
        "facet_countries": { "terms": { "field": "country", "size": FACET_SIZE } },
        "facet_states": { "terms": { "field": "state_prov", "size": FACET_SIZE } },
        "facet_collections": { "terms": { "field": "guid_prefix", "size": FACET_SIZE } }
    })
}

/// This function reads one `size: 0` response carrying [`aggs`] into a
/// [`Stats`]. `countries` and `states` are the schema's facet lists, used to
/// put the display case back on the lowercased buckets.
///
/// A missing or malformed aggregation reads as zero or empty rather than an
/// error: the numbers are for orientation, and the page beside them still works.
#[must_use]
pub fn stats(response: &Value, countries: &[Facet], states: &[Facet]) -> Stats {
    static NULL: Value = Value::Null;
    /// The value at a path, or null. Never panics on a shape the index did
    /// not send.
    fn at<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
        path.iter()
            .fold(value, |v, key| v.get(*key).unwrap_or(&NULL))
    }

    let a = at(response, &["aggregations"]);
    let count = |path: &[&str]| at(a, path).as_u64().unwrap_or(0);
    // The year comes back formatted, so no float has to be turned into an int.
    let year = |name: &str| {
        at(a, &[name, "value_as_string"])
            .as_str()
            .and_then(|s| s.parse::<i32>().ok())
    };
    let facet = |name: &str, known: &[Facet]| -> Vec<Facet> {
        at(a, &[name, "buckets"])
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|bucket| {
                let key = at(bucket, &["key"]).as_str()?;
                let value = known
                    .iter()
                    .find(|f| f.value.eq_ignore_ascii_case(key))
                    .map_or(key, |f| f.value.as_str());
                Some(Facet {
                    value: value.to_owned(),
                    count: at(bucket, &["doc_count"]).as_u64().unwrap_or(0),
                })
            })
            .collect()
    };

    Stats {
        matched: at(response, &["hits", "total", "value"])
            .as_u64()
            .unwrap_or(0),
        distinct: Distinct {
            taxa: count(&["taxa", "value"]),
            localities: count(&["localities", "n", "value"]),
            collectors: count(&["collectors", "role", "n", "value"]),
        },
        years: match (year("year_min"), year("year_max")) {
            (Some(min), Some(max)) => Some(Years { min, max }),
            _ => None,
        },
        facets: Facets {
            taxa: facet("facet_taxa", &[]),
            countries: facet("facet_countries", countries),
            states: facet("facet_states", states),
            collections: facet("facet_collections", &[]),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn known(values: &[&str]) -> Vec<Facet> {
        values
            .iter()
            .map(|v| Facet {
                value: (*v).to_owned(),
                count: 0,
            })
            .collect()
    }

    #[test]
    fn a_stats_response_reads_into_counts_years_and_facets() {
        let response = json!({
            "hits": { "total": { "value": 227, "relation": "eq" }, "hits": [] },
            "aggregations": {
                "taxa": { "value": 4 },
                "localities": { "doc_count": 300, "n": { "value": 61 } },
                "collectors": { "doc_count": 400, "role": { "doc_count": 250, "n": { "value": 38 } } },
                // The epoch-millis `value` is not read; the formatted string is.
                "year_min": { "value": -1.0, "value_as_string": "1958" },
                "year_max": { "value": 1.0, "value_as_string": "2019" },
                "facet_taxa": { "buckets": [
                    { "key": "Sorex cinereus", "doc_count": 140 },
                    { "key": "Sorex monticolus", "doc_count": 52 }
                ] },
                // Lowercased by the `lc` normalizer; the schema list restores the case.
                "facet_countries": { "buckets": [{ "key": "united states", "doc_count": 227 }] },
                "facet_states": { "buckets": [
                    { "key": "new mexico", "doc_count": 180 },
                    { "key": "nowhere", "doc_count": 1 }
                ] },
                "facet_collections": { "buckets": [{ "key": "MSB:Mamm", "doc_count": 186 }] }
            }
        });
        let s = stats(
            &response,
            &known(&["United States", "Mexico"]),
            &known(&["New Mexico", "Colorado"]),
        );

        assert_eq!(s.matched, 227);
        assert_eq!((s.distinct.taxa, s.distinct.localities, s.distinct.collectors), (4, 61, 38));
        let years = s.years.unwrap();
        assert_eq!((years.min, years.max), (1958, 2019));
        assert_eq!(s.facets.taxa[0].value, "Sorex cinereus");
        assert_eq!(s.facets.taxa[1].count, 52);
        assert_eq!(s.facets.countries[0].value, "United States");
        assert_eq!(s.facets.states[0].value, "New Mexico");
        // A bucket the schema does not list keeps the index's own text.
        assert_eq!(s.facets.states[1].value, "nowhere");
        assert_eq!(s.facets.collections[0].value, "MSB:Mamm");
    }

    #[test]
    fn a_set_with_no_dates_has_no_years_and_missing_aggregations_read_as_empty() {
        let response = json!({
            "hits": { "total": { "value": 3 }, "hits": [] },
            "aggregations": {
                "year_min": { "value": null },
                "year_max": { "value": null },
                "facet_taxa": { "buckets": [] }
            }
        });
        let s = stats(&response, &[], &[]);
        assert_eq!(s.matched, 3);
        assert!(s.years.is_none());
        assert_eq!(s.distinct.taxa, 0);
        assert!(s.facets.taxa.is_empty());
        assert!(s.facets.countries.is_empty());
    }

    #[test]
    fn a_relation_entry_carries_its_statistics_beside_its_name_not_under_them() {
        let entry = RelationStats {
            relation: "host of parasite".to_owned(),
            pairings: 229,
            stats: stats(&json!({ "hits": { "total": { "value": 188 } } }), &[], &[]),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["relation"], "host of parasite");
        assert_eq!(json["pairings"], 229);
        // Flattened: `matched` sits beside `relation`, not inside a `stats`.
        assert_eq!(json["matched"], 188);
        assert_eq!(json["distinct"]["taxa"], 0);
        assert!(json.get("stats").is_none());
    }

    #[test]
    fn the_aggregation_block_names_every_field_the_reader_expects() {
        let a = aggs();
        for name in [
            "taxa",
            "localities",
            "collectors",
            "year_min",
            "year_max",
            "facet_taxa",
            "facet_countries",
            "facet_states",
            "facet_collections",
        ] {
            assert!(a.get(name).is_some(), "{name}");
        }
        assert_eq!(a["facet_taxa"]["terms"]["size"], FACET_SIZE);
        assert_eq!(a["year_min"]["min"]["format"], "yyyy");
    }
}
