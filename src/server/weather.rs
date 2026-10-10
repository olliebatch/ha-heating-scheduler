use crate::climate::ClimateEntity;
use crate::server::AppState;
use crate::weather::{Weather, WeatherConfig, WeatherStatus, save_weather_config};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

type ApiError = (StatusCode, String);

/// The configured weather entity and the scheduler's last reading (or error)
pub async fn get_weather<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<WeatherStatus> {
    Json(state.weather.read().unwrap().clone())
}

#[derive(Serialize, Deserialize)]
pub struct WeatherEntityRequest {
    /// A `weather.*` entity id, or null to stop reading the weather
    pub entity_id: Option<String>,
}

/// Choose the Home Assistant weather entity; the next scheduler tick reads it
pub async fn set_weather_entity<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<WeatherEntityRequest>,
) -> Result<Json<WeatherStatus>, ApiError> {
    let entity_id = payload
        .entity_id
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    if let Some(id) = &entity_id {
        if !id.starts_with("weather.") || id.len() == "weather.".len() {
            return Err((
                StatusCode::BAD_REQUEST,
                "The weather entity must be a weather.* entity id".to_string(),
            ));
        }
    }

    let config = WeatherConfig {
        entity_id: entity_id.clone(),
    };
    if let Err(e) = save_weather_config(&config, &state.weather_file_path) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist weather config: {}", e),
        ));
    }

    let mut status = state.weather.write().unwrap();
    if status.entity_id != entity_id {
        // The last reading was for another entity
        *status = WeatherStatus {
            entity_id,
            ..Default::default()
        };
    }
    Ok(Json(status.clone()))
}

/// Set the mock weather (debug builds only); the next scheduler tick uses it
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub async fn set_mock_weather<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(weather): Json<Weather>,
) -> Result<Json<Weather>, ApiError> {
    let Some(mock) = &state.mock_weather else {
        return Err((
            StatusCode::NOT_FOUND,
            "Mock weather is only available in debug builds".to_string(),
        ));
    };
    *mock.write().unwrap() = weather.clone();
    Ok(Json(weather))
}
