use serde_json::{Value, json};

use crate::search::{AttrOp, SearchForm};

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

/// This list also serves as the CSV column list.
pub(crate) const SOURCE: &[&str] = &[
    "guid",
    "scientific_name",
    "relations",
    "country",
    "state_prov",
    "events",
    "event_date_min",
    "event_date_max",
    "use_license_url",
];

/// The service sets the page size. The service does not accept a `per_page`
/// value from the client. `/api/schema` sends this number to the client.
pub(crate) const PER_PAGE: usize = 100;
const TRACK_TOTAL_HITS: usize = 10_000;

struct Attr {
    atype: String,
    values: Vec<String>,
    negated: bool,
}

/// This function splits the string at the FIRST `|` only. A rank name and an
/// attribute type name never contain a `|`. A free-text attribute value can
/// contain a `|`.
fn split_once_pipe(s: &str) -> Option<(&str, &str)> {
    s.split_once('|').map(|(a, b)| (a.trim(), b.trim()))
}

/// This function splits several values that OR within one row. A `;`
/// character is a safe separator. A controlled value never contains a `;`.
fn split_values(joined: &str) -> Vec<String> {
    joined
        .split(';')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect()
}

/// This function converts one taxon row into one clause.
///
/// The row has the form `rank|name` or `rank|name|relationship; relationship`.
/// A rank name, a taxon name, and a relationship term never contain a `|`.
/// The row splits into at most three parts.
fn taxon_clause(raw: &str) -> Option<Value> {
    let (rank, rest) = split_once_pipe(raw)?;
    // No second `|` means the row has no relation filter.
    let (name, joined) = split_once_pipe(rest).unwrap_or((rest, ""));
    if name.is_empty() {
        return None;
    }

    // scientific_name is not a rank column. It matches as a phrase.
    let taxon = if rank == "scientific_name" {
        json!({ "match_phrase": { "scientific_name": name } })
    } else {
        let field = RANK_FIELDS.iter().find(|(id, _)| *id == rank)?.1;
        json!({ "match": { format!("{field}.split"): name } })
    };

    let relationships = split_values(joined);
    if relationships.is_empty() {
        return Some(taxon);
    }
    // The relation kinds AND with this taxon. They narrow this taxon. They do
    // not form a separate match condition.
    Some(json!({ "bool": { "filter": [
        taxon,
        { "nested": {
            "path": "relations",
            "query": { "terms": { "relations.relationship": relationships } }
        } }
    ] } }))
}

