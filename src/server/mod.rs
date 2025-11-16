use crate::server::handlers::{add_schedule_entry, delete_schedule_entry, get_schedule};
use crate::ScheduleState;
use axum::routing::{delete, post};
use axum::{routing::get, Router};
use tower_http::cors::CorsLayer;

mod handlers;

#[derive(Clone, Debug)]
pub struct AppState {
    schedule: ScheduleState,
    schedule_file_path: String,
}

pub async fn start_server(schedule: ScheduleState, schedule_file_path: String) {
    let app_state = AppState {
        schedule,
        schedule_file_path,
    };
    let cors_layer = CorsLayer::permissive();
    let app = Router::new()
        .route("/schedule", get(get_schedule))
        .route("/schedule", post(add_schedule_entry))
        .route("/schedule/{id}", delete(delete_schedule_entry))
        .layer(cors_layer)
        .with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
