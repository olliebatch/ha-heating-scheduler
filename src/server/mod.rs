use crate::schedule::Schedule;
use crate::server::handlers::get_schedule;
use axum::{routing::get, Router};
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;

mod handlers;

pub type AppState = Arc<Mutex<Schedule>>;

pub async fn start_server(schedule: Schedule) {
    let state: AppState = Arc::new(Mutex::new(schedule));
    let cors_layer = CorsLayer::permissive();
    let app = Router::new().route("/", get(|| async { "Hello, World!" }))
        .route("/schedule", get(get_schedule))
        .layer(cors_layer)
        .with_state(state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
