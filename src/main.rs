use ha_heating_scheduler::climate::ClimateEntityWrapper;
#[cfg(not(debug_assertions))]
use ha_heating_scheduler::climate::DefaultClimate;
#[cfg(debug_assertions)]
use ha_heating_scheduler::climate::MockClimate;
use ha_heating_scheduler::config;
use ha_heating_scheduler::schedule::persistence;
#[cfg(debug_assertions)]
use ha_heating_scheduler::schedule::HeatingState;
use ha_heating_scheduler::scheduler::{run_scheduler, SchedulerState};
use ha_heating_scheduler::server::start_server;
use ha_heating_scheduler::{api_client, ScheduleState};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env();
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    );

    let data_dir = Path::new(&config.data_path);
    std::fs::create_dir_all(data_dir)?;

    let schedule_file_path = data_dir.join("schedule.json");

    let schedule = persistence::load_or_create_default(&schedule_file_path)?;
    // Use mock climate entities in debug mode, real ones in release mode
    let climate_entities: Arc<RwLock<Vec<ClimateEntityWrapper>>> = {
        #[cfg(debug_assertions)]
        {
            println!("=== DEBUG MODE: Using Mock Climate Entities ===");
            Arc::new(RwLock::new(config.climate_entities
                .into_iter()
                .map(|entity_id| ClimateEntityWrapper::Mock(MockClimate::new(entity_id, HeatingState::Off)))
                .collect()))
        }

        #[cfg(not(debug_assertions))]
        {
            println!("=== PRODUCTION MODE: Using Real Climate Entities ===");
            Arc::new(RwLock::new(config.climate_entities
                .into_iter()
                .map(|entity_id| ClimateEntityWrapper::Real(DefaultClimate::new(entity_id)))
                .collect()))
        }
    };

    println!("=== Loaded Schedule: {} ===", schedule.name);
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
    let schedule: ScheduleState = Arc::new(RwLock::new(schedule));
    let api_task = tokio::spawn(start_server(
        Arc::clone(&schedule),
        schedule_file_path.to_string_lossy().to_string(),
        Arc::clone(&climate_entities),
    ));


    let scheduler_task = tokio::spawn(run_scheduler(SchedulerState { api_client, schedule, climate_entities: Arc::clone(&climate_entities) }));

    tokio::try_join!(api_task, scheduler_task).unwrap();
    Ok(())
}
