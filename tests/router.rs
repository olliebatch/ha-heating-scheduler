//! The real router, driven with `oneshot`: no server, no Home Assistant. Catches a route that is
//! missing, mistyped or registered under the wrong method, which the handler tests can't.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ha_heating_scheduler::climate::{ClimateEntityWrapper, MockClimate};
use ha_heating_scheduler::schedule::sets::ScheduleSets;
use ha_heating_scheduler::schedule::{HeatingState, Schedule};
use ha_heating_scheduler::server::{AppState, router};
use ha_heating_scheduler::shadow::Shadow;
use ha_heating_scheduler::weather::{MockWeather, WeatherStatus};
use ha_heating_scheduler::zones::Zones;
use ha_heating_scheduler::zones::areas::MockAreas;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

/// Every route in `src/server/mod.rs`: method and path. Add a line when you add a route.
/// `{id}`-style segments are filled with an id that names nothing.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/schedule"),
    ("POST", "/schedule"),
    ("DELETE", "/schedule/{id}"),
    ("GET", "/schedule_sets"),
    ("POST", "/schedule_sets"),
    ("PATCH", "/schedule_sets/{id}"),
    ("DELETE", "/schedule_sets/{id}"),
    ("POST", "/schedule_sets/{id}/activate"),
    ("POST", "/schedule_sets/{id}/entries"),
    ("PUT", "/schedule_sets/{id}/entries/{entry_id}"),
    ("DELETE", "/schedule_sets/{id}/entries/{entry_id}"),
    ("GET", "/zones"),
    ("POST", "/zones"),
    ("POST", "/zones/refresh"),
    ("POST", "/zones/merge"),
    ("POST", "/zones/{id}/preview"),
    ("PATCH", "/zones/{id}"),
    ("DELETE", "/zones/{id}"),
    ("GET", "/weather"),
    ("PUT", "/weather"),
    ("GET", "/entities"),
    ("POST", "/entities"),
    ("DELETE", "/entities"),
    ("GET", "/entities/available"),
    ("POST", "/boost_all"),
    ("POST", "/boost"),
    ("GET", "/dry_run"),
    ("GET", "/decisions"),
    // Debug builds only (`cfg(debug_assertions)` in the router)
    #[cfg(debug_assertions)]
    ("POST", "/weather/mock"),
];

const ID: &str = "00000000-0000-0000-0000-000000000000";

/// The router over mock climates, with its files in `dir`
fn app(dir: &std::path::Path) -> axum::Router {
    let file = |name: &str| dir.join(name).to_string_lossy().to_string();
    let climate = ClimateEntityWrapper::Mock(MockClimate::new(
        "climate.lounge_trv".to_string(),
        HeatingState::Off,
    ));
    let weather = MockWeather::default();
    router(AppState {
        schedule: Arc::new(RwLock::new(ScheduleSets::from_schedule(Schedule::new(
            "Default",
        )))),
        schedule_sets_file_path: file("schedule_sets.json"),
        climate_entities: Arc::new(RwLock::new(vec![climate])),
        entities_file_path: file("entities.json"),
        zones: Arc::new(RwLock::new(Zones::default())),
        zones_file_path: file("zones.json"),
        area_source: Arc::new(MockAreas::example()),
        weather: Arc::new(RwLock::new(WeatherStatus::default())),
        weather_file_path: file("weather.json"),
        mock_weather: Some(Arc::clone(&weather.weather)),
        dry_run: None,
        last_sent_targets: Default::default(),
        memories: Default::default(),
        shadow: Arc::new(RwLock::new(Shadow::load(file("decisions.json")))),
    })
}

/// Send an empty request. Returns the status and whether the body was empty.
async fn send(app: &axum::Router, method: &str, path: &str) -> (StatusCode, bool) {
    let request = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.is_empty())
}

fn fill(path: &str) -> String {
    path.replace("{id}", ID).replace("{entry_id}", ID)
}

/// The router's own 404 and 405 have an empty body. A handler's "not found" (an unknown zone, say)
/// carries a message, so it counts as routed.
fn is_unrouted(status: StatusCode, body_empty: bool) -> bool {
    (status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED) && body_empty
}

#[tokio::test]
async fn test_every_route_is_served_at_its_method_and_path() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for (method, path) in ROUTES {
        let (status, body_empty) = send(&app, method, &fill(path)).await;
        assert!(
            !is_unrouted(status, body_empty),
            "{method} {path} isn't routed: got {status}"
        );
    }
}

#[tokio::test]
async fn test_a_wrong_method_is_405() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for (_, path) in ROUTES {
        // A method none of the routes use
        let (status, _) = send(&app, "TRACE", &fill(path)).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "TRACE {path} should be 405"
        );
    }
    // The methods that are registered for other routes on the same path
    for (method, path) in ROUTES {
        for other in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            if ROUTES.iter().any(|(m, p)| p == path && *m == other) || other == *method {
                continue;
            }
            let (status, _) = send(&app, other, &fill(path)).await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{other} {path} should be 405"
            );
        }
    }
}

#[tokio::test]
async fn test_an_unknown_path_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for path in ["/", "/nope", "/schedulez", "/zones/x/nope", "/boost/extra"] {
        let (status, _) = send(&app, "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {path} should be 404");
    }
}
