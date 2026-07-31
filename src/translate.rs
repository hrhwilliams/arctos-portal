use serde_json::{Value, json};

use crate::search::SearchForm;

const RANK_FIELDS: &[(&str, &str)] = &[
    ("phylum", "phylum"),
    ("class", "phylclass"),
    ("order", "phylorder"),
    ("family", "family"),
    ("subfamily", "subfamily"),
    ("genus", "genus"),
    ("species", "species"),
];

const DETECTION_FIELDS: &[(&str, &str)] = &[
    ("detected", "detected"),
    ("not detected", "not_detected"),
    ("examined for", "examined_for"),
    ("not examined for", "not_examined_for"),
];

const SOURCE: &[&str] = &[
    "guid",
    "guid_prefix",
    "scientific_name",
    "family",
    "genus",
    "species",
    "country",
    "state_prov",
    "events",
    "event_date_min",
    "event_date_max",
    "use_license_url",
];

const PER_PAGE: usize = 100;
const TRACK_TOTAL_HITS: usize = 10_000;

struct Attr {
    atype: String,
    values: Vec<String>,
    negated: bool,
}

/// Split on the FIRST `|` only. Ranks and attribute types are controlled and
/// cannot contain one; free-text attribute values routinely do (`reproductive
/// data` uses `|` as its own internal separator).
fn split_once_pipe(s: &str) -> Option<(&str, &str)> {
    s.split_once('|').map(|(a, b)| (a.trim(), b.trim()))
}

fn decode_taxon(raw: &str) -> Option<(&str, &str)> {
    let (rank, name) = split_once_pipe(raw)?;
    (!name.is_empty()).then_some((rank, name))
}

fn decode_attr(raw: &str) -> Option<Attr> {
    let negated = raw.starts_with('!');
    let s = if negated { &raw[1..] } else { raw };
    // no `|` means no value: "carries this attribute type at all"
    let (atype, joined) = split_once_pipe(s).unwrap_or((s.trim(), ""));
    if atype.is_empty() {
        return None;
    }
    // Several values OR within the row. `;` is safe as the separator: controlled
    // values never contain one, and free-text types are single-valued in the form.
    let values = joined
        .split(';')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect();
    Some(Attr {
        atype: atype.to_string(),
        values,
        negated,
    })
}

fn any_of(mut clauses: Vec<Value>) -> Value {
    if clauses.len() == 1 {
        clauses.remove(0)
    } else {
        json!({ "bool": { "should": clauses, "minimum_should_match": 1 } })
    }
}

/// One attribute row -> one clause.
///
/// Values are optional and exact. With none the row asks "carries this attribute
/// type at all"; with several they OR. There is no parent/child expansion — a
/// search for `ectoparasite` matches `ectoparasite`, and a user who wants the
/// children selects them.
fn attribute_clause(a: &Attr) -> Value {
    let flat = DETECTION_FIELDS
        .iter()
        .find(|(id, _)| *id == a.atype)
        .map(|(_, field)| *field);

    // Fast path: a detection-type row needs no nested query at all. The flat
    // arrays are built from the same parse as the nested docs, so the two cannot
    // disagree. With no value, presence of the array is the test.
    if let Some(field) = flat {
        return if a.values.is_empty() {
            json!({ "exists": { "field": field } })
        } else {
            json!({ "terms": { field: a.values } })
        };
    }

    let mut filter = vec![json!({ "term": { "attributedetail.attribute_type": a.atype } })];
    if !a.values.is_empty() {
        filter.push(json!({ "terms": { "attributedetail.attribute_value": a.values } }));
    }
    json!({ "nested": { "path": "attributedetail", "query": { "bool": { "filter": filter } } } })
}

fn present(values: &Option<Vec<String>>) -> Vec<String> {
    values
        .iter()
        .flatten()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect()
}

fn one(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("").trim()
}

