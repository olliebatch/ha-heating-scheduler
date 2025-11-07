mod config;

use reqwest;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env();
    let client = reqwest::Client::new();

    // Get all states
    let response = client
        .get(format!("{}/api/states", config.ha_url))
        .header("Authorization", format!("Bearer {}", config.ha_token))
        .send()
        .await?;

    let states: Value = response.json().await?;
    println!("{:#}", states);

    Ok(())
}