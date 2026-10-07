//! One CSV writer for every row the service renders itself.
//!
//! The search page's CSV form and the index-side export both go through here.
//! The `DuckDB` export does not: `COPY ... (FORMAT csv)` quotes its own rows,
//! and the guid file it joins against is tab-separated on purpose.
//!
//! A [`Csv`] is a column list. It renders a header and, for each record as
//! `_source` holds it, one line: a string is itself, a nested value keeps its
//! JSON text, a missing value is an empty cell. One column is virtual:
//! [`RELATED_GUIDS`] is computed from the record's `relations` rather than
//! read from it, because the dump has no plain column for the backlinks.

use std::collections::BTreeSet;

use serde_json::Value;

/// The backlink column: the distinct guids of every record the row is related
/// to, sorted and `"; "`-joined. Ask for it by name like any other column.
pub const RELATED_GUIDS: &str = "related_guids";

/// The shape of one CSV: an optional lead column, then the named columns.
///
/// The lead is for a related-records export, where each row starts with the
/// specimens it was reached from. That value is not on the record, so the
/// caller supplies it per line through [`Csv::line_with`].
#[derive(Debug, Clone)]
pub struct Csv {
    lead: Option<String>,
    columns: Vec<String>,
}

impl Csv {
    #[must_use]
    pub const fn new(columns: Vec<String>) -> Self {
        Self {
            lead: None,
            columns,
        }
    }

    /// This function adds a lead column, filled per line by [`Csv::line_with`]
    /// and left empty by [`Csv::line`].
    #[must_use]
    pub fn with_lead(mut self, name: &str) -> Self {
        self.lead = Some(name.to_owned());
        self
    }

    /// The header line, newline-terminated.
    #[must_use]
    pub fn header(&self) -> String {
        let names = self
            .lead
            .iter()
            .chain(self.columns.iter())
            .map(|c| escape(c))
            .collect::<Vec<_>>()
            .join(",");
        format!("{names}\n")
    }

    /// One line for `record`, newline-terminated. A configured lead column is
    /// left empty.
    #[must_use]
    pub fn line(&self, record: &Value) -> String {
        self.render(self.lead.as_ref().map(|_| ""), record)
    }

    /// One line for `record` with `lead` in the lead column.
    #[must_use]
    pub fn line_with(&self, lead: &str, record: &Value) -> String {
        self.render(Some(lead), record)
    }

    fn render(&self, lead: Option<&str>, record: &Value) -> String {
        let cells = lead
            .map(escape)
            .into_iter()
            .chain(self.columns.iter().map(|c| {
                if c == RELATED_GUIDS {
                    escape(&backlinks(record))
                } else {
                    cell(&record[c.as_str()])
                }
            }))
            .collect::<Vec<_>>()
            .join(",");
        format!("{cells}\n")
    }
}

/// This function follows RFC 4180. It wraps the value in quotes when the
/// value has a delimiter. It doubles each quote inside the value.
fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// One cell from one `_source` value.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => escape(s),
        other => escape(&other.to_string()),
    }
}

/// The distinct `related_guid` of a record's `relations`, sorted.
fn backlinks(record: &Value) -> String {
    record
        .get("relations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("related_guid").and_then(Value::as_str))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_row_keeps_dump_columns_and_computes_the_backlinks() {
        let record = json!({
            "guid": "MSB:Mamm:1",
            "scientific_name": "Sorex \"cinereus\", sensu lato",
            "attributedetail": [{ "attribute_type": "sex" }],
            "relations": [
                { "relationship": "host of parasite", "related_guid": "MSB:Para:9" },
                { "relationship": "host of parasite", "related_guid": "DMNS:Para:2" },
                // The same record twice, and one that points outside Arctos.
                { "relationship": "sibling of", "related_guid": "MSB:Para:9" },
                { "relationship": "associated with", "related_identifier": "NK 1" }
            ]
        });
        let csv = Csv::new(columns(&[
            "guid",
            "scientific_name",
            "attributedetail",
            "nope",
            RELATED_GUIDS,
        ]));

        assert_eq!(csv.header(), "guid,scientific_name,attributedetail,nope,related_guids\n");
        // A plain value has no quotes, a quote forces them and doubles, a nested
        // value stays JSON, a column the record lacks is an empty cell, and the
        // backlinks are distinct and sorted.
        assert_eq!(
            csv.line(&record),
            "MSB:Mamm:1,\"Sorex \"\"cinereus\"\", sensu lato\",\
             \"[{\"\"attribute_type\"\":\"\"sex\"\"}]\",,DMNS:Para:2; MSB:Para:9\n"
        );
        // No relations at all is an empty backlink cell, not a missing one.
        assert_eq!(
            Csv::new(columns(&["guid", RELATED_GUIDS])).line(&json!({ "guid": "x" })),
            "x,\n"
        );
    }

    #[test]
    fn the_lead_column_is_first_and_empty_unless_supplied() {
        let csv = Csv::new(columns(&["guid"])).with_lead("related_guid");
        let record = json!({ "guid": "DMNS:Para:113" });

        assert_eq!(csv.header(), "related_guid,guid\n");
        assert_eq!(
            csv.line_with("MSB:Host:9; MSB:Mamm:9", &record),
            "MSB:Host:9; MSB:Mamm:9,DMNS:Para:113\n"
        );
        // The column count never changes: a line without a lead still has the cell.
        assert_eq!(csv.line(&record), ",DMNS:Para:113\n");
    }

    #[test]
    fn a_hostile_value_survives() {
        let csv = Csv::new(columns(&["a"]));
        for (value, expected) in [
            ("plain", "plain\n"),
            ("with,comma", "\"with,comma\"\n"),
            ("with \"quote\"", "\"with \"\"quote\"\"\"\n"),
            ("line\nbreak", "\"line\nbreak\"\n"),
            ("", "\n"),
        ] {
            assert_eq!(csv.line(&json!({ "a": value })), expected, "{value:?}");
        }
        // A number is rendered, not quoted.
        assert_eq!(csv.line(&json!({ "a": 12 })), "12\n");
        assert_eq!(csv.line(&json!({ "a": null })), "\n");
    }
}
