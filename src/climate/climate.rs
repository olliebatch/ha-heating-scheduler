use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateInfo};
use async_trait::async_trait;

#[async_trait]
pub trait ClimateEntity: Send + Sync {
    fn get_entity_id(&self) -> &str;
    fn get_cached_state(&self) -> &Option<ClimateInfo>;
    fn update_cached_state(&mut self, climate_info: Option<ClimateInfo>);

    fn get_boosted_status(&self) -> &BoostInfo;
    fn set_boost(&mut self, boost: BoostInfo);

    async fn fetch_and_update_state(&mut self, api_client: &ApiClient) -> Result<(), anyhow::Error>;
    async fn turn_on(&self, api_client: &ApiClient) -> Result<(), anyhow::Error>;
    async fn turn_off(&self, api_client: &ApiClient) -> Result<(), anyhow::Error>;
}