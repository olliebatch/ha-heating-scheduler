use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::persistence;
use crate::schedule::sets::{ScheduleSets, SetError};
use crate::schedule::{Schedule, ScheduleEntry, ScheduleEntryRequest};
use crate::server::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Local};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::climate::ClimateEntityWrapper;
#[cfg(not(debug_assertions))]
use crate::climate::DefaultClimate;
#[cfg(debug_assertions)]
use crate::climate::MockClimate;
#[cfg(debug_assertions)]
use crate::schedule::HeatingState;

type ApiError = (StatusCode, String);

/// Apply `change` to the schedule sets under the write lock, then persist them.
/// `change` must not modify the sets when it returns an error.
fn update_sets<T: ClimateEntity + Clone, R>(
    state: &AppState<T>,
    change: impl FnOnce(&mut ScheduleSets) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    let (result, snapshot) = {
        let mut sets = state.schedule.write().unwrap();
        let result = change(&mut sets)?;
        (result, sets.clone())
    };

    if let Err(e) = persistence::save_sets(&snapshot, &state.schedule_sets_file_path) {
        eprintln!("Failed to save schedule sets to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist schedule: {}", e),
        ));
    }
    Ok(result)
}

fn set_error(e: SetError) -> ApiError {
    match e {
        SetError::NotFound => (StatusCode::NOT_FOUND, "Schedule set not found".to_string()),
        SetError::Conflict(msg) => (StatusCode::CONFLICT, msg),
        SetError::Invalid(msg) => (StatusCode::BAD_REQUEST, msg),
    }
}

/// The active schedule
pub async fn get_schedule<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<Schedule> {
    let schedule = state.schedule.read().unwrap().active().clone();
    Json(schedule)
}

/// Add an entry to the active schedule
pub async fn add_schedule_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<ScheduleEntryRequest>,
) -> Result<Json<Schedule>, ApiError> {
    payload
        .validate()
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    // Convert request to ScheduleEntry (generates UUID automatically)
    let entry: ScheduleEntry = payload.into();

    let updated_schedule = update_sets(&state, |sets| {
        let schedule = sets.active_mut();
        schedule.add_entry(entry);
        Ok(schedule.clone())
    })?;

    println!("Schedule updated and saved");
    Ok(Json(updated_schedule))
}

/// Delete an entry from the active schedule
pub async fn delete_schedule_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<Schedule>, ApiError> {
    let updated_schedule = update_sets(&state, |sets| {
        let schedule = sets.active_mut();
        schedule.delete_entry(entry_id).map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                format!("Failed to delete entry: {}", e),
            )
        })?;
        Ok(schedule.clone())
    })?;

    println!("Schedule entry deleted and saved");
    Ok(Json(updated_schedule))
}

pub async fn get_schedule_sets<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<ScheduleSets> {
    Json(state.schedule.read().unwrap().clone())
}

#[derive(Serialize, Deserialize)]
pub struct CreateSetRequest {
    pub name: String,
    pub copy_from: Option<Uuid>,
}

/// Create a set: a full-day Off schedule, or a copy of `copy_from`
pub async fn create_schedule_set<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<CreateSetRequest>,
) -> Result<Json<Schedule>, ApiError> {
    let created = update_sets(&state, |sets| {
        sets.create(&payload.name, payload.copy_from)
            .cloned()
            .map_err(set_error)
    })?;
    Ok(Json(created))
}

#[derive(Serialize, Deserialize)]
pub struct RenameSetRequest {
    pub name: String,
}

pub async fn rename_schedule_set<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(set_id): Path<Uuid>,
    Json(payload): Json<RenameSetRequest>,
) -> Result<Json<Schedule>, ApiError> {
    let renamed = update_sets(&state, |sets| {
        sets.rename(set_id, &payload.name)
            .cloned()
            .map_err(set_error)
    })?;
    Ok(Json(renamed))
}

/// Delete a set; the active set, the last set and a set zones follow are refused with 409.
/// (Refusing, rather than moving those zones to the active set, means a room's heating never
/// changes as a side effect of tidying up sets.)
pub async fn delete_schedule_set<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(set_id): Path<Uuid>,
) -> Result<Json<ScheduleSets>, ApiError> {
    let sets = update_sets(&state, |sets| {
        // Lock order everywhere: schedule sets, then zones
        let zones = state.zones.read().unwrap();
        let users: Vec<String> = zones
            .using_set(set_id)
            .iter()
            .map(|z| format!("\"{}\"", z.name))
            .collect();
        if !users.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "Schedule set is used by zone(s) {}; move them to another set first",
                    users.join(", ")
                ),
            ));
        }
        sets.delete(set_id).map_err(set_error)?;
        Ok(sets.clone())
    })?;
    Ok(Json(sets))
}

