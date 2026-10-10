use super::Schedule;
use super::sets::ScheduleSets;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Load a schedule from a JSON file
pub fn load_schedule<P: AsRef<Path>>(path: P) -> Result<Schedule> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read schedule file: {}", path.display()))?;

    let mut schedule: Schedule = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse schedule JSON from: {}", path.display()))?;

    // Fix up schedules saved before entries were normalised
    schedule.normalise();

    Ok(schedule)
}

/// Save a schedule to a JSON file
pub fn save_schedule<P: AsRef<Path>>(schedule: &Schedule, path: P) -> Result<()> {
    let path = path.as_ref();
    let json =
        serde_json::to_string_pretty(schedule).context("Failed to serialize schedule to JSON")?;

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
        save_schedule(&schedule, path).context("Failed to save default schedule")?;

        println!("Default schedule saved to: {}", path.display());
        Ok(schedule)
    }
}

/// Load schedule sets from a JSON file
pub fn load_sets<P: AsRef<Path>>(path: P) -> Result<ScheduleSets> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read schedule sets file: {}", path.display()))?;

    let mut sets: ScheduleSets = serde_json::from_str(&contents).with_context(|| {
        format!(
            "Failed to parse schedule sets JSON from: {}",
            path.display()
        )
    })?;

    sets.normalise();
    Ok(sets)
}

/// Save schedule sets to a JSON file
pub fn save_sets<P: AsRef<Path>>(sets: &ScheduleSets, path: P) -> Result<()> {
    let path = path.as_ref();
    let json =
        serde_json::to_string_pretty(sets).context("Failed to serialize schedule sets to JSON")?;

    fs::write(path, json)
        .with_context(|| format!("Failed to write schedule sets file: {}", path.display()))?;

    Ok(())
}

