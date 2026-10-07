use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

/// How the `attr` rows combine.
#[derive(Deserialize, Debug, Default, PartialEq, Eq, Clone, Copy, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttrOp {
    #[default]
    And,
    Or,
}

/// The shape of a search page: JSON records, or the same page as CSV.
#[derive(Deserialize, Debug, Default, PartialEq, Eq, Clone, Copy, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Json,
    Csv,
}

/// The search form. Every endpoint that answers a search reads these same
/// parameters. A list parameter is repeated, `taxon=a&taxon=b`.
#[derive(Deserialize, Debug, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchForm {
    /// Taxon rows, `rank|name`, `rank|name|relationships`, or that followed by
    /// `rank|name` pairs for Related taxa. Rows OR together. A literal `|` is
    /// written `\|`.
    pub taxon: Option<Vec<String>>,
    /// Attribute rows, `[!]type|value|value…`. A leading `!` negates the row.
    /// A type with no value asks only whether the record carries it.
    pub attr: Option<Vec<String>>,
    /// This field holds part rows, `[!]field|value|value…`, the same grammar as
    /// `attr`. The field names a part field rather than an attribute type,
    /// because a part carries its values in named fields, not in type/value
    /// pairs. Rows always AND; `attr_op` does not reach them.
    pub part: Option<Vec<String>>,
    /// Collection prefixes, `MSB:Mamm`. Values OR together.
    pub prefix: Option<Vec<String>>,
    /// Countries, exact. Values OR together.
    pub country: Option<Vec<String>>,
    /// States or provinces, exact. Values OR together.
    pub state: Option<Vec<String>>,
    /// Earliest event date, `YYYY-MM-DD`. Both ends must hold for one event.
    pub from: Option<String>,
    /// Latest event date, `YYYY-MM-DD`.
    pub to: Option<String>,
    #[serde(default)]
    pub attr_op: AttrOp,
    /// A phrase matched against the record's own locality text.
    pub locality: Option<String>,
    /// An exact collector name.
    pub collector: Option<String>,
    /// This field names the table the user is looking at. Absent means the
    /// Specimens themselves. A value is a relationship, and selects the related
    /// records reached by it — `/api/relations` lists them, and a download
    /// exports them instead of the Specimens.
    pub tab: Option<String>,
    /// The page number, from 1. The page size is fixed; `/api/schema` reports it.
    pub page: Option<usize>,
    #[serde(default)]
    pub format: Format,
    /// This field applies to the download only. It holds a comma-separated
    /// list of dump columns to export. The system checks each column name
    /// against the schema's column list. When this field is absent, the
    /// system uses the default column set.
    pub cols: Option<String>,
}

impl SearchForm {
    /// This function reports whether the form asks for a related-records table
    /// rather than the Specimens.
    #[must_use]
    pub fn tab(&self) -> Option<&str> {
        self.tab.as_deref().map(str::trim).filter(|t| !t.is_empty())
    }
}