pub async fn activate_schedule_set<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(set_id): Path<Uuid>,
) -> Result<Json<ScheduleSets>, ApiError> {
    let sets = update_sets(&state, |sets| {
        sets.activate(set_id).map_err(set_error)?;
        Ok(sets.clone())
    })?;
    Ok(Json(sets))
}

/// Add an entry to any set; a zero-length period is refused with 400
pub async fn add_set_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(set_id): Path<Uuid>,
    Json(payload): Json<ScheduleEntryRequest>,
) -> Result<Json<Schedule>, ApiError> {
    payload
        .validate()
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    let period = payload.time_period;
    if period.start == period.end && !period.is_full_day() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Start and end must differ (use 00:00 - 00:00 for the whole day)".to_string(),
        ));
    }
    let entry: ScheduleEntry = payload.into();

    let updated = update_sets(&state, |sets| {
        let schedule = sets
            .get_mut(set_id)
            .ok_or_else(|| set_error(SetError::NotFound))?;
        schedule.add_entry(entry);
        Ok(schedule.clone())
    })?;
    Ok(Json(updated))
}

/// Delete an entry from any set; deleting the only entry is refused with 409
pub async fn delete_set_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path((set_id, entry_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Schedule>, ApiError> {
    let updated = update_sets(&state, |sets| {
        let schedule = sets
            .get_mut(set_id)
            .ok_or_else(|| set_error(SetError::NotFound))?;
        if !schedule.entries.iter().any(|e| e.id == entry_id) {
            return Err((StatusCode::NOT_FOUND, "Entry not found".to_string()));
        }
        if schedule.entries.len() == 1 {
            return Err((
                StatusCode::CONFLICT,
                "Cannot delete the only schedule entry".to_string(),
            ));
        }
        schedule
            .delete_entry(entry_id)
            .map_err(|e| (StatusCode::NOT_FOUND, e))?;
        Ok(schedule.clone())
    })?;
    Ok(Json(updated))
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
    pub target_temp: Option<f64>,
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
                    target_temp: cached_state.as_ref().and_then(|s| s.target_temp),
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
    use crate::config::entities_persistence::{EntitiesConfig, save_entities};

    // Get current entity IDs
    let current_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates
            .iter()
            .map(|e| e.get_entity_id().to_string())
            .collect()
    };

    // Filter out entities that already exist
    let new_entity_ids: Vec<String> = payload
        .entity_ids
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
            climates.push(ClimateEntityWrapper::Real(DefaultClimate::new(
                entity_id.clone(),
            )));
        }
    }

    // Get updated list
    let all_entity_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates
            .iter()
            .map(|e| e.get_entity_id().to_string())
            .collect()
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

    crate::server::zones::reconcile_zones(&state)?;
    println!("Added {} new entities", new_entity_ids.len());
    Ok(Json(all_entity_ids))
}

