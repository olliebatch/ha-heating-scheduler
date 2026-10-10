use crate::climate::ClimateEntity;
use crate::schedule::TimePeriod;
use crate::scheduler::{ZoneStatus, zone_status};
use crate::server::AppState;
use crate::weather::adjust::WindExposure;
use crate::zones::areas::{DISCOVERY_TIMEOUT, fetch_areas_with_timeout};
use crate::zones::{Zone, ZoneError, Zones, save_zones};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Local;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

type ApiError = (StatusCode, String);

fn zone_error(e: ZoneError) -> ApiError {
    match e {
        ZoneError::NotFound => (StatusCode::NOT_FOUND, "Zone not found".to_string()),
        ZoneError::Conflict(msg) => (StatusCode::CONFLICT, msg),
        ZoneError::Invalid(msg) => (StatusCode::BAD_REQUEST, msg),
    }
}

/// Ids of the climate entities we manage
pub fn managed_entity_ids<T: ClimateEntity + Clone>(state: &AppState<T>) -> Vec<String> {
    state
        .climate_entities
        .read()
        .unwrap()
        .iter()
        .map(|e| e.get_entity_id().to_string())
        .collect()
}

/// Apply `change` to the zones and persist them, all under the write lock.
/// `change` must not modify the zones when it returns an error.
fn update_zones<T: ClimateEntity + Clone, R>(
    state: &AppState<T>,
    change: impl FnOnce(&mut Zones, &[String]) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    let managed = managed_entity_ids(state);
    // Save while still holding the lock, so concurrent changes reach disk in order
    let mut zones = state.zones.write().unwrap();
    let result = change(&mut zones, &managed)?;
    if let Err(e) = save_zones(&zones, &state.zones_file_path) {
        eprintln!("Failed to save zones to disk: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist zones: {}", e),
        ));
    }
    Ok(result)
}

/// Re-sort managed entities into zones after the entity list changes
pub fn reconcile_zones<T: ClimateEntity + Clone>(state: &AppState<T>) -> Result<(), ApiError> {
    update_zones(state, |zones, managed| {
        zones.reconcile(None, managed);
        Ok(())
    })
}

/// A zone with what it is doing right now and why
#[derive(Serialize)]
pub struct ZoneView {
    #[serde(flatten)]
    pub zone: Zone,
    pub status: ZoneStatus,
}

/// All zones, each with its current state, scheduled and adjusted target, and the reasons,
/// using the scheduler's last weather reading
pub async fn get_zones<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<Vec<ZoneView>> {
    let now = Local::now();
    // The scheduler's last reading and the targets it is holding, so this shows what it sends
    let (weather, held, begun) = {
        let status = state.weather.read().unwrap();
        (
            status.weather.clone(),
            status.held.clone(),
            status.begun.clone(),
        )
    };
    // Lock order everywhere: schedule sets, then zones
    let sets = state.schedule.read().unwrap();
    let zones = state.zones.read().unwrap();
    Json(
        zones
            .zones
            .iter()
            .map(|zone| ZoneView {
                zone: zone.clone(),
                status: zone_status(
                    Some(zone),
                    &sets,
                    weather.as_ref(),
                    held.get(&zone.id).copied(),
                    begun.get(&zone.id),
                    &now,
                ),
            })
            .collect(),
    )
}

/// Ask Home Assistant for its areas again and rebuild the zones
pub async fn refresh_zones<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Result<Json<Vec<Zone>>, ApiError> {
    let areas = fetch_areas_with_timeout(state.area_source.as_ref(), DISCOVERY_TIMEOUT)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch areas from Home Assistant: {}", e),
            )
        })?;
    let zones = update_zones(&state, |zones, managed| {
        zones.reconcile(Some(areas), managed);
        Ok(zones.zones.clone())
    })?;
    Ok(Json(zones))
}

/// Distinguishes a missing field (None) from an explicit null (Some(None))
fn double_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<Uuid>>, D::Error> {
    Option::<Uuid>::deserialize(d).map(Some)
}

