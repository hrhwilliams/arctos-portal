use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct SearchForm {
    pub taxon: Option<Vec<String>>,
    pub attr: Option<Vec<String>>,
    pub prefix: Option<Vec<String>>,
    pub country: Option<Vec<String>>,
    pub state: Option<Vec<String>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub attr_op: Option<String>,
    pub locality: Option<String>,
    pub collector: Option<String>,
    pub page: Option<usize>,
}
