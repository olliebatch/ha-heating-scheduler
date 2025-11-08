use ha_heating_scheduler::api_client;
use ha_heating_scheduler::climate::ClimateEntity;
use ha_heating_scheduler::config;
use ha_heating_scheduler::schedule::Schedule;
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

    // Create a heating schedule
    let schedule = Schedule::new("Lounge Heating Schedule");

    start_server(schedule).await;
    Ok(())
}
