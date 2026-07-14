use std::sync::Arc;

use elasticsearch::{Elasticsearch, SearchParts};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::AppError;

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
    es: Elasticsearch,
    types: Arc<Vec<String>>,
}

impl AppState {
    pub async fn new(es: Elasticsearch) -> Result<Self, AppError> {
        let response = es
            .search(SearchParts::Index(&[INDEX]))
            .size(0)
            .body(json!({
                "aggs": {
                    "nested_attributes": {
                        "nested": {
                            "path": "attributedetail"
                        },
                        "aggs": {
                            "types": {
                                "terms": {
                                    "field": "attributedetail.attribute_type",
                                    "size": 100000
                                }
                            }
                        }
                    }
                }
            }))
            .send()
            .await?
            .error_for_status_code()?;

        let response_body = response.json::<Value>().await?;

        let types = Arc::new(
            response_body
                .get("aggregations")
                .and_then(|v| v.get("nested_attributes"))
                .and_then(|v| v.get("types"))
                .and_then(|v| v.get("buckets"))
                .and_then(|v| v.as_array())
                .ok_or(AppError::JsonMissingValue("buckets array missing".into()))?
                .iter()
                .filter_map(|bucket| bucket.get("key")?.as_str().map(String::from))
                .collect(),
        );

        Ok(Self { es, types })
    }

    pub fn types(&self) -> Arc<Vec<String>> {
        self.types.clone()
    }

    #[tracing::instrument(skip(self))]
    pub async fn search(
        &self,
        scientific_name: Option<String>,
        attribute_type: String,
        attribute_value: String,
    ) -> Result<Vec<SearchResult>, AppError> {
        let response = if let Some(scientific_name) = scientific_name {
            self
            .es
            .search(SearchParts::Index(&[INDEX]))
            .from(0)
            .size(100)
            .sort(&vec!["guid:asc"])
            .body(json!({
                "query": {
                    "bool": {
                        "must": [
                            {
                                "nested": {
                                    "path": "attributedetail",
                                    "query": {
                                        "bool": {
                                            "must": [
                                                {"term": {"attributedetail.attribute_type": attribute_type}},
                                                {"wildcard": {"attributedetail.attribute_value.keyword": format!("*{}*", attribute_value)}},
                                            ]
                                        }
                                    }
                                }
                            },
                            {
                                "wildcard": {
                                    "scientific_name.keyword": format!("*{}*", scientific_name)
                                }
                            }
                        ]
                    }
                }
            }))
            .send()
            .await?
            .error_for_status_code()?
        } else {
            self
            .es
            .search(SearchParts::Index(&[INDEX]))
            .from(0)
            .size(100)
            .sort(&vec!["guid:asc"])
            .body(json!({
                "query": {
                    "nested": {
                        "path": "attributedetail",
                        "query": {
                            "bool": {
                                "must": [
                                    {"term": {"attributedetail.attribute_type": attribute_type}},
                                    {"wildcard": {"attributedetail.attribute_value.keyword": format!("*{}*", attribute_value)}},
                                ]
                            }
                        }
                    }
                }
            }))
            .send()
            .await?
            .error_for_status_code()?
        };

        let response_body = response.json::<Value>().await?;

        let results = response_body
            .get("hits")
            .and_then(|v| v.get("hits"))
            .and_then(|v| v.as_array())
            .ok_or(AppError::JsonMissingValue("hits.hits array missing".into()))?
            .iter()
            .filter_map(|hit| {
                let source = hit["_source"].clone();
                match serde_path_to_error::deserialize::<_, SearchResult>(&source) {
                    Ok(valid) => Some(valid),
                    Err(e) => {
                        eprintln!("Field '{}' failed: {}", e.path(), e.inner());
                        None
                    }
                }
            })
            .collect();

        Ok(results)
    }
}
