use crate::schedule::{Schedule, ScheduleEntry};
use crate::server::AppState;
use axum::extract::State;
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
) -> Json<Schedule> {
    let mut schedule = state.schedule.write().unwrap();
    schedule.add_entry(payload);
    Json(schedule.clone())
}