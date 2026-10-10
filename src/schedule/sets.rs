use super::{HeatingState, Schedule};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Named schedules, one of which is active and drives the heating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSets {
    pub active_id: Uuid,
    pub sets: Vec<Schedule>,
    /// Comfort temperature (°C) given to On entries saved before targets existed, and used by
    /// boost when no On entry is active
    #[serde(default = "default_target_temp")]
    pub default_target_temp: f64,
}

pub const DEFAULT_TARGET_TEMP: f64 = 20.0;

fn default_target_temp() -> f64 {
    DEFAULT_TARGET_TEMP
}

#[derive(Debug, PartialEq)]
pub enum SetError {
    /// No set with that id
    NotFound,
    /// The change isn't allowed in the current state (e.g. deleting the active set)
    Conflict(String),
    /// The request itself is invalid (e.g. an empty name)
    Invalid(String),
}

impl ScheduleSets {
    /// Sets holding just `schedule`, marked active
    pub fn from_schedule(schedule: Schedule) -> Self {
        let mut sets = ScheduleSets {
            active_id: schedule.id,
            sets: vec![schedule],
            default_target_temp: DEFAULT_TARGET_TEMP,
        };
        sets.normalise();
        sets
    }

    pub fn active(&self) -> &Schedule {
        self.get(self.active_id)
            .expect("active_id always names a set")
    }

    pub fn active_mut(&mut self) -> &mut Schedule {
        let id = self.active_id;
        self.get_mut(id).expect("active_id always names a set")
    }

    pub fn get(&self, id: Uuid) -> Option<&Schedule> {
        self.sets.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut Schedule> {
        self.sets.iter_mut().find(|s| s.id == id)
    }

    /// Add a set: a full-day Off schedule, or a copy of `copy_from` with fresh ids
    pub fn create(&mut self, name: &str, copy_from: Option<Uuid>) -> Result<&Schedule, SetError> {
        let name = valid_name(name)?;
        let mut schedule = match copy_from {
            Some(source_id) => {
                let mut copy = self.get(source_id).ok_or(SetError::NotFound)?.clone();
                copy.id = Uuid::new_v4();
                for entry in &mut copy.entries {
                    entry.id = Uuid::new_v4();
                }
                copy
            }
            None => Schedule::new(""),
        };
        schedule.name = name;
        self.sets.push(schedule);
        Ok(self.sets.last().unwrap())
    }

    pub fn rename(&mut self, id: Uuid, name: &str) -> Result<&Schedule, SetError> {
        let name = valid_name(name)?;
        let schedule = self.get_mut(id).ok_or(SetError::NotFound)?;
        schedule.name = name;
        Ok(schedule)
    }

    /// Delete a set. The active set and the last set can't be deleted.
    pub fn delete(&mut self, id: Uuid) -> Result<(), SetError> {
        if self.get(id).is_none() {
            return Err(SetError::NotFound);
        }
        if id == self.active_id {
            return Err(SetError::Conflict(
                "Cannot delete the active schedule set".to_string(),
            ));
        }
        if self.sets.len() == 1 {
            return Err(SetError::Conflict(
                "Cannot delete the last schedule set".to_string(),
            ));
        }
        self.sets.retain(|s| s.id != id);
        Ok(())
    }

    pub fn activate(&mut self, id: Uuid) -> Result<(), SetError> {
        if self.get(id).is_none() {
            return Err(SetError::NotFound);
        }
        self.active_id = id;
        Ok(())
    }

    /// Repair sets loaded from disk: give On entries without a target the default target (and
    /// drop targets on Off entries), normalise each schedule, give duplicate set ids a new id,
    /// and point `active_id` at the first set if it names none.
    pub fn normalise(&mut self) {
        if self.sets.is_empty() {
            self.sets.push(Schedule::new("Default Heating Schedule"));
        }
        let mut seen = std::collections::HashSet::new();
        for schedule in &mut self.sets {
            if !seen.insert(schedule.id) {
                schedule.id = Uuid::new_v4();
            }
            for entry in &mut schedule.entries {
                entry.target_temp = match entry.heating_state {
                    HeatingState::On => entry.target_temp.or(Some(self.default_target_temp)),
                    HeatingState::Off => None,
                };
            }
            schedule.normalise();
        }
        if self.get(self.active_id).is_none() {
            self.active_id = self.sets[0].id;
        }
    }
}

fn valid_name(name: &str) -> Result<String, SetError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(SetError::Invalid("Name must not be empty".to_string()));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{HeatingState, ScheduleEntry, TimePeriod};