pub fn translate(form: &SearchForm) -> Value {
    let mut filter: Vec<Value> = Vec::new();
    let mut must_not: Vec<Value> = Vec::new();

    // ---- block 1: taxon, OR ------------------------------------------------
    let taxon_clauses: Vec<Value> = present(&form.taxon)
        .iter()
        .filter_map(|raw| decode_taxon(raw))
        .filter_map(|(rank, name)| {
            // Not a rank column: analysed text with no `.split` subfield, so a
            // binomial matches as a phrase.
            if rank == "scientific_name" {
                return Some(json!({ "match_phrase": { "scientific_name": name } }));
            }
            let field = RANK_FIELDS.iter().find(|(id, _)| *id == rank)?.1;
            Some(json!({ "match": { format!("{field}.split"): name } }))
        })
        .collect();
    if !taxon_clauses.is_empty() {
        filter.push(any_of(taxon_clauses));
    }

    // ---- block 2: attributes, AND or OR ------------------------------------
    let rows: Vec<Attr> = present(&form.attr)
        .iter()
        .filter_map(|raw| decode_attr(raw))
        .collect();

    if form.attr_op.as_deref() == Some("or") {
        // A negated row inside an OR reads as "... or does not carry this", so the
        // negation is local to the clause rather than hoisted to the top level.
        let clauses: Vec<Value> = rows
            .iter()
            .map(|r| {
                if r.negated {
                    json!({ "bool": { "must_not": [attribute_clause(r)] } })
                } else {
                    attribute_clause(r)
                }
            })
            .collect();
        if !clauses.is_empty() {
            filter.push(any_of(clauses));
        }
    } else {
        // AND: each row is a separate clause, because each describes a different
        // attribute record on the specimen.
        for row in &rows {
            let clause = attribute_clause(row);
            if row.negated {
                must_not.push(clause);
            } else {
                filter.push(clause);
            }
        }
    }

    // Collection, not identity: AND-ed against the taxon block rather than OR-ed
    // into it, or a prefix would return the whole collection regardless of taxon.
    let prefixes = present(&form.prefix);
    if !prefixes.is_empty() {
        filter.push(json!({ "terms": { "guid_prefix": prefixes } }));
    }

    // ---- block 3: scope ----------------------------------------------------
    // Record-level. These are NOT correlated with the event date: the source
    // carries no per-event political geography.
    for (field, values) in [("country", &form.country), ("state_prov", &form.state)] {
        let v = present(values);
        if !v.is_empty() {
            filter.push(json!({ "terms": { field: v } }));
        }
    }

    let collector = one(&form.collector);
    if !collector.is_empty() {
        filter.push(json!({ "term": { "collector_ids": collector } }));
    }

    // Per event, and in ONE clause so they describe the same visit. Two sibling
    // clauses would match a specimen collected at the place on one visit and in
    // the date range on another.
    let mut event_filter: Vec<Value> = Vec::new();
    let locality = one(&form.locality);
    if !locality.is_empty() {
        event_filter.push(json!({ "term": { "events.locality_search_terms": locality } }));
    }
    let (from, to) = (one(&form.from), one(&form.to));
    if !from.is_empty() || !to.is_empty() {
        let mut range = json!({});
        if !from.is_empty() {
            range["gte"] = json!(from);
        }
        if !to.is_empty() {
            range["lte"] = json!(to);
        }
        event_filter.push(json!({ "range": { "events.began_date": range } }));
    }
    if !event_filter.is_empty() {
        filter.push(json!({
            "nested": {
                "path": "events",
                "query": { "bool": { "filter": event_filter } }
            }
        }));
    }

    let mut bool_query = json!({ "filter": filter });
    if !must_not.is_empty() {
        bool_query["must_not"] = json!(must_not);
    }

    // page 0 is not a thing; a hand-edited link could still carry it
    let page = form.page.unwrap_or(1).max(1);

    json!({
        "track_total_hits": TRACK_TOTAL_HITS,
        "from": (page - 1) * PER_PAGE,
        "size": PER_PAGE,
        "sort": [{ "guid": "asc" }],
        "_source": SOURCE,
        "query": { "bool": bool_query }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> SearchForm {
        SearchForm {
            taxon: None,
            attr: None,
            prefix: None,
            country: None,
            state: None,
            from: None,
            to: None,
            attr_op: None,
            locality: None,
            collector: None,
            page: None,
        }
    }

    #[test]
    fn detection_row_takes_the_flat_fast_path() {
        let f = SearchForm {
            attr: Some(vec![
                "detected|ectoparasite: flea; ectoparasite: louse".into(),
            ]),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(clauses.len(), 1);
        // several values OR into ONE terms clause, exact, no nested query
        assert_eq!(clauses[0]["terms"]["detected"].as_array().unwrap().len(), 2);
        assert!(!b.to_string().contains("nested"));
        assert!(!b.to_string().contains("hierarchy"));
    }

    #[test]
    fn a_valueless_row_asks_only_whether_the_type_is_recorded() {
        let f = SearchForm {
            attr: Some(vec!["examined for".into()]),
            ..form()
        };
        let b = translate(&f);
        assert_eq!(
            b["query"]["bool"]["filter"][0]["exists"]["field"],
            "examined_for"
        );
    }

    #[test]
    fn a_non_detection_row_is_one_nested_clause() {
        let f = SearchForm {
            attr: Some(vec!["sex|male; female".into()]),
            ..form()
        };
        let b = translate(&f);
        let inner = &b["query"]["bool"]["filter"][0]["nested"]["query"]["bool"]["filter"];
        assert_eq!(inner[0]["term"]["attributedetail.attribute_type"], "sex");
        assert_eq!(
            inner[1]["terms"]["attributedetail.attribute_value"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_negated_row_becomes_must_not() {
        let f = SearchForm {
            attr: Some(vec!["!detected|ectoparasite".into()]),
            ..form()
        };
        let b = translate(&f);
        assert!(b["query"]["bool"]["filter"].as_array().unwrap().is_empty());
        assert_eq!(
            b["query"]["bool"]["must_not"][0]["terms"]["detected"][0],
            "ectoparasite"
        );
    }

    #[test]
    fn taxon_rows_or_and_hit_the_split_subfield() {
        let f = SearchForm {
            taxon: Some(vec!["genus|Sorex".into(), "species|Myodes gapperi".into()]),
            ..form()
        };
        let b = translate(&f);
        let should = &b["query"]["bool"]["filter"][0]["bool"]["should"];
        assert_eq!(
            b["query"]["bool"]["filter"][0]["bool"]["minimum_should_match"],
            1
        );
        assert_eq!(should[0]["match"]["genus.split"], "Sorex");
        assert_eq!(should[1]["match"]["species.split"], "Myodes gapperi");
    }

    #[test]
    fn scientific_name_matches_as_a_phrase_not_a_split_field() {
        let f = SearchForm {
            taxon: Some(vec!["scientific_name|Sorex cinereus".into()]),
            ..form()
        };
        let b = translate(&f);
        assert_eq!(
            b["query"]["bool"]["filter"][0]["match_phrase"]["scientific_name"],
            "Sorex cinereus"
        );
    }

    #[test]
    fn scope_and_paging() {
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            country: Some(vec!["United States".into(), "Mexico".into()]),
            state: Some(vec!["Alaska".into()]),
            collector: Some("agent/1234".into()),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        // one value or several, always `terms` — one clause shape to reason about
        assert_eq!(clauses[0]["terms"]["guid_prefix"][0], "MSB:Mamm");
        assert_eq!(clauses[1]["terms"]["country"].as_array().unwrap().len(), 2);
        assert_eq!(clauses[2]["terms"]["state_prov"][0], "Alaska");
        // the id rollup, not the name, so spelling variants still match
        assert_eq!(clauses[3]["term"]["collector_ids"], "agent/1234");

        assert_eq!(b["from"], 0);
        assert_eq!(
            translate(&SearchForm {
                page: Some(3),
                ..form()
            })["from"],
            100
        );
        // page 0 would underflow `usize`, not merely paginate oddly
        assert_eq!(
            translate(&SearchForm {
                page: Some(0),
                ..form()
            })["from"],
            0
        );
    }

    #[test]
    fn attr_op_or_puts_the_rows_in_one_should_block() {
        let f = SearchForm {
            attr: Some(vec!["detected|ectoparasite".into(), "sex|male".into()]),
            attr_op: Some("or".into()),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(clauses.len(), 1);
        let should = clauses[0]["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 2);
        assert_eq!(clauses[0]["bool"]["minimum_should_match"], 1);
    }

    #[test]
    fn a_negated_row_inside_an_or_stays_local_to_its_clause() {
        let f = SearchForm {
            attr: Some(vec!["!detected|ectoparasite".into(), "sex|male".into()]),
            attr_op: Some("or".into()),
            ..form()
        };
        let b = translate(&f);
        // not hoisted to a top-level must_not, which would AND it against the rest
        assert!(b["query"]["bool"]["must_not"].is_null());
        let should = &b["query"]["bool"]["filter"][0]["bool"]["should"];
        assert_eq!(
            should[0]["bool"]["must_not"][0]["terms"]["detected"][0],
            "ectoparasite"
        );
    }

    #[test]
    fn locality_and_date_share_one_nested_events_clause() {
        let f = SearchForm {
            locality: Some("Sandia Mountains".into()),
            from: Some("2024-01-01".into()),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(
            clauses.len(),
            1,
            "two sibling clauses would match two different visits"
        );
        let inner = clauses[0]["nested"]["query"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(
            inner[0]["term"]["events.locality_search_terms"],
            "Sandia Mountains"
        );
        assert_eq!(inner[1]["range"]["events.began_date"]["gte"], "2024-01-01");
    }

    #[test]
    fn an_empty_form_produces_a_match_all_that_the_caller_must_refuse() {
        let b = translate(&form());
        assert!(b["query"]["bool"]["filter"].as_array().unwrap().is_empty());
    }
}
