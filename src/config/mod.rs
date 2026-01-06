pub mod entities_persistence;

use std::path::Path;

pub struct Config {
    pub ha_url: String,
    pub ha_token: String,
    pub climate_entities: Vec<String>,
    pub data_path: String,
}

impl Config {
    pub fn new(ha_url: &str, ha_token: &str, climate_entities: Vec<String>, data_path: String) -> Self {
        Config {
            ha_url: ha_url.to_string(),
            ha_token: ha_token.to_string(),
            climate_entities,
            data_path,
        }
    }

    /// Load config from environment variables only (legacy method)
    /// Climate entities are loaded from CLIMATE_ENTITY env var
    pub fn from_env() -> Self {
        dotenv::dotenv().ok();
        let ha_url = std::env::var("HA_URL").expect("HA_URL must be set");
        let ha_token = std::env::var("HA_TOKEN").expect("HA_TOKEN must be set");
        let climate_entity = std::env::var("CLIMATE_ENTITY").expect("CLIMATE_ENTITY must be set");
        let data_path = std::env::var("DATA_PATH").expect("DATA_PATH must be set");
        let climates: Vec<String> = climate_entity.split(",").map(|s| s.trim().to_owned()).collect();
        Config::new(&ha_url, &ha_token, climates, data_path)
    }

    /// Load config with entities from persisted file
    /// Falls back to environment variable if file doesn't exist or is empty
    /// If loaded from env var, saves to file for future use (migration path)
    pub fn from_env_with_persisted_entities() -> anyhow::Result<Self> {
        dotenv::dotenv().ok();
        let ha_url = std::env::var("HA_URL").expect("HA_URL must be set");
        let ha_token = std::env::var("HA_TOKEN").expect("HA_TOKEN must be set");
        let data_path = std::env::var("DATA_PATH").expect("DATA_PATH must be set");

        // Try to load entities from persisted file
        let entities_file_path = Path::new(&data_path).join("entities.json");
        let entities_config = entities_persistence::load_or_create_default(&entities_file_path)?;

        let climate_entities = if entities_config.climate_entities.is_empty() {
            // Fall back to environment variable if no entities in file
            println!("No entities in persisted file, checking environment variable...");
            if let Ok(climate_entity) = std::env::var("CLIMATE_ENTITY") {
                let climates: Vec<String> = climate_entity.split(",").map(|s| s.trim().to_owned()).collect();
                println!("Loaded {} entities from CLIMATE_ENTITY env var", climates.len());

                // Save to file so next time we don't need the env var
                let new_config = entities_persistence::EntitiesConfig::new(climates.clone());
                if let Err(e) = entities_persistence::save_entities(&new_config, &entities_file_path) {
                    eprintln!("Warning: Failed to save entities from env var to file: {}", e);
                } else {
                    println!("Saved entities to {} for future use", entities_file_path.display());
                    println!("You can now remove CLIMATE_ENTITY from your .env file");
                }

                climates
            } else {
                println!("No CLIMATE_ENTITY env var found. Starting with empty entities list.");
                Vec::new()
            }
        } else {
            println!("Loaded {} entities from persisted file", entities_config.climate_entities.len());
            entities_config.climate_entities
        };

        Ok(Config::new(&ha_url, &ha_token, climate_entities, data_path))
    }
}
