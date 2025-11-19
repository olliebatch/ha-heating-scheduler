use crate::climate::ClimateEntity;
use crate::server::handlers::{add_schedule_entry, boost_all, delete_schedule_entry, get_schedule};
use crate::ScheduleState;
use axum::routing::{delete, post};
use axum::{routing::get, Router};
use std::sync::{Arc, RwLock};
use tower_http::cors::CorsLayer;

mod handlers;

#[derive(Clone, Debug)]
pub struct AppState<T: ClimateEntity + Clone> {
    schedule: ScheduleState,
    schedule_file_path: String,
    climate_entities: Arc<RwLock<Vec<T>>>,
}

pub async fn start_server<T: ClimateEntity + Clone + 'static>(schedule: ScheduleState, schedule_file_path: String, climate_entities: Arc<RwLock<Vec<T>>>) {
    let app_state = AppState {
        schedule,
        schedule_file_path,
        climate_entities,
    };
    let cors_layer = CorsLayer::permissive();
    let app = Router::new()
        .route("/schedule", get(get_schedule))
        .route("/schedule", post(add_schedule_entry))
        .route("/schedule/{id}", delete(delete_schedule_entry))
        .route("/boost_all", post(boost_all))
        .layer(cors_layer)
        .with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
