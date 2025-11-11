use serde::{Deserialize, Serialize};
use serde_json::Value;


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiHeatingState {
    Off,
    Heat,
}

impl Default for ApiHeatingState {
    fn default() -> Self {
        ApiHeatingState::Off
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClimateState {
    #[serde(rename = "entity_id")]
    pub entity_id: String,
    pub state: ApiHeatingState,
    pub attributes: Attributes,
    #[serde(rename = "last_changed")]
    pub last_changed: String,
    #[serde(rename = "last_reported")]
    pub last_reported: String,
    #[serde(rename = "last_updated")]
    pub last_updated: String,
    pub context: Context,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attributes {
    #[serde(rename = "hvac_modes")]
    pub hvac_modes: Vec<String>,
    #[serde(rename = "min_temp")]
    pub min_temp: f64,
    #[serde(rename = "max_temp")]
    pub max_temp: f64,
    #[serde(rename = "current_temperature")]
    pub current_temperature: f64,
    pub temperature: Value,
    #[serde(rename = "friendly_name")]
    pub friendly_name: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub id: String,
    #[serde(rename = "parent_id")]
    pub parent_id: Value,
    #[serde(rename = "user_id")]
    pub user_id: Value,
}