#[derive(Serialize, Deserialize)]
pub struct UpdateZoneRequest {
    pub name: Option<String>,
    /// A set id, or null to follow the active set; leave out to keep the current one
    #[serde(default, deserialize_with = "double_option")]
    pub schedule_set_id: Option<Option<Uuid>>,
    pub sun_windows: Option<Vec<TimePeriod>>,
    pub wind_exposure: Option<WindExposure>,
    pub weather_adjust: Option<bool>,
    #[serde(default, deserialize_with = "crate::zones::optional_minutes")]
    pub max_early_start_minutes: Option<u32>,
    #[serde(default, deserialize_with = "crate::zones::optional_minutes")]
    pub max_late_finish_minutes: Option<u32>,
    /// The whole block; fields left out get their defaults
    pub cold_warmups: Option<crate::weather::warmup::ColdWarmups>,
}

/// A request body that couldn't be read, as a 400 saying which field and why, e.g.
/// "max_late_finish_minutes: must be a whole number of minutes, 0 or more, not -5"
fn unreadable(rejection: JsonRejection) -> ApiError {
    let text = rejection.body_text();
    let text = text
        .strip_prefix("Failed to deserialize the JSON body into the target type: ")
        .unwrap_or(&text);
    // serde adds where in the JSON it stopped, which means nothing to a person
    let text = match text.rfind(" at line ") {
        Some(i) if text[i..].contains(" column ") => &text[..i],
        _ => text,
    };
    (StatusCode::BAD_REQUEST, text.to_string())
}

pub async fn update_zone<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(zone_id): Path<Uuid>,
    payload: Result<Json<UpdateZoneRequest>, JsonRejection>,
) -> Result<Json<Zone>, ApiError> {
    let Json(payload) = payload.map_err(unreadable)?;
    if let Some(Some(set_id)) = payload.schedule_set_id {
        if state.schedule.read().unwrap().get(set_id).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Schedule set {} not found", set_id),
            ));
        }
    }
    let zone = update_zones(&state, |zones, _| {
        if zones.get(zone_id).is_none() {
            return Err(zone_error(ZoneError::NotFound));
        }
        // Check the name before changing anything
        if payload.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(zone_error(ZoneError::Invalid(
                "Name must not be empty".to_string(),
            )));
        }
        // Check the warm-up settings before changing anything
        if let Some(warmups) = &payload.cold_warmups {
            warmups
                .validate()
                .map_err(|e| zone_error(ZoneError::Invalid(e)))?;
        }
        // set_profile checks the sun windows before changing anything
        zones
            .set_profile(
                zone_id,
                payload.sun_windows.clone(),
                payload.wind_exposure,
                payload.weather_adjust,
                payload.max_early_start_minutes,
                payload.max_late_finish_minutes,
            )
            .map_err(zone_error)?;
        if let Some(warmups) = payload.cold_warmups {
            zones
                .set_cold_warmups(zone_id, warmups)
                .map_err(zone_error)?;
        }
        if let Some(name) = &payload.name {
            zones.rename(zone_id, name).map_err(zone_error)?;
        }
        if let Some(set_id) = payload.schedule_set_id {
            zones
                .set_schedule_set(zone_id, set_id)
                .map_err(zone_error)?;
        }
        Ok(zones.get(zone_id).unwrap().clone())
    })?;
    Ok(Json(zone))
}

#[derive(Serialize, Deserialize)]
pub struct MergeZonesRequest {
    pub zone_ids: Vec<Uuid>,
    pub name: Option<String>,
}

/// Merge area zones into the first one listed
pub async fn merge_zones<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<MergeZonesRequest>,
) -> Result<Json<Zone>, ApiError> {
    let zone = update_zones(&state, |zones, managed| {
        // merge() may fail on the name after removing zones, so check it first
        if payload.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(zone_error(ZoneError::Invalid(
                "Name must not be empty".to_string(),
            )));
        }
        zones
            .merge(&payload.zone_ids, payload.name.as_deref(), managed)
            .cloned()
            .map_err(zone_error)
    })?;
    Ok(Json(zone))
}

