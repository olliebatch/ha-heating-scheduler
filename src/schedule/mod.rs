use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use std::fmt;

pub mod persistence;

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

    /// Subtract another period from this one, returning the remaining parts
    /// This handles splitting a period when another is inserted into it
    pub fn subtract(&self, other: &TimePeriod) -> Vec<TimePeriod> {
        // If no overlap, return self unchanged
        if !self.overlaps(other) {
            return vec![*self];
        }

        let mut result = Vec::new();

        // Special case: subtracting from a full day
        if self.is_full_day() {
            let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();

            if other.is_full_day() {
                // Full day - full day = nothing
                return vec![];
            }

            if other.start <= other.end {
                // Subtracting a normal period from full day
                // Results in: [00:00, other.start) and [other.end, 00:00)
                // But we need to avoid creating zero-length or full-day periods

                // Add the "before" period if other doesn't start at midnight
                if other.start != midnight {
                    result.push(TimePeriod {
                        start: midnight,
                        end: other.start,
                    });
                }

                // Add the "after" period if other doesn't end at midnight
                // (to avoid creating [00:00, 00:00) which would be a full day)
                if other.end != midnight {
                    result.push(TimePeriod {
                        start: other.end,
                        end: midnight, // This represents crossing midnight
                    });
                }
            } else {
                // Subtracting a midnight-crossing period from full day
                // Results in: [other.end, other.start)
                if other.end != other.start {
                    result.push(TimePeriod {
                        start: other.end,
                        end: other.start,
                    });
                }
            }
            return result;
        }

        // Handle normal periods (don't cross midnight)
        if self.start <= self.end && other.start <= other.end {
            // Before part: if self starts before other
            if self.start < other.start {
                result.push(TimePeriod {
                    start: self.start,
                    end: other.start,
                });
            }

            // After part: if self ends after other
            if other.end < self.end {
                result.push(TimePeriod {
                    start: other.end,
                    end: self.end,
                });
            }
        } else {
            // For midnight-crossing periods, we need more complex logic
            // Convert to two normal periods and process each
            if self.start > self.end {
                // Self crosses midnight: split into [start..midnight] and [midnight..end]
                let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();

                // If self.end is midnight, period1 would be identical to self, causing infinite recursion
                // In this case, just process it as the "before midnight" part
                if self.end == midnight {
                    // [self.start, 00:00) - only process the portion before midnight
                    if other.start <= other.end {
                        // Other is a normal period
                        // Add the part before other starts (if any)
                        if other.start > self.start {
                            result.push(TimePeriod {
                                start: self.start,
                                end: other.start,
                            });
                        }
                        // Add the part after other ends (if any)
                        // Since self goes to midnight, check if other ends before midnight
                        // and if other.end is within self's range [self.start, midnight)
                        if other.end >= self.start && other.end != midnight {
                            result.push(TimePeriod {
                                start: other.end,
                                end: midnight,
                            });
                        }
                    } else {
                        // Other also crosses midnight - complex case
                        // Just return self for now to avoid recursion
                        result.push(*self);
                    }
                } else {
                    let period1 = TimePeriod {
                        start: self.start,
                        end: midnight,
                    };
                    let period2 = TimePeriod {
                        start: midnight,
                        end: self.end,
                    };

                    // Subtract from both parts
                    result.extend(period1.subtract(other));
                    // Only process period2 if it's not zero-length (to avoid [00:00, 00:00) recursion)
                    if period2.start != period2.end {
                        result.extend(period2.subtract(other));
                    }
                }
            } else {
                // Other crosses midnight
                let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
                let other1 = TimePeriod {
                    start: other.start,
                    end: midnight,
                };
                let other2 = TimePeriod {
                    start: midnight,
                    end: other.end,
                };
                let mut temp = vec![*self];
                temp = temp
                    .iter()
                    .flat_map(|p| p.subtract(&other1))
                    .collect();
                temp = temp
                    .iter()
                    .flat_map(|p| p.subtract(&other2))
                    .collect();
                result = temp;
            }
        }

        result
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
            name: name.into(),
            time_period,
            heating_state,
        }
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
    pub name: String,
    pub entries: Vec<ScheduleEntry>,
}

