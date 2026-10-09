use ha_heating_scheduler::climate::ClimateEntity;
use ha_heating_scheduler::climate::ClimateEntityWrapper;
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::climate::DefaultClimate;
#[cfg(debug_assertions)]
use ha_heating_scheduler::climate::MockClimate;
use ha_heating_scheduler::config;
#[cfg(debug_assertions)]
use ha_heating_scheduler::schedule::HeatingState;
use ha_heating_scheduler::schedule::persistence;
use ha_heating_scheduler::scheduler::{SchedulerState, run_scheduler};
use ha_heating_scheduler::server::start_server;
use ha_heating_scheduler::zones::areas::AreaSource;
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::zones::areas::HomeAssistantAreas;
#[cfg(debug_assertions)]
use ha_heating_scheduler::zones::areas::MockAreas;
use ha_heating_scheduler::{ScheduleState, ZonesState, api_client, zones};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load config with persisted entities
    let config = config::Config::from_env_with_persisted_entities()?;
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    );

    let data_dir = Path::new(&config.data_path);
    std::fs::create_dir_all(data_dir)?;

    let schedule_file_path = data_dir.join("schedule.json");
    let schedule_sets_file_path = data_dir.join("schedule_sets.json");
    let entities_file_path = data_dir.join("entities.json");
    let zones_file_path = data_dir.join("zones.json");

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
                ),
            })
        }
    };
    let mut zones = zones::load_zones(&zones_file_path)?;
    let areas = match area_source.fetch_areas().await {
        Ok(areas) => Some(areas),
        Err(e) => {
            eprintln!("Failed to fetch areas, using the last known ones: {}", e);
            None
        }
    };
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

    let schedule: ScheduleState = Arc::new(RwLock::new(schedule_sets));
    let api_task = tokio::spawn(start_server(
        Arc::clone(&schedule),
        schedule_sets_file_path.to_string_lossy().to_string(),
        Arc::clone(&climate_entities),
        entities_file_path.to_string_lossy().to_string(),
        Arc::clone(&zones),
        zones_file_path.to_string_lossy().to_string(),
        area_source,
    ));

    let scheduler_task = tokio::spawn(run_scheduler(SchedulerState {
        api_client,
        schedule,
        zones,
        climate_entities: Arc::clone(&climate_entities),
    }));

    tokio::try_join!(api_task, scheduler_task).unwrap();
    Ok(())
}
