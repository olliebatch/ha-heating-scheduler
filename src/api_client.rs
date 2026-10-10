use crate::climate::ClimateInfo;
use crate::climate::climate_state_api::ClimateState;
use crate::dry_run::{DryRun, is_read};
use anyhow::anyhow;
use reqwest::{Client, Method, Response, Url};
use serde_json::Value;
use std::sync::Arc;

pub struct ApiClient {
    client: Client,
    base_url: Url,
    token: String,
    /// In a dry run, requests that would change Home Assistant are recorded here instead of sent
    dry_run: Option<Arc<DryRun>>,
}

impl ApiClient {
    /// A client for Home Assistant at `base_url`. With `dry_run`, it only reads HA: every other
    /// request is recorded there and answered with an empty 200, as if HA had accepted it.
    /// Required, so no client can be live just because someone forgot to pass it.
    #[must_use]
    pub fn new(base_url: Url, token: String, dry_run: Option<Arc<DryRun>>) -> Self {
        ApiClient {
            client: Client::new(),
            base_url,
            token,
            dry_run,
        }
    }

    pub async fn fetch_climate_state(&self, entity_id: &str) -> Result<ClimateInfo, anyhow::Error> {
        let endpoint = format!("/api/states/{}", entity_id);
        let resp = self
            .get(&endpoint)
            .await
            .map_err(|e| anyhow!(e))?
            .json::<ClimateState>()
            .await?;

        Ok(resp.into())
    }

    pub async fn get(&self, endpoint: &str) -> Result<Response, anyhow::Error> {
        self.send(Method::GET, endpoint, None).await
    }

    pub async fn post(&self, endpoint: &str, body: &Value) -> Result<Response, anyhow::Error> {
        self.send(Method::POST, endpoint, Some(body)).await
    }

    /// Every request to Home Assistant goes through here, so a dry run is cut in one place
    async fn send(
        &self,
        method: Method,
        endpoint: &str,
        body: Option<&Value>,
    ) -> Result<Response, anyhow::Error> {
        if let Some(dry_run) = self
            .dry_run
            .as_ref()
            .filter(|_| !is_read(&method, endpoint))
        {
            dry_run.record(
                &method,
                endpoint,
                body.unwrap_or(&Value::Null),
                chrono::Local::now(),
            );
            return Ok(Response::from(http::Response::new("")));
        }

        let url = self.base_url.join(endpoint).expect("Invalid endpoint");
        let mut request = self
            .client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request.json(body);
        }
        Ok(request.send().await?)
    }
}
