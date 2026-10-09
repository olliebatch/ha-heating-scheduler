use crate::climate::ClimateEntity;
use crate::server::AppState;
use crate::zones::{Zone, ZoneError, Zones, save_zones};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
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

/// Apply `change` to the zones under the write lock, then persist them.
/// `change` must not modify the zones when it returns an error.
fn update_zones<T: ClimateEntity + Clone, R>(
    state: &AppState<T>,
    change: impl FnOnce(&mut Zones, &[String]) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    let managed = managed_entity_ids(state);
    let (result, snapshot) = {
        let mut zones = state.zones.write().unwrap();
        let result = change(&mut zones, &managed)?;
        (result, zones.clone())
    };

    if let Err(e) = save_zones(&snapshot, &state.zones_file_path) {
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

pub async fn get_zones<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Json<Vec<Zone>> {
    Json(state.zones.read().unwrap().zones.clone())
}

/// Ask Home Assistant for its areas again and rebuild the zones
pub async fn refresh_zones<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
) -> Result<Json<Vec<Zone>>, ApiError> {
    let areas = state.area_source.fetch_areas().await.map_err(|e| {
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
}

pub async fn update_zone<T: ClimateEntity + Clone>(
    State(state): State<AppState<T>>,
    Path(zone_id): Path<Uuid>,
    Json(payload): Json<UpdateZoneRequest>,
) -> Result<Json<Zone>, ApiError> {
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
        if let Some(name) = &payload.name {
            if name.trim().is_empty() {
                return Err(zone_error(ZoneError::Invalid(
                    "Name must not be empty".to_string(),
                )));
            }
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

        let request = |json: &str| Json(serde_json::from_str::<UpdateZoneRequest>(json).unwrap());

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
}
