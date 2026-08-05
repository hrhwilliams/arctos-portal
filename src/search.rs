use serde::Deserialize;

#[derive(Deserialize, Debug, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum AttrOp {
    #[default]
    And,
    Or,
}

#[derive(Deserialize, Debug, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Json,
    Csv,
}

#[derive(Deserialize, Debug)]
pub struct SearchForm {
    pub taxon: Option<Vec<String>>,
    pub attr: Option<Vec<String>>,
    pub prefix: Option<Vec<String>>,
    pub country: Option<Vec<String>>,
    pub state: Option<Vec<String>>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    pub attr_op: AttrOp,
    pub locality: Option<String>,
    pub collector: Option<String>,
    pub page: Option<usize>,
    #[serde(default)]
    pub format: Format,
    /// This field applies to the download only. It holds a comma-separated
    /// list of dump columns to export. The system checks each column name
    /// against the schema's column list. When this field is absent, the
    /// system uses the default column set.
    pub cols: Option<String>,
}
