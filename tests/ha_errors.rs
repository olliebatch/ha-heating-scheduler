//! Home Assistant refusing a request is an error, not a silent success.
//! A fake HA on localhost refuses every service call and knows only some entities.

use axum::Router;
use axum::http::{StatusCode, Uri};
use ha_heating_scheduler::api_client::ApiClient;
use ha_heating_scheduler::climate::{ClimateEntity, DefaultClimate};

async fn answer(uri: Uri) -> (StatusCode, String) {
    match uri.path() {
        p if p.starts_with("/api/services/") => {
            (StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string())
        }
        // Off the network: HA keeps the entity but drops most of its attributes
        "/api/states/climate.flaky" => (
            StatusCode::OK,
            r#"{"entity_id": "climate.flaky", "state": "unavailable",
                "attributes": {"friendly_name": "TRV"}}"#
                .to_string(),
        ),
        _ => (StatusCode::NOT_FOUND, "{}".to_string()),
    }
}

/// A client for a fake HA on a free localhost port
async fn fake_ha() -> ApiClient {
    let app = Router::new().fallback(answer);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    ApiClient::new(
        reqwest::Url::parse(&url).unwrap(),
        "token".to_string(),
        None,
    )
}

#[tokio::test]
async fn test_refused_mode_change_is_an_error() {
    let client = fake_ha().await;
    let trv = DefaultClimate::new("climate.bedroom_trv".to_string());

    let on = trv.turn_on(&client).await;
    let off = trv.turn_off(&client).await;

    assert!(on.unwrap_err().to_string().contains("500"));
    assert!(off.unwrap_err().to_string().contains("500"));
}

#[tokio::test]
async fn test_reading_an_entity_ha_does_not_have_says_so() {
    let client = fake_ha().await;

    let err = client
        .fetch_climate_state("climate.missing")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("404"), "{err}");
}

#[tokio::test]
async fn test_reading_an_unavailable_entity_says_so() {
    let client = fake_ha().await;

    let err = client
        .fetch_climate_state("climate.flaky")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("unavailable"), "{err}");
}
