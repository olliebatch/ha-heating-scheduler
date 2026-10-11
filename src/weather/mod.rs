use crate::api_client::ApiClient;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

pub mod adjust;
pub mod early;
pub mod late;
pub mod warmup;

/// Outside conditions, in °C, km/h and percent. Any of them can be missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Weather {
    pub temperature: Option<f64>,
    pub wind_speed: Option<f64>,
    pub cloud_coverage: Option<f64>,
}

/// Where the scheduler reads the weather from
#[async_trait::async_trait]
pub trait WeatherSource: Send + Sync {
    /// The current weather, or None when no weather entity is configured
    async fn fetch(&self, entity_id: Option<&str>) -> Result<Option<Weather>>;
}

/// Reads a Home Assistant weather entity (`GET /api/states/<entity>`)
pub struct HomeAssistantWeather {
    pub api_client: ApiClient,
}

#[async_trait::async_trait]
impl WeatherSource for HomeAssistantWeather {
    async fn fetch(&self, entity_id: Option<&str>) -> Result<Option<Weather>> {
        let Some(entity_id) = entity_id else {
            return Ok(None);
        };
        let state: Value = self
            .api_client
            .get(&format!("/api/states/{}", entity_id))
            .await?
            .error_for_status()?
            .json()
            .await?;
        parse_weather_state(&state).map(Some)
    }
}

/// Read a weather entity's state. Uses the standard attributes `temperature`, `wind_speed`
/// and `cloud_coverage`, converting from `temperature_unit` (°C or °F) and `wind_speed_unit`
/// (km/h, m/s, mph or kn). Without `cloud_coverage`, the condition (`sunny`, `cloudy`, ...)
/// gives a rough cloud cover.
pub fn parse_weather_state(state: &Value) -> Result<Weather> {
    let attributes = state
        .get("attributes")
        .ok_or_else(|| anyhow!("Weather state has no attributes"))?;
    let number = |key: &str| attributes.get(key).and_then(Value::as_f64);
    let unit = |key: &str| attributes.get(key).and_then(Value::as_str).unwrap_or("");

    let temperature = number("temperature").map(|t| match unit("temperature_unit") {
        "°F" => (t - 32.0) * 5.0 / 9.0,
        _ => t,
    });
    let wind_speed = number("wind_speed").map(|w| match unit("wind_speed_unit") {
        "m/s" => w * 3.6,
        "mph" => w * 1.609_344,
        "kn" => w * 1.852,
        _ => w,
    });
    let cloud_coverage =
        number("cloud_coverage").or_else(|| match state.get("state").and_then(Value::as_str)? {
            "sunny" | "clear-night" => Some(0.0),
            "partlycloudy" => Some(50.0),
            "cloudy" | "fog" | "rainy" | "pouring" | "snowy" | "snowy-rainy" | "hail"
            | "lightning" | "lightning-rainy" => Some(100.0),
            _ => None,
        });

    Ok(Weather {
        temperature,
        wind_speed,
        cloud_coverage,
    })
}

/// Weather set by hand, for tests and debug mode (ignores the entity id)
#[derive(Default)]
pub struct MockWeather {
    pub weather: Arc<RwLock<Weather>>,
}

#[async_trait::async_trait]
impl WeatherSource for MockWeather {
    async fn fetch(&self, _entity_id: Option<&str>) -> Result<Option<Weather>> {
        Ok(Some(self.weather.read().unwrap().clone()))
    }
}

/// The weather entity to read, persisted to weather.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeatherConfig {
    pub entity_id: Option<String>,
}

/// The configured entity and the last reading, shared by the scheduler and the server
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeatherStatus {
    pub entity_id: Option<String>,
    /// The last good reading, kept for up to [`KEEP_READING_FOR`] while reads fail; None when
    /// nothing is configured or the last good reading is too old
    pub weather: Option<Weather>,
    /// When `weather` was read
    pub read_at: Option<DateTime<Local>>,
    /// Why the last read failed, if it did
    pub error: Option<String>,
    /// The adjusted target each zone is held at, by zone id (see `scheduler::zone_status`)
    #[serde(skip)]
    pub held: HashMap<uuid::Uuid, Held>,
    /// Early starts and late finishes in progress, by zone id, kept until they're over
    #[serde(skip)]
    pub begun: HashMap<uuid::Uuid, Begun>,
}

/// What a zone has begun and carries on with between ticks, whatever the weather does
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Begun {
    pub early_start: Option<early::EarlyStart>,
    pub late_finish: Option<late::LateFinish>,
    pub warm_up: Option<warmup::WarmUp>,
}

/// A zone's adjusted target, held until the weather moves it clearly
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Held {
    /// The scheduled target it was adjusted from
    pub scheduled: f64,
    pub target: f64,
}

/// How long to keep using the last good reading while reads fail
pub const KEEP_READING_FOR: chrono::TimeDelta = chrono::TimeDelta::minutes(30);

pub fn load_weather_config<P: AsRef<Path>>(path: P) -> Result<WeatherConfig> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(WeatherConfig::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read weather config: {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse weather config from: {}", path.display()))
}

pub fn save_weather_config<P: AsRef<Path>>(config: &WeatherConfig, path: P) -> Result<()> {
    let path = path.as_ref();
    let json =
        serde_json::to_string_pretty(config).context("Failed to serialize weather config")?;
    fs::write(path, json)
        .with_context(|| format!("Failed to write weather config: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_metric_weather() {
        let state = json!({
            "state": "partlycloudy",
            "attributes": {
                "temperature": 3.5, "temperature_unit": "°C",
                "wind_speed": 30.0, "wind_speed_unit": "km/h",
                "cloud_coverage": 40
            }
        });

        let weather = parse_weather_state(&state).unwrap();

        assert_eq!(
            weather,
            Weather {
                temperature: Some(3.5),
                wind_speed: Some(30.0),
                cloud_coverage: Some(40.0),
            }
        );
    }

    #[test]
    fn test_parse_converts_units() {
        let state = json!({
            "state": "sunny",
            "attributes": {
                "temperature": 41.0, "temperature_unit": "°F",
                "wind_speed": 10.0, "wind_speed_unit": "m/s"
            }
        });

        let weather = parse_weather_state(&state).unwrap();

        assert!((weather.temperature.unwrap() - 5.0).abs() < 1e-9);
        assert!((weather.wind_speed.unwrap() - 36.0).abs() < 1e-9);
        assert_eq!(weather.cloud_coverage, Some(0.0), "from the condition");
    }

    #[test]
    fn test_parse_handles_missing_values() {
        let state = json!({"state": "exceptional", "attributes": {}});
        assert_eq!(parse_weather_state(&state).unwrap(), Weather::default());

        assert!(parse_weather_state(&json!({"state": "sunny"})).is_err());
    }

    #[test]
    fn test_weather_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("weather.json");
        assert_eq!(load_weather_config(&path).unwrap().entity_id, None);

        let config = WeatherConfig {
            entity_id: Some("weather.example".to_string()),
        };
        save_weather_config(&config, &path).unwrap();

        assert_eq!(
            load_weather_config(&path).unwrap().entity_id.as_deref(),
            Some("weather.example")
        );
    }
}
