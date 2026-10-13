//! Shadow and smart zones: each tick, what the smart decision would do, kept for the API, logged
//! to decisions.jsonl when it changes, and how each comfort time turned out against the schedule.
//! Nothing here sends a command.

use crate::climate::ClimateEntity;
use crate::schedule::HeatingState;
use crate::schedule::sets::ScheduleSets;
use crate::scheduler::ZoneStatus;
use crate::smart::{
    Action, ComfortTime, Decision, Inputs, SMART_MAX_EARLY_MINUTES, comfort_time, decide,
};
use crate::weather::{Weather, WeatherStatus};
use crate::zones::Zone;
use chrono::{DateTime, Local, NaiveTime, TimeDelta, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

/// Entries kept in the log; older ones are dropped
pub const LOG_CAP: usize = 2000;
/// Entries the log may run over [`LOG_CAP`] before the file is rewritten without them
const LOG_SLACK: usize = 200;

/// A zone's smart decision and what the tick knows around it; None when its schedule has no On
/// period. (The scheduler asks only for shadow and smart zones; a preview asks for any.) The schedule's own early start, as `status` (the
/// zone's status this tick) has it, gives when the schedule starts heating.
#[allow(clippy::too_many_arguments)]
pub fn zone_tick(
    zone: &Zone,
    sets: &ScheduleSets,
    status: &ZoneStatus,
    weather: Option<&Weather>,
    weather_status: &WeatherStatus,
    room_c: Option<f64>,
    previous: Option<&Decision>,
    now: DateTime<Local>,
) -> Option<Tick> {
    let schedule = zone
        .schedule_set_id
        .and_then(|id| sets.get(id))
        .unwrap_or_else(|| sets.active());
    let (comfort, inside) = comfort_time(schedule, now.time(), sets.default_target_temp)?;
    let decision = decide(&Inputs {
        now,
        comfort,
        inside,
        room_c,
        weather,
        forecast: &weather_status.forecast,
        room: &zone.room,
        wind_exposure: zone.wind_exposure,
        sun_windows: &zone.sun_windows,
        daylight: weather_status.daylight,
        max_early_minutes: SMART_MAX_EARLY_MINUTES,
        previous,
    });
    let early = status
        .early_start
        .as_ref()
        .filter(|e| e.period_start == comfort.start);
    Some(Tick {
        decision: ZoneDecision {
            // Whole seconds, so the times recorded from it read plainly
            at: now.with_nanosecond(0).unwrap_or(now),
            room_c,
            schedule_start: early.map_or(comfort.start, |e| {
                comfort.start - TimeDelta::minutes(i64::from(e.minutes_early))
            }),
            decision,
        },
        inside,
        schedule_heating: status.state == HeatingState::On
            && status.warm_up.is_none()
            && (inside || early.is_some()),
    })
}

/// Whether any of the zone's entities has been read, so its room temperature is known (it isn't
/// on the first tick after a start). One that never reads doesn't hold the zone up.
pub fn zone_read<T: ClimateEntity>(zone: &Zone, entities: &[T]) -> bool {
    entities
        .iter()
        .filter(|e| zone.entity_ids.iter().any(|id| id == e.get_entity_id()))
        .any(|e| e.get_cached_state().is_some())
}

/// The room temperature: the zone's sensor reading when there is one, else the average of its
/// TRVs' readings, to 0.1 °C
pub fn room_temperature<T: ClimateEntity>(
    zone: &Zone,
    entities: &[T],
    sensor: Option<f64>,
) -> Option<f64> {
    let average = || {
        let readings: Vec<f64> = entities
            .iter()
            .filter(|e| zone.entity_ids.iter().any(|id| id == e.get_entity_id()))
            .filter_map(|e| e.get_cached_state().as_ref().map(|s| s.current_temperature))
            .collect();
        (!readings.is_empty()).then(|| readings.iter().sum::<f64>() / readings.len() as f64)
    };
    sensor.or_else(average).map(|c| (c * 10.0).round() / 10.0)
}

/// A zone's smart decision on the latest tick
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneDecision {
    pub at: DateTime<Local>,
    /// The room temperature it was made with
    pub room_c: Option<f64>,
    /// When the schedule starts heating for the same comfort time: its start, or earlier once an
    /// early start has begun
    pub schedule_start: NaiveTime,
    pub decision: Decision,
}

/// How a comfort time went: when Smart would have started heating against when the schedule did
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// When it was recorded: when the room reached the target, or the comfort time ended
    pub at: DateTime<Local>,
    pub comfort: ComfortTime,
    /// When Smart would have started heating; None when it found heating wasn't needed
    pub smart_start: Option<NaiveTime>,
    /// When the schedule started heating (its early start included); None if it didn't
    pub schedule_start: Option<NaiveTime>,
    /// The room when the schedule started heating
    pub room_c_at_heating_start: Option<f64>,
    /// The room at the comfort time's start
    pub room_c_at_start: Option<f64>,
    /// When the room reached the target; None if it didn't before the comfort time ended
    pub reached_at: Option<DateTime<Local>>,
}

