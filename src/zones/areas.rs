use crate::api_client::ApiClient;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A Home Assistant Area and the climate entities in it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    pub name: String,
    pub entities: Vec<String>,
}

/// Where zones discover areas from
#[async_trait::async_trait]
pub trait AreaSource: Send + Sync {
    async fn fetch_areas(&self) -> Result<Vec<Area>>;
}

/// Renders every area with its climate entities as a JSON list. `area_entities` includes
/// entities whose device is in the area.
const AREAS_TEMPLATE: &str = r#"[{% for a in areas() %}{{ {"id": a, "name": area_name(a), "entities": area_entities(a) | select("match", "climate[.]") | list} | tojson }}{{ "," if not loop.last }}{% endfor %}]"#;

/// Asks Home Assistant's REST template endpoint for its areas
pub struct HomeAssistantAreas {
    pub api_client: ApiClient,
}

#[async_trait::async_trait]
impl AreaSource for HomeAssistantAreas {
    async fn fetch_areas(&self) -> Result<Vec<Area>> {
        let text = self
            .api_client
            .post("/api/template")
            .json(&serde_json::json!({ "template": AREAS_TEMPLATE }))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_areas(&text)
    }
}

/// Parse the rendered template, keeping only climate entities
pub fn parse_areas(text: &str) -> Result<Vec<Area>> {
    let mut areas: Vec<Area> =
        serde_json::from_str(text).context("Failed to parse areas from Home Assistant")?;
    for area in &mut areas {
        area.entities.retain(|e| e.starts_with("climate."));
    }
    Ok(areas)
}

/// Fixed areas for tests and debug mode
pub struct MockAreas {
    pub areas: Vec<Area>,
}

impl MockAreas {
    /// Two made-up areas; anything else (e.g. climate.qa_mock) has no area
    pub fn example() -> Self {
        MockAreas {
            areas: vec![
                Area {
                    id: "lounge".to_string(),
                    name: "Lounge".to_string(),
                    entities: vec!["climate.lounge_trv".to_string()],
                },
                Area {
                    id: "study".to_string(),
                    name: "Study".to_string(),
                    entities: vec![
                        "climate.study_trv".to_string(),
                        "climate.study_trv_2".to_string(),
                    ],
                },
            ],
        }
    }
}

#[async_trait::async_trait]
impl AreaSource for MockAreas {
    async fn fetch_areas(&self) -> Result<Vec<Area>> {
        println!("[MOCK] Fetching areas (no API call)");
        Ok(self.areas.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_areas_keeps_climate_entities() {
        let text = r#"[
            {"id": "lounge", "name": "Lounge", "entities": ["climate.lounge_trv", "light.lounge"]},
            {"id": "hall", "name": "Hall", "entities": []}
        ]"#;

        let areas = parse_areas(text).unwrap();

        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].entities, vec!["climate.lounge_trv"]);
        assert!(areas[1].entities.is_empty());
    }

    #[test]
    fn test_parse_areas_rejects_garbage() {
        assert!(parse_areas("not json").is_err());
    }
}