/// Load schedule sets, or migrate from a single schedule if there are none yet.
///
/// When `sets_path` doesn't exist, the schedule at `schedule_path` (or the default schedule)
/// becomes the only set, marked active, and is saved to `sets_path`. `schedule_path` is left
/// on disk untouched as a backup.
pub fn load_or_migrate<P: AsRef<Path>, Q: AsRef<Path>>(
    sets_path: P,
    schedule_path: Q,
) -> Result<ScheduleSets> {
    let sets_path = sets_path.as_ref();
    let schedule_path = schedule_path.as_ref();

    if sets_path.exists() {
        println!("Loading schedule sets from: {}", sets_path.display());
        let sets = load_sets(sets_path)?;
        // If loading normalised or filled in anything, save it so the file matches what's served
        let on_disk: serde_json::Value = serde_json::from_str(&fs::read_to_string(sets_path)?)?;
        if serde_json::to_value(&sets)? != on_disk {
            println!(
                "Saving normalised schedule sets to: {}",
                sets_path.display()
            );
            save_sets(&sets, sets_path).context("Failed to save normalised schedule sets")?;
        }
        return Ok(sets);
    }

    let schedule = if schedule_path.exists() {
        println!(
            "Migrating schedule from {} into schedule sets",
            schedule_path.display()
        );
        load_schedule(schedule_path)?
    } else {
        println!("No schedule found, creating default schedule set...");
        Schedule::new("Default Heating Schedule")
    };

    let sets = ScheduleSets::from_schedule(schedule);
    save_sets(&sets, sets_path).context("Failed to save migrated schedule sets")?;
    println!("Schedule sets saved to: {}", sets_path.display());
    Ok(sets)
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

    #[test]
    fn test_load_normalises_fragments() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("schedule.json");

        let schedule = Schedule {
            id: uuid::Uuid::new_v4(),
            name: "Fragmented".into(),
            entries: vec![
                ScheduleEntry::new("a", TimePeriod::new(0, 0, 6, 0), HeatingState::Off),
                ScheduleEntry::new("b", TimePeriod::new(6, 0, 9, 0), HeatingState::On),
                ScheduleEntry::new("c", TimePeriod::new(9, 0, 22, 0), HeatingState::On),
                ScheduleEntry::new("d", TimePeriod::new(22, 0, 0, 0), HeatingState::Off),
            ],
        };
        save_schedule(&schedule, &file_path).unwrap();

        let loaded = load_or_create_default(&file_path).unwrap();

        let periods: Vec<_> = loaded.entries.iter().map(|e| e.time_period).collect();
        assert_eq!(
            periods,
            vec![TimePeriod::new(6, 0, 22, 0), TimePeriod::new(22, 0, 6, 0)]
        );
        assert_eq!(loaded.entries[0].name, "b");
        assert_eq!(loaded.entries[1].name, "d");
    }

    #[test]
    fn test_migrate_from_legacy_schedule_file() {
        let dir = tempdir().unwrap();
        let schedule_path = dir.path().join("schedule.json");
        let sets_path = dir.path().join("schedule_sets.json");

        // A schedule.json as saved before schedules had ids
        let legacy = r#"{
            "name": "Old",
            "entries": [
                {"id": "00000000-0000-4000-8000-000000000001", "name": "night",
                 "time_period": {"start": "22:00:00", "end": "00:00:00"}, "heating_state": "OFF"},
                {"id": "00000000-0000-4000-8000-000000000002", "name": "early",
                 "time_period": {"start": "00:00:00", "end": "06:00:00"}, "heating_state": "OFF"},
                {"id": "00000000-0000-4000-8000-000000000003", "name": "day",
                 "time_period": {"start": "06:00:00", "end": "22:00:00"}, "heating_state": "ON"}
            ]
        }"#;
        fs::write(&schedule_path, legacy).unwrap();

        let sets = load_or_migrate(&sets_path, &schedule_path).unwrap();

        assert_eq!(sets.sets.len(), 1);
        assert_eq!(sets.active().name, "Old");
        assert_eq!(sets.active().entries.len(), 2, "migrated set is normalised");
        let day = &sets.active().entries[0];
        assert_eq!(day.heating_state, HeatingState::On);
        assert_eq!(
            day.target_temp,
            Some(20.0),
            "On entries get the default target"
        );
        assert_eq!(sets.active().entries[1].target_temp, None);
        assert!(sets_path.exists());
        assert_eq!(fs::read_to_string(&schedule_path).unwrap(), legacy);

        // The id given on migration is stable across restarts
        let reloaded = load_or_migrate(&sets_path, &schedule_path).unwrap();
        assert_eq!(reloaded.active_id, sets.active_id);
        assert_eq!(reloaded.active().id, sets.active().id);
    }

    #[test]
    fn test_migrate_without_any_file_creates_default() {
        let dir = tempdir().unwrap();
        let schedule_path = dir.path().join("schedule.json");
        let sets_path = dir.path().join("schedule_sets.json");

        let sets = load_or_migrate(&sets_path, &schedule_path).unwrap();

        assert_eq!(sets.sets.len(), 1);
        assert!(sets.active().entries[0].time_period.is_full_day());
        assert!(sets_path.exists());
        assert!(!schedule_path.exists());
    }

    #[test]
    fn test_save_and_load_sets() {
        let dir = tempdir().unwrap();
        let sets_path = dir.path().join("schedule_sets.json");
        let mut sets = ScheduleSets::from_schedule(Schedule::new("Work week"));
        let holiday = sets.create("Holiday", None).unwrap().id;
        sets.activate(holiday).unwrap();

        save_sets(&sets, &sets_path).unwrap();
        let loaded = load_sets(&sets_path).unwrap();

        assert_eq!(loaded.active_id, holiday);
        let ids: Vec<_> = loaded.sets.iter().map(|s| s.id).collect();
        assert_eq!(ids, sets.sets.iter().map(|s| s.id).collect::<Vec<_>>());
    }

    #[test]
    fn test_load_sets_without_targets_uses_stored_default() {
        let dir = tempdir().unwrap();
        let sets_path = dir.path().join("schedule_sets.json");
        // Saved by 0.3.0: no target_temp on entries, no default_target_temp
        let old = r#"{
            "active_id": "10000000-0000-4000-8000-000000000000",
            "sets": [{
                "id": "10000000-0000-4000-8000-000000000000",
                "name": "Work week",
                "entries": [
                    {"id": "00000000-0000-4000-8000-000000000001", "name": "day",
                     "time_period": {"start": "06:00:00", "end": "22:00:00"}, "heating_state": "ON"},
                    {"id": "00000000-0000-4000-8000-000000000002", "name": "night",
                     "time_period": {"start": "22:00:00", "end": "06:00:00"}, "heating_state": "OFF"}
                ]
            }]
        }"#;
        fs::write(&sets_path, old).unwrap();

        let sets = load_sets(&sets_path).unwrap();
        assert_eq!(sets.default_target_temp, 20.0);
        assert_eq!(sets.active().entries[0].target_temp, Some(20.0));
        assert_eq!(sets.active().entries[1].target_temp, None);

        // A changed default is kept, and used for entries still missing a target
        let changed = old.replacen("{", "{\"default_target_temp\": 19.5,", 1);
        fs::write(&sets_path, changed).unwrap();
        let sets = load_sets(&sets_path).unwrap();
        assert_eq!(sets.default_target_temp, 19.5);
        assert_eq!(sets.active().entries[0].target_temp, Some(19.5));

        save_sets(&sets, &sets_path).unwrap();
        assert_eq!(load_sets(&sets_path).unwrap().default_target_temp, 19.5);
    }

    #[test]
    fn test_load_saves_what_it_normalised() {
        let dir = tempdir().unwrap();
        let sets_path = dir.path().join("schedule_sets.json");
        let schedule_path = dir.path().join("schedule.json");
        // Saved before targets existed, with two Off pieces that should merge
        let old = r#"{
            "active_id": "10000000-0000-4000-8000-000000000000",
            "sets": [{
                "id": "10000000-0000-4000-8000-000000000000",
                "name": "Work week",
                "entries": [
                    {"id": "00000000-0000-4000-8000-000000000001", "name": "day",
                     "time_period": {"start": "06:00:00", "end": "22:00:00"}, "heating_state": "ON"},
                    {"id": "00000000-0000-4000-8000-000000000002", "name": "late",
                     "time_period": {"start": "22:00:00", "end": "00:00:00"}, "heating_state": "OFF"},
                    {"id": "00000000-0000-4000-8000-000000000003", "name": "early",
                     "time_period": {"start": "00:00:00", "end": "06:00:00"}, "heating_state": "OFF"}
                ]
            }]
        }"#;
        fs::write(&sets_path, old).unwrap();

        let served = load_or_migrate(&sets_path, &schedule_path).unwrap();

        // The file now matches what is served
        let on_disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&sets_path).unwrap()).unwrap();
        assert_eq!(on_disk, serde_json::to_value(&served).unwrap());
        assert_eq!(served.active().entries.len(), 2);
        assert_eq!(on_disk["sets"][0]["entries"][0]["target_temp"], 20.0);

        // A file that needs no changes is left alone
        let before = fs::read_to_string(&sets_path).unwrap();
        fs::write(&sets_path, &before).unwrap();
        let modified = fs::metadata(&sets_path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        load_or_migrate(&sets_path, &schedule_path).unwrap();
        assert_eq!(
            fs::metadata(&sets_path).unwrap().modified().unwrap(),
            modified
        );
    }
}