/// A line of decisions.jsonl
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Logged {
    /// A zone's decision changed (its action, target, start or comfort time)
    Decision {
        zone_id: Uuid,
        decision: ZoneDecision,
    },
    /// A comfort time's outcome
    Outcome { zone_id: Uuid, outcome: Outcome },
}

impl Logged {
    pub fn zone_id(&self) -> Uuid {
        match self {
            Logged::Decision { zone_id, .. } | Logged::Outcome { zone_id, .. } => *zone_id,
        }
    }
    pub fn at(&self) -> DateTime<Local> {
        match self {
            Logged::Decision { decision, .. } => decision.at,
            Logged::Outcome { outcome, .. } => outcome.at,
        }
    }
}

/// A comfort time being watched for its outcome
#[derive(Debug, Clone)]
struct Tracking {
    comfort: ComfortTime,
    /// First seen during the comfort time (the service started then): there's no fair outcome
    unseen_start: bool,
    smart_start: Option<NaiveTime>,
    schedule_start: Option<(NaiveTime, Option<f64>)>,
    /// The room at the start, once it has started
    started: Option<Option<f64>>,
    /// The outcome is in the log
    recorded: bool,
}

impl Tracking {
    fn outcome(&self, at: DateTime<Local>, reached_at: Option<DateTime<Local>>) -> Outcome {
        Outcome {
            at,
            comfort: self.comfort,
            smart_start: self.smart_start,
            schedule_start: self.schedule_start.map(|(t, _)| t),
            room_c_at_heating_start: self.schedule_start.and_then(|(_, c)| c),
            room_c_at_start: self.started.flatten(),
            reached_at,
        }
    }
}

/// A zone's decision this tick, with what the tick knows about its comfort time
#[derive(Debug, Clone)]
pub struct Tick {
    pub decision: ZoneDecision,
    /// The comfort time is under way
    pub inside: bool,
    /// The schedule is heating the zone for its comfort time (an early start or the period itself)
    pub schedule_heating: bool,
}

/// The decisions of the latest tick and the log, shared by the scheduler and the server
#[derive(Debug, Default)]
pub struct Shadow {
    pub decisions: HashMap<Uuid, ZoneDecision>,
    tracking: HashMap<Uuid, Tracking>,
    log: VecDeque<Logged>,
    /// Where the log is kept; None keeps it in memory only
    path: Option<PathBuf>,
}

impl Shadow {
    /// The log in `path`, if there is one. Unreadable lines are skipped.
    pub fn load(path: impl Into<PathBuf>) -> Shadow {
        let path = path.into();
        let mut log: VecDeque<Logged> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        while log.len() > LOG_CAP {
            log.pop_front();
        }
        Shadow {
            log,
            path: Some(path),
            ..Default::default()
        }
    }

    /// Log entries, oldest first, for one zone or all, from `since` on
    pub fn log(&self, zone_id: Option<Uuid>, since: Option<DateTime<Local>>) -> Vec<Logged> {
        self.log
            .iter()
            .filter(|l| zone_id.is_none_or(|id| l.zone_id() == id))
            .filter(|l| since.is_none_or(|since| l.at() >= since))
            .cloned()
            .collect()
    }

