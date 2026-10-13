use crate::api_client::ApiClient;
use crate::schedule::TimePeriod;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Timelike, Utc};
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

/// Outside conditions, in °C, km/h, degrees and percent. Any of them can be missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Weather {
    pub temperature: Option<f64>,
    pub wind_speed: Option<f64>,
    /// The direction the wind blows from, in degrees clockwise from north (0 is a north wind)
    #[serde(default)]
    pub wind_bearing: Option<f64>,
    pub cloud_coverage: Option<f64>,
}

/// The weather forecast for the hour starting at `at`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastHour {
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub weather: Weather,
}

/// How long an hourly forecast is used before it's read again
pub const FORECAST_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(30);

/// Where the scheduler reads the weather from
#[async_trait::async_trait]
pub trait WeatherSource: Send + Sync {
    /// The current weather, or None when no weather entity is configured
    async fn fetch(&self, entity_id: Option<&str>) -> Result<Option<Weather>>;
    /// The hourly forecast, soonest first
    async fn fetch_forecast(&self, entity_id: &str) -> Result<Vec<ForecastHour>>;
    /// Sunrise to sunset, from Home Assistant's `sun.sun` (see [`parse_daylight`])
    async fn fetch_daylight(&self) -> Result<Option<TimePeriod>> {
        Ok(None)
    }
    /// A temperature sensor's reading in °C, or None while it has none
    async fn fetch_temperature(&self, _entity_id: &str) -> Result<Option<f64>> {
        Ok(None)
    }
}

/// The service call that returns a weather entity's forecast. It changes nothing, so a dry run
/// still makes it (see `dry_run::is_read`).
pub const FORECAST_ENDPOINT: &str = "/api/services/weather/get_forecasts?return_response";

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

    async fn fetch_forecast(&self, entity_id: &str) -> Result<Vec<ForecastHour>> {
        // The forecast is in the entity's units, which only its state gives
        let state: Value = self
            .api_client
            .get(&format!("/api/states/{}", entity_id))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let response: Value = self
            .api_client
            .post(
                FORECAST_ENDPOINT,
                &serde_json::json!({ "entity_id": entity_id, "type": "hourly" }),
            )
            .await?
            .error_for_status()?
            .json()
            .await?;
        parse_forecast(&response, entity_id, &state)
    }

    async fn fetch_daylight(&self) -> Result<Option<TimePeriod>> {
        let state: Value = self
            .api_client
            .get("/api/states/sun.sun")
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_daylight(&state))
    }

    async fn fetch_temperature(&self, entity_id: &str) -> Result<Option<f64>> {
        let state: Value = self
            .api_client
            .get(&format!("/api/states/{}", entity_id))
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_temperature(&state))
    }
}

/// Sunrise to sunset as local times of day, from `sun.sun`'s `next_rising` and `next_setting`.
/// Those are the next ones, so after sunset both are tomorrow's, and only their times of day are
/// used: a day later they're within a minute or two of today's.
pub fn parse_daylight(state: &Value) -> Option<TimePeriod> {
    let time = |key: &str| {
        let at = state.pointer(&format!("/attributes/{key}"))?.as_str()?;
        let local = DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&Local);
        local.time().with_second(0)?.with_nanosecond(0)
    };
    Some(TimePeriod {
        start: time("next_rising")?,
        end: time("next_setting")?,
    })
}

/// A temperature sensor's state in °C (from °F when its `unit_of_measurement` says so), or None
/// when it's `unavailable`, `unknown` or not a number
pub fn parse_temperature(state: &Value) -> Option<f64> {
    let c = state.get("state")?.as_str()?.parse::<f64>().ok()?;
    let unit = state.pointer("/attributes/unit_of_measurement");
    Some(match unit.and_then(Value::as_str) {
        Some("°F") => (c - 32.0) * 5.0 / 9.0,
        _ => c,
    })
}

