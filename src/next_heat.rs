//! Each zone's next scheduled heat, published to Home Assistant as `sensor.<zone>_next_heat` so a
//! dashboard card can show it (e.g. "19:00 → 22°"). The state is when the next On period starts;
//! the attributes say its target, its period and which schedule set it comes from.
//!
//! States posted through HA's REST states API aren't saved, so they vanish when Home Assistant
//! restarts; [`publish`] therefore runs every scheduler tick. It only reports: nothing here
//! touches how the heating is driven, and a failed post is logged and left for the next tick.

use crate::api_client::ApiClient;
use crate::schedule::HeatingState;
use crate::schedule::sets::ScheduleSets;
use crate::zones::{Zone, Zones};
use chrono::{DateTime, Datelike, Days, Local};
use serde_json::{Value, json};

/// How many days ahead to look for an On period, so a schedule set with no On period on some days
/// still finds one next week
const LOOK_AHEAD_DAYS: u64 = 7;

/// The sensor for one zone
#[derive(Debug, Clone, PartialEq)]
pub struct NextHeat {
    pub entity_id: String,
    /// The start of the next On period (RFC 3339), or "unknown" when there is none
    pub state: String,
    pub attributes: Value,
}

/// `sensor.<zone>_next_heat`, the zone's name as Home Assistant spells entity ids: lower case,
/// letters and digits with single underscores between words
pub fn entity_id(zone: &Zone) -> String {
    let mut slug = String::new();
    for c in zone.name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('_') && !slug.is_empty() {
            slug.push('_');
        }
    }
    let slug = slug.trim_end_matches('_');
    if slug.is_empty() {
        // A name with no letters or digits: the id is still unique
        format!("sensor.zone_{}_next_heat", zone.id.simple())
    } else {
        format!("sensor.{slug}_next_heat")
    }
}

/// A sensor for every zone, as of `now`
pub fn next_heats(zones: &Zones, sets: &ScheduleSets, now: DateTime<Local>) -> Vec<NextHeat> {
    zones
        .zones
        .iter()
        .map(|z| next_heat(z, sets, now))
        .collect()
}

/// The zone's next On period to start after `now`, from its schedule set (or, when it follows the
/// active set, the set that applies on each day, including days assigned their own set). Shadow
/// and smart zones use the same schedule, so they report the same way; the target is the one
/// scheduled, before any weather adjustment.
pub fn next_heat(zone: &Zone, sets: &ScheduleSets, now: DateTime<Local>) -> NextHeat {
    let mode = serde_json::to_value(zone.mode).unwrap_or(Value::Null);
    let mut attributes = json!({
        "friendly_name": format!("{} next heat", zone.name),
        "device_class": "timestamp",
        "zone": zone.name,
        "mode": mode,
    });
    let found = (0..=LOOK_AHEAD_DAYS).find_map(|offset| {
        let date = now.date_naive().checked_add_days(Days::new(offset))?;
        let set = zone
            .schedule_set_id
            .and_then(|id| sets.get(id))
            .unwrap_or_else(|| sets.for_day(date.weekday()));
        set.entries
            .iter()
            .filter(|e| e.heating_state == HeatingState::On)
            .filter(|e| offset > 0 || e.time_period.start > now.time())
            .filter_map(|e| {
                let at = date
                    .and_time(e.time_period.start)
                    .and_local_timezone(Local)
                    .earliest()?;
                Some((at, e, set))
            })
            .min_by_key(|(at, _, _)| *at)
    });
    let state = match found {
        Some((at, entry, set)) => {
            let fields = attributes.as_object_mut().expect("built as an object");
            fields.insert(
                "target_temperature".into(),
                json!(entry.target_temp.unwrap_or(sets.default_target_temp)),
            );
            fields.insert(
                "period".into(),
                json!(format!(
                    "{}-{}",
                    entry.time_period.start.format("%H:%M"),
                    entry.time_period.end.format("%H:%M")
                )),
            );
            fields.insert("schedule_set".into(), json!(set.name));
            fields.insert("schedule_set_id".into(), json!(set.id));
            at.to_rfc3339()
        }
        None => "unknown".to_string(),
    };
    NextHeat {
        entity_id: entity_id(zone),
        state,
        attributes,
    }
}

