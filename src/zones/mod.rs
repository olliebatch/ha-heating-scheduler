use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use uuid::Uuid;

use crate::schedule::TimePeriod;
use crate::weather::adjust::WindExposure;

pub mod areas;

use areas::Area;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneKind {
    /// One or more Home Assistant Areas (more than one when merged)
    Area,
    /// Entities picked by hand
    Manual,
    /// Every managed entity not in another zone
    WholeHouse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub id: Uuid,
    pub name: String,
    pub kind: ZoneKind,
    /// The areas this zone covers (Area zones only)
    #[serde(default)]
    pub area_ids: Vec<String>,
    pub entity_ids: Vec<String>,
    /// The schedule set this zone follows; None follows the active set
    #[serde(default)]
    pub schedule_set_id: Option<Uuid>,
    /// Times of day the zone usually gets direct sun
    #[serde(default)]
    pub sun_windows: Vec<TimePeriod>,
    #[serde(default)]
    pub wind_exposure: WindExposure,
    /// Adjust this zone's target for the weather
    #[serde(default)]
    pub weather_adjust: bool,
    /// Start On periods up to this many minutes early when it's cold or windy (0 disables)
    #[serde(default = "default_max_early_start")]
    pub max_early_start_minutes: u32,
}

/// Default for [`Zone::max_early_start_minutes`]
pub const DEFAULT_MAX_EARLY_START: u32 = 30;
/// Largest allowed [`Zone::max_early_start_minutes`]
pub const MAX_EARLY_START_LIMIT: u32 = 180;

fn default_max_early_start() -> u32 {
    DEFAULT_MAX_EARLY_START
}

impl Zone {
    fn new(name: impl Into<String>, kind: ZoneKind) -> Self {
        Zone {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            area_ids: Vec::new(),
            entity_ids: Vec::new(),
            schedule_set_id: None,
            sun_windows: Vec::new(),
            wind_exposure: WindExposure::default(),
            weather_adjust: false,
            max_early_start_minutes: DEFAULT_MAX_EARLY_START,
        }
    }

    /// Manual and merged zones can be deleted; their entities fall back to area zones or Whole house
    pub fn is_deletable(&self) -> bool {
        self.kind == ZoneKind::Manual || (self.kind == ZoneKind::Area && self.area_ids.len() > 1)
    }
}

#[derive(Debug, PartialEq)]
pub enum ZoneError {
    NotFound,
    Conflict(String),
    Invalid(String),
}

/// Zones, persisted to zones.json, plus the areas last seen in Home Assistant
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Zones {
    pub zones: Vec<Zone>,
    /// Areas from the last successful discovery, so zones can be rebuilt without asking HA
    #[serde(default)]
    pub known_areas: Vec<Area>,
}

impl Zones {
    pub fn get(&self, id: Uuid) -> Option<&Zone> {
        self.zones.iter().find(|z| z.id == id)
    }

    /// The zone an entity is in
    pub fn zone_for(&self, entity_id: &str) -> Option<&Zone> {
        self.zones
            .iter()
            .find(|z| z.entity_ids.iter().any(|e| e == entity_id))
    }

    /// Zones following the given schedule set
    pub fn using_set(&self, set_id: Uuid) -> Vec<&Zone> {
        self.zones
            .iter()
            .filter(|z| z.schedule_set_id == Some(set_id))
            .collect()
    }

