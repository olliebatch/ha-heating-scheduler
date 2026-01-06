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