/// Remove a climate entity
pub async fn remove_entity(
    State(state): State<AppState<ClimateEntityWrapper>>,
    Json(payload): Json<RemoveEntityRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    use crate::config::entities_persistence::{EntitiesConfig, save_entities};

    // Remove entity from the list
    {
        let mut climates = state.climate_entities.write().unwrap();
        climates.retain(|e| e.get_entity_id() != payload.entity_id);
    }

    // Get updated list
    let all_entity_ids: Vec<String> = {
        let climates = state.climate_entities.read().unwrap();
        climates
            .iter()
            .map(|e| e.get_entity_id().to_string())
            .collect()
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

    crate::server::zones::reconcile_zones(&state)?;
    println!("Removed entity: {}", payload.entity_id);
    Ok(Json(all_entity_ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::climate::MockClimate;
    use crate::schedule::{HeatingState, TimePeriod};
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    fn test_state() -> (AppState<MockClimate>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sets = ScheduleSets::from_schedule(Schedule::new("Work week"));
        let state = AppState {
            schedule: Arc::new(RwLock::new(sets)),
            schedule_sets_file_path: dir
                .path()
                .join("schedule_sets.json")
                .to_string_lossy()
                .to_string(),
            climate_entities: Arc::new(RwLock::new(Vec::new())),
            entities_file_path: dir
                .path()
                .join("entities.json")
                .to_string_lossy()
                .to_string(),
            zones: Arc::new(RwLock::new(crate::zones::Zones::default())),
            zones_file_path: dir.path().join("zones.json").to_string_lossy().to_string(),
            area_source: Arc::new(crate::zones::areas::MockAreas::example()),
        };
        (state, dir)
    }

    fn on(start: u32, end: u32) -> ScheduleEntryRequest {
        ScheduleEntryRequest {
            name: "On".to_string(),
            time_period: TimePeriod::new(start, 0, end, 0),
            heating_state: HeatingState::On,
            target_temp: Some(21.0),
        }
    }

    async fn create(state: &AppState<MockClimate>, name: &str) -> Schedule {
        create_schedule_set(
            State(state.clone()),
            Json(CreateSetRequest {
                name: name.to_string(),
                copy_from: None,
            }),
        )
        .await
        .unwrap()
        .0
    }

    #[tokio::test]
    async fn test_editing_inactive_set_leaves_active_alone_and_persists() {
        let (state, _dir) = test_state();
        let holiday = create(&state, "Holiday").await;

        let updated = add_set_entry(State(state.clone()), Path(holiday.id), Json(on(8, 17)))
            .await
            .unwrap()
            .0;

        assert_eq!(updated.entries.len(), 2);
        let active = get_schedule(State(state.clone())).await.0;
        assert_eq!(active.name, "Work week");
        assert_eq!(active.entries.len(), 1);

        let saved = persistence::load_sets(&state.schedule_sets_file_path).unwrap();
        assert_eq!(saved.get(holiday.id).unwrap().entries.len(), 2);
    }

    #[tokio::test]
    async fn test_legacy_routes_follow_the_active_set() {
        let (state, _dir) = test_state();
        let holiday = create(&state, "Holiday").await;
        let sets = activate_schedule_set(State(state.clone()), Path(holiday.id))
            .await
            .unwrap()
            .0;
        assert_eq!(sets.active_id, holiday.id);

        let updated = add_schedule_entry(State(state.clone()), Json(on(8, 17)))
            .await
            .unwrap()
            .0;

        assert_eq!(updated.id, holiday.id);
        let sets = get_schedule_sets(State(state.clone())).await.0;
        assert_eq!(sets.active_id, holiday.id);
        assert_eq!(sets.sets[0].entries.len(), 1, "Work week untouched");
    }

    #[tokio::test]
    async fn test_delete_set_status_codes() {
        let (state, _dir) = test_state();
        let active_id = state.schedule.read().unwrap().active_id;
        let holiday = create(&state, "Holiday").await;

        let err = delete_schedule_set(State(state.clone()), Path(active_id))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        let err = delete_schedule_set(State(state.clone()), Path(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        let sets = delete_schedule_set(State(state.clone()), Path(holiday.id))
            .await
            .unwrap()
            .0;
        assert_eq!(sets.sets.len(), 1);
    }

    #[tokio::test]
    async fn test_set_entry_status_codes() {
        let (state, _dir) = test_state();
        let set = state.schedule.read().unwrap().active().clone();

        // Zero-length add
        let err = add_set_entry(State(state.clone()), Path(set.id), Json(on(10, 10)))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // Only entry
        let err = delete_set_entry(State(state.clone()), Path((set.id, set.entries[0].id)))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        // Missing entry and missing set
        let err = delete_set_entry(State(state.clone()), Path((set.id, Uuid::new_v4())))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        let err = add_set_entry(State(state.clone()), Path(Uuid::new_v4()), Json(on(8, 9)))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_rename_and_empty_name() {
        let (state, _dir) = test_state();
        let holiday = create(&state, "Holiday").await;

        let renamed = rename_schedule_set(
            State(state.clone()),
            Path(holiday.id),
            Json(RenameSetRequest {
                name: "Away".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(renamed.id, holiday.id);
        assert_eq!(renamed.name, "Away");

        let err = rename_schedule_set(
            State(state.clone()),
            Path(holiday.id),
            Json(RenameSetRequest {
                name: " ".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_add_validates_target_temp() {
        let (state, _dir) = test_state();
        let set_id = state.schedule.read().unwrap().active_id;
        let no_target = ScheduleEntryRequest {
            target_temp: None,
            ..on(8, 17)
        };
        let off_with_target = ScheduleEntryRequest {
            heating_state: HeatingState::Off,
            ..on(8, 17)
        };

        for request in [no_target, off_with_target] {
            let err = add_set_entry(State(state.clone()), Path(set_id), Json(request.clone()))
                .await
                .unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            let err = add_schedule_entry(State(state.clone()), Json(request))
                .await
                .unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
        }

        let added = add_set_entry(State(state.clone()), Path(set_id), Json(on(8, 17)))
            .await
            .unwrap()
            .0;
        assert_eq!(added.entries[0].target_temp, Some(21.0));
    }

    #[tokio::test]
    async fn test_delete_set_used_by_zone_is_conflict() {
        let (state, _dir) = test_state();
        let holiday = create(&state, "Holiday").await;
        {
            let mut zones = state.zones.write().unwrap();
            zones.reconcile(Some(vec![]), &[]);
            let whole = zones.zones[0].id;
            zones.set_schedule_set(whole, Some(holiday.id)).unwrap();
        }

        let err = delete_schedule_set(State(state.clone()), Path(holiday.id))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(err.1.contains("Whole house"), "{}", err.1);
    }
}