/// Read the hourly forecast from a `weather.get_forecasts` response, in the units `state` (the
/// entity's state) gives. Hours without a readable time are skipped.
pub fn parse_forecast(
    response: &Value,
    entity_id: &str,
    state: &Value,
) -> Result<Vec<ForecastHour>> {
    let units = state
        .get("attributes")
        .ok_or_else(|| anyhow!("Weather state has no attributes"))?;
    let hours = response
        .pointer(&format!("/service_response/{entity_id}/forecast"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("No hourly forecast for {entity_id}"))?;
    let mut forecast: Vec<ForecastHour> = hours
        .iter()
        .filter_map(|hour| {
            let at = hour.get("datetime")?.as_str()?;
            let at = DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&Utc);
            let condition = hour.get("condition").and_then(Value::as_str);
            Some(ForecastHour {
                at,
                weather: read_conditions(hour, units, condition),
            })
        })
        .collect();
    forecast.sort_by_key(|h| h.at);
    Ok(forecast)
}

/// Read a weather entity's state. Uses the standard attributes `temperature`, `wind_speed`
/// and `cloud_coverage`, converting from `temperature_unit` (°C or °F) and `wind_speed_unit`
/// (km/h, m/s, mph or kn). Without `cloud_coverage`, the condition (`sunny`, `cloudy`, ...)
/// gives a rough cloud cover.
pub fn parse_weather_state(state: &Value) -> Result<Weather> {
    let attributes = state
        .get("attributes")
        .ok_or_else(|| anyhow!("Weather state has no attributes"))?;
    let condition = state.get("state").and_then(Value::as_str);
    Ok(read_conditions(attributes, attributes, condition))
}

/// The conditions in `values` (a state's attributes or one forecast hour), converted from the
/// units in `units` (the state's attributes). `wind_bearing` is degrees or a compass point.
fn read_conditions(values: &Value, units: &Value, condition: Option<&str>) -> Weather {
    let number = |key: &str| values.get(key).and_then(Value::as_f64);
    let unit = |key: &str| units.get(key).and_then(Value::as_str).unwrap_or("");

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
    let wind_bearing = number("wind_bearing")
        .or_else(|| compass_degrees(values.get("wind_bearing")?.as_str()?))
        .map(|b| b.rem_euclid(360.0));
    let cloud_coverage = number("cloud_coverage").or_else(|| match condition? {
        "sunny" | "clear-night" => Some(0.0),
        "partlycloudy" => Some(50.0),
        "cloudy" | "fog" | "rainy" | "pouring" | "snowy" | "snowy-rainy" | "hail" | "lightning"
        | "lightning-rainy" => Some(100.0),
        _ => None,
    });

    Weather {
        temperature,
        wind_speed,
        wind_bearing,
        cloud_coverage,
    }
}