    /// Keep this tick's decisions, logging the ones that changed and the outcomes of comfort
    /// times. Zones without a decision this tick are forgotten.
    pub fn record(&mut self, ticks: HashMap<Uuid, Tick>) {
        self.tracking.retain(|id, _| ticks.contains_key(id));
        let mut ticks: Vec<_> = ticks.into_iter().collect();
        ticks.sort_by_key(|(id, _)| *id);
        let mut kept = HashMap::new();
        for (zone_id, tick) in ticks {
            let changed = self.decisions.get(&zone_id).is_none_or(|last| {
                let (a, b) = (&last.decision, &tick.decision.decision);
                (a.action, a.target, a.start_at, a.comfort)
                    != (b.action, b.target, b.start_at, b.comfort)
            });
            if changed {
                self.append(Logged::Decision {
                    zone_id,
                    decision: tick.decision.clone(),
                });
            }
            if let Some(outcome) = self.track(zone_id, &tick) {
                self.append(Logged::Outcome { zone_id, outcome });
            }
            kept.insert(zone_id, tick.decision);
        }
        self.decisions = kept;
    }

    /// Follow a zone's comfort time: when Smart and the schedule start heating for it, and the room
    /// at its start. The outcome comes once the room reaches the target, or the comfort time ends
    /// (or changes) without that.
    fn track(&mut self, zone_id: Uuid, tick: &Tick) -> Option<Outcome> {
        let ZoneDecision {
            at,
            room_c,
            decision,
            ..
        } = &tick.decision;
        let comfort = decision.comfort;
        let mut ended = None;
        let over = self
            .tracking
            .get(&zone_id)
            .is_some_and(|t| t.comfort != comfort || (t.started.is_some() && !tick.inside));
        if over {
            let t = self.tracking.remove(&zone_id)?;
            if t.started.is_some() && !t.recorded && !t.unseen_start {
                ended = Some(t.outcome(*at, None));
            }
        }
        let t = self.tracking.entry(zone_id).or_insert(Tracking {
            comfort,
            unseen_start: tick.inside,
            smart_start: None,
            schedule_start: None,
            started: None,
            recorded: false,
        });
        if t.started.is_none() {
            if decision.action == Action::Heat
                && decision.target == Some(comfort.target)
                && t.smart_start.is_none()
            {
                t.smart_start = Some(decision.start_at);
            }
            if tick.schedule_heating && t.schedule_start.is_none() {
                t.schedule_start = Some((at.time(), *room_c));
            }
            if tick.inside {
                t.started = Some(*room_c);
            }
        }
        let reached = tick.inside && room_c.is_some_and(|c| c >= comfort.target);
        if reached && !t.recorded && !t.unseen_start && ended.is_none() {
            t.recorded = true;
            return Some(t.outcome(*at, Some(*at)));
        }
        ended
    }

