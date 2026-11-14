//! A Home Assistant that accepts a connection and never answers must give an error, not a hang.

use ha_heating_scheduler::api_client::{ApiClient, REQUEST_TIMEOUT};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

/// Accepts connections and holds them open without answering. Returns its base URL.
async fn silent_server() -> reqwest::Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            held.push(stream);
        }
    });
    reqwest::Url::parse(&url).unwrap()
}

#[tokio::test]
async fn test_a_read_that_is_never_answered_errors_within_the_timeout() {
    let client = ApiClient::with_timeout(
        silent_server().await,
        "token".into(),
        None,
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let result = client.fetch_climate_state("climate.trv").await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn test_a_post_that_is_never_answered_errors_within_the_timeout() {
    let client = ApiClient::with_timeout(
        silent_server().await,
        "token".into(),
        None,
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let result = client
        .post("/api/services/climate/set_hvac_mode", &json!({}))
        .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn test_the_default_timeout_is_about_ten_seconds() {
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
}
