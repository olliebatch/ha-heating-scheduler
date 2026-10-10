//! A dry run never sends a service call to Home Assistant, while reads still reach it.
//! A fake HA on localhost records every request.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, Uri};
use chrono::NaiveTime;
use ha_heating_scheduler::api_client::ApiClient;
use ha_heating_scheduler::climate::{BoostInfo, ClimateEntity, DefaultClimate};
use ha_heating_scheduler::dry_run::DryRun;
use ha_heating_scheduler::schedule::HeatingState;
use ha_heating_scheduler::scheduler::{Scheduled, apply_schedule_to_entity};
use ha_heating_scheduler::weather::{HomeAssistantWeather, WeatherSource};
use ha_heating_scheduler::zones::areas::{AreaSource, HomeAssistantAreas};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct FakeHa {
    /// Every request, as "METHOD /path"
    requests: Arc<Mutex<Vec<String>>>,
    /// What the climate reports: "off" or "heat"
    mode: Arc<Mutex<&'static str>>,
}

async fn answer(State(ha): State<FakeHa>, method: Method, uri: Uri, _body: Bytes) -> String {
    let path = uri.path().to_string();
    ha.requests.lock().unwrap().push(format!("{method} {path}"));
    if let Some(entity_id) = path.strip_prefix("/api/states/climate.") {
        let mode = *ha.mode.lock().unwrap();
        return json!({
            "entity_id": format!("climate.{entity_id}"),
            "state": mode,
            "attributes": {
                "hvac_modes": ["off", "heat"], "min_temp": 5.0, "max_temp": 30.0,
                "current_temperature": 19.0, "temperature": 18.0, "friendly_name": "TRV"
            },
            "last_changed": "", "last_reported": "", "last_updated": "",
            "context": {"id": "", "parent_id": null, "user_id": null}
        })
        .to_string();
    }
    if path.starts_with("/api/states/weather.") {
        return json!({"state": "cloudy", "attributes": {"temperature": 4.0, "temperature_unit": "°C"}})
            .to_string();
    }
    "[]".to_string()
}

/// A fake HA on a free localhost port
async fn fake_ha() -> (FakeHa, reqwest::Url) {
    let ha = FakeHa::default();
    *ha.mode.lock().unwrap() = "off";
    let app = Router::new().fallback(answer).with_state(ha.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (ha, reqwest::Url::parse(&url).unwrap())
}

fn scheduled(state: HeatingState, target: Option<f64>) -> Scheduled {
    Scheduled {
        state,
        target_temp: target,
    }
}

/// Turn on, change the target, boost during an Off period, then turn off
async fn run_through(client: &ApiClient, ha: &FakeHa) {
    let mut trv = DefaultClimate::new("climate.lounge_trv".to_string());
    let mut last_sent = None;
    let on = |t| scheduled(HeatingState::On, Some(t));

    apply_schedule_to_entity(&mut trv, &on(21.0), 20.0, client, &mut last_sent).await;
    apply_schedule_to_entity(&mut trv, &on(21.0), 20.0, client, &mut last_sent).await;
    apply_schedule_to_entity(&mut trv, &on(22.0), 20.0, client, &mut last_sent).await;

    // Boosted all day, so the time of day doesn't matter
    trv.set_boost(Some(BoostInfo {
        boost_start: NaiveTime::MIN,
        boost_end: NaiveTime::from_hms_opt(23, 59, 59).unwrap(),
    }));
    apply_schedule_to_entity(
        &mut trv,
        &scheduled(HeatingState::Off, None),
        20.0,
        client,
        &mut last_sent,
    )
    .await;
    trv.set_boost(None);

    // HA now reports it heating, so Off turns it off
    *ha.mode.lock().unwrap() = "heat";
    apply_schedule_to_entity(
        &mut trv,
        &scheduled(HeatingState::Off, None),
        20.0,
        client,
        &mut last_sent,
    )
    .await;
}

fn count(ha: &FakeHa, prefix: &str) -> usize {
    ha.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with(prefix))
        .count()
}

#[tokio::test]
async fn test_dry_run_sends_no_service_calls_but_still_reads() {
    let (ha, url) = fake_ha().await;
    let dry_run = Arc::new(DryRun::default());
    let client =
        || ApiClient::new(url.clone(), "fake".to_string()).with_dry_run(Some(Arc::clone(&dry_run)));

    run_through(&client(), &ha).await;
    HomeAssistantAreas {
        api_client: client(),
    }
    .fetch_areas()
    .await
    .unwrap();
    let weather = HomeAssistantWeather {
        api_client: client(),
    }
    .fetch(Some("weather.home"))
    .await
    .unwrap();

    assert_eq!(
        count(&ha, "POST /api/services"),
        0,
        "{:?}",
        ha.requests.lock().unwrap()
    );
    assert_eq!(count(&ha, "GET /api/states/climate.lounge_trv"), 5);
    assert_eq!(count(&ha, "POST /api/template"), 1);
    assert_eq!(count(&ha, "GET /api/states/weather.home"), 1);
    assert_eq!(weather.unwrap().temperature, Some(4.0));

    // One entry per change, newest first, however many ticks asked for it
    let calls: Vec<(String, u32)> = dry_run
        .calls()
        .into_iter()
        .map(|c| (c.summary, c.repeats))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("would turn climate.lounge_trv off".to_string(), 0),
            ("would set climate.lounge_trv to 20 °C".to_string(), 0),
            ("would set climate.lounge_trv to 22 °C".to_string(), 0),
            ("would set climate.lounge_trv to 21 °C".to_string(), 1),
            ("would turn climate.lounge_trv on (heat)".to_string(), 3),
        ]
    );
}

#[tokio::test]
async fn test_without_dry_run_the_same_steps_reach_ha() {
    let (ha, url) = fake_ha().await;
    run_through(&ApiClient::new(url, "fake".to_string()), &ha).await;
    // The fake HA keeps reporting Off, so each On step turns it on again
    assert_eq!(count(&ha, "POST /api/services/climate/set_hvac_mode"), 5);
    assert!(count(&ha, "POST /api/services/climate/set_temperature") >= 3);
}
