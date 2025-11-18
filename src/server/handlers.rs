use crate::schedule::persistence;
use crate::schedule::{Schedule, ScheduleEntry, ScheduleEntryRequest};
use crate::server::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use uuid::Uuid;

pub async fn get_schedule(
    State(state): State<AppState>,
) -> Json<Schedule> {
    let schedule = state.schedule.read().unwrap().clone();
    Json(schedule)
}

pub async fn add_schedule_entry(
    State(state): State<AppState>,
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

pub async fn delete_schedule_entry(
    State(state): State<AppState>,
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