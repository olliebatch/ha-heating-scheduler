use ha_heating_scheduler::climate::ClimateEntity;
use ha_heating_scheduler::climate::ClimateEntityWrapper;
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::climate::DefaultClimate;
#[cfg(debug_assertions)]
use ha_heating_scheduler::climate::MockClimate;
use ha_heating_scheduler::config;
use ha_heating_scheduler::dry_run::{self, DryRun};
#[cfg(debug_assertions)]
use ha_heating_scheduler::schedule::HeatingState;
use ha_heating_scheduler::schedule::persistence;
use ha_heating_scheduler::scheduler::{SchedulerState, run_scheduler};
use ha_heating_scheduler::server::{AppState, start_server};
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::weather::HomeAssistantWeather;
#[cfg(debug_assertions)]
use ha_heating_scheduler::weather::MockWeather;
use ha_heating_scheduler::weather::WeatherSource;
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::zones::areas::HomeAssistantAreas;
#[cfg(debug_assertions)]
use ha_heating_scheduler::zones::areas::MockAreas;
use ha_heating_scheduler::zones::areas::{AreaSource, DISCOVERY_TIMEOUT, fetch_areas_with_timeout};
use ha_heating_scheduler::{ScheduleState, WeatherState, ZonesState, api_client, weather, zones};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load config with persisted entities
    let config = config::Config::from_env_with_persisted_entities()?;
    let port = config::port(std::env::var("PORT").ok())?;

    // Dry run: read Home Assistant, but record the calls that would change it instead of sending them
    let dry_run = dry_run::enabled(std::env::args(), std::env::var("DRY_RUN").ok())
        .then(|| Arc::new(DryRun::default()));
    if dry_run.is_some() {
        println!("\n=== DRY RUN: NOT CONTROLLING THE HEATING ===");
        println!("Reading Home Assistant only; would-be commands are logged at GET /dry_run\n");
    }
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    )
    .with_dry_run(dry_run.clone());

    let data_dir = Path::new(&config.data_path);
    std::fs::create_dir_all(data_dir)?;

    let schedule_file_path = data_dir.join("schedule.json");
    let schedule_sets_file_path = data_dir.join("schedule_sets.json");
    let entities_file_path = data_dir.join("entities.json");
    let zones_file_path = data_dir.join("zones.json");
    let weather_file_path = data_dir.join("weather.json");

    let schedule_sets =
        persistence::load_or_migrate(&schedule_sets_file_path, &schedule_file_path)?;
    // Use mock climate entities in debug mode, real ones in release mode
    let climate_entities: Arc<RwLock<Vec<ClimateEntityWrapper>>> = {
        #[cfg(debug_assertions)]
        {
            println!("=== DEBUG MODE: Using Mock Climate Entities ===");
            Arc::new(RwLock::new(
                config
                    .climate_entities
                    .into_iter()
                    .map(|entity_id| {
                        ClimateEntityWrapper::Mock(MockClimate::new(entity_id, HeatingState::Off))
                    })
                    .collect(),
            ))
        }

        #[cfg(not(debug_assertions))]
        {
            println!("=== PRODUCTION MODE: Using Real Climate Entities ===");
            Arc::new(RwLock::new(
                config
                    .climate_entities
                    .into_iter()
                    .map(|entity_id| ClimateEntityWrapper::Real(DefaultClimate::new(entity_id)))
                    .collect(),
            ))
        }
    };

    let schedule = schedule_sets.active();
    println!(
        "=== Loaded {} schedule set(s), active: {} ===",
        schedule_sets.sets.len(),
        schedule.name
    );
    println!("Total entries: {}", schedule.entries.len());
    for (i, entry) in schedule.entries.iter().enumerate() {
        println!(
            "  {}. {} | {} | {:?}",
            i + 1,
            entry.time_period,
            entry.name,
            entry.heating_state
        );
    }
    println!();
    // Zones come from Home Assistant Areas (mock areas in debug mode)
    let area_source: Arc<dyn AreaSource> = {
        #[cfg(debug_assertions)]
        {
            Arc::new(MockAreas::example())
        }
        #[cfg(not(debug_assertions))]
        {
            Arc::new(HomeAssistantAreas {
                api_client: api_client::ApiClient::new(
                    reqwest::Url::parse(&config.ha_url)?,
                    config.ha_token.clone(),
                )
                .with_dry_run(dry_run.clone()),
            })
        }
    };
    // Start with the saved zones if Home Assistant is slow or down, and keep trying in the background
    let mut zones = zones::load_zones(&zones_file_path)?;
    let areas = match fetch_areas_with_timeout(area_source.as_ref(), DISCOVERY_TIMEOUT).await {
        Ok(areas) => Some(areas),
        Err(e) => {
            eprintln!(
                "Failed to fetch areas, starting with the saved zones: {}",
                e
            );
            None
        }
    };
    let discovery_failed = areas.is_none();
    let managed: Vec<String> = climate_entities
        .read()
        .unwrap()
        .iter()
        .map(|e| e.get_entity_id().to_string())
        .collect();
    zones.reconcile(areas, &managed);
    zones::save_zones(&zones, &zones_file_path)?;
    for zone in &zones.zones {
        println!("Zone {}: {}", zone.name, zone.entity_ids.join(", "));
    }
    let zones: ZonesState = Arc::new(RwLock::new(zones));
    if discovery_failed {
        tokio::spawn(retry_area_discovery(
            Arc::clone(&area_source),
            Arc::clone(&zones),
            zones_file_path.clone(),
            Arc::clone(&climate_entities),
        ));
    }

    // Weather from a Home Assistant weather entity (mock weather in debug mode)
    let weather_config = weather::load_weather_config(&weather_file_path)?;
    let weather: WeatherState = Arc::new(RwLock::new(weather::WeatherStatus {
        entity_id: weather_config.entity_id,
        ..Default::default()
    }));
    #[cfg(debug_assertions)]
    let mock_weather = {
        let mock = MockWeather::default();
        let handle = Arc::clone(&mock.weather);
        (Arc::new(mock) as Arc<dyn WeatherSource>, Some(handle))
    };
    #[cfg(not(debug_assertions))]
    let mock_weather = (
        Arc::new(HomeAssistantWeather {
            api_client: api_client::ApiClient::new(
                reqwest::Url::parse(&config.ha_url)?,
                config.ha_token.clone(),
            )
            .with_dry_run(dry_run.clone()),
        }) as Arc<dyn WeatherSource>,
        None,
    );
    let (weather_source, mock_weather) = mock_weather;

    let schedule: ScheduleState = Arc::new(RwLock::new(schedule_sets));
    let api_task = tokio::spawn(start_server(
        AppState {
            schedule: Arc::clone(&schedule),
            schedule_sets_file_path: schedule_sets_file_path.to_string_lossy().to_string(),
            climate_entities: Arc::clone(&climate_entities),
            entities_file_path: entities_file_path.to_string_lossy().to_string(),
            zones: Arc::clone(&zones),
            zones_file_path: zones_file_path.to_string_lossy().to_string(),
            area_source,
            weather: Arc::clone(&weather),
            weather_file_path: weather_file_path.to_string_lossy().to_string(),
            mock_weather,
            dry_run,
        },
        port,
    ));

    let scheduler_task = tokio::spawn(run_scheduler(SchedulerState {
        api_client,
        schedule,
        zones,
        weather,
        weather_source,
        climate_entities: Arc::clone(&climate_entities),
    }));

    tokio::try_join!(api_task, scheduler_task).unwrap();
    Ok(())
}

/// Retry area discovery every minute until it works, then rebuild and save the zones
async fn retry_area_discovery(
    area_source: Arc<dyn AreaSource>,
    zones: ZonesState,
    zones_file_path: std::path::PathBuf,
    climate_entities: Arc<RwLock<Vec<ClimateEntityWrapper>>>,
) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        match fetch_areas_with_timeout(area_source.as_ref(), DISCOVERY_TIMEOUT).await {
            Ok(areas) => {
                let managed: Vec<String> = climate_entities
                    .read()
                    .unwrap()
                    .iter()
                    .map(|e| e.get_entity_id().to_string())
                    .collect();
                // Save under the lock, like the server does
                let mut zones = zones.write().unwrap();
                zones.reconcile(Some(areas), &managed);
                if let Err(e) = zones::save_zones(&zones, &zones_file_path) {
                    eprintln!("Failed to save zones after discovery: {}", e);
                }
                println!("Area discovery succeeded; zones updated");
                return;
            }
            Err(e) => eprintln!("Area discovery failed again, retrying in 60 s: {}", e),
        }
    }
}
