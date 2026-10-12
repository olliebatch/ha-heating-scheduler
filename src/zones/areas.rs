use crate::api_client::ApiClient;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A Home Assistant Area and the climate entities in it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    pub name: String,
    pub entities: Vec<String>,
}

/// A climate entity as Home Assistant lists it, managed or not
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Climate {
    pub entity_id: String,
    /// Its friendly name
    pub name: String,
    /// Home Assistant's state, e.g. "heat", "off" or "unavailable"
    pub state: String,
}

/// Where zones discover areas from, and the settings page every climate entity
#[async_trait::async_trait]
pub trait AreaSource: Send + Sync {
    async fn fetch_areas(&self) -> Result<Vec<Area>>;
    async fn fetch_climates(&self) -> Result<Vec<Climate>>;
}

/// How long to wait for Home Assistant to list its areas
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch areas, giving up after `timeout` so a slow or hung Home Assistant can't hold things up
pub async fn fetch_areas_with_timeout(
    source: &dyn AreaSource,
    timeout: Duration,
) -> Result<Vec<Area>> {
    tokio::time::timeout(timeout, source.fetch_areas())
        .await
        .map_err(|_| anyhow!("timed out after {} s", timeout.as_secs()))?
}

/// Renders every area with its climate entities as a JSON list. `area_entities` includes
/// entities whose device is in the area.
const AREAS_TEMPLATE: &str = r#"[{% for a in areas() %}{{ {"id": a, "name": area_name(a), "entities": area_entities(a) | select("match", "climate[.]") | list} | tojson }}{{ "," if not loop.last }}{% endfor %}]"#;

/// Renders every climate entity with its friendly name and state as a JSON list
const CLIMATES_TEMPLATE: &str = r#"[{% for s in states.climate %}{{ {"entity_id": s.entity_id, "name": s.name, "state": s.state} | tojson }}{{ "," if not loop.last }}{% endfor %}]"#;

/// Asks Home Assistant's REST template endpoint for its areas and climate entities (reads, so
/// they also run in dry run)
pub struct HomeAssistantAreas {
    pub api_client: ApiClient,
}

#[async_trait::async_trait]
impl AreaSource for HomeAssistantAreas {
    async fn fetch_areas(&self) -> Result<Vec<Area>> {
        let text = self
            .api_client
            .post(
                "/api/template",
                &serde_json::json!({ "template": AREAS_TEMPLATE }),
            )
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_areas(&text)
    }

    async fn fetch_climates(&self) -> Result<Vec<Climate>> {
        let text = self
            .api_client
            .post(
                "/api/template",
                &serde_json::json!({ "template": CLIMATES_TEMPLATE }),
            )
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_climates(&text)
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

/// Parse the rendered climates template
pub fn parse_climates(text: &str) -> Result<Vec<Climate>> {
    serde_json::from_str(text).context("Failed to parse climate entities from Home Assistant")
}

/// Fixed areas and climate entities for tests and debug mode
pub struct MockAreas {
    pub areas: Vec<Area>,
    pub climates: Vec<Climate>,
}

impl MockAreas {
    /// Made-up areas; anything else (e.g. climate.qa_mock) has no area. Bedroom and Garage hold
    /// only TRVs that aren't managed at first, so they get no zone until one is added.
    pub fn example() -> Self {
        let climate = |id: &str, name: &str, state: &str| Climate {
            entity_id: format!("climate.{id}"),
            name: name.to_string(),
            state: state.to_string(),
        };
        MockAreas {
            climates: vec![
                climate("lounge_trv", "Lounge TRV", "heat"),
                climate("study_trv", "Study TRV", "heat"),
                climate("study_trv_2", "Study TRV 2", "off"),
                climate("qa_mock", "QA mock", "heat"),
                // Not managed: one in an area, one with no area, one unavailable
                climate("bedroom_trv", "Bedroom TRV", "heat"),
                climate("landing_trv", "Landing TRV", "heat"),
                climate("garage_trv", "Garage TRV", "unavailable"),
            ],
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
                Area {
                    id: "bedroom".to_string(),
                    name: "Bedroom".to_string(),
                    entities: vec!["climate.bedroom_trv".to_string()],
                },
                Area {
                    id: "garage".to_string(),
                    name: "Garage".to_string(),
                    entities: vec!["climate.garage_trv".to_string()],
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

    async fn fetch_climates(&self) -> Result<Vec<Climate>> {
        println!("[MOCK] Fetching climate entities (no API call)");
        Ok(self.climates.clone())
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
    fn test_parse_climates() {
        let text = r#"[
            {"entity_id": "climate.lounge_trv", "name": "Lounge TRV", "state": "heat"},
            {"entity_id": "climate.garage_trv", "name": "Garage TRV", "state": "unavailable"}
        ]"#;
        let climates = parse_climates(text).unwrap();
        assert_eq!(climates.len(), 2);
        assert_eq!(climates[1].state, "unavailable");
        assert!(parse_climates("not json").is_err());
    }

    #[test]
    fn test_parse_areas_rejects_garbage() {
        assert!(parse_areas("not json").is_err());
    }

    struct HungAreas;

    #[async_trait::async_trait]
    impl AreaSource for HungAreas {
        async fn fetch_areas(&self) -> Result<Vec<Area>> {
            std::future::pending().await
        }
        async fn fetch_climates(&self) -> Result<Vec<Climate>> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn test_fetch_areas_times_out() {
        let result = fetch_areas_with_timeout(&HungAreas, Duration::from_millis(20)).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let areas = fetch_areas_with_timeout(&MockAreas::example(), Duration::from_millis(20))
            .await
            .unwrap();
        assert_eq!(areas.len(), 4);
    }
}
