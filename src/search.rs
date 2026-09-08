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
    /// This field holds part rows, `[!]field|value|value…`, the same grammar as
    /// `attr`. The field names a part field rather than an attribute type,
    /// because a part carries its values in named fields, not in type/value
    /// pairs. Rows always AND; `attr_op` does not reach them.
    pub part: Option<Vec<String>>,
    pub prefix: Option<Vec<String>>,
    pub country: Option<Vec<String>>,
    pub state: Option<Vec<String>>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    pub attr_op: AttrOp,
    pub locality: Option<String>,
    pub collector: Option<String>,
    /// This field names the table the user is looking at. Absent means the
    /// Specimens themselves. A value is a relationship, and selects the related
    /// records reached by it — `/api/relations` lists them, and a download
    /// exports them instead of the Specimens.
    pub tab: Option<String>,
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

    /// This function reports whether `prefix` is the only filter the form
    /// sets. The caller can then read guids straight from the dump instead
    /// of asking Elasticsearch, because the dump is already partitioned by
    /// `guid_prefix`.
    ///
    /// A `tab` disqualifies the shortcut whatever else the form holds: the rows
    /// wanted are then the related records, which the dump is not partitioned by.
    #[must_use]
    pub fn guid_prefix_only(&self) -> bool {
        let non_empty = |v: &[String]| v.iter().any(|s| !s.trim().is_empty());
        let vec_empty = |v: &Option<Vec<String>>| v.as_deref().is_none_or(|v| !non_empty(v));
        let str_empty = |v: &Option<String>| v.as_ref().is_none_or(|s| s.trim().is_empty());

        self.tab().is_none()
            && self.prefix.as_deref().is_some_and(non_empty)
            && vec_empty(&self.taxon)
            && vec_empty(&self.attr)
            && vec_empty(&self.part)
            && vec_empty(&self.country)
            && vec_empty(&self.state)
            && str_empty(&self.from)
            && str_empty(&self.to)
            && str_empty(&self.locality)
            && str_empty(&self.collector)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
            attr_op: AttrOp::default(),
            locality: None,
            collector: None,
            tab: None,
            page: None,
            format: Format::default(),
            cols: None,
        }
    }

    #[test]
    fn prefix_only_needs_prefix_set_and_every_other_filter_empty() {
        assert!(!form().guid_prefix_only(), "no prefix at all");

        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            ..form()
        };
        assert!(f.guid_prefix_only());

        // A blank string in the prefix list, or a blank string field
        // elsewhere, does not count as set.
        let f = SearchForm {
            prefix: Some(vec![" ".into()]),
            ..form()
        };
        assert!(!f.guid_prefix_only());

        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            collector: Some(" ".into()),
            ..form()
        };
        assert!(f.guid_prefix_only(), "blank collector is still empty");

        // Any other real filter disqualifies the shortcut.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            country: Some(vec!["Mexico".into()]),
            ..form()
        };
        assert!(!f.guid_prefix_only());

        // So does a tab: the rows wanted are then the related records, which
        // the dump is not partitioned by.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            tab: Some("host of parasite".into()),
            ..form()
        };
        assert!(!f.guid_prefix_only());
        assert_eq!(f.tab(), Some("host of parasite"));
        // A blank tab is no tab.
        let f = SearchForm {
            prefix: Some(vec!["MSB:Mamm".into()]),
            tab: Some(" ".into()),
            ..form()
        };
        assert!(f.guid_prefix_only());
    }
}