fn decode_attr(raw: &str) -> Option<Attr> {
    let negated = raw.starts_with('!');
    let s = if negated { &raw[1..] } else { raw };
    // No `|` means the row asks only whether the record has this attribute type.
    let (atype, joined) = split_once_pipe(s).unwrap_or_else(|| (s.trim(), ""));
    if atype.is_empty() {
        return None;
    }
    let values = split_values(joined);
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

/// This function converts one attribute row into one clause.
///
/// A value is optional and exact. With no value, the row asks only whether
/// the record has this attribute type. With several values, the values OR.
/// The function does not expand a value to its child values. A search for
/// `ectoparasite` matches only `ectoparasite`. A user who wants the child
/// values must select them.
fn attribute_clause(a: &Attr) -> Value {
    let flat = DETECTION_FIELDS
        .iter()
        .find(|(id, _)| *id == a.atype)
        .map(|(_, field)| *field);

    // A detection-type row needs no nested query. With no value, the test
    // checks only whether the array field exists.
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

fn present(values: Option<&Vec<String>>) -> Vec<String> {
    values
        .into_iter()
        .flatten()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect()
}

fn one(value: Option<&String>) -> &str {
    value.map_or("", |v| v.trim())
}

#[must_use]
pub fn translate(form: &SearchForm) -> Value {
    // This function keeps the attribute clauses apart from the rest of the
    // filter. This split lets the summary count the records that the rest of
    // the form selects.
    let mut filter: Vec<Value> = Vec::new();
    let mut attr_filter: Vec<Value> = Vec::new();
    let mut must_not: Vec<Value> = Vec::new();

    // Block 1: taxon rows OR together.
    let taxon_clauses: Vec<Value> = present(form.taxon.as_ref())
        .iter()
        .filter_map(|raw| taxon_clause(raw))
        .collect();
    if !taxon_clauses.is_empty() {
        filter.push(any_of(taxon_clauses));
    }

    // Block 2: attribute rows AND or OR, per attr_op.
    let rows: Vec<Attr> = present(form.attr.as_ref())
        .iter()
        .filter_map(|raw| decode_attr(raw))
        .collect();

    if form.attr_op == AttrOp::Or {
        // A negated row inside an OR clause stays inside that clause. The
        // negation does not move to the top level.
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
            attr_filter.push(any_of(clauses));
        }
    } else {
        // With AND, each row is a separate clause. Each row describes a
        // different attribute record on the specimen.
        for row in &rows {
            let clause = attribute_clause(row);
            if row.negated {
                must_not.push(clause);
            } else {
                attr_filter.push(clause);
            }
        }
    }

    // A prefix clause ANDs against the taxon block. It does not OR into it.
    let prefixes = present(form.prefix.as_ref());
    if !prefixes.is_empty() {
        filter.push(json!({ "terms": { "guid_prefix": prefixes } }));
    }

    // Block 3: scope. These fields are record-level. They do not link to the
    // event date.
    for (field, values) in [
        ("country", form.country.as_ref()),
        ("state_prov", form.state.as_ref()),
    ] {
        let v = present(values);
        if !v.is_empty() {
            filter.push(json!({ "terms": { field: v } }));
        }
    }

    let collector = one(form.collector.as_ref());
    if !collector.is_empty() {
        filter.push(json!({ "term": { "collector_ids": collector } }));
    }

    // This block builds ONE clause for locality and date together. Both
    // conditions must apply to the same event.
    let mut event_filter: Vec<Value> = Vec::new();
    let locality = one(form.locality.as_ref());
    if !locality.is_empty() {
        event_filter.push(json!({ "term": { "events.locality_search_terms": locality } }));
    }
    let (from, to) = (one(form.from.as_ref()), one(form.to.as_ref()));
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

    // This filter list holds everything except the attribute rows. It is the
    // denominator of the summary.
    let context = json!({ "bool": { "filter": filter.clone() } });

    filter.extend(attr_filter);
    let mut bool_query = json!({ "filter": filter });
    if !must_not.is_empty() {
        bool_query["must_not"] = json!(must_not);
    }

    // Page 0 is not valid. A hand-edited link can still carry page 0.
    let page = form.page.unwrap_or(1).max(1);

    json!({
        "track_total_hits": TRACK_TOTAL_HITS,
        "from": (page - 1) * PER_PAGE,
        "size": PER_PAGE,
        "sort": [{ "guid": "asc" }],
        "_source": SOURCE,
        "aggs": summary_aggs(&context, &bool_query),
        "query": { "bool": bool_query }
    })
}

/// This function builds one page of guids for the same form as the search
/// page.
///
/// The export needs every match, not one page of records. The index answers
/// with the guids. The record bodies come from the Parquet file. This query
/// is [`translate`] with the page-only parts removed: no aggregation, no
/// `_source` field except the guid, and `search_after` instead of `from`.
/// `search_after` reads past the 10,000-document result window.
///
/// `after` holds the last guid of the previous page. `None` starts the
/// export.
#[must_use]
pub fn export_query(form: &SearchForm, after: Option<&str>, size: usize) -> Value {
    let mut query = translate(form);
    query["size"] = json!(size);
    // This query reads the guid from the doc values, not from `_source`.
    query["_source"] = json!(false);
    query["docvalue_fields"] = json!(["guid"]);
    // An export never reads `hits.total`.
    query["track_total_hits"] = json!(false);
    query.as_object_mut().map(|q| q.remove("aggs"));
    query.as_object_mut().map(|q| q.remove("from"));
    if let Some(after) = after {
        query["search_after"] = json!([after]);
    }
    query
}

/// This function builds the summary aggregation: "Out of N records, M carry
/// the attributes you asked for."
///
/// `context` gives N. N is what the rest of the form selects, without the
/// attribute rows. `matched` gives M. M is the full query. Both counts are
/// exact `doc_count` values. [`TRACK_TOTAL_HITS`] does not cap them. This
/// aggregation uses `global` because N covers a wider set than the query's
/// own result set.
fn summary_aggs(context: &Value, matched: &Value) -> Value {
    json!({
        "summary": {
            "global": {},
            "aggs": {
                "context": { "filter": context },
                "matched": { "filter": { "bool": matched } }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::search::Format;

    fn form() -> SearchForm {
        SearchForm {
            taxon: None,
            attr: None,
            prefix: None,
            country: None,
            state: None,
            from: None,
            to: None,
            attr_op: AttrOp::And,
            locality: None,
            collector: None,
            page: None,
            format: Format::Json,
            cols: None,
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
        // The values OR into ONE terms clause. The query has no nested query.
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
    fn the_summary_denominator_lifts_only_the_attribute_rows() {
        let f = SearchForm {
            taxon: Some(vec!["genus|Sorex".into()]),
            country: Some(vec!["United States".into()]),
            attr: Some(vec!["detected|virus: Orthohantavirus".into()]),
            ..form()
        };
        let b = translate(&f);
        let summary = &b["aggs"]["summary"];

        // N keeps the taxon clause and the country clause. N drops the attribute row.
        let context = summary["aggs"]["context"]["filter"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(context.len(), 2);
        assert_eq!(context[0]["match"]["genus.split"], "Sorex");
        assert_eq!(context[1]["terms"]["country"][0], "United States");

        // M holds the same clauses plus the attribute row. M is the real query.
        let matched = summary["aggs"]["matched"]["filter"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(matched.len(), 3);
        assert_eq!(matched[2]["terms"]["detected"][0], "virus: Orthohantavirus");
        assert_eq!(&summary["aggs"]["matched"]["filter"], &b["query"]);

        // This aggregation must count past the query's own result set.
        assert!(summary["global"].is_object());
    }

    #[test]
    fn taxon_relations_narrow_the_row_they_sit_in() {
        let f = SearchForm {
            taxon: Some(vec![
                "genus|Sorex|host of parasite; host of symbiont".into(),
                "genus|Myodes".into(),
            ]),
            ..form()
        };
        let b = translate(&f);
        let should = &b["query"]["bool"]["filter"][0]["bool"]["should"];
        // The relation kinds AND with their own taxon row. They do not AND with
        // the other row.
        let row = &should[0]["bool"]["filter"];
        assert_eq!(row[0]["match"]["genus.split"], "Sorex");
        assert_eq!(row[1]["nested"]["path"], "relations");
        let kinds = &row[1]["nested"]["query"]["terms"]["relations.relationship"];
        assert_eq!(kinds.as_array().unwrap().len(), 2);
        assert_eq!(kinds[0], "host of parasite");
        assert_eq!(kinds[1], "host of symbiont");
        // A row without the third segment keeps its plain shape.
        assert_eq!(should[1]["match"]["genus.split"], "Myodes");
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
        // One value or several values always builds a `terms` clause.
        assert_eq!(clauses[0]["terms"]["guid_prefix"][0], "MSB:Mamm");
        assert_eq!(clauses[1]["terms"]["country"].as_array().unwrap().len(), 2);
        assert_eq!(clauses[2]["terms"]["state_prov"][0], "Alaska");
        // This clause uses the collector id, not the collector name.
        assert_eq!(clauses[3]["term"]["collector_ids"], "agent/1234");

        assert_eq!(b["from"], 0);
        assert_eq!(
            translate(&SearchForm {
                page: Some(3),
                ..form()
            })["from"],
            PER_PAGE * 2
        );
        // Page 0 must not underflow the `usize` subtraction.
        assert_eq!(
            translate(&SearchForm {
                page: Some(0),
                ..form()
            })["from"],
            0
        );
    }

    #[test]
    fn the_export_query_pages_past_the_result_window_on_the_same_filters() {
        let f = SearchForm {
            country: Some(vec!["Mexico".into()]),
            page: Some(4),
            ..form()
        };
        let q = export_query(&f, None, 10_000);

        // The export filters match the search filters. The form page number
        // does not change them.
        assert_eq!(q["query"], translate(&f)["query"]);
        assert!(q.get("from").is_none());
        assert!(q.get("aggs").is_none());
        assert_eq!(q["size"], 10_000);
        // The guid comes from the doc values. No document is fetched.
        assert_eq!(q["_source"], false);
        assert_eq!(q["docvalue_fields"][0], "guid");
        // The sort is by guid. The next page starts at the last guid.
        assert_eq!(q["sort"][0]["guid"], "asc");
        assert!(q.get("search_after").is_none());

        let next = export_query(&f, Some("MSB:Mamm:9"), 10_000);
        assert_eq!(next["search_after"][0], "MSB:Mamm:9");
    }

    #[test]
    fn attr_op_or_puts_the_rows_in_one_should_block() {
        let f = SearchForm {
            attr: Some(vec!["detected|ectoparasite".into(), "sex|male".into()]),
            attr_op: AttrOp::Or,
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
            attr_op: AttrOp::Or,
            ..form()
        };
        let b = translate(&f);
        // The negation must stay inside the OR clause, not move to must_not.
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
            "two sibling clauses could match two different events"
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
