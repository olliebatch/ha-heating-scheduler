use crate::api_client::ApiClient;
use crate::climate::climate_state_api::ClimateState;
use anyhow::anyhow;

mod climate_state_api;

#[derive(Debug)]
pub struct ClimateEntity {
    pub entity_id: String,
    pub info: Option<ClimateInfo>,
}

#[derive(Debug)]
pub struct ClimateInfo {
    pub current_temperature: f64,
    pub hvac_mode: String,
    pub state: String,
}

impl ClimateEntity {
    pub fn new(entity_id: String) -> Self {
        ClimateEntity {
            entity_id,
            info: None,
        }
    }

    async fn fetch(&self, api_client: &ApiClient) -> Result<ClimateInfo, anyhow::Error> {
        let endpoint = format!("/api/states/{}", self.entity_id);
        let resp = api_client
            .get(&endpoint)
            .send()
            .await
            .map_err(|e| anyhow!(e))?
            .json::<ClimateState>()
            .await?;

        Ok(resp.into())
    }

    pub async fn get_state(
        &mut self,
        api_client: &ApiClient,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.info = Some(self.fetch(api_client).await?);
        Ok(())
    }
}

impl From<ClimateState> for ClimateInfo {
    fn from(state: ClimateState) -> Self {
        ClimateInfo {
            current_temperature: state.attributes.current_temperature,
            hvac_mode: state.attributes.system_mode,
            state: state.state,
        }
    }
}