/// Post each sensor. A failed post is logged and skipped, to be retried next tick; this never
/// fails, so it can't hold up or change anything else. In a dry run the client records the posts
/// instead of sending them (they're not reads).
pub async fn publish(api_client: &ApiClient, sensors: &[NextHeat]) {
    for sensor in sensors {
        let body = json!({"state": sensor.state, "attributes": sensor.attributes});
        let result = api_client
            .post(&format!("/api/states/{}", sensor.entity_id), &body)
            .await
            .and_then(|response| Ok(response.error_for_status()?));
        if let Err(e) = result {
            eprintln!("Couldn't publish {}: {e}", sensor.entity_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
    use crate::zones::room::ZoneMode;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn schedule(name: &str, on: &[(u32, u32, f64)]) -> Schedule {
        let mut s = Schedule::new(name);
        for &(from, to, t) in on {
            s.add_entry(
                ScheduleEntry::new("On", TimePeriod::new(from, 0, to, 0), HeatingState::On)
                    .with_target(t),
            );
        }
        s
    }

    fn zone(name: &str) -> Zone {
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "name": name, "kind": "manual", "entity_ids": []
        }))
        .unwrap()
    }

    /// Wednesday 2026-11-11 at `hour`:00
    fn wed(hour: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 11, hour, 0, 0).unwrap()
    }

    #[test]
    fn test_entity_id() {
        assert_eq!(entity_id(&zone("Lounge")), "sensor.lounge_next_heat");
        assert_eq!(
            entity_id(&zone("  Master Bed-room! ")),
            "sensor.master_bed_room_next_heat"
        );
        assert!(entity_id(&zone("🔥")).starts_with("sensor.zone_"));
    }

    #[test]
    fn test_next_on_period_after_now() {
        let sets =
            ScheduleSets::from_schedule(schedule("Weekday", &[(6, 8, 20.0), (19, 22, 22.0)]));
        let sensor = next_heat(&zone("Lounge"), &sets, wed(12));
        assert_eq!(sensor.entity_id, "sensor.lounge_next_heat");
        assert_eq!(sensor.state, wed(19).to_rfc3339());
        assert_eq!(sensor.attributes["target_temperature"], 22.0);
        assert_eq!(sensor.attributes["period"], "19:00-22:00");
        assert_eq!(sensor.attributes["schedule_set"], "Weekday");
        assert_eq!(sensor.attributes["mode"], "schedule");
        assert_eq!(sensor.attributes["zone"], "Lounge");
        // Inside a period, the next one is the next to start, not the one under way
        let sensor = next_heat(&zone("Lounge"), &sets, wed(20));
        assert_eq!(
            sensor.state,
            Local
                .with_ymd_and_hms(2026, 11, 12, 6, 0, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert_eq!(sensor.attributes["period"], "06:00-08:00");
    }

    #[test]
    fn test_day_assignment_set_differs_from_active_set() {
        let active = schedule("Normal", &[(7, 9, 20.0)]);
        let weekend = schedule("Weekend", &[(9, 23, 21.5)]);
        let weekend_id = weekend.id;
        let mut sets = ScheduleSets::from_schedule(active);
        sets.sets.push(weekend);
        // Thursday has its own set; Wednesday (today) uses the active one
        sets.assign_days(weekend_id, &[chrono::Weekday::Thu])
            .unwrap();
        // After today's 07:00, the next heat is Thursday's, from the Weekend set, not the active set
        let sensor = next_heat(&zone("Lounge"), &sets, wed(12));
        assert_eq!(
            sensor.state,
            Local
                .with_ymd_and_hms(2026, 11, 12, 9, 0, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert_eq!(sensor.attributes["schedule_set"], "Weekend");
        assert_eq!(sensor.attributes["schedule_set_id"], json!(weekend_id));
        assert_eq!(sensor.attributes["target_temperature"], 21.5);
        assert_ne!(sets.active().name, "Weekend");
    }

    #[test]
    fn test_a_zones_own_set_wins_over_day_assignments() {
        let active = schedule("Normal", &[(7, 9, 20.0)]);
        let own = schedule("Study", &[(15, 16, 18.0)]);
        let own_id = own.id;
        let mut sets = ScheduleSets::from_schedule(active);
        sets.sets.push(own);
        let mut z = zone("Study");
        z.schedule_set_id = Some(own_id);
        let sensor = next_heat(&z, &sets, wed(12));
        assert_eq!(sensor.attributes["schedule_set"], "Study");
        assert_eq!(sensor.attributes["target_temperature"], 18.0);
    }

    #[test]
    fn test_shadow_and_smart_zones_report_too() {
        let sets = ScheduleSets::from_schedule(schedule("Normal", &[(19, 22, 22.0)]));
        for (mode, name) in [(ZoneMode::Shadow, "shadow"), (ZoneMode::Smart, "smart")] {
            let mut z = zone("Lounge");
            z.mode = mode;
            let sensor = next_heat(&z, &sets, wed(12));
            assert_eq!(sensor.state, wed(19).to_rfc3339());
            assert_eq!(sensor.attributes["mode"], name);
        }
    }

    #[test]
    fn test_no_upcoming_heat_is_unknown_without_a_target() {
        let sets = ScheduleSets::from_schedule(schedule("Never", &[]));
        let sensor = next_heat(&zone("Lounge"), &sets, wed(12));
        assert_eq!(sensor.entity_id, "sensor.lounge_next_heat");
        assert_eq!(sensor.state, "unknown");
        let fields = sensor.attributes.as_object().unwrap();
        assert!(!fields.contains_key("target_temperature"));
        assert!(!fields.contains_key("period"));
        assert!(!fields.contains_key("schedule_set"));
    }
}
