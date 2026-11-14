//! The next-heat sensors are posted to Home Assistant, but not in a dry run, and a failed post
//! never gets in the way. A fake HA on localhost records every request.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use ha_heating_scheduler::api_client::ApiClient;
use ha_heating_scheduler::dry_run::DryRun;
use ha_heating_scheduler::next_heat::{NextHeat, publish};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct FakeHa {
    /// Every request: method, path and body
    requests: Arc<Mutex<Vec<(String, String, Value)>>>,
    /// Refuse posts for this entity
    refuse: Arc<Mutex<Option<String>>>,
}

async fn answer(
    State(ha): State<FakeHa>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> (StatusCode, String) {
    let path = uri.path().to_string();
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    ha.requests
        .lock()
        .unwrap()
        .push((method.to_string(), path.clone(), body));
    let refused = ha.refuse.lock().unwrap().clone();
    if refused.is_some_and(|r| path.ends_with(&r)) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string());
    }
    // HA answers 201 the first time a state is posted, 200 after
    (StatusCode::CREATED, "{}".to_string())
}

async fn fake_ha() -> (FakeHa, reqwest::Url) {
    let ha = FakeHa::default();
    let app = Router::new().fallback(answer).with_state(ha.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (ha, reqwest::Url::parse(&url).unwrap())
}

fn sensor(zone: &str, state: &str) -> NextHeat {
    NextHeat {
        entity_id: format!("sensor.{zone}_next_heat"),
        state: state.to_string(),
        attributes: json!({"target_temperature": 22.0}),
    }
}

#[tokio::test]
async fn test_each_sensor_is_posted_to_the_states_api() {
    let (ha, url) = fake_ha().await;
    let client = ApiClient::new(url, "fake".to_string(), None);
    publish(
        &client,
        &[
            sensor("lounge", "2026-11-11T19:00:00+00:00"),
            sensor("study", "unknown"),
        ],
    )
    .await;

    let requests = ha.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[0].1, "/api/states/sensor.lounge_next_heat");
    assert_eq!(
        requests[0].2,
        json!({"state": "2026-11-11T19:00:00+00:00", "attributes": {"target_temperature": 22.0}})
    );
    assert_eq!(requests[1].1, "/api/states/sensor.study_next_heat");
    assert_eq!(requests[1].2["state"], "unknown");
}

#[tokio::test]
async fn test_a_dry_run_posts_nothing() {
    let (ha, url) = fake_ha().await;
    let dry_run = Arc::new(DryRun::default());
    let client = ApiClient::new(url, "fake".to_string(), Some(Arc::clone(&dry_run)));
    publish(&client, &[sensor("lounge", "unknown")]).await;

    assert!(ha.requests.lock().unwrap().is_empty(), "nothing reached HA");
    // It's recorded as a would-be call, like any other change
    let calls = dry_run.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].endpoint, "/api/states/sensor.lounge_next_heat");
}

#[tokio::test]
async fn test_a_failed_post_is_skipped_and_the_rest_still_go() {
    let (ha, url) = fake_ha().await;
    *ha.refuse.lock().unwrap() = Some("sensor.lounge_next_heat".to_string());
    let client = ApiClient::new(url, "fake".to_string(), None);
    // Returns normally (it can't fail), and the next zone is still posted
    publish(
        &client,
        &[sensor("lounge", "unknown"), sensor("study", "unknown")],
    )
    .await;
    assert_eq!(ha.requests.lock().unwrap().len(), 2);

    // And an unreachable HA is just logged
    let gone = ApiClient::new(
        reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
        "fake".to_string(),
        None,
    );
    publish(&gone, &[sensor("lounge", "unknown")]).await;
}
