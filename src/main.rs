use ha_heating_scheduler::config;
use ha_heating_scheduler::schedule::{HeatingState, Schedule, ScheduleEntry, TimePeriod};
use ha_heating_scheduler::scheduler::{run_scheduler, SchedulerState};
use ha_heating_scheduler::server::start_server;
use ha_heating_scheduler::{api_client, ScheduleState};
use std::sync::{Arc, RwLock};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env();
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    );

    // Create a heating schedule with automatic full-day coverage
    let mut schedule = Schedule::new("Lounge Heating Schedule");

    println!("=== Initial Schedule ===");
    println!("Starting with default full-day OFF schedule");
    println!("Entries: {}", schedule.entries.len());
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

    // Add morning heating - this will automatically split the default entry
    println!("=== Adding Morning Heating (10:00-11:00) ===");
    schedule.add_entry(ScheduleEntry::new(
        "Morning Heating",
        TimePeriod::new(10, 0, 11, 0),
        HeatingState::On,
    ));

    println!("Schedule now has {} entries:", schedule.entries.len());
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

    // Add evening heating
    println!("=== Adding Evening Heating (17:00-22:00) ===");
    schedule.add_entry(ScheduleEntry::new(
        "Evening Heating",
        TimePeriod::new(21, 50, 21, 55),
        HeatingState::On,
    ));

    println!("Schedule now has {} entries:", schedule.entries.len());
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

    println!("=== Full Day Coverage Maintained ===");
    println!(
        "The schedule automatically maintains full 24-hour coverage by splitting existing entries!"
    );
    println!();
    let schedule: ScheduleState = Arc::new(RwLock::new(schedule));
    let api_task = tokio::spawn(start_server(Arc::clone(&schedule)));
    let scheduler_task = tokio::spawn(run_scheduler(SchedulerState { api_client, schedule }));

    tokio::try_join!(api_task, scheduler_task).unwrap();
    Ok(())
}