    /// Rebuild entity lists from `areas` (when given, they replace `known_areas`) and the
    /// managed entities, so every managed entity is in exactly one zone:
    /// a manual zone first, else the zone covering its area, else Whole house.
    ///
    /// Zones keep their ids. An area zone whose areas are all gone is dropped; a new area with
    /// managed entities gets a new zone. Whole house always exists.
    pub fn reconcile(&mut self, areas: Option<Vec<Area>>, managed: &[String]) {
        if let Some(areas) = areas {
            self.known_areas = areas;
        }
        let managed: HashSet<&str> = managed.iter().map(String::as_str).collect();
        let mut placed: HashSet<String> = HashSet::new();

        // Manual zones first: they override areas
        for zone in self.zones.iter_mut().filter(|z| z.kind == ZoneKind::Manual) {
            zone.entity_ids
                .retain(|e| managed.contains(e.as_str()) && placed.insert(e.clone()));
        }

        // Area zones: drop areas that no longer exist, then zones with none left
        let known: HashSet<&str> = self.known_areas.iter().map(|a| a.id.as_str()).collect();
        for zone in self.zones.iter_mut().filter(|z| z.kind == ZoneKind::Area) {
            zone.area_ids.retain(|a| known.contains(a.as_str()));
        }
        self.zones
            .retain(|z| z.kind != ZoneKind::Area || !z.area_ids.is_empty());

        // New zones for areas with managed entities that no zone covers yet
        let covered: HashSet<String> = self
            .zones
            .iter()
            .flat_map(|z| z.area_ids.iter().cloned())
            .collect();
        for area in &self.known_areas {
            let has_managed = area.entities.iter().any(|e| managed.contains(e.as_str()));
            if has_managed && !covered.contains(&area.id) {
                let mut zone = Zone::new(&area.name, ZoneKind::Area);
                zone.area_ids.push(area.id.clone());
                self.zones.push(zone);
            }
        }

        // Fill area zones from their areas
        for zone in self.zones.iter_mut().filter(|z| z.kind == ZoneKind::Area) {
            zone.entity_ids = self
                .known_areas
                .iter()
                .filter(|a| zone.area_ids.contains(&a.id))
                .flat_map(|a| a.entities.iter())
                .filter(|e| managed.contains(e.as_str()) && placed.insert((*e).clone()))
                .cloned()
                .collect();
        }

        // Everything else goes in Whole house
        if !self.zones.iter().any(|z| z.kind == ZoneKind::WholeHouse) {
            self.zones
                .push(Zone::new("Whole house", ZoneKind::WholeHouse));
        }
        let mut rest: Vec<String> = managed
            .iter()
            .filter(|e| !placed.contains(**e))
            .map(|e| e.to_string())
            .collect();
        rest.sort();
        let whole_house = self
            .zones
            .iter_mut()
            .find(|z| z.kind == ZoneKind::WholeHouse)
            .unwrap();
        whole_house.entity_ids = rest;
    }

    /// Merge area zones into the first one, which keeps its id and schedule set
    pub fn merge(
        &mut self,
        zone_ids: &[Uuid],
        name: Option<&str>,
        managed: &[String],
    ) -> Result<&Zone, ZoneError> {
        // Check everything before changing anything
        let name = name.map(valid_name).transpose()?;
        if zone_ids.len() < 2 {
            return Err(ZoneError::Invalid(
                "Merge needs at least two zones".to_string(),
            ));
        }
        for id in zone_ids {
            let zone = self.get(*id).ok_or(ZoneError::NotFound)?;
            if zone.kind != ZoneKind::Area {
                return Err(ZoneError::Invalid(format!(
                    "Only area zones can be merged; \"{}\" is not one",
                    zone.name
                )));
            }
        }
        let mut area_ids: Vec<String> = Vec::new();
        for id in zone_ids {
            for area in &self.get(*id).unwrap().area_ids {
                if !area_ids.contains(area) {
                    area_ids.push(area.clone());
                }
            }
        }
        let target = zone_ids[0];
        self.zones
            .retain(|z| z.id == target || !zone_ids.contains(&z.id));
        let zone = self.zones.iter_mut().find(|z| z.id == target).unwrap();
        zone.area_ids = area_ids;
        if let Some(name) = name {
            zone.name = name;
        }
        self.reconcile(None, managed);
        Ok(self.get(target).unwrap())
    }

    /// Add a manual zone; its entities leave whatever zone they were in
    pub fn create_manual(
        &mut self,
        name: &str,
        entity_ids: &[String],
        managed: &[String],
    ) -> Result<&Zone, ZoneError> {
        let name = valid_name(name)?;
        if entity_ids.is_empty() {
            return Err(ZoneError::Invalid(
                "A zone needs at least one entity".to_string(),
            ));
        }
        if let Some(unknown) = entity_ids.iter().find(|e| !managed.contains(e)) {
            return Err(ZoneError::Invalid(format!(
                "{} is not a managed entity",
                unknown
            )));
        }
        for entity in entity_ids {
            if let Some(zone) = self.zone_for(entity).filter(|z| z.kind == ZoneKind::Manual) {
                return Err(ZoneError::Conflict(format!(
                    "{} is already in manual zone \"{}\"",
                    entity, zone.name
                )));
            }
        }
        let mut zone = Zone::new(name, ZoneKind::Manual);
        zone.entity_ids = entity_ids.to_vec();
        let id = zone.id;
        self.zones.push(zone);
        self.reconcile(None, managed);
        Ok(self.get(id).unwrap())
    }

