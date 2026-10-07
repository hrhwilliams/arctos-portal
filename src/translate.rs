//! The search model.
//!
//! [`Search::parse`] reads a [`SearchForm`] once and raises every 400 the
//! grammar can raise. Everything after that reads the model: the page query,
//! the export query, and the relation predicate are pure functions of a
//! [`Search`], so the query that selects records and the predicate that
//! filters their relations cannot disagree.

use serde_json::{Value, json};

use crate::{
    errors::AppError,
    ranks,
    schema::Relation,
    search::{AttrOp, SearchForm},
};

/// This list also serves as the CSV column list.
pub(crate) const SOURCE: &[&str] = &[
    "guid",
    "scientific_name",
    "relations",
    "country",
    "state_prov",
    "spec_locality",
    "events",
    "attributedetail",
    "partdetail",
];

/// The service sets the page size. The service does not accept a `per_page`
/// value from the client. `/api/schema` sends this number to the client.
pub(crate) const PER_PAGE: usize = 100;
const TRACK_TOTAL_HITS: usize = 10_000;

/// `value[key] = new` without the panicking index. A non-object `value` is
/// left alone; every caller holds an object it built itself.
pub(crate) fn set(value: &mut Value, key: &str, new: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_owned(), new);
    }
}

/// One decoded `attr` or `part` row: `[!]type|value|value…`.
#[derive(Debug, Clone)]
pub struct AttrRow {
    /// The row as the request sent it. The summary labels its count with it.
    pub raw: String,
    pub atype: String,
    pub values: Vec<String>,
    pub negated: bool,
}

/// This function splits a `taxon` or `attr` value on its unescaped `|`
/// separators. Every field the client joins is escaped first: a literal `|` is
/// written `\|` and a literal `\` is written `\\`. A doubled-pipe scheme would
/// be ambiguous, so the client does not use one.
///
/// The reference implementation is `splitPipes` in `src/lib/query.ts`.
fn split_pipes(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    cur.push(escaped);
                }
            }
            '|' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }

    out.push(cur);
    // Each segment is trimmed after the split, not before it.
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

/// One decoded `taxon` row.
///
/// The row selects **records**, through [`TaxonRow::clause`]. The same row also
/// has to be applied to one relation at a time, by `/api/relations`, which walks
/// the relations of the matched records to collect the other party of each.
/// [`TaxonRow::matches`] is that second reading, off the same fields.
#[derive(Debug, Clone)]
pub struct TaxonRow {
    rank: String,
    /// Empty when the row constrains the relation only: "anything that is a
    /// parasite of a Sorex" names no taxon on the specimen itself.
    name: String,
    relationships: Vec<String>,
    /// `(rank id, name)` pairs, each rank already checked.
    related: Vec<(String, String)>,
}

impl TaxonRow {
    /// A row naming neither a relationship nor a Related taxon puts no condition
    /// on relations at all. It selects records by taxon alone.
    #[must_use]
    pub const fn constrains_relations(&self) -> bool {
        !self.relationships.is_empty() || !self.related.is_empty()
    }

    /// This function reports whether one relation is one this row asked for.
    ///
    /// The comparison is case-insensitive, because the fields it reads are
    /// indexed through the `lc` normalizer and match that way in the query.
    #[must_use]
    fn matches(&self, relation: &Value) -> bool {
        let field = |key: &str| relation[key].as_str().unwrap_or_default().trim().to_owned();

        if !self.relationships.is_empty()
            && !self
                .relationships
                .iter()
                .any(|kind| field("relationship").eq_ignore_ascii_case(kind))
        {
            return false;
        }
        // The relationship and the Related taxon must hold for the SAME relation,
        // which is what testing them together on one relation means.
        if self.related.is_empty() {
            return true;
        }
        self.related
            .iter()
            .any(|(rank, name)| field(&related_field(rank)).eq_ignore_ascii_case(name))
    }

    /// This row in plain English, for [`Search::description`].
    fn describe(&self) -> String {
        let taxon = |rank: &str, name: &str| {
            if rank == "scientific_name" {
                name.to_owned()
            } else {
                format!("{rank} {name}")
            }
        };
        let mut text = if self.name.is_empty() {
            "any record".to_owned()
        } else {
            taxon(&self.rank, &self.name)
        };
        if !self.relationships.is_empty() {
            text = format!(
                "{text} with relationship {}",
                self.relationships.join(" or ")
            );
        }
        if !self.related.is_empty() {
            let names: Vec<String> = self
                .related
                .iter()
                .map(|(rank, name)| taxon(rank, name))
                .collect();
            text = format!("{text} to {}", names.join(" or "));
        }
        text
    }

    /// The clause that selects this row's records.
    ///
    /// The relation clause ANDs with its own taxon. It narrows that taxon. It
    /// does not form a separate match condition. The relationship and the
    /// Related taxa share ONE nested query, because they must describe the SAME
    /// relation.
    ///
    /// Note that this clause selects records, not relations: a matching record
    /// still carries every relation it has in `_source`, including the ones this
    /// clause did not match. The caller filters them for display.
    fn clause(&self) -> Value {
        let mut relation: Vec<Value> = Vec::new();
        if !self.relationships.is_empty() {
            relation.push(json!({ "terms": { "relations.relationship": self.relationships } }));
        }
        if !self.related.is_empty() {
            relation.push(any_of(
                self.related
                    .iter()
                    .map(|(rank, name)| related_clause(rank, name))
                    .collect(),
            ));
        }

        // An empty searched name is a real search as long as the row names
        // something else. Only a row naming nothing at all is refused, by
        // `Search::parse`.
        let specimen = (!self.name.is_empty() || relation.is_empty())
            .then(|| rank_clause(&self.rank, &self.name));
        let nested = (!relation.is_empty()).then(|| {
            json!({ "nested": {
                "path": "relations",
                "query": { "bool": { "filter": relation } }
            } })
        });
        match (specimen, nested) {
            (Some(specimen), Some(nested)) => json!({ "bool": { "filter": [specimen, nested] } }),
            (Some(clause), None) | (None, Some(clause)) => clause,
            // Parsing refuses a row that names nothing. This arm is not reached.
            (None, None) => json!({ "match_all": {} }),
        }
    }
}

