pub struct Config {
    pub ha_url: String,
    pub ha_token: String,
    pub climate_entities: Vec<String>,
}

impl Config {
    pub fn new(ha_url: &str, ha_token: &str, climate_entities: Vec<String>) -> Self {
        Config {
            ha_url: ha_url.to_string(),
            ha_token: ha_token.to_string(),
            climate_entities,
        }
    }

    pub fn from_env() -> Self {
        dotenv::dotenv().ok();
        let ha_url = std::env::var("HA_URL").expect("HA_URL must be set");
        let ha_token = std::env::var("HA_TOKEN").expect("HA_TOKEN must be set");
        let climate_entity = std::env::var("CLIMATE_ENTITY").expect("CLIMATE_ENTITY must be set");
        let climates: Vec<String> = climate_entity.split(",").map(|s| s.trim().to_owned()).collect();
        Config::new(&ha_url, &ha_token, climates)
    }
}