    fn append(&mut self, entry: Logged) {
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&entry).unwrap_or_default();
            let written = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{line}"));
            if let Err(e) = written {
                eprintln!("Failed to log a decision: {}", e);
            }
        }
        self.log.push_back(entry);
        if self.log.len() > LOG_CAP + LOG_SLACK {
            self.log.drain(..self.log.len() - LOG_CAP);
            self.rewrite();
        }
    }

    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let lines: String = self
            .log
            .iter()
            .filter_map(|l| serde_json::to_string(l).ok())
            .map(|l| l + "\n")
            .collect();
        let tmp = path.with_extension("jsonl.tmp");
        if let Err(e) = std::fs::write(&tmp, lines).and_then(|()| std::fs::rename(&tmp, path)) {
            eprintln!("Failed to trim the decisions log: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::climate::MockClimate;
    use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
    use crate::weather::early::EarlyStart;
    use crate::zones::room::ZoneMode;
    use crate::zones::{ZoneKind, Zones};
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 1, 15, h, m, 0).unwrap()
    }
    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }
    const EVENING: ComfortTime = ComfortTime {
        start: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
        end: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
        target: 20.0,
    };

    /// A tick for the evening comfort time at `now`
    fn tick(now: DateTime<Local>, action: Action, room_c: f64, schedule_heating: bool) -> Tick {
        let inside = now.time() >= EVENING.start && now.time() < EVENING.end;
        Tick {
            decision: ZoneDecision {
                at: now,
                room_c: Some(room_c),
                schedule_start: EVENING.start,
                decision: Decision {
                    action,
                    target: (action == Action::Heat).then_some(20.0),
                    start_at: if inside { EVENING.start } else { time(16, 10) },
                    comfort: EVENING,
                    reasons: Vec::new(),
                },
            },
            inside,
            schedule_heating,
        }
    }

    fn record(shadow: &mut Shadow, zone: Uuid, tick: Tick) {
        shadow.record(HashMap::from([(zone, tick)]));
    }

    fn outcomes(shadow: &Shadow) -> Vec<Outcome> {
        shadow
            .log(None, None)
            .into_iter()
            .filter_map(|l| match l {
                Logged::Outcome { outcome, .. } => Some(outcome),
                Logged::Decision { .. } => None,
            })
            .collect()
    }

    #[test]
    fn test_outcome_compares_smart_with_the_schedule_once_the_room_is_warm() {
        let mut shadow = Shadow::default();
        let zone = Uuid::new_v4();
        record(&mut shadow, zone, tick(at(16, 0), Action::Off, 16.0, false));
        record(
            &mut shadow,
            zone,
            tick(at(16, 10), Action::Heat, 16.0, false),
        );
        record(
            &mut shadow,
            zone,
            tick(at(16, 45), Action::Heat, 17.5, true),
        );
        record(&mut shadow, zone, tick(at(17, 0), Action::Heat, 18.5, true));
        assert!(outcomes(&shadow).is_empty());
        record(
            &mut shadow,
            zone,
            tick(at(17, 25), Action::Heat, 20.0, true),
        );
        record(
            &mut shadow,
            zone,
            tick(at(17, 40), Action::NotNeeded, 20.2, true),
        );

        assert_eq!(
            outcomes(&shadow),
            vec![Outcome {
                at: at(17, 25),
                comfort: EVENING,
                smart_start: Some(time(16, 10)),
                schedule_start: Some(time(16, 45)),
                room_c_at_heating_start: Some(17.5),
                room_c_at_start: Some(18.5),
                reached_at: Some(at(17, 25)),
            }]
        );
        // Each change of decision is logged, not each tick
        assert_eq!(shadow.log(Some(zone), None).len(), 4 + 1);
        assert_eq!(shadow.log(Some(zone), Some(at(17, 1))).len(), 2);
        assert!(shadow.log(Some(Uuid::new_v4()), None).is_empty());
    }

    #[test]
    fn test_outcome_of_a_comfort_time_that_ends_cold() {
        let mut shadow = Shadow::default();
        let zone = Uuid::new_v4();
        record(
            &mut shadow,
            zone,
            tick(at(16, 55), Action::Off, 19.0, false),
        );
        record(
            &mut shadow,
            zone,
            tick(at(17, 0), Action::NotNeeded, 19.0, true),
        );
        record(
            &mut shadow,
            zone,
            tick(at(21, 59), Action::Heat, 19.5, true),
        );
        assert!(outcomes(&shadow).is_empty());
        // 22:00: the next comfort time is tomorrow's
        record(&mut shadow, zone, tick(at(22, 0), Action::Off, 19.5, false));
        let [outcome] = outcomes(&shadow).try_into().unwrap();
        assert_eq!(
            (
                outcome.smart_start,
                outcome.schedule_start,
                outcome.reached_at
            ),
            (None, Some(time(17, 0)), None)
        );
    }

    #[test]
    fn test_no_outcome_for_a_comfort_time_first_seen_under_way() {
        let mut shadow = Shadow::default();
        let zone = Uuid::new_v4();
        record(&mut shadow, zone, tick(at(18, 0), Action::Heat, 19.0, true));
        record(
            &mut shadow,
            zone,
            tick(at(18, 30), Action::Heat, 20.0, true),
        );
        record(&mut shadow, zone, tick(at(22, 0), Action::Off, 20.0, false));
        assert!(outcomes(&shadow).is_empty());
    }

    #[test]
    fn test_a_zone_left_out_of_a_tick_is_forgotten() {
        let mut shadow = Shadow::default();
        let zone = Uuid::new_v4();
        record(&mut shadow, zone, tick(at(16, 0), Action::Off, 19.0, false));
        shadow.record(HashMap::new());
        assert!(shadow.decisions.is_empty());
        assert!(shadow.tracking.is_empty());
    }

    #[test]
    fn test_the_log_is_kept_in_a_file_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let zone = Uuid::new_v4();
        let mut shadow = Shadow::load(&path);
        for i in 0..LOG_CAP + LOG_SLACK + 1 {
            let action = if i % 2 == 0 {
                Action::Off
            } else {
                Action::Heat
            };
            record(&mut shadow, zone, tick(at(16, 0), action, 19.0, false));
        }
        assert_eq!(shadow.log(None, None).len(), LOG_CAP);
        let loaded = Shadow::load(&path);
        assert_eq!(loaded.log(None, None), shadow.log(None, None));

        // A line it can't read is skipped
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "not json").unwrap();
        assert_eq!(Shadow::load(&path).log(None, None).len(), LOG_CAP);
    }

    fn shadow_zone(mode: ZoneMode) -> (Zone, ScheduleSets) {
        let mut schedule = Schedule::new("Evenings");
        schedule.add_entry(
            ScheduleEntry::new("Evening", TimePeriod::new(17, 0, 22, 0), HeatingState::On)
                .with_target(20.0),
        );
        let sets = ScheduleSets::from_schedule(schedule);
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.a".to_string(), "climate.b".to_string()]);
        let mut zone = zones.zones[0].clone();
        assert_eq!(zone.kind, ZoneKind::WholeHouse);
        zone.mode = mode;
        (zone, sets)
    }

    fn off() -> ZoneStatus {
        crate::scheduler::zone_status(
            None,
            &ScheduleSets::from_schedule(Schedule::new("Off")),
            None,
            None,
            None,
            &at(12, 0),
        )
    }

    #[test]
    fn test_zone_tick() {
        let weather = WeatherStatus::default();
        let (zone, sets) = shadow_zone(ZoneMode::Shadow);
        let tick = |zone: &Zone, status: &ZoneStatus| {
            zone_tick(
                zone,
                &sets,
                status,
                None,
                &weather,
                Some(18.0),
                None,
                at(16, 30),
            )
        };
        let t = tick(&zone, &off()).unwrap();
        assert_eq!(t.decision.decision.comfort, EVENING);
        let late = zone_tick(
            &zone,
            &sets,
            &off(),
            None,
            &weather,
            None,
            None,
            at(16, 30) + TimeDelta::milliseconds(250),
        );
        assert_eq!(late.unwrap().decision.at, at(16, 30), "whole seconds");
        assert_eq!(t.decision.schedule_start, time(17, 0));
        assert!(!t.inside && !t.schedule_heating);

        // The schedule's early start has begun: it started heating 30 minutes early
        let mut early = off();
        early.state = HeatingState::On;
        early.early_start = Some(EarlyStart {
            period_start: time(17, 0),
            lead_minutes: 30,
            minutes_early: 30,
            causes: Vec::new(),
        });
        let t = tick(&zone, &early).unwrap();
        assert_eq!(t.decision.schedule_start, time(16, 30));
        assert!(t.schedule_heating);
    }

    #[test]
    fn test_room_temperature_from_the_sensor_or_the_trvs() {
        let (zone, _) = shadow_zone(ZoneMode::Shadow);
        let trv = |id: &str, c: f64| {
            let mut trv = MockClimate::new(id.to_string(), HeatingState::Off);
            trv.info.as_mut().unwrap().current_temperature = c;
            trv
        };
        let trvs = [
            trv("climate.a", 18.0),
            trv("climate.b", 19.25),
            trv("climate.other", 30.0),
        ];
        assert_eq!(room_temperature(&zone, &trvs, None), Some(18.6));
        assert_eq!(room_temperature(&zone, &trvs, Some(17.04)), Some(17.0));
        assert_eq!(room_temperature(&zone, &trvs[2..], None), None);
        assert!(zone_read(&zone, &trvs));
        let mut unread = trv("climate.a", 18.0);
        unread.info = None;
        assert!(!zone_read(&zone, std::slice::from_ref(&unread)));
        // One radiator that never reads doesn't hold the zone up
        assert!(zone_read(&zone, &[unread, trv("climate.b", 19.0)]));
    }
}