    /// Delete a manual or merged zone; its entities fall back to area zones or Whole house
    pub fn delete(&mut self, id: Uuid, managed: &[String]) -> Result<(), ZoneError> {
        let zone = self.get(id).ok_or(ZoneError::NotFound)?;
        if !zone.is_deletable() {
            return Err(ZoneError::Conflict(format!(
                "\"{}\" comes from Home Assistant and can't be deleted",
                zone.name
            )));
        }
        self.zones.retain(|z| z.id != id);
        self.reconcile(None, managed);
        Ok(())
    }

    pub fn rename(&mut self, id: Uuid, name: &str) -> Result<(), ZoneError> {
        let name = valid_name(name)?;
        let zone = self
            .zones
            .iter_mut()
            .find(|z| z.id == id)
            .ok_or(ZoneError::NotFound)?;
        zone.name = name;
        Ok(())
    }

    /// Update the weather profile; leaves out any part that is None
    pub fn set_profile(
        &mut self,
        id: Uuid,
        sun_windows: Option<Vec<TimePeriod>>,
        wind_exposure: Option<WindExposure>,
        weather_adjust: Option<bool>,
        max_early_start_minutes: Option<u32>,
    ) -> Result<(), ZoneError> {
        if max_early_start_minutes.is_some_and(|m| m > MAX_EARLY_START_LIMIT) {
            return Err(ZoneError::Invalid(format!(
                "max_early_start_minutes must be 0-{}",
                MAX_EARLY_START_LIMIT
            )));
        }
        if let Some(windows) = &sun_windows {
            if windows.iter().any(|w| w.start == w.end && !w.is_full_day()) {
                return Err(ZoneError::Invalid(
                    "Sun windows need different start and end times".to_string(),
                ));
            }
        }
        let zone = self
            .zones
            .iter_mut()
            .find(|z| z.id == id)
            .ok_or(ZoneError::NotFound)?;
        if let Some(windows) = sun_windows {
            zone.sun_windows = windows;
        }
        if let Some(exposure) = wind_exposure {
            zone.wind_exposure = exposure;
        }
        if let Some(adjust) = weather_adjust {
            zone.weather_adjust = adjust;
        }
        if let Some(minutes) = max_early_start_minutes {
            zone.max_early_start_minutes = minutes;
        }
        Ok(())
    }

    pub fn set_schedule_set(&mut self, id: Uuid, set_id: Option<Uuid>) -> Result<(), ZoneError> {
        let zone = self
            .zones
            .iter_mut()
            .find(|z| z.id == id)
            .ok_or(ZoneError::NotFound)?;
        zone.schedule_set_id = set_id;
        Ok(())
    }
}

fn valid_name(name: &str) -> Result<String, ZoneError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ZoneError::Invalid("Name must not be empty".to_string()));
    }
    Ok(name.to_string())
}

/// Load zones from a JSON file, or start with none if it doesn't exist
pub fn load_zones<P: AsRef<Path>>(path: P) -> Result<Zones> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Zones::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read zones file: {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse zones JSON from: {}", path.display()))
}

