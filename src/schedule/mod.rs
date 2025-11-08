use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use std::fmt;

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

    /// Check if a given time falls within this period
    pub fn contains(&self, time: NaiveTime) -> bool {
        if self.start <= self.end {
            // Normal case: e.g., 08:00 - 22:00
            time >= self.start && time < self.end
        } else {
            // Crosses midnight: e.g., 22:00 - 06:00
            time >= self.start || time < self.end
        }
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
        //todo fix end time
        let full_day = TimePeriod::new(00, 00, 23, 59);
        ScheduleEntry::new("default", full_day, HeatingState::Off)
    }
}

/// A complete heating schedule containing multiple entries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub name: String,
    pub entries: Vec<ScheduleEntry>,
}

impl Schedule {
    /// Create a new empty schedule
    pub fn new(name: impl Into<String>) -> Self {
        Schedule {
            name: name.into(),
            entries: vec![ScheduleEntry::default()],
        }
    }

    /// Add an entry to the schedule
    pub fn add_entry(&mut self, entry: ScheduleEntry) {
        self.entries.push(entry);
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
}
