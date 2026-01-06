use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::persistence;
use crate::schedule::{Schedule, ScheduleEntry, ScheduleEntryRequest};
use crate::server::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Local};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::climate::ClimateEntityWrapper;
#[cfg(debug_assertions)]
use crate::climate::MockClimate;
#[cfg(not(debug_assertions))]
use crate::climate::DefaultClimate;
#[cfg(debug_assertions)]
use crate::schedule::HeatingState;

pub async fn get_schedule<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<Schedule> {
    let schedule = state.schedule.read().unwrap().clone();
    Json(schedule)
}

pub async fn add_schedule_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<ScheduleEntryRequest>,
) -> Result<Json<Schedule>, (StatusCode, String)> {
    // Convert request to ScheduleEntry (generates UUID automatically)
    let entry: ScheduleEntry = payload.into();

    // Add entry to the in-memory schedule
    let updated_schedule = {
        let mut schedule = state.schedule.write().unwrap();
        schedule.add_entry(entry);
        schedule.clone()
    };

    // Persist the updated schedule to disk
    if let Err(e) = persistence::save_schedule(&updated_schedule, &state.schedule_file_path) {
        eprintln!("Failed to save schedule to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist schedule: {}", e),
        ));
    }

    println!("Schedule updated and saved");
    Ok(Json(updated_schedule))
}

pub async fn delete_schedule_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<Schedule>, (StatusCode, String)> {
    // Delete entry from the in-memory schedule
    let updated_schedule = {
        let mut schedule = state.schedule.write().unwrap();

        // Attempt to delete the entry
        if let Err(e) = schedule.delete_entry(entry_id) {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Failed to delete entry: {}", e),
            ));
        }

        schedule.clone()
    };

    // Persist the updated schedule to disk
    if let Err(e) = persistence::save_schedule(&updated_schedule, &state.schedule_file_path) {
        eprintln!("Failed to save schedule to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist schedule: {}", e),
        ));
    }

    println!("Schedule entry deleted and saved");
    Ok(Json(updated_schedule))
}

pub async fn boost_all<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Result<StatusCode, (StatusCode, String)> {
    if let Ok(mut climates) = state.climate_entities.write() {
        for entity in climates.iter_mut() {
            let now = Local::now().time();
            entity.set_boost(Some(BoostInfo {
                boost_start: now.clone(),
                boost_end: now + Duration::minutes(45),
            }));
        }
        return Ok(StatusCode::OK);
    }
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Error Locking".to_string(),
    ))
}

#[derive(Serialize, Deserialize)]
pub struct BoostInput {
    climate_names: Vec<String>,
    time_length: u8,
}
pub async fn boost<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(boost_climates): Json<BoostInput>,
) -> Result<StatusCode, (StatusCode, String)> {
    if let Ok(mut climates) = state.climate_entities.write() {
        for entity in climates.iter_mut() {
            // Only boost climates whose entity_id matches one in the climate_names list
            if boost_climates
                .climate_names
                .contains(&entity.get_entity_id().to_string())
            {
                let now = Local::now().time();
                entity.set_boost(Some(BoostInfo {
                    boost_start: now,
                    boost_end: now + Duration::minutes(boost_climates.time_length as i64),
                }));
            }
        }
        return Ok(StatusCode::OK);
    }
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Error Locking".to_string(),
    ))
}

#[derive(Serialize, Deserialize)]
pub struct ClimateEntityInfo {
    pub entity_id: String,
    pub current_temperature: Option<f64>,
    pub state: Option<String>,
    pub boost_active: bool,
    pub boost_start: Option<String>,
    pub boost_end: Option<String>,
}

pub async fn get_entities<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Result<Json<Vec<ClimateEntityInfo>>, (StatusCode, String)> {
    if let Ok(climates) = state.climate_entities.read() {
        let entities: Vec<ClimateEntityInfo> = climates
            .iter()
            .map(|entity| {
                let cached_state = entity.get_cached_state();
                let boost_info = entity.get_boosted_status();

                ClimateEntityInfo {
                    entity_id: entity.get_entity_id().to_string(),
                    current_temperature: cached_state.as_ref().map(|s| s.current_temperature),
                    state: cached_state.as_ref().map(|s| format!("{:?}", s.state)),
                    boost_active: boost_info.is_some(),
                    boost_start: boost_info.as_ref().map(|b| b.boost_start.to_string()),
                    boost_end: boost_info.as_ref().map(|b| b.boost_end.to_string()),
                }
            })
            .collect();

        return Ok(Json(entities));
    }

    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to read climate entities".to_string(),
    ))
}

/// Request body for adding entities
#[derive(Serialize, Deserialize)]
pub struct AddEntitiesRequest {
    pub entity_ids: Vec<String>,
}

/// Request body for removing an entity
#[derive(Serialize, Deserialize)]
pub struct RemoveEntityRequest {
    pub entity_id: String,
}

/// Add new climate entities
pub async fn add_entities(
    State(state): State<AppState<ClimateEntityWrapper>>,
    Json(payload): Json<AddEntitiesRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    use crate::config::entities_persistence::{save_entities, EntitiesConfig};

    // Get current entity IDs
    let current_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates.iter().map(|e| e.get_entity_id().to_string()).collect()
    };

    // Filter out entities that already exist
    let new_entity_ids: Vec<String> = payload.entity_ids
        .into_iter()
        .filter(|id| !current_ids.contains(id))
        .collect();

    if new_entity_ids.is_empty() {
        return Ok(Json(current_ids));
    }

    // Add new entities to the climate_entities list
    #[cfg(debug_assertions)]
    {
        let mut climates = state.climate_entities.write().unwrap();
        for entity_id in &new_entity_ids {
            climates.push(ClimateEntityWrapper::Mock(MockClimate::new(
                entity_id.clone(),
                HeatingState::Off,
            )));
        }
    }

    #[cfg(not(debug_assertions))]
    {
        let mut climates = state.climate_entities.write().unwrap();
        for entity_id in &new_entity_ids {
            climates.push(ClimateEntityWrapper::Real(DefaultClimate::new(entity_id.clone())));
        }
    }

    // Get updated list
    let all_entity_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates.iter().map(|e| e.get_entity_id().to_string()).collect()
    };

    // Persist to disk
    let entities_config = EntitiesConfig::new(all_entity_ids.clone());
    if let Err(e) = save_entities(&entities_config, &state.entities_file_path) {
        eprintln!("Failed to save entities to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist entities: {}", e),
        ));
    }

    println!("Added {} new entities", new_entity_ids.len());
    Ok(Json(all_entity_ids))
}

/// Remove a climate entity
pub async fn remove_entity(
    State(state): State<AppState<ClimateEntityWrapper>>,
    Json(payload): Json<RemoveEntityRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    use crate::config::entities_persistence::{save_entities, EntitiesConfig};

    // Remove entity from the list
    {
        let mut climates = state.climate_entities.write().unwrap();
        climates.retain(|e| e.get_entity_id() != payload.entity_id);
    }

    // Get updated list
    let all_entity_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates.iter().map(|e| e.get_entity_id().to_string()).collect()
    };

    // Persist to disk
    let entities_config = EntitiesConfig::new(all_entity_ids.clone());
    if let Err(e) = save_entities(&entities_config, &state.entities_file_path) {
        eprintln!("Failed to save entities to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist entities: {}", e),
        ));
    }

    println!("Removed entity: {}", payload.entity_id);
    Ok(Json(all_entity_ids))
}
