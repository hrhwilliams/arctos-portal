//! The rank table: the one list of taxonomic ranks and detection types every
//! part of the system reads.
//!
//! `src/ranks.json` is the source. `ingest.py` reads the same file, so what
//! the index carries and what the service asks it for cannot drift. The file
//! is compiled in, so it travels with the binary the way the mapper config
//! does.
//!
//! `scientific_name` is not in the table: it is a sentinel for a non-rank, and
//! the code that offers it handles it where it is used.

use std::sync::LazyLock;

use serde::Deserialize;

/// One rank: the id the form sends, the label it shows, and the dump column
/// the index splits.
#[derive(Deserialize, Debug)]
pub struct Rank<'a> {
    #[serde(borrow)]
    pub id: &'a str,
    #[serde(borrow)]
    pub label: &'a str,
    #[serde(borrow)]
    pub column: &'a str,
}

/// One detection attribute type and the flat index field it rolls up into.
#[derive(Deserialize, Debug)]
pub struct Detection<'a> {
    #[serde(borrow, rename = "type")]
    pub attribute_type: &'a str,
    #[serde(borrow)]
    pub field: &'a str,
}

#[derive(Deserialize, Debug, Default)]
struct Table<'a> {
    #[serde(borrow)]
    ranks: Vec<Rank<'a>>,
    #[serde(borrow)]
    detections: Vec<Detection<'a>>,
}

const JSON: &str = include_str!("ranks.json");

/// The parsed table. A malformed file parses as an empty table, which the test
/// below turns into a failed build rather than an empty form.
static TABLE: LazyLock<Table<'static>> =
    LazyLock::new(|| serde_json::from_str(JSON).unwrap_or_default());

/// The ranks, highest first. This order is the form's and the rank chain's.
#[must_use]
pub fn ranks() -> &'static [Rank<'static>] {
    &TABLE.ranks
}

/// The detection types and their flat fields.
#[must_use]
pub fn detections() -> &'static [Detection<'static>] {
    &TABLE.detections
}

/// The dump column a rank id names, `phylclass` for `class`. `None` for an id
/// that is not a rank.
#[must_use]
pub fn column(id: &str) -> Option<&'static str> {
    ranks().iter().find(|r| r.id == id).map(|r| r.column)
}

/// The flat index field a detection type rolls up into. `None` for an
/// attribute type that is not a detection.
#[must_use]
pub fn detection_field(attribute_type: &str) -> Option<&'static str> {
    detections()
        .iter()
        .find(|d| d.attribute_type == attribute_type)
        .map(|d| d.field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_compiles_in_and_reads_back() {
        // A file that failed to parse would read back empty. This is the
        // check that turns that into a failed build.
        assert_eq!(ranks().len(), 7, "{JSON}");
        assert_eq!(detections().len(), 4);

        // The two columns whose names differ from their rank, and the chain
        // order the ingest relies on.
        assert_eq!(column("class"), Some("phylclass"));
        assert_eq!(column("order"), Some("phylorder"));
        assert_eq!(column("genus"), Some("genus"));
        assert_eq!(ranks().first().map(|r| r.id), Some("phylum"));
        assert_eq!(ranks().last().map(|r| r.id), Some("species"));
        // Not ranks: the sentinel, and nonsense.
        assert_eq!(column("scientific_name"), None);
        assert_eq!(column("nope"), None);

        assert_eq!(detection_field("not examined for"), Some("not_examined_for"));
        assert_eq!(detection_field("sex"), None);

        // Ids are distinct, or the form would offer one rank twice.
        let mut ids: Vec<&str> = ranks().iter().map(|r| r.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), ranks().len());
    }
}
