use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Represents the persisted entities configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitiesConfig {
    pub climate_entities: Vec<String>,
}

impl EntitiesConfig {
    pub fn new(climate_entities: Vec<String>) -> Self {
        Self { climate_entities }
    }

    pub fn default() -> Self {
        Self {
            climate_entities: Vec::new(),
        }
    }
}

/// Load entities from a JSON file
pub fn load_entities<P: AsRef<Path>>(path: P) -> Result<EntitiesConfig> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read entities file: {}", path.display()))?;

    let entities: EntitiesConfig = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse entities JSON from: {}", path.display()))?;

    Ok(entities)
}

/// Save entities to a JSON file
pub fn save_entities<P: AsRef<Path>>(entities: &EntitiesConfig, path: P) -> Result<()> {
    let path = path.as_ref();
    let json = serde_json::to_string_pretty(entities)
        .context("Failed to serialize entities to JSON")?;

    fs::write(path, json)
        .with_context(|| format!("Failed to write entities file: {}", path.display()))?;

    Ok(())
}

/// Load entities from file, or create empty config if it doesn't exist
pub fn load_or_create_default<P: AsRef<Path>>(path: P) -> Result<EntitiesConfig> {
    let path = path.as_ref();

    if path.exists() {
        println!("Loading entities from: {}", path.display());
        load_entities(path)
    } else {
        println!("No entities file found at: {}", path.display());
        println!("Creating empty entities config...");

        let entities = EntitiesConfig::default();

        // Save the default entities for next time
        save_entities(&entities, path)
            .context("Failed to save default entities config")?;

        println!("Empty entities config saved to: {}", path.display());
        Ok(entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_save_and_load_entities() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("entities.json");

        let entities = EntitiesConfig::new(vec![
            "climate.living_room".to_string(),
            "climate.bedroom".to_string(),
        ]);

        // Save it
        save_entities(&entities, &file_path).unwrap();

        // Load it back
        let loaded = load_entities(&file_path).unwrap();

        // Verify
        assert_eq!(loaded.climate_entities.len(), 2);
        assert_eq!(loaded.climate_entities[0], "climate.living_room");
    }

    #[test]
    fn test_load_or_create_default() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("entities.json");

        // First call should create default
        let entities1 = load_or_create_default(&file_path).unwrap();
        assert!(file_path.exists());
        assert_eq!(entities1.climate_entities.len(), 0);

        // Second call should load existing
        let entities2 = load_or_create_default(&file_path).unwrap();
        assert_eq!(entities1.climate_entities.len(), entities2.climate_entities.len());
    }
}
