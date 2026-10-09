use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub mod persistence;
pub mod sets;

/// Represents a time period within a day (e.g., 08:00 - 22:00)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimePeriod {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl TimePeriod {
    /// Create a new time period
    pub fn new(start_hour: u32, start_minute: u32, end_hour: u32, end_minute: u32) -> Self {
        TimePeriod {
            start: NaiveTime::from_hms_opt(start_hour, start_minute, 0)
                .expect("Invalid start time"),
            end: NaiveTime::from_hms_opt(end_hour, end_minute, 0).expect("Invalid end time"),
        }
    }

    /// Check if this is a full day period (00:00 - 00:00)
    pub fn is_full_day(&self) -> bool {
        let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        self.start == midnight && self.end == midnight
    }

    /// Check if a given time falls within this period
    pub fn contains(&self, time: NaiveTime) -> bool {
        // Special case: full day (00:00 - 00:00) contains all times
        if self.is_full_day() {
            return true;
        }

        if self.start <= self.end {
            // Normal case: e.g., 08:00 - 22:00
            time >= self.start && time < self.end
        } else {
            // Crosses midnight: e.g., 22:00 - 06:00
            time >= self.start || time < self.end
        }
    }

    /// Check if this period overlaps with another
    pub fn overlaps(&self, other: &TimePeriod) -> bool {
        // Full day always overlaps with everything
        if self.is_full_day() || other.is_full_day() {
            return true;
        }
        self.contains(other.start) || other.contains(self.start)
    }
}

impl fmt::Display for TimePeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02} - {:02}:{:02}",
            self.start.hour(),
            self.start.minute(),
            self.end.hour(),
            self.end.minute()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HeatingState {
    Off,
    On,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub id: Uuid,
    pub name: String,
    pub time_period: TimePeriod,
    pub heating_state: HeatingState,
}

impl ScheduleEntry {
    /// Create a new schedule entry
    pub fn new(
        name: impl Into<String>,
        time_period: TimePeriod,
        heating_state: HeatingState,
    ) -> Self {
        ScheduleEntry {
            id: Uuid::new_v4(),
            name: name.into(),
            time_period,
            heating_state,
        }
    }
}

/// Request DTO for creating a new schedule entry (without ID)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntryRequest {
    pub name: String,
    pub time_period: TimePeriod,
    pub heating_state: HeatingState,
}

impl From<ScheduleEntryRequest> for ScheduleEntry {
    fn from(request: ScheduleEntryRequest) -> Self {
        ScheduleEntry::new(request.name, request.time_period, request.heating_state)
    }
}

impl Default for ScheduleEntry {
    fn default() -> Self {
        // Full day period: 00:00 - 00:00 (represents entire day)
        let full_day = TimePeriod::new(0, 0, 0, 0);
        ScheduleEntry::new("default", full_day, HeatingState::Off)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Stable across renames and restarts. Files saved before ids existed get a new one on load.
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub name: String,
    pub entries: Vec<ScheduleEntry>,
}

impl Schedule {
    pub fn new(name: impl Into<String>) -> Self {
        Schedule {
            id: Uuid::new_v4(),
            name: name.into(),
            entries: vec![ScheduleEntry::default()],
        }
    }

    pub fn get_active_entry(
        &self,
        time: &chrono::DateTime<chrono::Local>,
    ) -> Option<&ScheduleEntry> {
        let naive_time = time.time();
        self.entries
            .iter()
            .find(|entry| entry.time_period.contains(naive_time))
    }

    pub fn get_current_state(&self, time: &chrono::DateTime<chrono::Local>) -> HeatingState {
        self.get_active_entry(time)
            .map(|entry| entry.heating_state.clone())
            .unwrap_or(HeatingState::Off)
    }

    /// Add an entry on top of the schedule. It replaces whatever covered its period before.
    pub fn add_entry(&mut self, entry: ScheduleEntry) {
        self.entries.push(entry);
        self.normalise();
    }

    /// Delete an entry by ID. The previous entry (by start time, wrapping round midnight)
    /// is extended to fill the gap. Deleting the only entry is refused.
    pub fn delete_entry(&mut self, entry_id: Uuid) -> Result<(), String> {
        if !self.entries.iter().any(|e| e.id == entry_id) {
            return Err(format!("Entry with ID {} not found", entry_id));
        }
        if self.entries.len() == 1 {
            return Err("Cannot delete the only schedule entry".to_string());
        }

        self.entries.retain(|e| e.id != entry_id);
        self.normalise();
        Ok(())
    }