    fn two_sets() -> ScheduleSets {
        let mut sets = ScheduleSets::from_schedule(Schedule::new("Work week"));
        sets.create("Holiday", None).unwrap();
        sets
    }

    #[test]
    fn test_create_is_full_day_off_and_not_active() {
        let sets = two_sets();
        let holiday = &sets.sets[1];

        assert_eq!(holiday.name, "Holiday");
        assert_eq!(holiday.entries.len(), 1);
        assert!(holiday.entries[0].time_period.is_full_day());
        assert_eq!(holiday.entries[0].heating_state, HeatingState::Off);
        assert_eq!(sets.active().name, "Work week");
    }

    #[test]
    fn test_create_copy_has_fresh_ids() {
        let mut sets = ScheduleSets::from_schedule(Schedule::new("Work week"));
        sets.active_mut().add_entry(ScheduleEntry::new(
            "Day",
            TimePeriod::new(8, 0, 17, 0),
            HeatingState::On,
        ));
        let source = sets.active().clone();

        let copy = sets.create("Copy", Some(source.id)).unwrap().clone();

        assert_ne!(copy.id, source.id);
        assert_eq!(copy.entries.len(), source.entries.len());
        for (a, b) in copy.entries.iter().zip(&source.entries) {
            assert_ne!(a.id, b.id);
            assert_eq!(a.time_period, b.time_period);
            assert_eq!(a.heating_state, b.heating_state);
        }
    }

    #[test]
    fn test_create_copy_from_missing_is_not_found() {
        let mut sets = two_sets();
        assert_eq!(
            sets.create("Copy", Some(Uuid::new_v4())).unwrap_err(),
            SetError::NotFound
        );
    }

    #[test]
    fn test_empty_name_is_invalid() {
        let mut sets = two_sets();
        let id = sets.sets[1].id;
        assert!(matches!(sets.create("  ", None), Err(SetError::Invalid(_))));
        assert!(matches!(sets.rename(id, ""), Err(SetError::Invalid(_))));
    }

    #[test]
    fn test_rename_keeps_id() {
        let mut sets = two_sets();
        let id = sets.sets[1].id;

        sets.rename(id, " Away ").unwrap();

        assert_eq!(sets.get(id).unwrap().name, "Away");
    }

    #[test]
    fn test_delete_refuses_active_and_last() {
        let mut sets = two_sets();
        let active = sets.active_id;
        let other = sets.sets[1].id;

        assert!(matches!(sets.delete(active), Err(SetError::Conflict(_))));
        assert_eq!(sets.delete(Uuid::new_v4()), Err(SetError::NotFound));

        sets.delete(other).unwrap();
        assert_eq!(sets.sets.len(), 1);
        assert!(matches!(sets.delete(active), Err(SetError::Conflict(_))));
    }

    #[test]
    fn test_activate() {
        let mut sets = two_sets();
        let other = sets.sets[1].id;

        sets.activate(other).unwrap();

        assert_eq!(sets.active().name, "Holiday");
        assert_eq!(sets.activate(Uuid::new_v4()), Err(SetError::NotFound));
    }

    #[test]
    fn test_normalise_repairs_bad_active_id_and_duplicate_ids() {
        let mut sets = two_sets();
        sets.sets[1].id = sets.sets[0].id;
        sets.active_id = Uuid::new_v4();

        sets.normalise();

        assert_ne!(sets.sets[0].id, sets.sets[1].id);
        assert_eq!(sets.active_id, sets.sets[0].id);
    }
}
