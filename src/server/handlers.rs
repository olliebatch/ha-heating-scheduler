use crate::schedule::persistence;
use crate::schedule::{Schedule, ScheduleEntry};
use crate::server::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

pub async fn get_schedule(
    State(state): State<AppState>,
) -> Json<Schedule> {
    let schedule = state.schedule.read().unwrap().clone();
    Json(schedule)
}

pub async fn add_schedule_entry(
    State(state): State<AppState>,
    Json(payload): Json<ScheduleEntry>,
) -> Result<Json<Schedule>, (StatusCode, String)> {
    // Add entry to the in-memory schedule
    let updated_schedule = {
        let mut schedule = state.schedule.write().unwrap();
        schedule.add_entry(payload);
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