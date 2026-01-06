use crate::ScheduleState;
use crate::climate::{ClimateEntity, ClimateEntityWrapper};
use crate::server::handlers::{
    add_entities, add_schedule_entry, boost, boost_all, delete_schedule_entry,
    get_entities, get_schedule, remove_entity,
};
use axum::routing::{delete, post};
use axum::{Router, routing::get};
use std::sync::{Arc, RwLock};
use tower_http::cors::CorsLayer;

mod handlers;

#[derive(Clone, Debug)]
pub struct AppState<T: ClimateEntity + Clone> {
    pub schedule: ScheduleState,
    pub schedule_file_path: String,
    pub climate_entities: Arc<RwLock<Vec<T>>>,
    pub entities_file_path: String,
}

pub async fn start_server(
    schedule: ScheduleState,
    schedule_file_path: String,
    climate_entities: Arc<RwLock<Vec<ClimateEntityWrapper>>>,
    entities_file_path: String,
) {
    let app_state = AppState {
        schedule,
        schedule_file_path,
        climate_entities,
        entities_file_path,
    };
    let cors_layer = CorsLayer::permissive();
    let app = Router::new()
        .route("/schedule", get(get_schedule::<ClimateEntityWrapper>))
        .route("/schedule", post(add_schedule_entry::<ClimateEntityWrapper>))
        .route("/schedule/{id}", delete(delete_schedule_entry::<ClimateEntityWrapper>))
        .route("/entities", get(get_entities::<ClimateEntityWrapper>))
        .route("/entities", post(add_entities))
        .route("/entities", delete(remove_entity))
        .route("/boost_all", post(boost_all::<ClimateEntityWrapper>))
        .route("/boost", post(boost::<ClimateEntityWrapper>))
        .layer(cors_layer)
        .with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
