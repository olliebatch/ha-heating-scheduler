use crate::climate::{ClimateEntity, ClimateEntityWrapper};
use crate::server::handlers::{
    activate_schedule_set, add_entities, add_schedule_entry, add_set_entry, boost, boost_all,
    create_schedule_set, delete_schedule_entry, delete_schedule_set, delete_set_entry,
    get_entities, get_schedule, get_schedule_sets, remove_entity, rename_schedule_set,
};
use crate::server::zones::{
    create_zone, delete_zone, get_zones, merge_zones, refresh_zones, update_zone,
};
use crate::weather::Weather;
use crate::zones::areas::AreaSource;
use crate::{ScheduleState, WeatherState, ZonesState};
use axum::routing::{delete, patch, post};
use axum::{Router, routing::get};
use std::sync::{Arc, RwLock};
use tower_http::cors::CorsLayer;

mod handlers;
mod weather;
mod zones;

#[derive(Clone)]
pub struct AppState<T: ClimateEntity + Clone> {
    pub schedule: ScheduleState,
    pub schedule_sets_file_path: String,
    pub climate_entities: Arc<RwLock<Vec<T>>>,
    pub entities_file_path: String,
    pub zones: ZonesState,
    pub zones_file_path: String,
    pub area_source: Arc<dyn AreaSource>,
    pub weather: WeatherState,
    pub weather_file_path: String,
    /// The mock weather to set in debug builds; None in release
    pub mock_weather: Option<Arc<RwLock<Weather>>>,
}

pub async fn start_server(app_state: AppState<ClimateEntityWrapper>) {
    let cors_layer = CorsLayer::permissive();
    let app = Router::new()
        .route("/schedule", get(get_schedule::<ClimateEntityWrapper>))
        .route(
            "/schedule",
            post(add_schedule_entry::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule/{id}",
            delete(delete_schedule_entry::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule_sets",
            get(get_schedule_sets::<ClimateEntityWrapper>)
                .post(create_schedule_set::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule_sets/{id}",
            patch(rename_schedule_set::<ClimateEntityWrapper>)
                .delete(delete_schedule_set::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule_sets/{id}/activate",
            post(activate_schedule_set::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule_sets/{id}/entries",
            post(add_set_entry::<ClimateEntityWrapper>),
        )
        .route(
            "/schedule_sets/{id}/entries/{entry_id}",
            delete(delete_set_entry::<ClimateEntityWrapper>),
        )
        .route(
            "/zones",
            get(get_zones::<ClimateEntityWrapper>).post(create_zone::<ClimateEntityWrapper>),
        )
        .route(
            "/zones/refresh",
            post(refresh_zones::<ClimateEntityWrapper>),
        )
        .route("/zones/merge", post(merge_zones::<ClimateEntityWrapper>))
        .route(
            "/zones/{id}",
            patch(update_zone::<ClimateEntityWrapper>).delete(delete_zone::<ClimateEntityWrapper>),
        )
        .route(
            "/weather",
            get(weather::get_weather::<ClimateEntityWrapper>)
                .put(weather::set_weather_entity::<ClimateEntityWrapper>),
        )
        .route("/entities", get(get_entities::<ClimateEntityWrapper>))
        .route("/entities", post(add_entities))
        .route("/entities", delete(remove_entity))
        .route("/boost_all", post(boost_all::<ClimateEntityWrapper>))
        .route("/boost", post(boost::<ClimateEntityWrapper>));

    // Set the mock weather by hand (debug builds only)
    #[cfg(debug_assertions)]
    let app = app.route(
        "/weather/mock",
        post(weather::set_mock_weather::<ClimateEntityWrapper>),
    );

    let app = app.layer(cors_layer).with_state(app_state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
