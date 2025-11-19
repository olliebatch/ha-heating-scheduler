use crate::api_client::ApiClient;
use crate::climate::climate_state_api::{ApiHeatingState, ClimateState as ApiClimateState};
use crate::schedule::HeatingState;
use anyhow::anyhow;
use chrono::NaiveTime;

pub mod climate_state_api;
pub mod climate;

pub use climate::ClimateEntity;

#[derive(Debug, Clone)]
pub struct ClimateInfo {
    pub current_temperature: f64,
    pub state: HeatingState,
}


#[derive(Debug, Default, Clone)]
pub struct BoostInfo {
    pub boosted: bool,
    pub boost_start: Option<NaiveTime>,
    pub boost_end: Option<NaiveTime>,
}

/// Wrapper enum to allow using either Mock or Real climate entities
#[derive(Debug, Clone)]
pub enum ClimateEntityWrapper {
    Mock(MockClimate),
    Real(DefaultClimate),
}

#[async_trait::async_trait]
impl ClimateEntity for ClimateEntityWrapper {
    fn get_entity_id(&self) -> &str {
        match self {
            ClimateEntityWrapper::Mock(m) => m.get_entity_id(),
            ClimateEntityWrapper::Real(r) => r.get_entity_id(),
        }
    }

    fn get_cached_state(&self) -> &Option<ClimateInfo> {
        match self {
            ClimateEntityWrapper::Mock(m) => m.get_cached_state(),
            ClimateEntityWrapper::Real(r) => r.get_cached_state(),
        }
    }

    fn update_cached_state(&mut self, climate_info: Option<ClimateInfo>) {
        match self {
            ClimateEntityWrapper::Mock(m) => m.update_cached_state(climate_info),
            ClimateEntityWrapper::Real(r) => r.update_cached_state(climate_info),
        }
    }

    fn get_boosted_status(&self) -> &BoostInfo {
        match self {
            ClimateEntityWrapper::Mock(m) => m.get_boosted_status(),
            ClimateEntityWrapper::Real(r) => r.get_boosted_status(),
        }
    }
    fn set_boost(&mut self, boost: BoostInfo) {
        match self {
            ClimateEntityWrapper::Mock(m) => m.set_boost(boost),
            ClimateEntityWrapper::Real(r) => r.set_boost(boost),
        }
    }

    async fn fetch_and_update_state(&mut self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        match self {
            ClimateEntityWrapper::Mock(m) => m.fetch_and_update_state(api_client).await,
            ClimateEntityWrapper::Real(r) => r.fetch_and_update_state(api_client).await,
        }
    }

    async fn turn_on(&self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        match self {
            ClimateEntityWrapper::Mock(m) => m.turn_on(api_client).await,
            ClimateEntityWrapper::Real(r) => r.turn_on(api_client).await,
        }
    }