/// This function decodes one taxon row.
///
/// The row splits, by [`split_pipes`], into `rank|name`, or
/// `rank|name|relations`, or that followed by a `rank|name` pair for each
/// Related taxon. The relations stay inside the third segment, separated by
/// `;`, because the segment count is what identifies the shape. The third
/// segment can be empty: a row can carry Related taxa with no relationship
/// selected.
///
/// The searched name may be empty as long as the row names something else — a
/// relationship, a Related taxon, or both:
///
/// ```text
/// ||host of parasite              anything that hosts a parasite
/// ||parasite of|genus|Sorex       anything that is a parasite of a Sorex
/// ```
///
/// Such a row constrains the relation and leaves the specimen itself
/// unconstrained. Segment 1 selects nothing there, so segment 0 is ignored
/// rather than checked. Only a row naming nothing at all is refused.
///
/// A row that does not fit the grammar is a 400. The client never emits one,
/// so such a row is a hand-edited URL, and salvaging it would answer a search
/// the user did not ask for.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] for a segment count outside
/// 2 and 3 + 2k, for a row that names nothing at all, for an empty Related taxon
/// name, for an unknown rank, and for a relationship that the schema does not
/// list.
fn taxon_row(raw: &str, known: &[Relation]) -> Result<TaxonRow, AppError> {
    let segments = split_pipes(raw);
    // Valid counts are 2, or 3 + 2k. An even count above two means an unpaired
    // Related taxon rank.
    if segments.len() < 2 || (segments.len() > 2 && segments.len().is_multiple_of(2)) {
        return Err(bad(format!(
            "`taxon` has {} fields, not 2 or 3 + 2 per related taxon",
            segments.len()
        )));
    }

    // The Related taxa of one row OR together: "Sorex that is the host of any
    // of these parasites."
    let related: Vec<(String, String)> = segments
        .get(3..)
        .unwrap_or_default()
        .chunks_exact(2)
        .filter_map(|pair| match pair {
            [rank, name] => Some((rank.clone(), name.clone())),
            _ => None,
        })
        .collect();
    for (rank, name) in &related {
        if name.is_empty() {
            return Err(bad("a related taxon carries an empty name"));
        }
        if rank != "scientific_name" && ranks::column(rank).is_none() {
            return Err(bad(format!("no rank named `{rank}`")));
        }
    }

    let relationships: Vec<String> = segments
        .get(2)
        .map(|joined| {
            joined
                .split(';')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    for kind in &relationships {
        if !known.iter().any(|k| k.value == *kind) {
            return Err(bad(format!("no relationship named `{kind}`")));
        }
    }

    let rank = segments.first().cloned().unwrap_or_default();
    let name = segments.get(1).cloned().unwrap_or_default();
    let constrains = !relationships.is_empty() || !related.is_empty();
    if !name.is_empty() || !constrains {
        if name.is_empty() {
            return Err(bad("`taxon` carries an empty name"));
        }
        if rank != "scientific_name" && ranks::column(&rank).is_none() {
            return Err(bad(format!("no rank named `{rank}`")));
        }
    }

    Ok(TaxonRow {
        rank,
        name,
        relationships,
        related,
    })
}

/// This function matches a `rank|name` pair on the searched specimen. The rank
/// was checked by [`taxon_row`].
fn rank_clause(rank: &str, name: &str) -> Value {
    // scientific_name is not a rank column. It matches as a phrase.
    if rank == "scientific_name" {
        return json!({ "match_phrase": { "scientific_name": name } });
    }
    let field = ranks::column(rank).unwrap_or(rank);
    json!({ "match": { format!("{field}.split"): name } })
}

/// This function matches a `rank|name` pair on the OTHER party of a relation.
/// A Related taxon is not a second constraint on the searched specimen: no
/// specimen is both a rodent and a tick.
///
/// A rank matches its own `relations.related_<field>`, which `ingest.py` fills
/// from the related record's rank chain. Matching the name against the relation's
/// free-text `related_identification` instead would ignore the rank outright: a
/// tapeworm identified as `Cestoda` would answer `genus|Cestoda` as readily as
/// `class|Cestoda`.
///
/// A `scientific_name` is not a rank. It matches the identification as a phrase,
/// which is also the one clause that still works for a related record whose name
/// the snapshot does not know.
fn related_clause(rank: &str, name: &str) -> Value {
    let field = related_field(rank);
    if rank == "scientific_name" {
        return json!({ "match_phrase": { format!("relations.{field}"): name } });
    }
    // The `lc` normalizer on these fields makes the term match case-insensitive,
    // as the searched taxon's own rank match already is.
    json!({ "term": { format!("relations.{field}"): name } })
}

/// This function names the field of a relation that a rank matches, for example
/// `class` -> `related_phylclass`. `ingest.py` fills these from the related
/// record's rank chain.
///
/// `scientific_name` is not a rank: it matches the related record's own
/// identification, which is also the only field a relation whose name the
/// snapshot does not know still carries.
fn related_field(rank: &str) -> String {
    if rank == "scientific_name" {
        return "related_identification".to_owned();
    }
    let field = ranks::column(rank).unwrap_or(rank);
    format!("related_{field}")
}

/// This function decodes one attribute row of the form `[!]type|value|value…`.
///
/// The row splits by [`split_pipes`], one segment per value, any number of
/// them. A leading `!` for a negated row sits before the escaped type and is
/// not itself escaped; no attribute type begins with `!`. A type with no value
/// is valid: the row then asks only whether the record has this type.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] when the type is empty.
fn decode_attr(raw: &str) -> Result<AttrRow, AppError> {
    let (negated, rest) = raw.strip_prefix('!').map_or((false, raw), |r| (true, r));
    let mut segments = split_pipes(rest);
    let values = segments.split_off(1);
    let atype = segments.remove(0);
    if atype.is_empty() {
        return Err(bad("`attr` carries an empty attribute type"));
    }
    Ok(AttrRow {
        raw: raw.to_owned(),
        atype,
        // The values are escaped, so a value holding a `;`, such as
        // `location in host` = `liver; spleen`, stays one value.
        values: values.into_iter().filter(|v| !v.is_empty()).collect(),
        negated,
    })
}

/// The part fields a `part` row may name. A part holds its values in named
/// fields rather than in type/value pairs, so the row's first segment is a
/// field name. Only the keyword fields are here: the two identifier fields are
/// not indexed, and the two text fields answer a different kind of question.
const PART_FIELDS: &[&str] = &["part_name", "disposition", "condition", "part_barcode"];

/// This function decodes one part row, the same grammar as an attribute row,
/// and checks the field it names.
///
/// # Errors
///
/// The function returns [`AppError::BadRequest`] when the row names a field
/// outside [`PART_FIELDS`], or an empty one.
fn decode_part(raw: &str) -> Result<AttrRow, AppError> {
    let row = decode_attr(raw)?;
    if !PART_FIELDS.contains(&row.atype.as_str()) {
        return Err(bad(format!(
            "`part` names `{}`, which is not a searchable part field",
            row.atype
        )));
    }
    Ok(row)
}

/// This function converts one part row into one clause. With no value the row
/// asks only whether the record has a part carrying that field at all.
fn part_clause(a: &AttrRow) -> Value {
    let field = format!("partdetail.{}", a.atype);
    let filter = if a.values.is_empty() {
        json!({ "exists": { "field": field } })
    } else {
        json!({ "terms": { field: a.values } })
    };
    json!({ "nested": { "path": "partdetail", "query": { "bool": { "filter": [filter] } } } })
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
fn attribute_clause(a: &AttrRow) -> Value {
    let flat = ranks::detection_field(&a.atype);

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

/// This function matches an exact collector name against a `collector`-role
/// agent on the record.
fn collector_clause(name: &str) -> Value {
    json!({ "nested": {
        "path": "agents",
        "query": { "bool": { "filter": [
            { "term": { "agents.agent_name.keyword": name } },
            { "term": { "agents.agent_role": "collector" } }
        ] } }
    } })
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

fn one(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// A search, parsed once from the form.
///
/// Every field is already trimmed, non-blank, and checked. `page` starts at 1.
/// `tab` names a relationship the schema lists.
#[derive(Debug, Clone)]
pub struct Search {
    pub taxa: Vec<TaxonRow>,
    pub attrs: Vec<AttrRow>,
    pub attr_op: AttrOp,
    pub parts: Vec<AttrRow>,
    pub prefixes: Vec<String>,
    pub countries: Vec<String>,
    pub states: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub locality: Option<String>,
    pub collector: Option<String>,
    /// The table the user is looking at. Absent means the Specimens themselves.
    /// A value is a relationship, and selects the related records reached by it.
    pub tab: Option<String>,
    pub page: usize,
}

impl Search {
    /// This function reads the form once and raises every 400 the grammar can
    /// raise. See [`taxon_row`], [`decode_attr`], and [`decode_part`].
    ///
    /// # Errors
    ///
    /// The function returns [`AppError::BadRequest`] for a `taxon`, `attr`, or
    /// `part` row outside its grammar, and for a `tab` that is not a relationship
    /// the schema lists.
    pub fn parse(form: &SearchForm, known: &[Relation]) -> Result<Self, AppError> {
        let tab = form.tab().map(str::to_owned);
        if let Some(tab) = &tab
            && !known.iter().any(|r| r.value == *tab)
        {
            return Err(bad(format!("no relationship named `{tab}`")));
        }
        Ok(Self {
            taxa: present(form.taxon.as_ref())
                .iter()
                .map(|raw| taxon_row(raw, known))
                .collect::<Result<_, _>>()?,
            attrs: present(form.attr.as_ref())
                .iter()
                .map(|raw| decode_attr(raw))
                .collect::<Result<_, _>>()?,
            attr_op: form.attr_op,
            parts: present(form.part.as_ref())
                .iter()
                .map(|raw| decode_part(raw))
                .collect::<Result<_, _>>()?,
            prefixes: present(form.prefix.as_ref()),
            countries: present(form.country.as_ref()),
            states: present(form.state.as_ref()),
            from: one(form.from.as_ref()),
            to: one(form.to.as_ref()),
            locality: one(form.locality.as_ref()),
            collector: one(form.collector.as_ref()),
            tab,
            // Page 0 is not valid. A hand-edited link can still carry page 0.
            page: form.page.unwrap_or(1).max(1),
        })
    }

    /// This function reports whether `prefix` is the only filter the search
    /// sets. The caller can then read guids straight from the dump instead of
    /// asking Elasticsearch, because the dump is already partitioned by
    /// `guid_prefix`.
    ///
    /// A `tab` disqualifies the shortcut whatever else the search holds: the
    /// rows wanted are then the related records, which the dump is not
    /// partitioned by.
    #[must_use]
    pub const fn guid_prefix_only(&self) -> bool {
        self.tab.is_none()
            && !self.prefixes.is_empty()
            && self.taxa.is_empty()
            && self.attrs.is_empty()
            && self.parts.is_empty()
            && self.countries.is_empty()
            && self.states.is_empty()
            && self.from.is_none()
            && self.to.is_none()
            && self.locality.is_none()
            && self.collector.is_none()
    }

    /// This function reports whether the search names a relationship or a
    /// Related taxon on any row, which is when `/api/relations` has a table
    /// to show.
    #[must_use]
    pub fn constrains_relations(&self) -> bool {
        self.taxa.iter().any(TaxonRow::constrains_relations)
    }

    /// The relationships the rows name, deduplicated, in the order they were
    /// named. A `tab` is the only one, since it narrows every row to itself.
    ///
    /// A row naming a Related taxon and no relationship names nothing here: it
    /// asks for any relationship that reaches that taxon, which is only known
    /// once the relations are walked.
    #[must_use]
    pub fn relationships(&self) -> Vec<String> {
        if let Some(tab) = &self.tab {
            return vec![tab.clone()];
        }
        let mut out: Vec<String> = Vec::new();
        for kind in self.taxa.iter().flat_map(|row| &row.relationships) {
            if !out.iter().any(|k| k.eq_ignore_ascii_case(kind)) {
                out.push(kind.clone());
            }
        }
        out
    }

    /// One sentence restating the search in plain English, for a researcher
    /// to confirm they searched what they meant to. `None` for an empty form.
    ///
    /// This is the template version: it never blocks and never fails. A model
    /// could write a better sentence from the same fields; if one is added, it
    /// must keep this behaviour of answering `None` rather than delaying the
    /// numbers.
    #[must_use]
    pub fn description(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.taxa.is_empty() {
            parts.push(
                self.taxa
                    .iter()
                    .map(TaxonRow::describe)
                    .collect::<Vec<_>>()
                    .join(", or "),
            );
        }
        if !self.attrs.is_empty() {
            let joiner = if self.attr_op == AttrOp::Or { " or " } else { " and " };
            let rows: Vec<String> = self.attrs.iter().map(describe_attr).collect();
            parts.push(format!("with {}", rows.join(joiner)));
        }
        if !self.parts.is_empty() {
            let rows: Vec<String> = self.parts.iter().map(describe_attr).collect();
            parts.push(format!("having parts with {}", rows.join(" and ")));
        }
        if !self.prefixes.is_empty() {
            parts.push(format!("in {}", self.prefixes.join(", ")));
        }
        if !self.countries.is_empty() {
            parts.push(format!("from {}", self.countries.join(", ")));
        }
        if !self.states.is_empty() {
            parts.push(format!("in {}", self.states.join(", ")));
        }
        if let Some(locality) = &self.locality {
            parts.push(format!("at \"{locality}\""));
        }
        if let Some(collector) = &self.collector {
            parts.push(format!("collected by {collector}"));
        }
        match (&self.from, &self.to) {
            (Some(from), Some(to)) => parts.push(format!("collected {from} to {to}")),
            (Some(from), None) => parts.push(format!("collected from {from}")),
            (None, Some(to)) => parts.push(format!("collected up to {to}")),
            (None, None) => {}
        }
        if parts.is_empty() {
            return None;
        }
        let mut sentence = parts.join(" ");
        // Capitalise the first letter; a rank id is lower case.
        if let Some(first) = sentence.get(..1) {
            let upper = first.to_uppercase();
            sentence.replace_range(..1, &upper);
        }
        sentence.push('.');
        Some(sentence)
    }

    /// This function reports whether a relation is one the search asked for.
    ///
    /// Taxon rows OR with each other, so one row is enough. A row that
    /// constrains no relation is skipped rather than treated as matching
    /// everything: it selected its records by taxon alone and asked for no
    /// pairing.
    ///
    /// `tab` narrows the result to one relationship. It ANDs with the rows
    /// rather than replacing them: a tab can only ever show a subset of what the
    /// search asked for. A tab naming a relationship no row asked for therefore
    /// matches nothing, which is the honest answer for a table that cannot hold
    /// anything.
    #[must_use]
    pub fn relation_matches(&self, relation: &Value) -> bool {
        let kind = relation["relationship"].as_str().unwrap_or_default().trim();
        if let Some(tab) = &self.tab
            && !kind.eq_ignore_ascii_case(tab)
        {
            return false;
        }
        self.taxa
            .iter()
            .filter(|row| row.constrains_relations())
            .any(|row| row.matches(relation))
    }

    /// The page query, with the summary aggregation.
    #[must_use]
    pub fn query(&self) -> Value {
        // This function keeps the attribute clauses apart from the rest of the
        // filter. This split lets the summary count the records that the rest
        // of the form selects.
        let mut filter: Vec<Value> = Vec::new();
        let mut attr_filter: Vec<Value> = Vec::new();
        let mut must_not: Vec<Value> = Vec::new();

        // Block 1: taxon rows OR together.
        let taxon_clauses: Vec<Value> = self.taxa.iter().map(TaxonRow::clause).collect();
        // The taxon block on its own is the summary's widest number: "there are
        // X Sorex", before any filter of the form narrows it. With no taxon row
        // it is an empty filter list, which counts every record.
        let taxon_only = json!({ "bool": { "filter": &taxon_clauses } });
        if !taxon_clauses.is_empty() {
            filter.push(any_of(taxon_clauses));
        }

        // Block 2: attribute rows AND or OR, per attr_op.
        if self.attr_op == AttrOp::Or {
            // A negated row inside an OR clause stays inside that clause. The
            // negation does not move to the top level.
            let clauses: Vec<Value> = self
                .attrs
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
            for row in &self.attrs {
                let clause = attribute_clause(row);
                if row.negated {
                    must_not.push(clause);
                } else {
                    attr_filter.push(clause);
                }
            }
        }

        // Block 2b: part rows always AND, and sit in the context rather than
        // beside the attribute rows. The summary counts attribute rows one at a
        // time; a part row is a scope like the collector, not one of those rows.
        for row in &self.parts {
            let clause = part_clause(row);
            if row.negated {
                must_not.push(clause);
            } else {
                filter.push(clause);
            }
        }

        // A prefix clause ANDs against the taxon block. It does not OR into it.
        if !self.prefixes.is_empty() {
            filter.push(json!({ "terms": { "guid_prefix": self.prefixes } }));
        }

        // Block 3: scope. These fields are record-level. They do not link to
        // the event date.
        for (field, values) in [("country", &self.countries), ("state_prov", &self.states)] {
            if !values.is_empty() {
                filter.push(json!({ "terms": { field: values } }));
            }
        }

        if let Some(collector) = &self.collector {
            filter.push(collector_clause(collector));
        }

        // The locality is the record's own `spec_locality`, matched as a
        // phrase: "Sandia Mountains" matches "Sandia Mountains, Cibola National
        // Forest" and does not match "Mountains of Sandia". The field is
        // analysed text, so the match is on the words, not on the whole string.
        //
        // This clause is record-level, unlike the date below. A record with
        // several events carries one `spec_locality` at the top, so a locality
        // and a date are not held to the same event the way two dates are.
        if let Some(locality) = &self.locality {
            filter.push(json!({ "match_phrase": { "spec_locality": locality } }));
        }

        // Both ends of a date range have to hold for ONE event, or a record
        // collected in 2024 and again in 2026 matches a 2025 search.
        if self.from.is_some() || self.to.is_some() {
            let mut range = json!({});
            if let Some(from) = &self.from {
                set(&mut range, "gte", json!(from));
            }
            if let Some(to) = &self.to {
                set(&mut range, "lte", json!(to));
            }
            filter.push(json!({
                "nested": {
                    "path": "events",
                    "query": { "bool": { "filter": [{ "range": { "events.began_date": range } }] } }
                }
            }));
        }

        // This filter list holds everything except the attribute rows. It is
        // the denominator of the summary.
        let context = json!({ "bool": { "filter": filter.clone() } });
        let per_row = per_row_filters(&self.attrs, &filter);

        filter.extend(attr_filter);
        let mut bool_query = json!({ "filter": filter });
        if !must_not.is_empty() {
            set(&mut bool_query, "must_not", json!(must_not));
        }

        json!({
            "track_total_hits": TRACK_TOTAL_HITS,
            "from": self.page.saturating_sub(1).saturating_mul(PER_PAGE),
            "size": PER_PAGE,
            "sort": [{ "guid": "asc" }],
            "_source": SOURCE,
            "aggs": summary_aggs(&taxon_only, &context, &bool_query, &per_row),
            "query": { "bool": bool_query }
        })
    }

    /// One page of guids for the same search as the page query.
    ///
    /// The export needs every match, not one page of records. This query is
    /// [`Search::query`] with the page-only parts removed: no aggregation, no
    /// `_source` field except the guid, and `search_after` instead of `from`.
    /// `search_after` reads past the 10,000-document result window.
    ///
    /// `after` holds the last guid of the previous page. `None` starts the
    /// export.
    #[must_use]
    pub fn export_query(&self, after: Option<&str>, size: usize) -> Value {
        let mut query = self.query();
        set(&mut query, "size", json!(size));
        // This query reads the guid from the doc values, not from `_source`.
        set(&mut query, "_source", json!(false));
        set(&mut query, "docvalue_fields", json!(["guid"]));
        // An export never reads `hits.total`.
        set(&mut query, "track_total_hits", json!(false));
        query.as_object_mut().map(|q| q.remove("aggs"));
        query.as_object_mut().map(|q| q.remove("from"));
        if let Some(after) = after {
            set(&mut query, "search_after", json!([after]));
        }
        query
    }
}

/// One attribute or part row in plain English: `sex male or female`, `no sex
/// recorded`, `detected recorded`.
fn describe_attr(row: &AttrRow) -> String {
    let prefix = if row.negated { "no " } else { "" };
    if row.values.is_empty() {
        format!("{prefix}{} recorded", row.atype)
    } else {
        format!("{prefix}{} {}", row.atype, row.values.join(" or "))
    }
}

/// This function builds one filter per attribute row, each measured against the
/// same denominator: "of the N records the rest of the form selects, M carry
/// this attribute."
///
/// The rows are counted one at a time, so `attr_op` does not apply here. A
/// negated row counts the records that do not carry it, which is what that row
/// filters for.
fn per_row_filters(rows: &[AttrRow], context: &[Value]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            let mut filter = context.to_vec();
            let clause = attribute_clause(row);
            if row.negated {
                json!({ "bool": { "filter": filter, "must_not": [clause] } })
            } else {
                filter.push(clause);
                json!({ "bool": { "filter": filter } })
            }
        })
        .collect()
}

/// This function builds the summary aggregation, which reads as a funnel:
///
/// ```text
/// taxon    there are X Sorex
/// context  Y of them match the rest of the form
/// row_0    Z of those Y carry the first attribute
/// row_1    W of those Y carry the second
/// matched  and M match the whole form at once
/// ```
///
/// Every count is an exact `doc_count`. [`TRACK_TOTAL_HITS`] does not cap them.
/// The aggregation is `global` because each of these sets is wider than the
/// query's own result set.
///
/// A row count is not a share of `matched`, and the row counts do not sum to it:
/// each row is measured on its own against `context`, so one record carrying two
/// of the attributes is counted in both rows.
fn summary_aggs(taxon: &Value, context: &Value, matched: &Value, per_row: &[Value]) -> Value {
    let mut aggs = serde_json::Map::new();
    aggs.insert("taxon".to_owned(), json!({ "filter": taxon }));
    aggs.insert("context".to_owned(), json!({ "filter": context }));
    aggs.insert("matched".to_owned(), json!({ "filter": { "bool": matched } }));
    for (i, row) in per_row.iter().enumerate() {
        // The key carries the row's position in `attr`, so the client can line
        // the counts back up with the rows it sent.
        aggs.insert(format!("row_{i}"), json!({ "filter": row }));
    }

    json!({ "summary": { "global": {}, "aggs": aggs } })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::search::Format;

    /// The relationship terms the schema lists in these tests.
    fn known() -> Vec<Relation> {
        ["host of parasite", "host of symbiont", "parasite of"]
            .iter()
            .map(|value| Relation {
                value: (*value).to_string(),
                description: String::new(),
            })
            .collect()
    }

    fn parse(form: &SearchForm) -> Result<Search, AppError> {
        Search::parse(form, &known())
    }

    /// Every test that builds a valid form asserts on the query, so the tests
    /// unwrap here rather than threading the schema through each call.
    fn translate(form: &SearchForm) -> Value {
        parse(form).unwrap().query()
    }

    fn taxon_form(row: &str) -> SearchForm {
        SearchForm {
            taxon: Some(vec![row.to_string()]),
            ..form()
        }
    }

    /// This helper mirrors the client's field encoder. The service never
    /// encodes, but a round trip is the only way to test a hostile name.
    fn escape(field: &str) -> String {
        field.replace('\\', r"\\").replace('|', r"\|")
    }

    fn form() -> SearchForm {
        SearchForm {
            taxon: None,
            attr: None,
            part: None,
            prefix: None,
            country: None,
            state: None,
            from: None,
            to: None,
            attr_op: AttrOp::And,
            locality: None,
            collector: None,
            tab: None,
            page: None,
            format: Format::Json,
            cols: None,
        }
    }

    #[test]
    fn a_part_row_filters_on_the_named_part_field() {
        let f = SearchForm {
            part: Some(vec!["part_name|tissue|skull".into()]),
            ..form()
        };
        let b = translate(&f);
        let nested = &b["query"]["bool"]["filter"][0]["nested"];
        assert_eq!(nested["path"], "partdetail");
        assert_eq!(
            nested["query"]["bool"]["filter"][0]["terms"]["partdetail.part_name"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // A valueless row asks only whether a part carries the field at all.
        let f = SearchForm {
            part: Some(vec!["part_barcode".into()]),
            ..form()
        };
        let b = translate(&f);
        assert_eq!(
            b["query"]["bool"]["filter"][0]["nested"]["query"]["bool"]["filter"][0]["exists"]
                ["field"],
            "partdetail.part_barcode"
        );

        // A field outside the allowlist is a 400, not a query that silently
        // matches nothing.
        let f = SearchForm {
            part: Some(vec!["container_path|UAF".into()]),
            ..form()
        };
        assert!(Search::parse(&f, &[]).is_err());
    }

    #[test]
    fn detection_row_takes_the_flat_fast_path() {
        let f = SearchForm {
            attr: Some(vec![
                "detected|ectoparasite: flea|ectoparasite: louse".into(),
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
            attr: Some(vec!["sex|male|female".into()]),
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
    fn the_summary_is_a_funnel_from_the_taxon_down_to_each_attribute_row() {
        let f = SearchForm {
            taxon: Some(vec!["genus|Sorex".into()]),
            country: Some(vec!["United States".into()]),
            attr: Some(vec!["detected|ectoparasite".into(), "!sex|male".into()]),
            ..form()
        };
        let aggs = &translate(&f)["aggs"]["summary"]["aggs"];

        // X: the taxon rows alone, with the country filter lifted off.
        let taxon = aggs["taxon"]["filter"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(taxon.len(), 1);
        assert_eq!(taxon[0]["match"]["genus.split"], "Sorex");

        // Y: the taxon plus every filter except the attribute rows.
        let context = aggs["context"]["filter"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(context.len(), 2);

        // Each row is measured against Y, one row at a time, so a row carries
        // the country filter but never the other row.
        let row_0 = aggs["row_0"]["filter"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(row_0.len(), 3);
        assert_eq!(row_0[2]["terms"]["detected"][0], "ectoparasite");

        // A negated row counts the records that do not carry it.
        let row_1 = &aggs["row_1"]["filter"]["bool"];
        assert_eq!(row_1["filter"].as_array().unwrap().len(), 2);
        assert_eq!(
            row_1["must_not"][0]["nested"]["query"]["bool"]["filter"][0]["term"]
                ["attributedetail.attribute_type"],
            "sex"
        );

        // A form with no attribute rows has no row aggregations at all.
        let bare = SearchForm {
            taxon: Some(vec!["genus|Sorex".into()]),
            ..form()
        };
        assert!(translate(&bare)["aggs"]["summary"]["aggs"]["row_0"].is_null());
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
        let kinds =
            &row[1]["nested"]["query"]["bool"]["filter"][0]["terms"]["relations.relationship"];
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
            collector: Some("Smith, J.".into()),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        // One value or several values always builds a `terms` clause.
        assert_eq!(clauses[0]["terms"]["guid_prefix"][0], "MSB:Mamm");
        assert_eq!(clauses[1]["terms"]["country"].as_array().unwrap().len(), 2);
        assert_eq!(clauses[2]["terms"]["state_prov"][0], "Alaska");
        // This clause is a nested query on agents, matched by name and role.
        let inner = clauses[3]["nested"]["query"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(clauses[3]["nested"]["path"], "agents");
        assert_eq!(inner[0]["term"]["agents.agent_name.keyword"], "Smith, J.");
        assert_eq!(inner[1]["term"]["agents.agent_role"], "collector");

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
        let search = parse(&f).unwrap();
        let q = search.export_query(None, 10_000);

        // The export filters match the search filters. The form page number
        // does not change them.
        assert_eq!(q["query"], search.query()["query"]);
        assert!(q.get("from").is_none());
        assert!(q.get("aggs").is_none());
        assert_eq!(q["size"], 10_000);
        // The guid comes from the doc values. No document is fetched.
        assert_eq!(q["_source"], false);
        assert_eq!(q["docvalue_fields"][0], "guid");
        // The sort is by guid. The next page starts at the last guid.
        assert_eq!(q["sort"][0]["guid"], "asc");
        assert!(q.get("search_after").is_none());

        let next = search.export_query(Some("MSB:Mamm:9"), 10_000);
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
    fn the_locality_matches_the_record_and_the_dates_match_one_event() {
        let f = SearchForm {
            locality: Some("Sandia Mountains".into()),
            from: Some("2024-01-01".into()),
            to: Some("2024-12-31".into()),
            ..form()
        };
        let b = translate(&f);
        let clauses = b["query"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(clauses.len(), 2);

        // The locality reads the record's own field, as a phrase over the
        // analysed text rather than a term over the whole string.
        assert_eq!(clauses[0]["match_phrase"]["spec_locality"], "Sandia Mountains");

        // Both ends of a date range still have to hold for ONE event, or a
        // record collected in 2024 and again in 2026 matches a 2025 search.
        let inner = clauses[1]["nested"]["query"]["bool"]["filter"]
            .as_array()
            .unwrap();
        assert_eq!(clauses[1]["nested"]["path"], "events");
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["range"]["events.began_date"]["gte"], "2024-01-01");
        assert_eq!(inner[0]["range"]["events.began_date"]["lte"], "2024-12-31");
    }

    #[test]
    fn a_pipe_inside_a_field_survives_the_round_trip() {
        // The hostile list `src/lib/query.test.ts` round-trips. Two invariants:
        // an escaped field never loses a segment boundary, and never invents
        // one.
        for name in [
            "|",
            "||",
            "|||",
            "a|",
            "|a",
            "a||b",
            r"\",
            r"\\",
            r"\|",
            r"|\",
            r"a\|b",
            r"\|\|",
            "x|y|z",
            "a; b",
            // This one matters most: a name full of pipes must not be able to
            // masquerade as a related-taxon search.
            "species|Ixodidae|family|Aus",
        ] {
            let row = format!("{}|{}", escape("genus"), escape(name));
            assert_eq!(
                split_pipes(&row),
                vec!["genus".to_string(), name.to_string()],
                "round trip of {name:?}"
            );
            let b = translate(&taxon_form(&row));
            assert_eq!(b["query"]["bool"]["filter"][0]["match"]["genus.split"], name);
        }
    }

    #[test]
    fn a_related_taxon_matches_the_other_party_of_the_same_relation() {
        let b = translate(&taxon_form(
            "genus|Sorex|host of parasite|family|Ixodidae|genus|Aus",
        ));
        let row = &b["query"]["bool"]["filter"][0]["bool"]["filter"];
        assert_eq!(row[0]["match"]["genus.split"], "Sorex");

        // The relationship and the related taxa share ONE nested clause. Two
        // sibling clauses could match two different relations.
        let relation = &row[1]["nested"]["query"]["bool"]["filter"];
        assert_eq!(row[1]["nested"]["path"], "relations");
        assert_eq!(
            relation[0]["terms"]["relations.relationship"][0],
            "host of parasite"
        );

        // The related taxa OR together, each on its own rank field.
        let should = &relation[1]["bool"]["should"];
        assert_eq!(relation[1]["bool"]["minimum_should_match"], 1);
        assert_eq!(should[0]["term"]["relations.related_family"], "Ixodidae");
        assert_eq!(should[1]["term"]["relations.related_genus"], "Aus");
    }

    #[test]
    fn a_related_rank_selects_the_field_it_names() {
        // The reported bug: every rank matched the same free-text
        // identification, so a class name answered a genus search.
        for (rank, field) in [
            ("class", "relations.related_phylclass"),
            ("order", "relations.related_phylorder"),
            ("genus", "relations.related_genus"),
            ("species", "relations.related_species"),
        ] {
            let b = translate(&taxon_form(&format!("genus|Sorex||{rank}|Cestoda")));
            let related = &b["query"]["bool"]["filter"][0]["bool"]["filter"][1]["nested"]["query"]
                ["bool"]["filter"][0];
            assert_eq!(related["term"][field], "Cestoda", "{rank}");
        }

        // A scientific name is not a rank. It stays a whole-name match on the
        // related record's identification.
        let b = translate(&taxon_form("genus|Sorex||scientific_name|Sorex cinereus"));
        let related = &b["query"]["bool"]["filter"][0]["bool"]["filter"][1]["nested"]["query"]
            ["bool"]["filter"][0];
        assert_eq!(
            related["match_phrase"]["relations.related_identification"],
            "Sorex cinereus"
        );
    }

    #[test]
    fn a_related_taxon_needs_no_relationship() {
        let b = translate(&taxon_form("genus|Sorex||family|Ixodidae"));
        let relation = &b["query"]["bool"]["filter"][0]["bool"]["filter"][1]["nested"]["query"]
            ["bool"]["filter"];
        // The empty relations segment adds no clause of its own.
        assert!(relation[1].is_null());
        assert_eq!(relation[0]["term"]["relations.related_family"], "Ixodidae");
    }

    #[test]
    fn the_relation_predicate_reads_the_same_row_the_clause_does() {
        let search = |taxon: &str| parse(&taxon_form(taxon)).unwrap();
        let with_tab = |taxon: &str, tab: &str| {
            parse(&SearchForm {
                tab: Some(tab.to_owned()),
                ..taxon_form(taxon)
            })
            .unwrap()
        };
        let cestode = json!({
            "relationship": "host of parasite",
            "related_guid": "DMNS:Para:581",
            "related_identification": "Cestoda",
            "related_phylum": "Platyhelminthes",
            "related_phylclass": "Cestoda",
        });
        let mite = json!({
            "relationship": "host of parasite",
            "related_guid": "MSB:Para:9",
            "related_identification": "Androlaelaps fahrenholzi",
            "related_phylclass": "Arachnida",
            "related_genus": "Androlaelaps",
        });
        let littermate = json!({ "relationship": "sibling of", "related_guid": "MSB:Mamm:2" });

        let asked = search("genus|Sorex|host of parasite|class|Cestoda");
        assert!(asked.relation_matches(&cestode));
        // Same relationship, wrong taxon. Same taxon, wrong relationship.
        assert!(!asked.relation_matches(&mite));
        assert!(!asked.relation_matches(&littermate));

        // The rank is read off the field it names, so a class name does not
        // answer a genus row.
        assert!(!search("genus|Sorex|host of parasite|genus|Cestoda").relation_matches(&cestode));

        // The comparison is case-insensitive, as the indexed field is.
        assert!(search("genus|Sorex|host of parasite|class|cestoda").relation_matches(&cestode));

        // Related taxa within a row OR. So do rows.
        let two = search("genus|Sorex|host of parasite|class|Cestoda|genus|Androlaelaps");
        assert!(two.relation_matches(&cestode) && two.relation_matches(&mite));

        // A row with a relationship and no related taxon takes any pairing of
        // that kind; a row with neither asks for no relation at all.
        assert!(search("genus|Sorex|host of parasite").relation_matches(&mite));
        assert!(!search("genus|Sorex").relation_matches(&mite));

        // A scientific name matches the related record's own identification.
        assert!(
            search("genus|Sorex||scientific_name|Androlaelaps fahrenholzi").relation_matches(&mite)
        );

        // The tab narrows to one relationship and ANDs with the rows. It never
        // widens them: a tab the search did not ask for holds nothing.
        let both = "genus|Sorex|host of parasite; parasite of";
        assert!(with_tab(both, "host of parasite").relation_matches(&mite));
        assert!(!with_tab(both, "parasite of").relation_matches(&mite));
        assert!(!with_tab(both, "host of parasite").relation_matches(&littermate));
        // A tab the schema does not list is refused at parse time.
        assert!(
            parse(&SearchForm {
                tab: Some("sibling of".into()),
                ..taxon_form(both)
            })
            .is_err()
        );
    }

    #[test]
    fn an_empty_searched_name_inverts_the_search_onto_the_related_taxon() {
        // "Everything that is a parasite of a Sorex": the row constrains the
        // other party only, so the clause is the nested relation alone, with no
        // clause on the specimen.
        let b = translate(&taxon_form("||parasite of|genus|Sorex"));
        let clause = &b["query"]["bool"]["filter"][0];
        assert_eq!(clause["nested"]["path"], "relations");
        assert!(
            clause["bool"].is_null(),
            "an empty name must not leave a match-all clause on the specimen"
        );
        let relation = &clause["nested"]["query"]["bool"]["filter"];
        assert_eq!(relation[0]["terms"]["relations.relationship"][0], "parasite of");
        assert_eq!(relation[1]["term"]["relations.related_genus"], "Sorex");

        // A relationship alone holds a row up just as well: "anything that hosts
        // a parasite", with no taxon on either side.
        let b = translate(&taxon_form("||host of parasite"));
        let clause = &b["query"]["bool"]["filter"][0];
        assert!(clause["bool"].is_null());
        let relation = &clause["nested"]["query"]["bool"]["filter"];
        assert_eq!(
            relation[0]["terms"]["relations.relationship"][0],
            "host of parasite"
        );
        assert!(relation[1].is_null(), "no related taxon to constrain");

        // The forward search still constrains both parties.
        let b = translate(&taxon_form("genus|Sorex|host of parasite|class|Cestoda"));
        let row = &b["query"]["bool"]["filter"][0]["bool"]["filter"];
        assert_eq!(row[0]["match"]["genus.split"], "Sorex");
        assert_eq!(row[1]["nested"]["path"], "relations");
    }

    #[test]
    fn a_taxon_row_outside_the_grammar_is_refused_not_salvaged() {
        let refused = |row: &str| {
            let e = parse(&taxon_form(row)).unwrap_err();
            assert!(
                matches!(e, AppError::BadRequest(_)),
                "{row:?} must be a 400, not {e:?}"
            );
        };

        // A one-segment value is no longer emitted, so it is no longer read.
        refused("Myodes gapperi");
        // An even count above two leaves a related rank unpaired.
        refused("genus|Sorex|host of parasite|family");
        refused("genus|Sorex|host of parasite|family|Ixodidae|genus");
        // A row that names nothing at all is a match-all, not a search.
        refused("genus|");
        refused("|");
        refused("||");
        // An empty related name is refused whatever the searched name is.
        refused("genus|Sorex|host of parasite|family|");
        refused("||host of parasite|family|");
        // An unknown rank, in either position.
        refused("nope|Sorex");
        refused("genus|Sorex||nope|Ixodidae");
        // A relationship the schema does not list.
        refused("genus|Sorex|host of nothing");
        refused("genus|Sorex|host of parasite; host of nothing");

        // The valid shapes stay valid.
        for row in [
            "genus|Sorex",
            "genus|Sorex|host of parasite",
            "genus|Sorex|host of parasite; parasite of",
            "genus|Sorex|host of parasite|family|Ixodidae",
            "genus|Sorex|host of parasite|family|Ixodidae|genus|Aus",
            "genus|Sorex||family|Ixodidae",
            "scientific_name|Sorex cinereus",
            // An empty searched name, held up by what else the row names: a
            // relationship, a related taxon, or both.
            "||host of parasite",
            "||parasite of|genus|Sorex",
            "|||genus|Sorex",
            "||host of parasite; parasite of",
        ] {
            assert!(parse(&taxon_form(row)).is_ok(), "{row}");
        }
    }

    #[test]
    fn an_attr_value_holding_a_semicolon_stays_one_value() {
        let f = SearchForm {
            attr: Some(vec![format!(
                "{}|{}",
                escape("location in host"),
                escape("liver; spleen")
            )]),
            ..form()
        };
        let b = translate(&f);
        let values = &b["query"]["bool"]["filter"][0]["nested"]["query"]["bool"]["filter"][1]
            ["terms"]["attributedetail.attribute_value"];
        assert_eq!(values.as_array().unwrap().len(), 1);
        assert_eq!(values[0], "liver; spleen");
    }

    #[test]
    fn a_negation_sits_outside_the_escaped_type() {
        let f = SearchForm {
            attr: Some(vec![format!("!{}|a|b", escape("has|pipe"))]),
            ..form()
        };
        let b = translate(&f);
        let filter = &b["query"]["bool"]["must_not"][0]["nested"]["query"]["bool"]["filter"];
        assert_eq!(filter[0]["term"]["attributedetail.attribute_type"], "has|pipe");
        assert_eq!(
            filter[1]["terms"]["attributedetail.attribute_value"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // An empty type is a 400, not a dropped row.
        let f = SearchForm {
            attr: Some(vec!["!|male".into()]),
            ..form()
        };
        assert!(matches!(parse(&f).unwrap_err(), AppError::BadRequest(_)));
    }

    #[test]
    fn an_empty_form_produces_a_match_all_that_the_caller_must_refuse() {
        let b = translate(&form());
        assert!(b["query"]["bool"]["filter"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_description_restates_the_search_and_is_absent_for_an_empty_form() {
        assert!(parse(&form()).unwrap().description().is_none());

        // The numbers-to-check-against query from the summary spec.
        let s = parse(&taxon_form("genus|Sorex|host of parasite|class|Cestoda")).unwrap();
        assert_eq!(
            s.description().unwrap(),
            "Genus Sorex with relationship host of parasite to class Cestoda."
        );
        assert!(s.constrains_relations());
        assert!(!parse(&taxon_form("genus|Sorex")).unwrap().constrains_relations());

        // Every other block, in the order the sentence carries them.
        let f = SearchForm {
            taxon: Some(vec!["scientific_name|Sorex cinereus".into(), "||parasite of".into()]),
            attr: Some(vec!["sex|male|female".into(), "!detected".into()]),
            attr_op: AttrOp::Or,
            country: Some(vec!["United States".into()]),
            state: Some(vec!["New Mexico".into()]),
            from: Some("1958".into()),
            to: Some("2019".into()),
            collector: Some("Cook, J.".into()),
            ..form()
        };
        assert_eq!(
            parse(&f).unwrap().description().unwrap(),
            "Sorex cinereus, or any record with relationship parasite of with sex male or \
             female or no detected recorded from United States in New Mexico collected by \
             Cook, J. collected 1958 to 2019."
        );
    }

    #[test]
    fn the_relationships_named_are_the_rows_own_unless_a_tab_narrows_them() {
        let named = |row: &str| parse(&taxon_form(row)).unwrap().relationships();

        assert_eq!(named("genus|Sorex"), Vec::<String>::new());
        assert_eq!(
            named("genus|Sorex|host of parasite; host of symbiont"),
            ["host of parasite", "host of symbiont"]
        );
        // A row naming only a Related taxon names no relationship: which one
        // reaches that taxon is known only once the relations are walked.
        assert_eq!(named("genus|Sorex||class|Cestoda"), Vec::<String>::new());
        // The same relationship on two rows is named once.
        let f = SearchForm {
            taxon: Some(vec![
                "genus|Sorex|host of parasite".into(),
                "genus|Myodes|host of parasite; parasite of".into(),
            ]),
            ..form()
        };
        assert_eq!(
            parse(&f).unwrap().relationships(),
            ["host of parasite", "parasite of"]
        );
        // A tab narrows every row to itself, so it is the only one.
        let f = SearchForm {
            tab: Some("host of parasite".into()),
            ..taxon_form("genus|Sorex|host of parasite; host of symbiont")
        };
        assert_eq!(parse(&f).unwrap().relationships(), ["host of parasite"]);
    }

    #[test]
    fn the_model_keeps_the_raw_attr_rows_for_the_summary_labels() {
        // The labels are the rows as sent, blanks dropped, in the order sent.
        let f = SearchForm {
            attr: Some(vec!["sex|male".into(), "  ".into(), "!examined for".into()]),
            ..form()
        };
        let search = parse(&f).unwrap();
        assert_eq!(
            search.attrs.iter().map(|a| a.raw.as_str()).collect::<Vec<_>>(),
            ["sex|male", "!examined for"]
        );
        assert!(search.attrs[1].negated);
    }

    #[test]
    fn prefix_only_needs_prefix_set_and_every_other_filter_empty() {
        assert!(!parse(&form()).unwrap().guid_prefix_only(), "no prefix at all");

        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            ..form()
        };
        assert!(parse(&f).unwrap().guid_prefix_only());

        // A blank string in the prefix list, or a blank string field
        // elsewhere, does not count as set.
        let f = SearchForm {
            prefix: Some(vec![" ".into()]),
            ..form()
        };
        assert!(!parse(&f).unwrap().guid_prefix_only());

        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            collector: Some(" ".into()),
            ..form()
        };
        assert!(
            parse(&f).unwrap().guid_prefix_only(),
            "blank collector is still empty"
        );

        // Any other real filter disqualifies the shortcut.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            country: Some(vec!["Mexico".into()]),
            ..form()
        };
        assert!(!parse(&f).unwrap().guid_prefix_only());

        // So does a tab: the rows wanted are then the related records, which
        // the dump is not partitioned by.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            tab: Some("host of parasite".into()),
            ..form()
        };
        let search = parse(&f).unwrap();
        assert!(!search.guid_prefix_only());
        assert_eq!(search.tab.as_deref(), Some("host of parasite"));
        // A blank tab is no tab.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            tab: Some(" ".into()),
            ..form()
        };
        assert!(parse(&f).unwrap().guid_prefix_only());
    }
}
