use crate::schedule::Schedule;
use crate::server::AppState;
use axum::extract::State;
use axum::Json;

pub async fn get_schedule(
    State(state): State<AppState>,
) -> Json<Schedule> {
    let schedules = state.lock().unwrap().clone();
    Json(schedules)
}