/// Save zones to a JSON file
pub fn save_zones<P: AsRef<Path>>(zones: &Zones, path: P) -> Result<()> {
    let path = path.as_ref();
    let json = serde_json::to_string_pretty(zones).context("Failed to serialize zones to JSON")?;
    fs::write(path, json).with_context(|| format!("Failed to write zones file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(id: &str, entities: &[&str]) -> Area {
        Area {
            id: id.to_string(),
            name: id.to_uppercase(),
            entities: entities.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn areas() -> Vec<Area> {
        vec![
            area("lounge", &["climate.lounge"]),
            area("study", &["climate.study", "climate.study_2"]),
            area("hall", &[]),
        ]
    }

    fn managed(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|e| e.to_string()).collect()
    }

    fn all_managed() -> Vec<String> {
        managed(&[
            "climate.lounge",
            "climate.study",
            "climate.study_2",
            "climate.loft",
        ])
    }

    fn by_name<'a>(zones: &'a Zones, name: &str) -> &'a Zone {
        zones.zones.iter().find(|z| z.name == name).unwrap()
    }

    /// Every managed entity is in exactly one zone
    fn assert_partition(zones: &Zones, managed: &[String]) {
        for entity in managed {
            let count = zones
                .zones
                .iter()
                .filter(|z| z.entity_ids.contains(entity))
                .count();
            assert_eq!(count, 1, "{} is in {} zones: {:#?}", entity, count, zones);
        }
        let total: usize = zones.zones.iter().map(|z| z.entity_ids.len()).sum();
        assert_eq!(total, managed.len());
    }

    #[test]
    fn test_reconcile_builds_zones_from_areas() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        assert_partition(&zones, &all_managed());
        assert_eq!(zones.zones.len(), 3, "no zone for an area without climates");
        assert_eq!(by_name(&zones, "LOUNGE").entity_ids, vec!["climate.lounge"]);
        assert_eq!(by_name(&zones, "STUDY").entity_ids.len(), 2);
        assert_eq!(
            by_name(&zones, "Whole house").entity_ids,
            vec!["climate.loft"]
        );
    }

    #[test]
    fn test_no_areas_is_one_whole_house_zone() {
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &all_managed());

        assert_eq!(zones.zones.len(), 1);
        assert_eq!(zones.zones[0].kind, ZoneKind::WholeHouse);
        assert_partition(&zones, &all_managed());
    }

    #[test]
    fn test_unmanaged_area_entities_are_ignored() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &managed(&["climate.study"]));

        assert_partition(&zones, &managed(&["climate.study"]));
        assert_eq!(zones.zones.len(), 2, "Study and Whole house");
    }

    #[test]
    fn test_refresh_keeps_ids_and_schedule_sets() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        let study = by_name(&zones, "STUDY").id;
        let set = Uuid::new_v4();
        zones.set_schedule_set(study, Some(set)).unwrap();

        // An entity moves from Study to Lounge in HA
        let moved = vec![
            area("lounge", &["climate.lounge", "climate.study_2"]),
            area("study", &["climate.study"]),
        ];
        zones.reconcile(Some(moved), &all_managed());

        let zone = zones.get(study).unwrap();
        assert_eq!(zone.entity_ids, vec!["climate.study"]);
        assert_eq!(zone.schedule_set_id, Some(set));
        assert_eq!(by_name(&zones, "LOUNGE").entity_ids.len(), 2);
        assert_partition(&zones, &all_managed());
    }

    #[test]
    fn test_area_removed_in_ha_drops_zone() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        zones.reconcile(
            Some(vec![area("lounge", &["climate.lounge"])]),
            &all_managed(),
        );

        assert!(zones.zones.iter().all(|z| z.name != "STUDY"));
        assert_eq!(by_name(&zones, "Whole house").entity_ids.len(), 3);
        assert_partition(&zones, &all_managed());
    }

    #[test]
    fn test_merge_and_unmerge() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        let lounge = by_name(&zones, "LOUNGE").id;
        let study = by_name(&zones, "STUDY").id;

        let merged = zones
            .merge(&[lounge, study], Some("Downstairs"), &all_managed())
            .unwrap()
            .clone();

        assert_eq!(merged.id, lounge);
        assert_eq!(merged.entity_ids.len(), 3);
        assert!(zones.get(study).is_none());
        assert_partition(&zones, &all_managed());

        // Merged zones survive a refresh
        zones.reconcile(Some(areas()), &all_managed());
        assert_eq!(zones.get(lounge).unwrap().entity_ids.len(), 3);

        // Deleting the merged zone gives each area its own zone back
        zones.delete(lounge, &all_managed()).unwrap();
        assert_eq!(zones.zones.len(), 3);
        assert_partition(&zones, &all_managed());
    }

    #[test]
    fn test_merge_needs_area_zones() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        let lounge = by_name(&zones, "LOUNGE").id;
        let whole = by_name(&zones, "Whole house").id;

        assert!(matches!(
            zones.merge(&[lounge, whole], None, &all_managed()),
            Err(ZoneError::Invalid(_))
        ));
        assert!(matches!(
            zones.merge(&[lounge], None, &all_managed()),
            Err(ZoneError::Invalid(_))
        ));
        assert_eq!(
            zones
                .merge(&[lounge, Uuid::new_v4()], None, &all_managed())
                .unwrap_err(),
            ZoneError::NotFound
        );
    }

    #[test]
    fn test_manual_zone_overrides_area_and_falls_back_on_delete() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        let manual = zones
            .create_manual(
                "Loft and study",
                &managed(&["climate.loft", "climate.study_2"]),
                &all_managed(),
            )
            .unwrap()
            .id;

        assert_eq!(by_name(&zones, "STUDY").entity_ids, vec!["climate.study"]);
        assert!(by_name(&zones, "Whole house").entity_ids.is_empty());
        assert_partition(&zones, &all_managed());

        // A refresh doesn't take entities back from a manual zone
        zones.reconcile(Some(areas()), &all_managed());
        assert_eq!(zones.get(manual).unwrap().entity_ids.len(), 2);

        zones.delete(manual, &all_managed()).unwrap();
        assert_eq!(by_name(&zones, "STUDY").entity_ids.len(), 2);
        assert_eq!(
            by_name(&zones, "Whole house").entity_ids,
            vec!["climate.loft"]
        );
    }

    #[test]
    fn test_manual_zone_validation() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        zones
            .create_manual("Loft", &managed(&["climate.loft"]), &all_managed())
            .unwrap();

        let err = |r: Result<&Zone, ZoneError>| r.map(|_| ()).unwrap_err();
        assert!(matches!(
            err(zones.create_manual(" ", &managed(&["climate.study"]), &all_managed())),
            ZoneError::Invalid(_)
        ));
        assert!(matches!(
            err(zones.create_manual("X", &[], &all_managed())),
            ZoneError::Invalid(_)
        ));
        assert!(matches!(
            err(zones.create_manual("X", &managed(&["climate.nope"]), &all_managed())),
            ZoneError::Invalid(_)
        ));
        assert!(matches!(
            err(zones.create_manual("X", &managed(&["climate.loft"]), &all_managed())),
            ZoneError::Conflict(_)
        ));
    }

    #[test]
    fn test_area_and_whole_house_zones_cant_be_deleted() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        for name in ["LOUNGE", "Whole house"] {
            let id = by_name(&zones, name).id;
            assert!(matches!(
                zones.delete(id, &all_managed()),
                Err(ZoneError::Conflict(_))
            ));
        }
        assert_eq!(
            zones.delete(Uuid::new_v4(), &all_managed()),
            Err(ZoneError::NotFound)
        );
    }

    #[test]
    fn test_removed_entity_leaves_its_zone() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        let fewer = managed(&["climate.lounge", "climate.loft"]);
        zones.reconcile(None, &fewer);

        assert_partition(&zones, &fewer);
        assert!(by_name(&zones, "STUDY").entity_ids.is_empty());
    }

    #[test]
    fn test_save_and_load_keep_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zones.json");
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());

        save_zones(&zones, &path).unwrap();
        let loaded = load_zones(&path).unwrap();

        let ids = |z: &Zones| z.zones.iter().map(|z| z.id).collect::<Vec<_>>();
        assert_eq!(ids(&loaded), ids(&zones));
        assert_eq!(loaded.known_areas, zones.known_areas);
        assert!(
            load_zones(dir.path().join("missing.json"))
                .unwrap()
                .zones
                .is_empty()
        );
    }

    #[test]
    fn test_merge_with_blank_name_changes_nothing() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        let before = zones.zones.clone();
        let lounge = by_name(&zones, "LOUNGE").id;
        let study = by_name(&zones, "STUDY").id;

        let result = zones.merge(&[lounge, study], Some("  "), &all_managed());

        assert!(matches!(result, Err(ZoneError::Invalid(_))));
        assert_eq!(zones.zones, before);
    }

    #[test]
    fn test_max_early_start_bounds() {
        let mut zones = Zones::default();
        zones.reconcile(Some(areas()), &all_managed());
        let id = zones.zones[0].id;
        assert_eq!(zones.zones[0].max_early_start_minutes, 30);

        zones.set_profile(id, None, None, None, Some(0)).unwrap();
        assert_eq!(zones.get(id).unwrap().max_early_start_minutes, 0);
        assert!(matches!(
            zones.set_profile(id, None, None, Some(true), Some(181)),
            Err(ZoneError::Invalid(_))
        ));
        assert!(!zones.get(id).unwrap().weather_adjust, "nothing changed");

        // Older zones.json files get the default
        let json = r#"{"id": "00000000-0000-4000-8000-000000000001", "name": "Old",
                       "kind": "manual", "entity_ids": []}"#;
        let zone: Zone = serde_json::from_str(json).unwrap();
        assert_eq!(zone.max_early_start_minutes, 30);
    }
}
