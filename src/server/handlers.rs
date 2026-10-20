use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::persistence;
use crate::schedule::sets::{ScheduleSets, SetError};
use crate::schedule::{Schedule, ScheduleEntry, ScheduleEntryRequest};
use crate::server::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Local;
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

/// Apply `change` to the schedule sets and persist them, all under the write lock.
/// `change` must not modify the sets when it returns an error.
fn update_sets<T: ClimateEntity + Clone, R>(
    state: &AppState<T>,
    change: impl FnOnce(&mut ScheduleSets) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    // Save while still holding the lock, so concurrent changes reach disk in order
    let mut sets = state.schedule.write().unwrap();
    let result = change(&mut sets)?;
    if let Err(e) = persistence::save_sets(&sets, &state.schedule_sets_file_path) {
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

/// Delete an entry: 404 if it isn't there, 409 if it's the schedule's only entry
fn delete_entry_checked(schedule: &mut Schedule, entry_id: Uuid) -> Result<(), ApiError> {
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
        .map_err(|e| (StatusCode::NOT_FOUND, e))
}

/// Delete an entry from the active schedule
pub async fn delete_schedule_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<Schedule>, ApiError> {
    let updated_schedule = update_sets(&state, |sets| {
        let schedule = sets.active_mut();
        delete_entry_checked(schedule, entry_id)?;
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

/// The checks an added or edited entry must pass: its target, and no zero-length period (400)
fn check_set_entry(payload: &ScheduleEntryRequest) -> Result<(), ApiError> {
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
    Ok(())
}

/// Add an entry to any set; a zero-length period is refused with 400
pub async fn add_set_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(set_id): Path<Uuid>,
    Json(payload): Json<ScheduleEntryRequest>,
) -> Result<Json<Schedule>, ApiError> {
    check_set_entry(&payload)?;
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
        delete_entry_checked(schedule, entry_id)?;
        Ok(schedule.clone())
    })?;
    Ok(Json(updated))
}

/// Change an entry's times, state and target with the same checks as add: 404 if the set or
/// entry isn't there, and 409 for new times on a set's only entry, which must cover the day
pub async fn update_set_entry<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path((set_id, entry_id)): Path<(Uuid, Uuid)>,
    Json(payload): Json<ScheduleEntryRequest>,
) -> Result<Json<Schedule>, ApiError> {
    check_set_entry(&payload)?;
    let updated = update_sets(&state, |sets| {
        let schedule = sets
            .get_mut(set_id)
            .ok_or_else(|| set_error(SetError::NotFound))?;
        if !schedule.entries.iter().any(|e| e.id == entry_id) {
            return Err((StatusCode::NOT_FOUND, "Entry not found".to_string()));
        }
        if schedule.entries.len() == 1 && !payload.time_period.is_full_day() {
            return Err((
                StatusCode::CONFLICT,
                "The only period covers the whole day; add another period to change its times"
                    .to_string(),
            ));
        }
        schedule
            .update_entry(entry_id, payload.into())
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
            entity.set_boost(Some(BoostInfo::starting(Local::now(), 45)));
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
                entity.set_boost(Some(BoostInfo::starting(
                    Local::now(),
                    i64::from(boost_climates.time_length),
                )));
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

/// Whether this is a dry run, and the calls it would have made to Home Assistant, newest first
#[derive(Serialize)]
pub struct DryRunView {
    pub dry_run: bool,
    pub calls: Vec<crate::dry_run::WouldBeCall>,
}

pub async fn get_dry_run<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<DryRunView> {
    Json(DryRunView {
        dry_run: state.dry_run.is_some(),
        calls: state
            .dry_run
            .as_ref()
            .map(|d| d.calls())
            .unwrap_or_default(),
    })
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
                    boost_start: boost_info
                        .as_ref()
                        .map(|b| b.boost_start.time().to_string()),
                    boost_end: boost_info.as_ref().map(|b| b.boost_end.time().to_string()),
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

/// A climate entity Home Assistant has, or one this scheduler manages that Home Assistant
/// doesn't list (`name` and `state` are then None)
#[derive(Debug, Serialize, Deserialize)]
pub struct AvailableEntity {
    pub entity_id: String,
    pub name: Option<String>,
    /// Home Assistant's state, e.g. "heat", "off" or "unavailable"
    pub state: Option<String>,
    /// Its Home Assistant Area, if it has one
    pub area: Option<AreaName>,
    pub managed: bool,
    /// The zone it's in, or would land in if added
    pub zone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AreaName {
    pub id: String,
    pub name: String,
}

/// Every climate entity Home Assistant has, whether it's managed, its area and its zone. Only
/// reads Home Assistant; 502 if it can't be reached.
pub async fn get_available_entities<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Result<Json<Vec<AvailableEntity>>, ApiError> {
    use crate::zones::areas::{DISCOVERY_TIMEOUT, fetch_areas_with_timeout};
    let unreachable = |e: anyhow::Error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read climate entities from Home Assistant: {}", e),
        )
    };
    let areas = fetch_areas_with_timeout(state.area_source.as_ref(), DISCOVERY_TIMEOUT)
        .await
        .map_err(unreachable)?;
    let climates = tokio::time::timeout(DISCOVERY_TIMEOUT, state.area_source.fetch_climates())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {} s", DISCOVERY_TIMEOUT.as_secs()))
        .and_then(|r| r)
        .map_err(unreachable)?;

    let managed: Vec<String> = state
        .climate_entities
        .read()
        .unwrap()
        .iter()
        .map(|e| e.get_entity_id().to_string())
        .collect();
    let zones = state.zones.read().unwrap();
    let entry = |entity_id: String, name: Option<String>, state: Option<String>| {
        let area = areas
            .iter()
            .find(|a| a.entities.contains(&entity_id))
            .map(|a| AreaName {
                id: a.id.clone(),
                name: a.name.clone(),
            });
        AvailableEntity {
            zone: zones.zone_with(&areas, &managed, &entity_id),
            managed: managed.contains(&entity_id),
            entity_id,
            name,
            state,
            area,
        }
    };
    let mut entities: Vec<AvailableEntity> = climates
        .into_iter()
        .map(|c| entry(c.entity_id, Some(c.name), Some(c.state)))
        .collect();
    // Managed entities Home Assistant no longer lists, so they can be seen and removed
    for id in &managed {
        if !entities.iter().any(|e| &e.entity_id == id) {
            entities.push(entry(id.clone(), None, None));
        }
    }
    entities.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    Ok(Json(entities))
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

/// Change the climate entities and save entities.json while still holding the lock, so
/// concurrent adds and removes reach disk in order (like [`update_sets`]). Returns `change`'s
/// result and the entity ids afterwards. Zones are reconciled by the caller, after the lock is
/// released, since that reads the entities too.
fn update_entities<R>(
    state: &AppState<ClimateEntityWrapper>,
    change: impl FnOnce(&mut Vec<ClimateEntityWrapper>) -> R,
) -> Result<(R, Vec<String>), (StatusCode, String)> {
    use crate::config::entities_persistence::{EntitiesConfig, save_entities};

    let mut climates = state.climate_entities.write().unwrap();
    let result = change(&mut climates);
    let all_entity_ids: Vec<String> = climates
        .iter()
        .map(|e| e.get_entity_id().to_string())
        .collect();
    let entities_config = EntitiesConfig::new(all_entity_ids.clone());
    if let Err(e) = save_entities(&entities_config, &state.entities_file_path) {
        eprintln!("Failed to save entities to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist entities: {}", e),
        ));
    }
    Ok((result, all_entity_ids))
}

/// Add new climate entities
pub async fn add_entities(
    State(state): State<AppState<ClimateEntityWrapper>>,
    Json(payload): Json<AddEntitiesRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let payload_ids = payload.entity_ids.clone();
    // Check for existing entities under the same lock as the add, so two requests can't both add one
    let (added, all_entity_ids) = update_entities(&state, |climates| {
        let mut added = 0;
        for entity_id in payload.entity_ids {
            if climates.iter().any(|e| e.get_entity_id() == entity_id) {
                continue;
            }
            #[cfg(debug_assertions)]
            climates.push(ClimateEntityWrapper::Mock(MockClimate::new(
                entity_id,
                HeatingState::Off,
            )));
            #[cfg(not(debug_assertions))]
            climates.push(ClimateEntityWrapper::Real(DefaultClimate::new(entity_id)));
            added += 1;
        }
        added
    })?;
    // A re-added entity has its target sent again
    let mut last_sent = state.last_sent_targets.lock().unwrap();
    for id in &payload_ids {
        last_sent.remove(id);
    }
    drop(last_sent);

    if added > 0 {
        crate::server::zones::reconcile_zones(&state)?;
        println!("Added {} new entities", added);
    }
    Ok(Json(all_entity_ids))
}

/// Remove a climate entity
pub async fn remove_entity(
    State(state): State<AppState<ClimateEntityWrapper>>,
    Json(payload): Json<RemoveEntityRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let ((), all_entity_ids) = update_entities(&state, |climates| {
        climates.retain(|e| e.get_entity_id() != payload.entity_id);
    })?;
    state
        .last_sent_targets
        .lock()
        .unwrap()
        .remove(&payload.entity_id);

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
            weather: Default::default(),
            weather_file_path: dir
                .path()
                .join("weather.json")
                .to_string_lossy()
                .to_string(),
            mock_weather: None,
            dry_run: None,
            last_sent_targets: Default::default(),
            shadow: Default::default(),
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
    async fn test_update_set_entry_changes_it_in_place_and_persists() {
        let (state, _dir) = test_state();
        let set_id = state.schedule.read().unwrap().active_id;
        let added = add_set_entry(State(state.clone()), Path(set_id), Json(on(8, 17)))
            .await
            .unwrap()
            .0;
        let id = added.entries.iter().find(|e| e.name == "On").unwrap().id;

        let updated = update_set_entry(
            State(state.clone()),
            Path((set_id, id)),
            Json(ScheduleEntryRequest {
                target_temp: Some(19.5),
                ..on(9, 18)
            }),
        )
        .await
        .unwrap()
        .0;

        let edited = updated.entries.iter().find(|e| e.id == id).unwrap();
        assert_eq!(edited.time_period, TimePeriod::new(9, 0, 18, 0));
        assert_eq!(edited.target_temp, Some(19.5));
        let saved = persistence::load_sets(&state.schedule_sets_file_path).unwrap();
        let saved = saved
            .get(set_id)
            .unwrap()
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned();
        assert_eq!(saved.unwrap().time_period, TimePeriod::new(9, 0, 18, 0));
    }

    #[tokio::test]
    async fn test_refused_set_entry_updates_change_nothing() {
        let (state, _dir) = test_state();
        let set_id = state.schedule.read().unwrap().active_id;
        let added = add_set_entry(State(state.clone()), Path(set_id), Json(on(8, 17)))
            .await
            .unwrap()
            .0;
        let id = added.entries.iter().find(|e| e.name == "On").unwrap().id;
        let read = |state: &AppState<MockClimate>| {
            serde_json::to_string(&*state.schedule.read().unwrap()).unwrap()
        };
        let before = read(&state);
        let on_disk = std::fs::read_to_string(&state.schedule_sets_file_path).unwrap();

        let refusals = [
            (
                set_id,
                id,
                on(10, 10),
                StatusCode::BAD_REQUEST,
                "Start and end must differ",
            ),
            (
                set_id,
                id,
                ScheduleEntryRequest {
                    target_temp: None,
                    ..on(8, 17)
                },
                StatusCode::BAD_REQUEST,
                "need a target_temp",
            ),
            (
                set_id,
                id,
                ScheduleEntryRequest {
                    target_temp: Some(31.0),
                    ..on(8, 17)
                },
                StatusCode::BAD_REQUEST,
                "between 5 and 30",
            ),
            (
                set_id,
                id,
                ScheduleEntryRequest {
                    heating_state: HeatingState::Off,
                    ..on(8, 17)
                },
                StatusCode::BAD_REQUEST,
                "can't have a target_temp",
            ),
            (
                set_id,
                Uuid::new_v4(),
                on(8, 17),
                StatusCode::NOT_FOUND,
                "Entry not found",
            ),
            (
                Uuid::new_v4(),
                id,
                on(8, 17),
                StatusCode::NOT_FOUND,
                "Schedule set not found",
            ),
        ];
        for (set, entry, request, status, message) in refusals {
            let err = update_set_entry(State(state.clone()), Path((set, entry)), Json(request))
                .await
                .unwrap_err();
            assert_eq!(err.0, status, "{}", err.1);
            assert!(err.1.contains(message), "{}", err.1);
        }

        assert_eq!(read(&state), before, "nothing changed in memory");
        assert_eq!(
            std::fs::read_to_string(&state.schedule_sets_file_path).unwrap(),
            on_disk,
            "nothing changed on disk"
        );
    }

    #[tokio::test]
    async fn test_only_entry_keeps_the_whole_day() {
        let (state, _dir) = test_state();
        let set = state.schedule.read().unwrap().active().clone();
        let only = set.entries[0].id;

        // New times on the only entry would leave the rest of the day uncovered
        let err = update_set_entry(State(state.clone()), Path((set.id, only)), Json(on(8, 17)))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(
            state.schedule.read().unwrap().active().entries[0].heating_state,
            HeatingState::Off
        );

        // Its state and target can still change, all day
        let updated = update_set_entry(State(state.clone()), Path((set.id, only)), Json(on(0, 0)))
            .await
            .unwrap()
            .0;
        assert_eq!(updated.entries.len(), 1);
        assert_eq!(updated.entries[0].id, only);
        assert_eq!(updated.entries[0].heating_state, HeatingState::On);
    }

    #[tokio::test]
    async fn test_available_entities_list_every_climate_and_where_it_goes() {
        let (mut state, _dir) = test_state();
        state.climate_entities = Arc::new(RwLock::new(
            ["climate.lounge_trv", "climate.qa_mock", "climate.gone"]
                .map(|id| MockClimate::new(id.to_string(), HeatingState::Off))
                .to_vec(),
        ));
        let zones_before = serde_json::to_string(&*state.zones.read().unwrap()).unwrap();

        let listed = get_available_entities(State(state.clone()))
            .await
            .unwrap()
            .0;
        let find = |id: &str| listed.iter().find(|e| e.entity_id == id).unwrap();

        // Every climate HA has, plus the managed one it no longer lists
        assert_eq!(listed.len(), 8);
        let lounge = find("climate.lounge_trv");
        assert!(lounge.managed);
        assert_eq!(lounge.area.as_ref().unwrap().name, "Lounge");
        assert_eq!(lounge.zone.as_deref(), Some("Lounge"));
        assert_eq!(find("climate.qa_mock").zone.as_deref(), Some("Whole house"));
        // Not managed: where each would land
        let bedroom = find("climate.bedroom_trv");
        assert!(!bedroom.managed);
        assert_eq!(bedroom.name.as_deref(), Some("Bedroom TRV"));
        assert_eq!(
            bedroom.zone.as_deref(),
            Some("Bedroom"),
            "a new zone for its area"
        );
        let landing = find("climate.landing_trv");
        assert!(landing.area.is_none());
        assert_eq!(landing.zone.as_deref(), Some("Whole house"), "no area");
        assert_eq!(
            find("climate.garage_trv").state.as_deref(),
            Some("unavailable")
        );
        // Managed but gone from Home Assistant
        let gone = find("climate.gone");
        assert!(gone.managed);
        assert_eq!((gone.name.as_ref(), gone.state.as_ref()), (None, None));

        // Listing changes nothing
        assert_eq!(
            serde_json::to_string(&*state.zones.read().unwrap()).unwrap(),
            zones_before
        );
        assert_eq!(state.climate_entities.read().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_available_entities_without_home_assistant_is_502() {
        struct Unreachable;
        #[async_trait::async_trait]
        impl crate::zones::areas::AreaSource for Unreachable {
            async fn fetch_areas(&self) -> anyhow::Result<Vec<crate::zones::areas::Area>> {
                Ok(vec![])
            }
            async fn fetch_climates(&self) -> anyhow::Result<Vec<crate::zones::areas::Climate>> {
                Err(anyhow::anyhow!("connection refused"))
            }
        }
        let (mut state, _dir) = test_state();
        state.area_source = Arc::new(Unreachable);

        let err = get_available_entities(State(state)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("connection refused"), "{}", err.1);
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

    #[tokio::test]
    async fn test_weather_entity_and_mock_routes() {
        use crate::server::weather::{
            WeatherEntityRequest, get_weather, set_mock_weather, set_weather_entity,
        };
        let (mut state, _dir) = test_state();
        let set = |id: Option<&str>| {
            Json(WeatherEntityRequest {
                entity_id: id.map(str::to_string),
            })
        };

        let status = set_weather_entity(State(state.clone()), set(Some(" weather.example ")))
            .await
            .unwrap()
            .0;
        assert_eq!(status.entity_id.as_deref(), Some("weather.example"));
        let saved = crate::weather::load_weather_config(&state.weather_file_path).unwrap();
        assert_eq!(saved.entity_id.as_deref(), Some("weather.example"));

        for bad in ["sensor.outside", "weather."] {
            let err = set_weather_entity(State(state.clone()), set(Some(bad)))
                .await
                .unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{}", bad);
        }

        let status = set_weather_entity(State(state.clone()), set(None))
            .await
            .unwrap()
            .0;
        assert_eq!(status.entity_id, None);
        assert_eq!(get_weather(State(state.clone())).await.0.entity_id, None);

        // Mock weather: 404 without a mock, set when there is one
        let cold = crate::weather::Weather {
            temperature: Some(-2.0),
            ..Default::default()
        };
        let err = set_mock_weather(State(state.clone()), Json(cold.clone()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        let mock = Arc::new(RwLock::new(crate::weather::Weather::default()));
        state.mock_weather = Some(Arc::clone(&mock));
        let set = set_mock_weather(State(state), Json(cold.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(set, cold);
        assert_eq!(*mock.read().unwrap(), cold);
    }

    #[tokio::test]
    async fn test_legacy_delete_status_codes() {
        let (state, _dir) = test_state();
        let only = state.schedule.read().unwrap().active().entries[0].id;

        let err = delete_schedule_entry(State(state.clone()), Path(only))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        let err = delete_schedule_entry(State(state.clone()), Path(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        // With two entries, deleting one works
        let added = add_schedule_entry(State(state.clone()), Json(on(8, 17)))
            .await
            .unwrap()
            .0;
        let on_id = added
            .entries
            .iter()
            .find(|e| e.heating_state == HeatingState::On)
            .unwrap()
            .id;
        let after = delete_schedule_entry(State(state.clone()), Path(on_id))
            .await
            .unwrap()
            .0;
        assert_eq!(after.entries.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_changes_reach_disk_in_order() {
        let (state, _dir) = test_state();
        let set_id = state.schedule.read().unwrap().active_id;

        let tasks: Vec<_> = (0..40)
            .map(|i| {
                let state = state.clone();
                tokio::spawn(async move {
                    let hour = i % 24;
                    let request = ScheduleEntryRequest {
                        target_temp: Some(15.0 + f64::from(i % 10)),
                        ..on(hour, (hour + 1) % 24)
                    };
                    let _updated = add_set_entry(State(state), Path(set_id), Json(request))
                        .await
                        .unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }

        // The file holds the final state, not an earlier snapshot saved late
        let saved = persistence::load_sets(&state.schedule_sets_file_path).unwrap();
        let memory = state.schedule.read().unwrap().clone();
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            serde_json::to_value(&memory).unwrap()
        );
    }

    /// The entity handlers take the real wrapper type (mock climates in test builds)
    fn wrapper_state() -> (AppState<ClimateEntityWrapper>, TempDir) {
        let (state, dir) = test_state();
        let state = AppState {
            schedule: state.schedule,
            schedule_sets_file_path: state.schedule_sets_file_path,
            climate_entities: Arc::new(RwLock::new(Vec::new())),
            entities_file_path: state.entities_file_path,
            zones: state.zones,
            zones_file_path: state.zones_file_path,
            area_source: state.area_source,
            weather: state.weather,
            weather_file_path: state.weather_file_path,
            mock_weather: None,
            dry_run: None,
            last_sent_targets: state.last_sent_targets,
            shadow: state.shadow,
        };
        (state, dir)
    }

    #[tokio::test]
    async fn test_adding_or_removing_an_entity_forgets_its_last_target() {
        let (state, _dir) = wrapper_state();
        let lounge = "climate.lounge_trv".to_string();
        let remembered = |state: &AppState<ClimateEntityWrapper>| {
            state
                .last_sent_targets
                .lock()
                .unwrap()
                .contains_key(&lounge)
        };
        let _ = add_entities(
            State(state.clone()),
            Json(AddEntitiesRequest {
                entity_ids: vec![lounge.clone()],
            }),
        )
        .await
        .unwrap();

        // The scheduler sent it 21 °C; removing it forgets that
        state
            .last_sent_targets
            .lock()
            .unwrap()
            .insert(lounge.clone(), Some(21.0));
        let _ = remove_entity(
            State(state.clone()),
            Json(RemoveEntityRequest {
                entity_id: lounge.clone(),
            }),
        )
        .await
        .unwrap();
        assert!(!remembered(&state));

        // And so does adding it, in case the scheduler wrote it back meanwhile
        state
            .last_sent_targets
            .lock()
            .unwrap()
            .insert(lounge.clone(), Some(21.0));
        let _ = add_entities(
            State(state.clone()),
            Json(AddEntitiesRequest {
                entity_ids: vec![lounge.clone()],
            }),
        )
        .await
        .unwrap();
        assert!(!remembered(&state));
    }

    fn ids(state: &AppState<ClimateEntityWrapper>) -> Vec<String> {
        let climates = state.climate_entities.read().unwrap();
        climates
            .iter()
            .map(|e| e.get_entity_id().to_string())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_entity_changes_reach_disk_in_order() {
        use crate::config::entities_persistence::load_entities;
        let (state, _dir) = wrapper_state();

        // Only the last save of a burst decides the file, so check after each of many short
        // bursts of 8 requests, released together so they really overlap
        for round in 0..300 {
            let barrier = Arc::new(tokio::sync::Barrier::new(8));
            let tasks: Vec<_> = (0..8)
                .map(|i| {
                    let (state, barrier) = (state.clone(), Arc::clone(&barrier));
                    tokio::spawn(async move {
                        let a = format!("climate.trv_{}", (round + i) % 8);
                        let b = format!("climate.trv_{}", (round + i + 3) % 8);
                        barrier.wait().await;
                        if i % 2 == 1 {
                            let request = RemoveEntityRequest { entity_id: a };
                            let _ = remove_entity(State(state), Json(request)).await.unwrap();
                        } else {
                            let request = AddEntitiesRequest {
                                entity_ids: vec![a, b],
                            };
                            let _ = add_entities(State(state), Json(request)).await.unwrap();
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }

            // The file holds the current list, not an earlier snapshot saved late, without duplicates
            let saved = load_entities(&state.entities_file_path)
                .unwrap()
                .climate_entities;
            let memory = ids(&state);
            assert_eq!(saved, memory, "after round {round}");
            let mut unique = memory.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                memory.len(),
                "duplicates after round {round}: {memory:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_add_entities_skips_existing_and_repeated_ids() {
        use crate::config::entities_persistence::load_entities;
        let (state, _dir) = wrapper_state();
        let add = |ids: &[&str]| AddEntitiesRequest {
            entity_ids: ids.iter().map(|s| s.to_string()).collect(),
        };
        let all = add_entities(
            State(state.clone()),
            Json(add(&["climate.a", "climate.a", "climate.b"])),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(all, vec!["climate.a", "climate.b"]);
        let all = add_entities(State(state.clone()), Json(add(&["climate.b", "climate.c"])))
            .await
            .unwrap()
            .0;
        assert_eq!(all, vec!["climate.a", "climate.b", "climate.c"]);
        let all = remove_entity(
            State(state.clone()),
            Json(RemoveEntityRequest {
                entity_id: "climate.a".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(all, vec!["climate.b", "climate.c"]);
        assert_eq!(
            load_entities(&state.entities_file_path)
                .unwrap()
                .climate_entities,
            all
        );
    }
}
