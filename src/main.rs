use ha_heating_scheduler::api_client;
use ha_heating_scheduler::climate::ClimateEntity;
use ha_heating_scheduler::config;
use ha_heating_scheduler::schedule::{HeatingState, Schedule, ScheduleEntry, TimePeriod};
use ha_heating_scheduler::server::start_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env();
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    );

    // Get climate state
    let mut lounge_trv = ClimateEntity::new("climate.lounge_trv".to_string());
    lounge_trv.get_state(&api_client).await?;

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
        TimePeriod::new(17, 0, 22, 0),
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

    start_server(schedule).await;
    Ok(())
}
