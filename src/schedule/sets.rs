use super::{HeatingState, Schedule};
use chrono::Weekday;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use uuid::Uuid;

/// Named schedules. On a day of the week assigned to a set (`day_sets`), that set drives the
/// heating; on any other day the active set does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSets {
    pub active_id: Uuid,
    pub sets: Vec<Schedule>,
    /// The set assigned to each day of the week ("Mon" to "Sun"); a day can have at most one.
    /// Days not listed use the active set. Left out of the JSON when no day is assigned, so a
    /// file saved before day assignments existed loads and saves unchanged.
    #[serde(
        default,
        serialize_with = "days_in_order",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub day_sets: HashMap<Weekday, Uuid>,
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
            day_sets: HashMap::new(),
            default_target_temp: DEFAULT_TARGET_TEMP,
        };
        sets.normalise();
        sets
    }

    pub fn active(&self) -> &Schedule {
        self.get(self.active_id)
            .expect("active_id always names a set")
    }

    /// The set that drives the heating on `day`: the set assigned to it, or the active set
    pub fn for_day(&self, day: Weekday) -> &Schedule {
        self.day_sets
            .get(&day)
            .and_then(|id| self.get(*id))
            .unwrap_or_else(|| self.active())
    }

    /// The days assigned to a set, Monday first
    pub fn days_of(&self, id: Uuid) -> Vec<Weekday> {
        ALL_DAYS
            .into_iter()
            .filter(|day| self.day_sets.get(day) == Some(&id))
            .collect()
    }

    /// Assign a set to exactly `days`: days it had that aren't listed go back to the active set.
    /// A day already assigned to another set is refused, and nothing changes.
    pub fn assign_days(&mut self, id: Uuid, days: &[Weekday]) -> Result<(), SetError> {
        if self.get(id).is_none() {
            return Err(SetError::NotFound);
        }
        let taken: Vec<String> = ALL_DAYS
            .into_iter()
            .filter(|day| days.contains(day))
            .filter_map(|day| {
                let other = *self.day_sets.get(&day)?;
                if other == id {
                    return None;
                }
                let name = self.get(other).map(|s| s.name.as_str()).unwrap_or("?");
                Some(format!("{} (\"{}\")", day, name))
            })
            .collect();
        if !taken.is_empty() {
            return Err(SetError::Conflict(format!(
                "Already assigned to another set: {}; unassign them there first",
                taken.join(", ")
            )));
        }
        self.day_sets.retain(|_, set| *set != id);
        for day in days {
            self.day_sets.insert(*day, id);
        }
        Ok(())
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

    /// Delete a set. The active set, the last set and a set assigned to days can't be deleted.
    pub fn delete(&mut self, id: Uuid) -> Result<(), SetError> {
        if self.get(id).is_none() {
            return Err(SetError::NotFound);
        }
        if id == self.active_id {
            return Err(SetError::Conflict(
                "Cannot delete the active schedule set".to_string(),
            ));
        }
        let days = self.days_of(id);
        if !days.is_empty() {
            let days: Vec<String> = days.iter().map(|d| d.to_string()).collect();
            return Err(SetError::Conflict(format!(
                "Schedule set is assigned to {}; unassign its days first",
                days.join(", ")
            )));
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
    /// point `active_id` at the first set if it names none, and drop day assignments to sets
    /// that aren't there.
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
        let ids: std::collections::HashSet<Uuid> = self.sets.iter().map(|s| s.id).collect();
        self.day_sets.retain(|_, id| ids.contains(id));
    }
}

pub const ALL_DAYS: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

/// Save `day_sets` Monday first, so the file doesn't reorder itself on every save
fn days_in_order<S: Serializer>(
    day_sets: &HashMap<Weekday, Uuid>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(
        ALL_DAYS
            .iter()
            .filter_map(|day| day_sets.get(day).map(|id| (day, id))),
    )
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

    use chrono::Weekday::{Fri, Mon, Sat, Sun, Thu, Tue, Wed};

    /// "Work week" (active), "Weekend" and "Holiday"
    fn three_sets() -> (ScheduleSets, Uuid, Uuid) {
        let mut sets = ScheduleSets::from_schedule(Schedule::new("Work week"));
        let weekend = sets.create("Weekend", None).unwrap().id;
        let holiday = sets.create("Holiday", None).unwrap().id;
        (sets, weekend, holiday)
    }

    #[test]
    fn test_for_day_uses_assigned_set_and_falls_back_to_active() {
        let (mut sets, weekend, _) = three_sets();
        let active = sets.active_id;

        // No assignments: the active set every day, as before day assignments
        for day in ALL_DAYS {
            assert_eq!(sets.for_day(day).id, active);
        }

        sets.assign_days(weekend, &[Sat, Sun]).unwrap();

        assert_eq!(sets.for_day(Sat).id, weekend);
        assert_eq!(sets.for_day(Sun).id, weekend);
        for day in [Mon, Tue, Wed, Thu, Fri] {
            assert_eq!(sets.for_day(day).id, active, "{day} falls back");
        }
        assert_eq!(sets.days_of(weekend), vec![Sat, Sun]);
    }

    #[test]
    fn test_assign_days_replaces_the_sets_days() {
        let (mut sets, weekend, _) = three_sets();
        sets.assign_days(weekend, &[Fri, Sat, Sun]).unwrap();

        sets.assign_days(weekend, &[Sun, Sat]).unwrap();
        assert_eq!(sets.days_of(weekend), vec![Sat, Sun]);
        assert_eq!(sets.for_day(Fri).id, sets.active_id);

        sets.assign_days(weekend, &[]).unwrap();
        assert!(sets.day_sets.is_empty());
    }

    #[test]
    fn test_overlapping_assignment_is_rejected_and_changes_nothing() {
        let (mut sets, weekend, holiday) = three_sets();
        sets.assign_days(weekend, &[Sat, Sun]).unwrap();

        let err = sets.assign_days(holiday, &[Fri, Sat]).unwrap_err();

        assert_eq!(
            err,
            SetError::Conflict(
                "Already assigned to another set: Sat (\"Weekend\"); unassign them there first"
                    .to_string()
            )
        );
        assert_eq!(sets.days_of(weekend), vec![Sat, Sun]);
        assert!(sets.days_of(holiday).is_empty(), "Fri not assigned either");
        assert_eq!(
            sets.assign_days(Uuid::new_v4(), &[Mon]),
            Err(SetError::NotFound)
        );
    }

    #[test]
    fn test_delete_refuses_a_set_with_days() {
        let (mut sets, weekend, _) = three_sets();
        sets.assign_days(weekend, &[Sat, Sun]).unwrap();

        assert_eq!(
            sets.delete(weekend),
            Err(SetError::Conflict(
                "Schedule set is assigned to Sat, Sun; unassign its days first".to_string()
            ))
        );
        sets.assign_days(weekend, &[]).unwrap();
        sets.delete(weekend).unwrap();
    }

    #[test]
    fn test_normalise_drops_days_of_missing_sets() {
        let (mut sets, weekend, _) = three_sets();
        sets.day_sets.insert(Mon, Uuid::new_v4());
        sets.day_sets.insert(Sat, weekend);

        sets.normalise();

        assert_eq!(sets.day_sets.len(), 1);
        assert_eq!(sets.for_day(Mon).id, sets.active_id);
    }

    #[test]
    fn test_day_sets_json() {
        let (mut sets, weekend, holiday) = three_sets();
        // None assigned: the field is left out, as in files saved before it existed
        let json = serde_json::to_value(&sets).unwrap();
        assert!(json.get("day_sets").is_none());

        sets.assign_days(holiday, &[Wed]).unwrap();
        sets.assign_days(weekend, &[Sun, Sat]).unwrap();
        let text = serde_json::to_string(&sets).unwrap();
        let expected =
            format!(r#""day_sets":{{"Wed":"{holiday}","Sat":"{weekend}","Sun":"{weekend}"}}"#);
        assert!(text.contains(&expected), "{text}");

        let loaded: ScheduleSets = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded.day_sets, sets.day_sets);
    }
}