#[derive(Serialize, Deserialize)]
pub struct CreateZoneRequest {
    pub name: String,
    pub entity_ids: Vec<String>,
}

/// Create a manual zone
pub async fn create_zone<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Json(payload): Json<CreateZoneRequest>,
) -> Result<Json<Zone>, ApiError> {
    let zone = update_zones(&state, |zones, managed| {
        zones
            .create_manual(&payload.name, &payload.entity_ids, managed)
            .cloned()
            .map_err(zone_error)
    })?;
    Ok(Json(zone))
}

/// Delete a manual or merged zone; its entities fall back to area zones or Whole house
pub async fn delete_zone<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(zone_id): Path<Uuid>,
) -> Result<Json<Vec<Zone>>, ApiError> {
    let zones = update_zones(&state, |zones, managed| {
        zones.delete(zone_id, managed).map_err(zone_error)?;
        Ok(zones.zones.clone())
    })?;
    Ok(Json(zones))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::climate::MockClimate;
    use crate::schedule::sets::ScheduleSets;
    use crate::schedule::{HeatingState, Schedule};
    use crate::zones::areas::{AreaSource, MockAreas};
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    fn test_state() -> (AppState<MockClimate>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_string_lossy().to_string();
        let entities = ["climate.lounge_trv", "climate.study_trv", "climate.qa_mock"]
            .iter()
            .map(|id| MockClimate::new(id.to_string(), HeatingState::Off))
            .collect();
        let state = AppState {
            schedule: Arc::new(RwLock::new(ScheduleSets::from_schedule(Schedule::new(
                "Work week",
            )))),
            schedule_sets_file_path: path("schedule_sets.json"),
            climate_entities: Arc::new(RwLock::new(entities)),
            entities_file_path: path("entities.json"),
            zones: Arc::new(RwLock::new(Zones::default())),
            zones_file_path: path("zones.json"),
            area_source: Arc::new(MockAreas::example()),
            weather: Default::default(),
            weather_file_path: path("weather.json"),
            mock_weather: None,
            dry_run: None,
        };
        (state, dir)
    }

    fn zone_named<'a>(zones: &'a [Zone], name: &str) -> &'a Zone {
        zones.iter().find(|z| z.name == name).unwrap()
    }

    #[tokio::test]
    async fn test_refresh_discovers_zones_and_persists() {
        let (state, _dir) = test_state();

        let zones = refresh_zones(State(state.clone())).await.unwrap().0;

        assert_eq!(zones.len(), 3);
        assert_eq!(
            zone_named(&zones, "Lounge").entity_ids,
            vec!["climate.lounge_trv"]
        );
        assert_eq!(
            zone_named(&zones, "Study").entity_ids,
            vec!["climate.study_trv"]
        );
        assert_eq!(
            zone_named(&zones, "Whole house").entity_ids,
            vec!["climate.qa_mock"]
        );
        let saved = crate::zones::load_zones(&state.zones_file_path).unwrap();
        assert_eq!(saved.zones.len(), 3);
    }

    struct FailingAreas;

    #[async_trait::async_trait]
    impl AreaSource for FailingAreas {
        async fn fetch_areas(&self) -> anyhow::Result<Vec<crate::zones::areas::Area>> {
            Err(anyhow::anyhow!("unreachable"))
        }
    }

    #[tokio::test]
    async fn test_refresh_failure_is_bad_gateway() {
        let (mut state, _dir) = test_state();
        state.area_source = Arc::new(FailingAreas);

        let err = refresh_zones(State(state)).await.unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn test_update_zone_schedule_set() {
        let (state, _dir) = test_state();
        let zones = refresh_zones(State(state.clone())).await.unwrap().0;
        let study = zone_named(&zones, "Study").id;
        let holiday = state
            .schedule
            .write()
            .unwrap()
            .create("Holiday", None)
            .unwrap()
            .id;

        let request = |json: &str| Json::<UpdateZoneRequest>::from_bytes(json.as_bytes());

        let zone = update_zone(
            State(state.clone()),
            Path(study),
            request(&format!(r#"{{"schedule_set_id": "{}"}}"#, holiday)),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(zone.schedule_set_id, Some(holiday));

        // Renaming leaves the set alone
        let zone = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"name": "Office"}"#),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(zone.name, "Office");
        assert_eq!(zone.schedule_set_id, Some(holiday));

        // null follows the active set again
        let zone = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"schedule_set_id": null}"#),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(zone.schedule_set_id, None);

        let err = update_zone(
            State(state.clone()),
            Path(study),
            request(&format!(r#"{{"schedule_set_id": "{}"}}"#, Uuid::new_v4())),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"name": " "}"#),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = update_zone(State(state), Path(Uuid::new_v4()), request("{}"))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_merge_manual_and_delete_routes() {
        let (state, _dir) = test_state();
        let zones = refresh_zones(State(state.clone())).await.unwrap().0;
        let lounge = zone_named(&zones, "Lounge").id;
        let study = zone_named(&zones, "Study").id;
        let whole = zone_named(&zones, "Whole house").id;

        let merged = merge_zones(
            State(state.clone()),
            Json(MergeZonesRequest {
                zone_ids: vec![lounge, study],
                name: Some("Downstairs".to_string()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(merged.entity_ids.len(), 2);

        let manual = create_zone(
            State(state.clone()),
            Json(CreateZoneRequest {
                name: "Spare".to_string(),
                entity_ids: vec!["climate.qa_mock".to_string()],
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(manual.entity_ids, vec!["climate.qa_mock"]);

        let err = delete_zone(State(state.clone()), Path(whole))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        let zones = delete_zone(State(state.clone()), Path(manual.id))
            .await
            .unwrap()
            .0;
        assert_eq!(
            zone_named(&zones, "Whole house").entity_ids,
            vec!["climate.qa_mock"]
        );

        let err = merge_zones(
            State(state.clone()),
            Json(MergeZonesRequest {
                zone_ids: vec![lounge, whole],
                name: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_profile_update_and_status() {
        let (state, _dir) = test_state();
        let zones = refresh_zones(State(state.clone())).await.unwrap().0;
        let study = zone_named(&zones, "Study").id;
        let request = |json: &str| Json::<UpdateZoneRequest>::from_bytes(json.as_bytes());

        let zone = update_zone(
            State(state.clone()),
            Path(study),
            request(
                r#"{"weather_adjust": true, "wind_exposure": "high",
                    "sun_windows": [{"start": "08:00:00", "end": "11:00:00"}]}"#,
            ),
        )
        .await
        .unwrap()
        .0;
        assert!(zone.weather_adjust);
        assert_eq!(zone.wind_exposure, WindExposure::High);
        assert_eq!(zone.sun_windows.len(), 1);

        // A zero-length sun window is refused and changes nothing
        let err = update_zone(
            State(state.clone()),
            Path(study),
            request(
                r#"{"name": "Office", "sun_windows": [{"start": "08:00:00", "end": "08:00:00"}]}"#,
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            state.zones.read().unwrap().get(study).unwrap().name,
            "Study"
        );

        // With an On schedule and cold, windy weather, GET /zones explains the adjustment
        state.schedule.write().unwrap().active_mut().add_entry(
            crate::schedule::ScheduleEntry::new(
                "All day",
                TimePeriod::new(0, 0, 0, 0),
                HeatingState::On,
            )
            .with_target(20.0),
        );
        state.weather.write().unwrap().weather = Some(crate::weather::Weather {
            temperature: Some(-5.0),
            wind_speed: Some(50.0),
            cloud_coverage: Some(100.0),
        });
        let views = get_zones(State(state.clone())).await.0;
        let study_view = views.iter().find(|v| v.zone.id == study).unwrap();
        assert_eq!(study_view.status.scheduled_target, Some(20.0));
        assert_eq!(study_view.status.target, Some(22.5));
        let lounge_view = views.iter().find(|v| v.zone.name == "Lounge").unwrap();
        assert_eq!(
            lounge_view.status.target,
            Some(20.0),
            "weather adjust is off"
        );

        let json = serde_json::to_value(study_view).unwrap();
        assert_eq!(json["name"], "Study", "zone fields are flattened");
        assert_eq!(json["status"]["reasons"][0]["cause"], "cold");
    }

    #[tokio::test]
    async fn test_cold_warmups_update() {
        let (state, _dir) = test_state();
        let zones = refresh_zones(State(state.clone())).await.unwrap().0;
        let study = zone_named(&zones, "Study").id;
        let request = |json: &str| Json::<UpdateZoneRequest>::from_bytes(json.as_bytes());

        // Fields left out of the block get their defaults
        let zone = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"cold_warmups": {"enabled": true, "below_c": -3}}"#),
        )
        .await
        .unwrap()
        .0;
        assert!(zone.cold_warmups.enabled);
        assert_eq!(zone.cold_warmups.below_c, -3.0);
        assert_eq!(zone.cold_warmups.every_minutes, 180);

        // A burst as long as the interval is refused with readable text, and changes nothing
        let err = update_zone(
            State(state.clone()),
            Path(study),
            request(
                r#"{"weather_adjust": true,
                    "cold_warmups": {"enabled": true, "burst_minutes": 60, "every_minutes": 60}}"#,
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1
                .contains("burst_minutes must be less than every_minutes"),
            "{}",
            err.1
        );
        let zone = state.zones.read().unwrap().get(study).unwrap().clone();
        assert!(!zone.weather_adjust);
        assert_eq!(zone.cold_warmups.below_c, -3.0);
    }

    #[tokio::test]
    async fn test_unreadable_numbers_are_400_with_plain_text() {
        let (state, _dir) = test_state();
        let zones = refresh_zones(State(state.clone())).await.unwrap().0;
        let study = zone_named(&zones, "Study").id;
        let request = |json: &str| Json::<UpdateZoneRequest>::from_bytes(json.as_bytes());

        for (json, text) in [
            (
                r#"{"max_late_finish_minutes": -5}"#,
                "max_late_finish_minutes: must be a whole number of minutes, 0 or more, not -5",
            ),
            (
                r#"{"max_early_start_minutes": -1}"#,
                "max_early_start_minutes: must be a whole number of minutes, 0 or more, not -1",
            ),
            (
                r#"{"max_early_start_minutes": 2.5}"#,
                "max_early_start_minutes: must be a whole number of minutes, 0 or more, not 2.5",
            ),
            (
                r#"{"cold_warmups": {"enabled": true, "burst_minutes": -10}}"#,
                "cold_warmups.burst_minutes: must be a whole number of minutes, 0 or more, not -10",
            ),
            (
                r#"{"cold_warmups": {"every_minutes": "often"}}"#,
                "cold_warmups.every_minutes: must be a whole number of minutes, 0 or more, not \"often\"",
            ),
        ] {
            let err = update_zone(State(state.clone()), Path(study), request(json))
                .await
                .unwrap_err();
            assert_eq!(err, (StatusCode::BAD_REQUEST, text.to_string()), "{json}");
        }
        // Too big is still refused by the range check, with its own message
        let err = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"max_late_finish_minutes": 181}"#),
        )
        .await
        .unwrap_err();
        assert_eq!(err.1, "max_late_finish_minutes must be 0-180");
        // null leaves a limit as it is, as before
        let _ = update_zone(
            State(state.clone()),
            Path(study),
            request(r#"{"max_early_start_minutes": null}"#),
        )
        .await
        .unwrap();
        // Nothing changed, and a valid value still works
        assert_eq!(
            state
                .zones
                .read()
                .unwrap()
                .get(study)
                .unwrap()
                .max_early_start_minutes,
            30
        );
        let zone = update_zone(
            State(state),
            Path(study),
            request(r#"{"max_late_finish_minutes": 15}"#),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(zone.max_late_finish_minutes, 15);
    }
}