    async fn turn_off(&self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        match self {
            ClimateEntityWrapper::Mock(m) => m.turn_off(api_client).await,
            ClimateEntityWrapper::Real(r) => r.turn_off(api_client).await,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DefaultClimate {
    pub entity_id: String,
    pub info: Option<ClimateInfo>,
    pub boosted: BoostInfo,
}

impl DefaultClimate {
    pub fn new(entity_id: String) -> Self {
        DefaultClimate {
            entity_id,
            info: None,
            boosted: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl ClimateEntity for DefaultClimate {
    fn get_entity_id(&self) -> &str {
        &self.entity_id
    }

    fn get_cached_state(&self) -> &Option<ClimateInfo> {
        &self.info
    }

    fn update_cached_state(&mut self, climate_info: Option<ClimateInfo>) {
        self.info = climate_info;
    }

    fn get_boosted_status(&self) -> &BoostInfo {
        &self.boosted
    }
    fn set_boost(&mut self, boost: BoostInfo) {
        self.boosted = boost;
    }

    async fn fetch_and_update_state(&mut self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        // Actually calls the Home Assistant API
        let climate_info = api_client.fetch_climate_state(&self.entity_id).await?;
        self.info = Some(climate_info);
        Ok(())
    }

    async fn turn_on(&self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        println!("  → Turning ON: {}", self.entity_id);
        let body = serde_json::json!({
            "entity_id": self.entity_id,
            "hvac_mode": "heat"
        });

        api_client
            .post("/api/services/climate/set_hvac_mode")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!(e))?;

        Ok(())
    }

    async fn turn_off(&self, api_client: &ApiClient) -> Result<(), anyhow::Error> {
        println!("  → Turning OFF: {}", self.entity_id);
        let body = serde_json::json!({
            "entity_id": self.entity_id,
            "hvac_mode": "off"
        });

        api_client
            .post("/api/services/climate/set_hvac_mode")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!(e))?;

        Ok(())
    }
}


#[derive(Debug, Clone)]
pub struct MockClimate {
    pub entity_id: String,
    pub info: Option<ClimateInfo>,
    pub boosted: BoostInfo,
}

impl MockClimate {
    pub fn new(entity_id: String, initial_state: HeatingState) -> Self {
        MockClimate {
            entity_id,
            info: Some(ClimateInfo {
                current_temperature: 20.0,
                state: initial_state,
            }),
            boosted: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl ClimateEntity for MockClimate {
    fn get_entity_id(&self) -> &str {
        &self.entity_id
    }

    fn get_cached_state(&self) -> &Option<ClimateInfo> {
        &self.info
    }

    fn update_cached_state(&mut self, climate_info: Option<ClimateInfo>) {
        self.info = climate_info;
    }

    fn get_boosted_status(&self) -> &BoostInfo {
        &self.boosted
    }
    fn set_boost(&mut self, boost: BoostInfo) {
        self.boosted = boost
    }

    async fn fetch_and_update_state(&mut self, _api_client: &ApiClient) -> Result<(), anyhow::Error> {
        // Mock: doesn't call API, just returns success
        println!("[MOCK] Fetching state for {} (no API call)", self.entity_id);
        // Optionally update with mock data
        self.info = Some(ClimateInfo {
            current_temperature: 21.0,
            state: self.info.as_ref().map(|i| i.state.clone()).unwrap_or(HeatingState::Off),
        });
        Ok(())
    }

    async fn turn_on(&self, _api_client: &ApiClient) -> Result<(), anyhow::Error> {
        println!("[MOCK] Turning ON: {}", self.entity_id);
        // In a real mock, you might update internal state here
        Ok(())
    }

    async fn turn_off(&self, _api_client: &ApiClient) -> Result<(), anyhow::Error> {
        println!("[MOCK] Turning OFF: {}", self.entity_id);
        // In a real mock, you might update internal state here
        Ok(())
    }
}

// ============================================================================
// Conversion from API response to ClimateInfo
// ============================================================================

impl From<ApiClimateState> for ClimateInfo {
    fn from(state: ApiClimateState) -> Self {
        ClimateInfo {
            current_temperature: state.attributes.current_temperature,
            state: match state.state {
                ApiHeatingState::Off => HeatingState::Off,
                ApiHeatingState::Heat => HeatingState::On,
            },
        }
    }
}


pub async fn get_initial_states(inital_strings: Vec<String>) -> Result<Vec<DefaultClimate>, anyhow::Error> {
    let mut initial: Vec<DefaultClimate> = Vec::new();
    for string in inital_strings {
        let mut new_climate = DefaultClimate::new(string.as_str().to_string());
        new_climate.update_cached_state(None);
        initial.push(new_climate);
    }
    Ok(initial)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_climate() {
        let mut mock = MockClimate::new("climate.test".to_string(), HeatingState::Off);

        // No real API client needed for mock
        let fake_client = ApiClient::new(
            reqwest::Url::parse("http://fake").unwrap(),
            "fake_token".to_string(),
        );

        // Test fetch doesn't actually call API
        mock.fetch_and_update_state(&fake_client).await.unwrap();
        assert!(mock.get_cached_state().is_some());

        // Test controls don't call API
        mock.turn_on(&fake_client).await.unwrap();
        mock.turn_off(&fake_client).await.unwrap();
    }
}
