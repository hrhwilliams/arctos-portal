use reqwest::header::CONTENT_TYPE;
use serde_json::Value;

use crate::{errors::AppError, search::SearchForm, translate::translate};

/// Cheap to clone: `reqwest::Client` is `Arc`-backed, so every handler shares
/// the one connection pool.
#[derive(Clone)]
pub struct AppState {
    elasticsearch_url: String,
    client: reqwest::Client,
}

impl AppState {
    #[must_use]
    pub fn new(elasticsearch_url: &str) -> Self {
        Self {
            elasticsearch_url: elasticsearch_url.into(),
            client: reqwest::Client::new(),
        }
    }

    /// # Errors
    ///
    /// Returns an error if the Elasticsearch request fails or returns a
    /// non-success status.
    #[tracing::instrument(skip(self))]
    pub async fn search(&self, search_form: SearchForm) -> Result<Value, AppError> {
        let query = translate(&search_form);
        tracing::info!("{query:#}");

        let response = self
            .client
            .post(format!("{}/arctos/_search", self.elasticsearch_url))
            .header(CONTENT_TYPE, "application/json")
            .json(&query)
            .send()
            .await?
            .error_for_status()?;

        let response_body = response.json().await?;

        Ok(response_body)
    }
}