/// A compass point ("N", "ENE", "sw", ...) in degrees
fn compass_degrees(point: &str) -> Option<f64> {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let point = point.trim().to_uppercase();
    let i = POINTS.iter().position(|p| *p == point)?;
    Some(i as f64 * 22.5)
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

    /// The next 24 hours, starting this hour: the mock weather, 0.5 °C colder each hour
    async fn fetch_forecast(&self, _entity_id: &str) -> Result<Vec<ForecastHour>> {
        let now = Utc::now();
        let hour = now - chrono::TimeDelta::seconds(now.timestamp().rem_euclid(3600));
        let weather = self.weather.read().unwrap().clone();
        Ok((0..24)
            .map(|h| ForecastHour {
                at: hour + chrono::TimeDelta::hours(h),
                weather: Weather {
                    temperature: weather.temperature.map(|t| t - 0.5 * h as f64),
                    ..weather.clone()
                },
            })
            .collect())
    }

    /// 07:00 to 19:00
    async fn fetch_daylight(&self) -> Result<Option<TimePeriod>> {
        Ok(Some(TimePeriod::new(7, 0, 19, 0)))
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
    /// The hourly forecast, soonest first; the last one read is kept while reads fail
    pub forecast: Vec<ForecastHour>,
    /// When the forecast was last asked for, read or not
    pub forecast_tried_at: Option<DateTime<Local>>,
    /// Why the last forecast read failed, if it did
    pub forecast_error: Option<String>,
    /// Sunrise to sunset, read with the forecast; the last one read is kept while reads fail
    #[serde(default)]
    pub daylight: Option<TimePeriod>,
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
    use chrono::TimeZone;
    use serde_json::json;

    /// `sun.sun` as Home Assistant gives it, with the next sunrise and sunset at these local times
    fn sun(rising: (u32, u32, u32), setting: (u32, u32, u32)) -> Value {
        let utc = |(day, h, m): (u32, u32, u32)| {
            Local
                .with_ymd_and_hms(2026, 1, day, h, m, 30)
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339()
        };
        json!({
            "state": "below_horizon",
            "attributes": { "next_rising": utc(rising), "next_setting": utc(setting) }
        })
    }

    #[test]
    fn test_daylight_is_todays_sunrise_to_sunset_whatever_the_time() {
        // 12:00 on the 15th: the next sunrise is tomorrow's, the next sunset today's
        let noon = parse_daylight(&sun((16, 7, 58), (15, 16, 5)));
        assert_eq!(noon, Some(TimePeriod::new(7, 58, 16, 5)));
        assert!(noon.unwrap().contains(hm(12, 0)));
        // 22:00: both are tomorrow's, a minute or two on
        let night = parse_daylight(&sun((16, 7, 58), (16, 16, 7)));
        assert_eq!(night, Some(TimePeriod::new(7, 58, 16, 7)));
        assert!(!night.unwrap().contains(hm(22, 0)));
        // Without the attributes, there's no daylight
        assert_eq!(parse_daylight(&json!({ "state": "above_horizon" })), None);
    }

    fn hm(h: u32, m: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn test_parse_temperature() {
        let reading = |state: &str, unit: &str| {
            parse_temperature(&json!({
                "state": state, "attributes": { "unit_of_measurement": unit }
            }))
        };
        assert_eq!(reading("19.5", "°C"), Some(19.5));
        assert_eq!(reading("68", "°F"), Some(20.0));
        assert_eq!(reading("unavailable", "°C"), None);
        assert_eq!(reading("unknown", "°C"), None);
    }

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
                wind_bearing: None,
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
    fn test_parse_wind_bearing() {
        let bearing = |b: Value| {
            parse_weather_state(&json!({"state": "sunny", "attributes": {"wind_bearing": b}}))
                .unwrap()
                .wind_bearing
        };
        assert_eq!(bearing(json!(225.0)), Some(225.0));
        assert_eq!(bearing(json!(360)), Some(0.0));
        assert_eq!(bearing(json!("NE")), Some(45.0));
        assert_eq!(bearing(json!("wsw")), Some(247.5));
        assert_eq!(bearing(json!("somewhere")), None);
    }

    #[test]
    fn test_parse_forecast_in_the_entitys_units() {
        let state = json!({
            "state": "cloudy",
            "attributes": {"temperature_unit": "°F", "wind_speed_unit": "mph"}
        });
        let response = json!({
            "changed_states": [],
            "service_response": {"weather.example": {"forecast": [
                {"datetime": "2026-10-13T16:00:00+00:00", "temperature": 41.0, "wind_speed": 10.0,
                 "wind_bearing": "N", "condition": "sunny"},
                {"datetime": "not a time", "temperature": 50.0},
                {"datetime": "2026-10-13T16:00:00+01:00", "temperature": 32.0, "cloud_coverage": 20}
            ]}}
        });

        let forecast = parse_forecast(&response, "weather.example", &state).unwrap();

        // Soonest first, the unreadable hour skipped
        assert_eq!(forecast.len(), 2);
        assert_eq!(forecast[0].at.to_rfc3339(), "2026-10-13T15:00:00+00:00");
        assert_eq!(forecast[0].weather.temperature, Some(0.0));
        assert_eq!(forecast[0].weather.cloud_coverage, Some(20.0));
        let later = &forecast[1].weather;
        assert!((later.temperature.unwrap() - 5.0).abs() < 1e-9);
        assert!((later.wind_speed.unwrap() - 16.09344).abs() < 1e-9);
        assert_eq!(later.wind_bearing, Some(0.0));
        assert_eq!(later.cloud_coverage, Some(0.0), "from the condition");

        assert!(parse_forecast(&response, "weather.other", &state).is_err());
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
