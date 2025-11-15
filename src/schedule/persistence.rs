use super::Schedule;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Load a schedule from a JSON file
pub fn load_schedule<P: AsRef<Path>>(path: P) -> Result<Schedule> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read schedule file: {}", path.display()))?;

    let schedule: Schedule = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse schedule JSON from: {}", path.display()))?;

    Ok(schedule)
}

/// Save a schedule to a JSON file
pub fn save_schedule<P: AsRef<Path>>(schedule: &Schedule, path: P) -> Result<()> {
    let path = path.as_ref();
    let json = serde_json::to_string_pretty(schedule)
        .context("Failed to serialize schedule to JSON")?;

    fs::write(path, json)
        .with_context(|| format!("Failed to write schedule file: {}", path.display()))?;

    Ok(())
}

/// Load schedule from default location, or create a default one if it doesn't exist
pub fn load_or_create_default<P: AsRef<Path>>(path: P) -> Result<Schedule> {
    let path = path.as_ref();

    if path.exists() {
        println!("Loading schedule from: {}", path.display());
        load_schedule(path)
    } else {
        println!("No schedule file found at: {}", path.display());
        println!("Creating default schedule...");

        let schedule = Schedule::new("Default Heating Schedule");

        // Save the default schedule for next time
        save_schedule(&schedule, path)
            .context("Failed to save default schedule")?;

        println!("Default schedule saved to: {}", path.display());
        Ok(schedule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{HeatingState, ScheduleEntry, TimePeriod};
    use tempfile::tempdir;

    #[test]
    fn test_save_and_load_schedule() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_schedule.json");

        // Create a schedule with some entries
        let mut schedule = Schedule::new("Test Schedule");
        schedule.add_entry(ScheduleEntry::new(
            "Morning Heating",
            TimePeriod::new(6, 0, 9, 0),
            HeatingState::On,
        ));
        schedule.add_entry(ScheduleEntry::new(
            "Evening Heating",
            TimePeriod::new(17, 0, 22, 0),
            HeatingState::On,
        ));

        // Save it
        save_schedule(&schedule, &file_path).unwrap();

        // Load it back
        let loaded = load_schedule(&file_path).unwrap();

        // Verify
        assert_eq!(loaded.name, "Test Schedule");
        assert_eq!(loaded.entries.len(), schedule.entries.len());
    }

    #[test]
    fn test_load_or_create_default() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("schedule.json");

        // First call should create default
        let schedule1 = load_or_create_default(&file_path).unwrap();
        assert!(file_path.exists());

        // Second call should load existing
        let schedule2 = load_or_create_default(&file_path).unwrap();
        assert_eq!(schedule1.name, schedule2.name);
    }
}