impl Schedule {
    pub fn new(name: impl Into<String>) -> Self {
        Schedule {
            name: name.into(),
            entries: vec![ScheduleEntry::default()],
        }
    }

    pub fn get_active_entry(&self, time: &chrono::DateTime<chrono::Local>) -> Option<&ScheduleEntry> {
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

    pub fn add_entry(&mut self, entry: ScheduleEntry) {
        let mut new_entries = Vec::new();

        // Process each existing entry
        for existing in &self.entries {
            // If the existing entry overlaps with the new one, split it
            if existing.time_period.overlaps(&entry.time_period) {
                // Subtract the new entry's period from the existing one
                let remaining_periods = existing.time_period.subtract(&entry.time_period);

                // Create new entries for each remaining period with the same properties
                for period in remaining_periods {
                    new_entries.push(ScheduleEntry {
                        name: existing.name.clone(),
                        time_period: period,
                        heating_state: existing.heating_state.clone(),
                    });
                }
            } else {
                // No overlap, keep the existing entry as-is
                new_entries.push(existing.clone());
            }
        }

        // Add the new entry
        new_entries.push(entry);

        // Sort entries by start time for cleaner organization
        new_entries.sort_by(|a, b| {
            a.time_period.start.cmp(&b.time_period.start)
        });

        self.entries = new_entries;
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
    fn test_time_period_subtract_simple() {
        // Test: 00:00-23:59 subtract 08:00-17:00 should give two periods
        let full_day = TimePeriod::new(0, 0, 23, 59);
        let work_hours = TimePeriod::new(8, 0, 17, 0);

        let result = full_day.subtract(&work_hours);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], TimePeriod::new(0, 0, 8, 0));
        assert_eq!(result[1], TimePeriod::new(17, 0, 23, 59));
    }

    #[test]
    fn test_time_period_subtract_no_overlap() {
        // Test: subtracting non-overlapping periods returns original
        let period1 = TimePeriod::new(8, 0, 12, 0);
        let period2 = TimePeriod::new(14, 0, 18, 0);

        let result = period1.subtract(&period2);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], period1);
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

        // Should now have 3 entries: before work, work hours, after work
        assert_eq!(schedule.entries.len(), 3);

        // Verify the entries cover the full day
        assert_eq!(schedule.entries[0].time_period, TimePeriod::new(0, 0, 8, 0));
        assert_eq!(schedule.entries[0].heating_state, HeatingState::Off);

        assert_eq!(schedule.entries[1].time_period, TimePeriod::new(8, 0, 17, 0));
        assert_eq!(schedule.entries[1].heating_state, HeatingState::On);

        // The after-work period goes from 17:00 to 00:00 (midnight crossing)
        assert_eq!(schedule.entries[2].time_period, TimePeriod::new(17, 0, 0, 0));
        assert_eq!(schedule.entries[2].heating_state, HeatingState::Off);
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
                current_end, next_start,
                "Gap found between entries {} and {}",
                i, i + 1
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

        // Verify we have the 22:00-00:00 period
        let has_22_to_midnight = schedule.entries.iter().any(|e| {
            e.time_period.start == NaiveTime::from_hms_opt(22, 0, 0).unwrap()
                && e.time_period.end == NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        });
        assert!(
            has_22_to_midnight,
            "Missing coverage for 22:00-00:00 period. Entries: {:#?}",
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
            HeatingState::On,
        ));

        // Add a larger entry that covers it
        schedule.add_entry(ScheduleEntry::new(
            "Large",
            TimePeriod::new(8, 0, 14, 0),
            HeatingState::Off,
        ));

        // The small entry should be completely replaced
        let has_small = schedule.entries.iter().any(|e| e.name == "Small");
        assert!(!has_small, "Small entry should be completely covered");

        let has_large = schedule.entries.iter().any(|e| e.name == "Large");
        assert!(has_large, "Large entry should exist");
    }
}