    /// Rebuild the entries into the smallest set that covers the day exactly once, with no two
    /// neighbouring entries (including across midnight) sharing a heating state.
    ///
    /// - Where entries overlap, the later entry in `entries` wins.
    /// - A gap is filled by the entry before it, wrapping round midnight.
    /// - When neighbouring pieces merge, the merged entry keeps the id and name of the earliest
    ///   piece in loop order: the piece with the earliest start, except across midnight, where
    ///   the piece that starts before midnight wins. A schedule that is one state all day
    ///   becomes a single 00:00 - 00:00 entry named after the piece that covers 00:00.
    /// - Any other piece that would repeat an id already used gets a new id.
    pub fn normalise(&mut self) {
        const DAY: u32 = 24 * 60 * 60;
        let secs = |t: NaiveTime| t.num_seconds_from_midnight();
        let time = |s: u32| NaiveTime::from_num_seconds_from_midnight_opt(s % DAY, 0).unwrap();

        if self.entries.is_empty() {
            self.entries.push(ScheduleEntry::default());
            return;
        }

        // Work in whole seconds, so a fractional start can't leave an entry owning nothing
        for e in &mut self.entries {
            e.time_period.start = time(secs(e.time_period.start));
            e.time_period.end = time(secs(e.time_period.end));
        }

        // Every start and end is a boundary; between two boundaries the owner can't change.
        let mut bounds: Vec<u32> = vec![0, DAY];
        for e in &self.entries {
            bounds.push(secs(e.time_period.start));
            bounds.push(secs(e.time_period.end));
        }
        bounds.sort_unstable();
        bounds.dedup();

        // (start, end, index of owning entry) for each slice; the last entry containing it wins
        let mut slices: Vec<(u32, u32, Option<usize>)> = bounds
            .windows(2)
            .map(|w| {
                let owner = self
                    .entries
                    .iter()
                    .rposition(|e| e.time_period.contains(time(w[0])));
                (w[0], w[1], owner)
            })
            .collect();

        // Fill gaps from the previous slice, wrapping round midnight
        let Some(last_owned) = slices.iter().rev().find_map(|s| s.2) else {
            self.entries = vec![ScheduleEntry::default()];
            return;
        };
        let mut prev = last_owned;
        for slice in &mut slices {
            prev = *slice.2.get_or_insert(prev);
        }

        // Merge neighbouring slices with the same state; the first slice names the run
        let mut runs: Vec<(u32, u32, usize)> = Vec::new();
        for (start, end, owner) in slices {
            let owner = owner.unwrap();
            match runs.last_mut() {
                Some(run)
                    if self.entries[run.2].heating_state == self.entries[owner].heating_state =>
                {
                    run.1 = end
                }
                _ => runs.push((start, end, owner)),
            }
        }

        // Merge across midnight: the run that starts before midnight takes over the first run
        if runs.len() > 1
            && self.entries[runs[0].2].heating_state
                == self.entries[runs[runs.len() - 1].2].heating_state
        {
            let first = runs.remove(0);
            runs.last_mut().unwrap().1 = first.1;
        }

        let mut used_ids = std::collections::HashSet::new();
        self.entries = runs
            .into_iter()
            .map(|(start, end, owner)| {
                let source = &self.entries[owner];
                let id = if used_ids.insert(source.id) {
                    source.id
                } else {
                    Uuid::new_v4()
                };
                ScheduleEntry {
                    id,
                    name: source.name.clone(),
                    time_period: TimePeriod {
                        start: time(start),
                        end: time(end),
                    },
                    heating_state: source.heating_state.clone(),
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_period_contains() {
        let period = TimePeriod::new(8, 0, 22, 0);

        assert!(period.contains(NaiveTime::from_hms_opt(8, 0, 0).unwrap()));
        assert!(period.contains(NaiveTime::from_hms_opt(15, 30, 0).unwrap()));
        assert!(!period.contains(NaiveTime::from_hms_opt(22, 0, 0).unwrap()));
        assert!(!period.contains(NaiveTime::from_hms_opt(7, 59, 0).unwrap()));
    }

    #[test]
    fn test_time_period_crosses_midnight() {
        let period = TimePeriod::new(22, 0, 6, 0);

        assert!(period.contains(NaiveTime::from_hms_opt(22, 0, 0).unwrap()));
        assert!(period.contains(NaiveTime::from_hms_opt(23, 30, 0).unwrap()));
        assert!(period.contains(NaiveTime::from_hms_opt(0, 0, 0).unwrap()));
        assert!(period.contains(NaiveTime::from_hms_opt(5, 59, 0).unwrap()));
        assert!(!period.contains(NaiveTime::from_hms_opt(6, 0, 0).unwrap()));
        assert!(!period.contains(NaiveTime::from_hms_opt(12, 0, 0).unwrap()));
    }

    #[test]
    fn test_time_period_overlaps() {
        let period1 = TimePeriod::new(8, 0, 17, 0);
        let period2 = TimePeriod::new(12, 0, 14, 0);
        let period3 = TimePeriod::new(18, 0, 20, 0);

        assert!(period1.overlaps(&period2)); // period2 is inside period1
        assert!(period2.overlaps(&period1)); // symmetric
        assert!(!period1.overlaps(&period3)); // no overlap
    }

    #[test]
    fn test_schedule_add_entry_splits_default() {
        // Test: Adding an entry to a new schedule should split the default entry
        let mut schedule = Schedule::new("Test Schedule");

        // Initially should have one default entry (full day, Off)
        assert_eq!(schedule.entries.len(), 1);

        // Add a work hours entry (heating On)
        schedule.add_entry(ScheduleEntry::new(
            "Work Hours",
            TimePeriod::new(8, 0, 17, 0),
            HeatingState::On,
        ));

        // Should now have 2 entries: work hours, and the rest of the day across midnight
        assert_eq!(schedule.entries.len(), 2);

        assert_eq!(
            schedule.entries[0].time_period,
            TimePeriod::new(8, 0, 17, 0)
        );
        assert_eq!(schedule.entries[0].heating_state, HeatingState::On);

        assert_eq!(
            schedule.entries[1].time_period,
            TimePeriod::new(17, 0, 8, 0)
        );
        assert_eq!(schedule.entries[1].heating_state, HeatingState::Off);
    }

    #[test]
    fn test_schedule_add_multiple_entries() {
        // Test: Adding multiple entries maintains full coverage
        let mut schedule = Schedule::new("Test Schedule");

        // Add morning heating
        schedule.add_entry(ScheduleEntry::new(
            "Morning",
            TimePeriod::new(6, 0, 9, 0),
            HeatingState::On,
        ));

        // Add evening heating
        schedule.add_entry(ScheduleEntry::new(
            "Evening",
            TimePeriod::new(17, 0, 22, 0),
            HeatingState::On,
        ));

        // Add lunch break (turns off during work hours if we add work hours)
        schedule.add_entry(ScheduleEntry::new(
            "Work",
            TimePeriod::new(9, 0, 17, 0),
            HeatingState::On,
        ));

        // Now add lunch break
        schedule.add_entry(ScheduleEntry::new(
            "Lunch Break",
            TimePeriod::new(12, 0, 13, 0),
            HeatingState::Off,
        ));

        // Verify we have the expected entries
        // Should be: 00:00-06:00 (Off), 06:00-09:00 (On), 09:00-12:00 (On),
        //            12:00-13:00 (Off), 13:00-17:00 (On), 17:00-22:00 (On), 22:00-23:59 (Off)
        assert!(schedule.entries.len() >= 4);

        // Verify no gaps: check that entries are properly ordered
        let mut entries_sorted = schedule.entries.clone();
        entries_sorted.sort_by(|a, b| a.time_period.start.cmp(&b.time_period.start));

        for i in 0..entries_sorted.len() - 1 {
            let current_end = entries_sorted[i].time_period.end;
            let next_start = entries_sorted[i + 1].time_period.start;
            // End of current should equal start of next (no gaps)
            assert_eq!(
                current_end,
                next_start,
                "Gap found between entries {} and {}",
                i,
                i + 1
            );
        }
    }

    #[test]
    fn test_schedule_coverage_22_to_midnight() {
        // Specific test for the bug: ensure 22:00-00:00 is covered
        let mut schedule = Schedule::new("Test Schedule");

        // Add heating from 10:00-11:00
        schedule.add_entry(ScheduleEntry::new(
            "Morning",
            TimePeriod::new(10, 0, 11, 0),
            HeatingState::On,
        ));

        // Add heating from 17:00-22:00
        schedule.add_entry(ScheduleEntry::new(
            "Evening",
            TimePeriod::new(17, 0, 22, 0),
            HeatingState::On,
        ));

        // 22:00-00:00 is merged with 00:00-10:00 into one entry across midnight
        let has_22_to_10 = schedule
            .entries
            .iter()
            .any(|e| e.time_period == TimePeriod::new(22, 0, 10, 0));
        assert!(
            has_22_to_10,
            "Missing coverage for 22:00-10:00 period. Entries: {:#?}",
            schedule.entries
        );

        // Verify 23:00 is covered by some entry
        let time_23 = NaiveTime::from_hms_opt(23, 0, 0).unwrap();
        let is_covered = schedule
            .entries
            .iter()
            .any(|e| e.time_period.contains(time_23));
        assert!(is_covered, "Time 23:00 is not covered by any entry");
    }

    #[test]
    fn test_schedule_entry_completely_covered() {
        // Test: Adding an entry that completely covers an existing one
        let mut schedule = Schedule::new("Test Schedule");

        // Add a small entry
        schedule.add_entry(ScheduleEntry::new(
            "Small",
            TimePeriod::new(10, 0, 12, 0),
            HeatingState::Off,
        ));

        // Add a larger entry that covers it
        schedule.add_entry(ScheduleEntry::new(
            "Large",
            TimePeriod::new(8, 0, 14, 0),
            HeatingState::On,
        ));

        // The small entry should be completely replaced
        let has_small = schedule.entries.iter().any(|e| e.name == "Small");
        assert!(!has_small, "Small entry should be completely covered");

        let has_large = schedule.entries.iter().any(|e| e.name == "Large");
        assert!(has_large, "Large entry should exist");
    }

    #[test]
    fn test_delete_entry_extends_previous() {
        // Create a schedule with multiple entries
        let mut schedule = Schedule::new("Test Schedule");

        // Add morning heating (06:00-09:00)
        schedule.add_entry(ScheduleEntry::new(
            "Morning",
            TimePeriod::new(6, 0, 9, 0),
            HeatingState::On,
        ));

        // Add work hours (09:00-17:00)
        schedule.add_entry(ScheduleEntry::new(
            "Work",
            TimePeriod::new(9, 0, 17, 0),
            HeatingState::Off,
        ));

        // Add evening heating (17:00-22:00)
        schedule.add_entry(ScheduleEntry::new(
            "Evening",
            TimePeriod::new(17, 0, 22, 0),
            HeatingState::On,
        ));

        // Should have: 00:00-06:00 (default off), 06:00-09:00 (morning), 09:00-17:00 (work), 17:00-22:00 (evening), 22:00-00:00 (default off)
        let initial_count = schedule.entries.len();
        assert!(initial_count >= 4);

        // Find the "Work" entry to delete
        let work_entry_id = schedule
            .entries
            .iter()
            .find(|e| e.name == "Work")
            .unwrap()
            .id;

        // Delete the "Work" entry
        schedule.delete_entry(work_entry_id).unwrap();

        // Morning extends over Work, then merges with Evening (both On)
        assert_eq!(schedule.entries.len(), initial_count - 2);

        let morning_entry = schedule
            .entries
            .iter()
            .find(|e| e.name == "Morning")
            .unwrap();
        assert_eq!(
            morning_entry.time_period,
            TimePeriod::new(6, 0, 22, 0),
            "Morning entry should cover 06:00-22:00 after deleting Work entry"
        );

        // Verify no gaps: 12:00 (during old work hours) should now be in the Morning entry
        let noon = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        assert!(
            morning_entry.time_period.contains(noon),
            "Morning entry should now cover noon (12:00)"
        );
    }

    #[test]
    fn test_delete_entry_wraps_around_midnight() {
        // Test deleting the first entry (should extend the last entry)
        let mut schedule = Schedule::new("Test Schedule");

        schedule.add_entry(ScheduleEntry::new(
            "Morning",
            TimePeriod::new(6, 0, 12, 0),
            HeatingState::On,
        ));
        schedule.add_entry(ScheduleEntry::new(
            "Early",
            TimePeriod::new(0, 0, 3, 0),
            HeatingState::On,
        ));

        // 00:00-03:00 On, 03:00-06:00 Off, 06:00-12:00 On, 12:00-00:00 Off
        assert_eq!(schedule.entries.len(), 4);
        let first_entry_id = schedule.entries[0].id;

        // Delete the first entry (00:00-03:00)
        schedule.delete_entry(first_entry_id).unwrap();

        // The last entry extends to 03:00 and merges with 03:00-06:00 (both Off)
        let last_entry = schedule
            .entries
            .iter()
            .find(|e| e.time_period == TimePeriod::new(12, 0, 6, 0));

        assert!(
            last_entry.is_some(),
            "Last entry should extend to 06:00 after deleting first entry: {:#?}",
            schedule.entries
        );
    }

    /// Every second of the day is covered exactly once, and neighbours differ in state
    fn assert_normalised(schedule: &Schedule) {
        let entries = &schedule.entries;
        assert!(!entries.is_empty());
        for minute in 0..24 * 60 {
            let t = NaiveTime::from_hms_opt(minute / 60, minute % 60, 0).unwrap();
            let covering = entries.iter().filter(|e| e.time_period.contains(t)).count();
            assert_eq!(
                covering, 1,
                "{} covered {} times: {:#?}",
                t, covering, entries
            );
        }
        if entries.len() > 1 {
            for i in 0..entries.len() {
                let a = &entries[i];
                let b = &entries[(i + 1) % entries.len()];
                assert_eq!(a.time_period.end, b.time_period.start, "{:#?}", entries);
                assert_ne!(a.heating_state, b.heating_state, "{:#?}", entries);
            }
        }
        let ids: std::collections::HashSet<_> = entries.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), entries.len(), "duplicate ids: {:#?}", entries);
    }

    fn entry(name: &str, period: TimePeriod, state: HeatingState) -> ScheduleEntry {
        ScheduleEntry::new(name, period, state)
    }

    #[test]
    fn test_normalise_merges_across_midnight() {
        let late = entry("Late", TimePeriod::new(22, 0, 0, 0), HeatingState::Off);
        let early = entry("Early", TimePeriod::new(0, 0, 6, 0), HeatingState::Off);
        let day = entry("Day", TimePeriod::new(6, 0, 22, 0), HeatingState::On);
        let late_id = late.id;
        let mut schedule = Schedule {
            id: Uuid::new_v4(),
            name: "Test".into(),
            entries: vec![early, day, late],
        };

        schedule.normalise();

        assert_normalised(&schedule);
        assert_eq!(schedule.entries.len(), 2);
        let night = &schedule.entries[1];
        assert_eq!(night.time_period, TimePeriod::new(22, 0, 6, 0));
        // The piece that starts before midnight survives
        assert_eq!(night.id, late_id);
        assert_eq!(night.name, "Late");
    }

    #[test]
    fn test_normalise_merge_keeps_earliest_piece() {
        let first = entry("First", TimePeriod::new(6, 0, 9, 0), HeatingState::On);
        let second = entry("Second", TimePeriod::new(9, 0, 17, 0), HeatingState::On);
        let first_id = first.id;
        let mut schedule = Schedule::new("Test");
        schedule.add_entry(second);
        schedule.add_entry(first);

        assert_normalised(&schedule);
        assert_eq!(schedule.entries.len(), 2);
        let on = &schedule.entries[0];
        assert_eq!(on.time_period, TimePeriod::new(6, 0, 17, 0));
        assert_eq!(on.id, first_id);
        assert_eq!(on.name, "First");
    }

    #[test]
    fn test_normalise_all_one_state_is_full_day() {
        let mut schedule = Schedule::new("Test");
        schedule.add_entry(entry(
            "Day",
            TimePeriod::new(8, 0, 17, 0),
            HeatingState::Off,
        ));

        assert_eq!(schedule.entries.len(), 1);
        assert!(schedule.entries[0].time_period.is_full_day());

        schedule.add_entry(entry("On", TimePeriod::new(22, 0, 6, 0), HeatingState::On));
        schedule.add_entry(entry(
            "All On",
            TimePeriod::new(6, 0, 22, 0),
            HeatingState::On,
        ));
        assert_eq!(schedule.entries.len(), 1);
        assert!(schedule.entries[0].time_period.is_full_day());
        assert_eq!(schedule.entries[0].heating_state, HeatingState::On);
    }

    #[test]
    fn test_add_midnight_crossing_onto_midnight_crossing() {
        // The case TimePeriod::subtract used to leave overlapping
        let mut schedule = Schedule::new("Test");
        schedule.add_entry(entry("Day", TimePeriod::new(6, 0, 22, 0), HeatingState::On));
        assert_eq!(
            schedule.entries[1].time_period,
            TimePeriod::new(22, 0, 6, 0)
        );

        schedule.add_entry(entry(
            "Late",
            TimePeriod::new(23, 0, 1, 0),
            HeatingState::On,
        ));

        assert_normalised(&schedule);
        let periods: Vec<_> = schedule.entries.iter().map(|e| e.time_period).collect();
        assert_eq!(
            periods,
            vec![
                TimePeriod::new(1, 0, 6, 0),
                TimePeriod::new(6, 0, 22, 0),
                TimePeriod::new(22, 0, 23, 0),
                TimePeriod::new(23, 0, 1, 0),
            ]
        );
    }

    #[test]
    fn test_delete_leaves_no_same_state_neighbours() {
        let mut schedule = Schedule::new("Test");
        schedule.add_entry(entry(
            "Morning",
            TimePeriod::new(6, 0, 9, 0),
            HeatingState::On,
        ));
        schedule.add_entry(entry(
            "Evening",
            TimePeriod::new(17, 0, 22, 0),
            HeatingState::On,
        ));
        let morning_id = schedule
            .entries
            .iter()
            .find(|e| e.name == "Morning")
            .unwrap()
            .id;

        // Deleting Morning extends the Off before it: Off | Off must merge
        schedule.delete_entry(morning_id).unwrap();

        assert_normalised(&schedule);
        assert_eq!(schedule.entries.len(), 2);
    }

    #[test]
    fn test_delete_wrapping_entry_extends_previous() {
        let mut schedule = Schedule::new("Test");
        schedule.add_entry(entry("Day", TimePeriod::new(6, 0, 22, 0), HeatingState::On));
        let night_id = schedule.entries[1].id;

        schedule.delete_entry(night_id).unwrap();

        assert_normalised(&schedule);
        assert_eq!(schedule.entries.len(), 1);
        assert!(schedule.entries[0].time_period.is_full_day());
        assert_eq!(schedule.entries[0].heating_state, HeatingState::On);
    }

    #[test]
    fn test_add_fractional_seconds_is_kept() {
        let mut schedule = Schedule::new("Test");
        let start = NaiveTime::from_hms_milli_opt(8, 0, 0, 500).unwrap();
        let end = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        schedule.add_entry(entry("Half", TimePeriod { start, end }, HeatingState::On));

        assert_normalised(&schedule);
        assert_eq!(schedule.entries[0].time_period, TimePeriod::new(8, 0, 9, 0));
        assert_eq!(schedule.entries[0].heating_state, HeatingState::On);
    }

    #[test]
    fn test_delete_only_entry_is_refused() {
        let mut schedule = Schedule::new("Test");
        let id = schedule.entries[0].id;

        assert!(schedule.delete_entry(id).is_err());
        assert_eq!(schedule.entries.len(), 1);
    }

    #[test]
    fn test_many_random_edits_stay_normalised() {
        // Deterministic pseudo-random adds and deletes
        let mut seed: u32 = 12345;
        let mut next = |n: u32| {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let mut schedule = Schedule::new("Test");
        for _ in 0..500 {
            if next(4) == 0 && schedule.entries.len() > 1 {
                let i = next(schedule.entries.len() as u32) as usize;
                schedule.delete_entry(schedule.entries[i].id).unwrap();
            } else {
                let state = if next(2) == 0 {
                    HeatingState::On
                } else {
                    HeatingState::Off
                };
                let period = TimePeriod::new(next(24), next(4) * 15, next(24), next(4) * 15);
                schedule.add_entry(entry("Random", period, state));
            }
            assert_normalised(&schedule);
        }
    }
}
