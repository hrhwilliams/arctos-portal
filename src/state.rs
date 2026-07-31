use std::sync::Arc;

use elasticsearch::{Elasticsearch, SearchParts};
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{errors::AppError, search::SearchForm, translate::translate};

/*
types_values_agg = es.search(
    index="arctos_test",
    size=0,
    aggs={
        "nested_attributes": {
            "nested": {
                "path": "attributedetail"
            },
            "aggs": {
                "types": {
                    "terms": {
                        "field": "attributedetail.attribute_type.keyword",
                        "size": 100000
                    }
                },
                "values": {
                    "terms": {
                        "field": "attributedetail.attribute_value.keyword",
                        "size": 100000,
                        "min_doc_count": 20}
                }
            }
        }
    }
)["aggregations"]["nested_attributes"]
*/

const INDEX: &str = "arctos";

#[derive(Deserialize)]
pub struct PartDetail {
    #[serde(rename = "partID")]
    pub part_id: String,
    pub part_name: String,
    pub condition: Option<String>,
    pub disposition: Option<String>,
    pub part_remark: Option<String>,
    pub part_barcode: Option<String>,
    pub part_attributes: Option<Vec<PartAttributes>>,
}

#[derive(Deserialize)]
pub struct AttributeDetail {
    pub attribute_type: String,
    pub attribute_value: String,
    pub attribute_remark: Option<String>,
    pub attribute_date: Option<String>,
}

#[derive(Deserialize)]
pub struct PartAttributes {
    pub attribute_type: String,
    pub attribute_value: String,
    pub attribute_remark: Option<String>,
    pub attribute_date: Option<String>,
}

#[derive(Deserialize)]
pub struct SearchResult {
    pub guid: String,
    pub species: String,
    pub relatedcatalogeditems: Option<String>,
    pub dec_lat: Option<String>,
    pub dec_long: Option<String>,
    pub spec_locality: Option<String>,
    pub partdetail: Option<Vec<PartDetail>>,
    pub attributedetail: Option<Vec<AttributeDetail>>,
}

#[derive(Clone)]
pub struct AppState {
    elasticsearch_url: String,
}

impl AppState {
    pub fn new(elasticsearch_url: &str) -> Self {
        Self {
            elasticsearch_url: elasticsearch_url.into(),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn search(&self, search_form: SearchForm) -> Result<String, AppError> {
        let query = translate(&search_form);
        tracing::info!("{}", serde_json::to_string_pretty(&query).unwrap());

        let client = reqwest::Client::new();

        let response = client
            .post(format!("{}/arctos/_search", self.elasticsearch_url))
            .header(CONTENT_TYPE, "application/json")
            .json(&query)
            .send()
            .await?
            .error_for_status()?;

        let response_body = response.text().await?;

        Ok(response_body)
    }
}
