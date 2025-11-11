use crate::climate::climate_state_api::ClimateState;
use crate::climate::ClimateInfo;
use anyhow::anyhow;
use reqwest::{Client, RequestBuilder, Url};

pub struct ApiClient {
    client: Client,
    base_url: Url,
    token: String,
}

impl ApiClient {
    pub fn new(base_url: Url, token: String) -> Self {
        ApiClient {
            client: Client::new(),
            base_url,
            token,
        }
    }

    pub async fn fetch_climate_state(&self, entity_id: &str) -> Result<ClimateInfo, anyhow::Error> {
        let endpoint = format!("/api/states/{}", entity_id);
        let resp = self
            .get(&endpoint)
            .send()
            .await
            .map_err(|e| anyhow!(e))?
            .json::<ClimateState>()
            .await?;

        Ok(resp.into())
    }

    pub fn get(&self, endpoint: &str) -> RequestBuilder {
        let url = self.base_url.join(endpoint).expect("Invalid endpoint");
        self.client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token))
    }

    pub fn post(&self, endpoint: &str) -> RequestBuilder {
        let url = self.base_url.join(endpoint).expect("Invalid endpoint");
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
    }
}
