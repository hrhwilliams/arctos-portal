use elasticsearch::{Elasticsearch, SearchParts};
use serde_json::{Value, json};

use crate::errors::AppError;

#[derive(Clone)]
pub struct AppState {
    es: Elasticsearch,
}

impl AppState {
    pub fn new(es: Elasticsearch) -> Self {
        Self { es }
    }

    pub async fn search(
        &self,
        attribute_type: String,
        attribute_value: String,
    ) -> Result<(), AppError> {
        let response = self
            .es
            .search(SearchParts::Index(&["arctos"]))
            .from(0)
            .size(10)
            .body(json!({
                "query": {
                    "nested": {
                        "path": "attributedetail",
                        "query": {
                            "bool": {
                                "must": [
                                    {"term": {"attributedetail.attribute_type.keyword": attribute_type}},
                                    {"wildcard": {"attributedetail.attribute_value.keyword": attribute_value}},
                                ]
                            }
                        }
                    }
                }
            }))
            .send()
            .await?;

        let response_body = response.json::<Value>().await?;
        println!("{}", response_body);

        /* example elasticsearch error response:
        {
        "error":{
            "index":"arctos",
            "index_uuid":"_na_",
            "reason":"no such index [arctos]",
            "resource.id":"arctos",
            "resource.type":"index_or_alias",
            "root_cause":[
                {
                    "index":"arctos",
                    "index_uuid":"_na_",
                    "reason":"no such index [arctos]",
                    "resource.id":"arctos",
                    "resource.type":"index_or_alias",
                    "type":"index_not_found_exception"
                }
            ],
            "type":"index_not_found_exception"
        },
        "status":404
        }
         */

        Ok(())
    }
}
