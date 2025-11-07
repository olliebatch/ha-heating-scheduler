mod api_client;
mod climate;
mod config;

use crate::climate::ClimateEntity;
use reqwest;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env();
    let api_client = api_client::ApiClient::new(
        reqwest::Url::parse(&config.ha_url)?,
        config.ha_token.clone(),
    );

    let mut lounge_trv = ClimateEntity::new("climate.lounge_trv".to_string());
    lounge_trv.get_state(&api_client).await.unwrap();

    println!("{:?}", lounge_trv);

    Ok(())
}
