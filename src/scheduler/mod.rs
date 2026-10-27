use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateEntity};
use crate::dry_run::CALL_CONTEXT;
use crate::schedule::HeatingState;
use crate::schedule::TARGET_TEMP_RANGE;
use crate::schedule::sets::ScheduleSets;
use crate::shadow::{room_temperature, zone_tick};
use crate::weather::Begun;
use crate::weather::adjust::{Reason, adjust_target};
use crate::weather::early::{EarlyStart, continue_early_start, early_start};
use crate::weather::late::{LateFinish, continue_late_finish, late_finish};
use crate::weather::warmup::{WarmUp, continue_warm_up, warm_up};
use crate::weather::{
    FORECAST_EVERY, Held, KEEP_READING_FOR, Weather, WeatherSource, WeatherStatus,
};
use crate::zones::{Zone, Zones};
use crate::{ScheduleState, ShadowState, WeatherState, ZonesState};
use chrono::Local;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

/// The last target sent to each entity, so an unchanged one isn't sent every tick. Shared with
/// the API, which forgets an entity's entry when it's added or removed, so a re-added entity
/// gets its target sent again.
pub type LastSentTargets = Arc<std::sync::Mutex<HashMap<String, Option<f64>>>>;

/// What the scheduler keeps about each entity between ticks, besides the last target sent
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EntityMemory {
    /// When its target was last put back after being changed on the TRV
    pub put_back_at: Option<chrono::DateTime<Local>>,
    /// It's being driven harder because its room is well below the target (see [`drive`])
    pub driving: bool,
    /// The `max_temp` it's being driven to, while driving
    pub drive_setpoint: Option<f64>,
    /// The room reading the last tick used to decide that (see [`drive_reading`])
    pub room_c: Option<f64>,
    /// Its state was read on its last tick, so its reading is current
    pub read: bool,
}

/// Each entity's [`EntityMemory`], by entity id. Shared with the API, which shows the zones being
/// driven harder and forgets an entity's entry when it's added or removed.
pub type EntityMemories = Arc<std::sync::Mutex<HashMap<String, EntityMemory>>>;

pub struct SchedulerState<T: ClimateEntity + Clone> {
    pub api_client: ApiClient,
    pub schedule: ScheduleState,
    pub zones: ZonesState,
    pub weather: WeatherState,
    pub weather_source: Arc<dyn WeatherSource>,
    pub climate_entities: Arc<RwLock<Vec<T>>>,
    pub last_sent_targets: LastSentTargets,
    pub memories: EntityMemories,
    pub shadow: ShadowState,
}

/// Represents an action to be taken on a climate entity
#[derive(Debug, Clone, PartialEq)]
pub enum HeatingAction {
    TurnOn,
    TurnOff,
    NoChange,
}

/// Calculate what heating action should be taken based on the schedule
#[must_use]
pub fn calculate_heating_action_for_schedule(
    current_state: &HeatingState,
    desired_state: &HeatingState,
) -> HeatingAction {
    match (current_state, desired_state) {
        // If current state matches desired, no change needed
        (HeatingState::On, HeatingState::On) | (HeatingState::Off, HeatingState::Off) => {
            HeatingAction::NoChange
        }
        // If states differ, change to desired state
        (HeatingState::Off, HeatingState::On) => HeatingAction::TurnOn,
        (HeatingState::On, HeatingState::Off) => HeatingAction::TurnOff,
    }
}

// Return a tuple containing the desired heating state and a boolean indicating if the state should be updated
pub fn calculate_desired_heating_state_for_boost(
    boost_info: &Option<BoostInfo>,
) -> (HeatingState, bool) {
    boost_state_at(boost_info, Local::now())
}

fn boost_state_at(
    boost_info: &Option<BoostInfo>,
    now: chrono::DateTime<Local>,
) -> (HeatingState, bool) {
    if let Some(boosted) = boost_info {
        if boosted.is_active(now) {
            return (HeatingState::On, false);
        }
        (HeatingState::Off, true)
    } else {
        (HeatingState::Off, false)
    }
}

pub fn final_desired_heating_state(
    scheduled_heating_state: &HeatingState,
    boosted_heating_state: &HeatingState,
) -> HeatingState {
    match (scheduled_heating_state, boosted_heating_state) {
        // If current state matches desired, no change needed
        (HeatingState::On, HeatingState::On) => HeatingState::On,
        (HeatingState::Off, HeatingState::Off) => HeatingState::Off,

        // If states differ, change to desired state
        (HeatingState::Off, HeatingState::On) => HeatingState::On,
        (HeatingState::On, HeatingState::Off) => HeatingState::On,
    }
}

/// Apply heating action to a climate entity
async fn apply_heating_action(
    entity: &impl ClimateEntity,
    action: HeatingAction,
    api_client: &ApiClient,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        HeatingAction::TurnOn => {
            entity.turn_on(api_client).await?;
        }
        HeatingAction::TurnOff => {
            entity.turn_off(api_client).await?;
        }
        HeatingAction::NoChange => {
            println!("  ✓ No change needed: {}", entity.get_entity_id());
        }
    }
    Ok(())
}

/// The heating state and target temperature the active schedule asks for right now
#[derive(Debug, Clone, PartialEq)]
pub struct Scheduled {
    pub state: HeatingState,
    pub target_temp: Option<f64>,
    /// The zone's room reading, to drive its TRVs harder while the room is well below the target
    /// (see [`drive_reading`] and [`drive`]); None without one
    pub room_c: Option<f64>,
}

/// What a zone is doing right now and why
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneStatus {
    pub state: HeatingState,
    /// The active entry's target from the zone's schedule set (On only)
    pub scheduled_target: Option<f64>,
    /// The target after weather adjustment (equal to `scheduled_target` without it)
    pub target: Option<f64>,
    pub reasons: Vec<Reason>,
    /// The scheduled target plus the reasons, before clamping, rounding and holding
    /// (when the weather was applied)
    pub raw_target: Option<f64>,
    /// The target was kept at its previous value because the weather moved it only slightly
    pub held: bool,
    /// Set while an upcoming On period is being started early; `state` and the targets are
    /// then that period's
    pub early_start: Option<EarlyStart>,
    /// Set while an On period that has ended is kept going; `state` and the targets are then
    /// that period's
    pub late_finish: Option<LateFinish>,
    /// Set during a cold-day warm-up in an Off gap; `state` is then On at the default target
    pub warm_up: Option<WarmUp>,
    /// The room reading the scheduler's last tick used to decide whether to drive its TRVs
    /// harder: its room sensor, else its TRVs' average plus its TRV offset (see [`drive_reading`])
    pub room_c: Option<f64>,
    /// Its TRVs are being sent their `max_temp` rather than `target`, because the room is well
    /// below it (see [`drive`]); from the scheduler's last tick
    pub driving: bool,
    /// The setpoint sent to its TRVs while driving (the highest, should their `max_temp`s differ)
    pub drive_setpoint: Option<f64>,
}

/// How far the unrounded target must move from a held target before the target changes:
/// 0.25 °C past the rounding boundary, which is itself 0.25 °C away
const HOLD_BAND: f64 = 0.5;

/// A zone's state and target at `time`: from its schedule set (or the active set when it follows
/// the active set, its set no longer exists, or there is no zone), adjusted for the weather when
/// the zone has weather adjust on and is scheduled On. Off is never adjusted.
///
/// With weather adjust on, a zone that is Off is turned On early for its next On period when the
/// weather calls for it (see [`early_start`]), at that period's (adjusted) target.
///
/// `held` is the zone's previous adjusted target. While the scheduled target is the same and the
/// unrounded target stays within [`HOLD_BAND`] of it, the held target is kept, so an outside
/// temperature wobbling around a rounding boundary doesn't re-command TRVs.
pub fn zone_status(
    zone: Option<&Zone>,
    sets: &ScheduleSets,
    weather: Option<&Weather>,
    held: Option<Held>,
    begun: Option<&Begun>,
    time: &chrono::DateTime<Local>,
) -> ZoneStatus {
    let begun_early = begun.and_then(|b| b.early_start.as_ref());
    let schedule = zone
        .and_then(|z| z.schedule_set_id)
        .and_then(|id| sets.get(id))
        .unwrap_or_else(|| sets.active());
    let mut entry = schedule.get_active_entry(time);

    // Start the next On period early when it's cold or windy (weather adjust zones only)
    let early_start = match (zone, weather) {
        (Some(zone), Some(weather)) if zone.weather_adjust => early_start(
            schedule,
            time.time(),
            weather,
            zone.wind_exposure,
            zone.max_early_start_minutes,
        ),
        _ => None,
    };
    // An early start that has begun keeps going until its period starts, even if the weather
    // warms meanwhile, so the zone isn't turned Off and On again
    let early_start = early_start.or_else(|| {
        zone.filter(|z| z.weather_adjust && z.max_early_start_minutes > 0)?;
        let begun = begun_early?;
        continue_early_start(schedule, time.time(), begun).map(|next| (next, begun.clone()))
    });
    let early_start = early_start.map(|(next, mut early)| {
        // Keep when heating began: a fresh check each tick would count down to the period
        if let Some(begun) = begun_early.filter(|b| b.period_start == early.period_start) {
            early.minutes_early = begun.minutes_early;
        }
        entry = Some(next);
        early
    });

    // Keep an On period going past its end when it's cold or windy (weather adjust zones only),
    // for the minutes decided at the end. Starting the next period early takes over if they meet.
    let late = match zone {
        Some(zone)
            if zone.weather_adjust && zone.max_late_finish_minutes > 0 && early_start.is_none() =>
        {
            let begun_late = begun.and_then(|b| b.late_finish.as_ref());
            begun_late
                .and_then(|b| {
                    continue_late_finish(schedule, time.time(), b).map(|e| (e, b.clone()))
                })
                .or_else(|| {
                    late_finish(
                        schedule,
                        time.time(),
                        weather?,
                        zone.wind_exposure,
                        zone.max_late_finish_minutes,
                    )
                })
        }
        _ => None,
    };
    let late_finish = late.map(|(ended, late)| {
        entry = Some(ended);
        late
    });

    // A short warm-up partway through a long Off gap on a very cold day, for zones that opt in.
    // The slots keep clear of the zone's early starts and late finishes.
    let warm_up = match zone {
        Some(zone)
            if zone.cold_warmups.enabled && early_start.is_none() && late_finish.is_none() =>
        {
            let (before_on, after_on) = if zone.weather_adjust {
                (zone.max_early_start_minutes, zone.max_late_finish_minutes)
            } else {
                (0, 0)
            };
            begun
                .and_then(|b| b.warm_up.as_ref())
                .and_then(|b| continue_warm_up(schedule, time.time(), b))
                .or_else(|| {
                    warm_up(
                        schedule,
                        time.time(),
                        weather,
                        &zone.cold_warmups,
                        before_on,
                        after_on,
                    )
                })
        }
        _ => None,
    };

    let mut state = entry
        .map(|e| e.heating_state.clone())
        .unwrap_or(HeatingState::Off);
    let mut scheduled_target = entry.and_then(|e| e.target_temp);
    // During a warm-up: On at the default target, not adjusted for the weather
    if warm_up.is_some() {
        state = HeatingState::On;
        scheduled_target = Some(sets.default_target_temp);
    }

    let adjusted = match (zone, weather, &state, scheduled_target) {
        (Some(zone), Some(weather), HeatingState::On, Some(target))
            if zone.weather_adjust && warm_up.is_none() =>
        {
            Some(adjust_target(
                target,
                weather,
                &zone.sun_windows,
                zone.wind_exposure,
                time.time(),
            ))
        }
        _ => None,
    };
    let hold = match (&adjusted, held) {
        (Some(a), Some(h)) => {
            let raw = a
                .raw
                .clamp(*TARGET_TEMP_RANGE.start(), *TARGET_TEMP_RANGE.end());
            scheduled_target == Some(h.scheduled)
                && a.target != h.target
                && (raw - h.target).abs() < HOLD_BAND
        }
        _ => false,
    };
    ZoneStatus {
        state,
        scheduled_target,
        target: match (&adjusted, held) {
            (Some(_), Some(h)) if hold => Some(h.target),
            (Some(a), _) => Some(a.target),
            (None, _) => scheduled_target,
        },
        raw_target: adjusted.as_ref().map(|a| a.raw),
        reasons: adjusted.map(|a| a.reasons).unwrap_or_default(),
        held: hold,
        early_start,
        late_finish,
        warm_up,
        room_c: None,
        driving: false,
        drive_setpoint: None,
    }
}

/// Every zone's status, keeping adjusted targets in `held` and early starts in progress in
/// `early_starts` between ticks
pub fn zone_statuses(
    zones: &Zones,
    sets: &ScheduleSets,
    weather: Option<&Weather>,
    held: &mut HashMap<Uuid, Held>,
    begun: &mut HashMap<Uuid, Begun>,
    time: &chrono::DateTime<Local>,
) -> HashMap<Uuid, ZoneStatus> {
    let statuses: HashMap<Uuid, ZoneStatus> = zones
        .zones
        .iter()
        .map(|z| {
            let status = zone_status(
                Some(z),
                sets,
                weather,
                held.get(&z.id).copied(),
                begun.get(&z.id),
                time,
            );
            (z.id, status)
        })
        .collect();
    held.clear();
    begun.clear();
    for (id, status) in &statuses {
        if let (Some(_), Some(scheduled), Some(target)) =
            (status.raw_target, status.scheduled_target, status.target)
        {
            held.insert(*id, Held { scheduled, target });
        }
        if status.early_start.is_some() || status.late_finish.is_some() || status.warm_up.is_some()
        {
            begun.insert(
                *id,
                Begun {
                    early_start: status.early_start.clone(),
                    late_finish: status.late_finish.clone(),
                    warm_up: status.warm_up.clone(),
                },
            );
        }
    }
    statuses
}

/// Target to set while the heating is On: the scheduled On entry's target, or the default when
/// nothing On is scheduled (e.g. a boost during an Off period). None while Off.
pub fn desired_target_temp(
    final_state: &HeatingState,
    scheduled: &Scheduled,
    default_target_temp: f64,
) -> Option<f64> {
    match (final_state, &scheduled.state) {
        (HeatingState::Off, _) => None,
        (HeatingState::On, HeatingState::On) => {
            Some(scheduled.target_temp.unwrap_or(default_target_temp))
        }
        (HeatingState::On, HeatingState::Off) => Some(default_target_temp),
    }
}

/// Below this room temperature, an entity that would otherwise be Off is heated
pub const FROST_BELOW_C: f64 = 7.0;
/// The target it's heated to, and kept heating to, until the room reaches it
pub const FROST_TARGET_C: f64 = 10.0;

/// Whether an entity that would be Off is held On by the frost floor: the room has dropped below
/// [`FROST_BELOW_C`], or the floor turned it on (it's heating to [`FROST_TARGET_C`]) and the room
/// hasn't reached that yet
pub fn frost_floor(
    desired: &HeatingState,
    room_c: f64,
    heating: &HeatingState,
    last_sent: Option<f64>,
) -> bool {
    let warming = *heating == HeatingState::On && last_sent == Some(FROST_TARGET_C);
    *desired == HeatingState::Off
        && (room_c < FROST_BELOW_C || (warming && room_c < FROST_TARGET_C))
}

/// Fetch one entity's state and bring it in line with the schedule and any boost.
/// Turning On sets the mode, then the target temperature.
///
/// `last_sent` is the target last sent to this entity successfully. The target is sent when it
/// differs from that, or right after turning On. A failed send leaves `last_sent` unchanged, so
/// it is retried on the next tick.
///
/// While On, a target changed on the TRV itself (a preset, its buttons) is put back when it's
/// more than [`PUT_BACK_BEYOND_C`] from the one sent, at most every [`PUT_BACK_EVERY`]
/// (`memory.put_back_at`), so a TRV that ignores it isn't commanded every tick. A TRV that only
/// rounds the target stays within the tolerance and isn't re-commanded.
///
/// While the room is well below the target, the entity is driven harder: sent its `max_temp`
/// (see [`drive`]; `memory.driving`), so its valve opens fully. One with no `max_temp` known
/// isn't. Put-back then compares against `max_temp`, as it's what was sent.
///
/// While boosted, the entity is sent its `max_temp` whatever the schedule, so its valve opens
/// fully for the whole boost. It isn't driven meanwhile (a boost already sends `max_temp`), so
/// driving starts afresh when the boost ends. One with no `max_temp` known gets the usual target.
pub async fn apply_schedule_to_entity<T: ClimateEntity>(
    entity: &mut T,
    scheduled: &Scheduled,
    default_target_temp: f64,
    api_client: &ApiClient,
    last_sent: &mut Option<f64>,
    memory: &mut EntityMemory,
    still_managed: &(dyn Fn() -> bool + Sync),
) {
    let now = Local::now();
    memory.read = false;
    if let Err(e) = entity.fetch_and_update_state(api_client).await {
        eprintln!(
            "[{}] Error fetching state for {}: {}",
            now.format("%Y-%m-%d %H:%M:%S"),
            entity.get_entity_id(),
            e
        );
        return;
    }
    memory.read = true;
    // Removed while its state was read: it's no longer controlled, so send it nothing
    if !still_managed() {
        return;
    }
    let (boosted_state, should_update) =
        calculate_desired_heating_state_for_boost(entity.get_boosted_status());
    if should_update {
        entity.set_boost(None);
    }
    let mut final_desired_state = final_desired_heating_state(&scheduled.state, &boosted_state);

    let cached = entity.get_cached_state().clone().unwrap();
    let boost_max = cached
        .max_temp
        .filter(|_| boosted_state == HeatingState::On);
    let heating_state = cached.state;

    let frost = frost_floor(
        &final_desired_state,
        cached.current_temperature,
        &heating_state,
        *last_sent,
    );
    if frost {
        println!(
            "  Frost floor: {} °C in the room, heating to {FROST_TARGET_C} °C",
            cached.current_temperature
        );
        final_desired_state = HeatingState::On;
    }

    let action = calculate_heating_action_for_schedule(&heating_state, &final_desired_state);

    println!("[{}] Action: {:?}", now.format("%Y-%m-%d %H:%M:%S"), action);

    // Only apply changes when action is needed
    if action != HeatingAction::NoChange {
        println!(
            "  Schedule change: {:?} → {:?}",
            heating_state, final_desired_state
        );

        if let Err(e) = apply_heating_action(entity, action.clone(), api_client).await {
            eprintln!("  ✗ Error applying action: {}", e);
            return;
        }
    }

    let target = if frost {
        Some(FROST_TARGET_C)
    } else {
        desired_target_temp(&final_desired_state, scheduled, default_target_temp)
    };
    // Never for the frost floor, which heats to exactly its target, nor while boosted at max
    let driving = match (target, cached.max_temp) {
        (Some(target), Some(_)) if !frost && boost_max.is_none() => {
            drive(target, scheduled.room_c, memory.driving)
        }
        _ => false,
    };
    memory.room_c = scheduled.room_c;
    memory.drive_setpoint = cached.max_temp.filter(|_| driving);
    let started_driving = driving && !memory.driving;
    if memory.driving && !driving {
        println!(
            "  {}: room {:?} °C, no longer driving harder; back to the target",
            entity.get_entity_id(),
            scheduled.room_c
        );
    }
    memory.driving = driving;
    let Some(target) = target else {
        *last_sent = None;
        return;
    };
    let room_target = target;
    // Keep within what the entity accepts
    let target = target
        .max(cached.min_temp.unwrap_or(f64::MIN))
        .min(cached.max_temp.unwrap_or(f64::MAX));
    let target = match (boost_max, cached.max_temp) {
        (Some(max_temp), _) => max_temp,
        (None, Some(max_temp)) if driving => max_temp,
        _ => target,
    };
    if boost_max.is_some() && *last_sent != Some(target) {
        println!("  {}: boosted, at {target} °C", entity.get_entity_id());
    }
    if started_driving {
        println!(
            "  {}: room {:?} °C is well below {room_target} °C, driving harder at {target} °C",
            entity.get_entity_id(),
            scheduled.room_c
        );
    }
    if !still_managed() {
        return;
    }
    let changed_on_trv = action == HeatingAction::NoChange
        && *last_sent == Some(target)
        && cached
            .target_temp
            .is_some_and(|t| (t - target).abs() > PUT_BACK_BEYOND_C);
    let put_back = changed_on_trv
        && memory
            .put_back_at
            .is_none_or(|at| now - at >= PUT_BACK_EVERY);
    if put_back {
        println!(
            "  {} reports {:?} °C, putting back {target} °C",
            entity.get_entity_id(),
            cached.target_temp
        );
        memory.put_back_at = Some(now);
    }
    if action == HeatingAction::TurnOn || *last_sent != Some(target) || put_back {
        match entity.set_temperature(api_client, target).await {
            Ok(()) => *last_sent = Some(target),
            Err(e) => eprintln!("  ✗ Error setting temperature: {}", e),
        }
    }
}

/// How far a TRV's reported target may be from the one sent before it's put back: past a
/// 0.5 °C step's rounding
pub const PUT_BACK_BEYOND_C: f64 = 0.5;
/// The least time between putting back a TRV's target
pub const PUT_BACK_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

/// A TRV throttles its valve as its own sensor, on the radiator, nears the target, so a cold room
/// heats slowly. While the room reads this far or more below the target, its TRVs are driven
/// harder: sent their `max_temp`, so their valves open fully.
pub const DRIVE_BELOW_C: f64 = 1.5;
/// Driving stops once the room is within this of the target (or above it), so it doesn't
/// overshoot. It starts again only at [`DRIVE_BELOW_C`] below, so TRVs aren't re-commanded
/// every tick.
pub const DRIVE_UNTIL_WITHIN_C: f64 = 0.5;

/// Whether to drive an entity harder towards `target`, given the room sensor's reading and
/// whether it's being driven already. Never without a reading.
pub fn drive(target: f64, room_c: Option<f64>, driving: bool) -> bool {
    room_c.is_some_and(|room_c| {
        let below = target - room_c;
        if driving {
            below > DRIVE_UNTIL_WITHIN_C
        } else {
            below >= DRIVE_BELOW_C
        }
    })
}

/// The room reading to drive a zone's TRVs by: its room sensor's, else its TRVs' average plus the
/// zone's TRV offset (their sensors sit by the radiator), to 0.1 °C. None without either.
pub fn drive_reading<T: ClimateEntity + Clone>(
    zone: &Zone,
    entities: &[T],
    sensor: Option<f64>,
    memories: &HashMap<String, EntityMemory>,
) -> Option<f64> {
    // A zone with a sensor is read only from it, as its offset is for its TRVs
    if zone.room.sensor_entity_id.is_some() {
        return sensor;
    }
    room_temperature(zone, &read_entities(entities, memories), None)
        .map(|c| ((c + zone.room.trv_offset_c) * 10.0).round() / 10.0)
}

/// The room reading Smart and Shadow decide a zone by: [`room_temperature`] from its room sensor,
/// else from only its TRVs that were read on their last tick. None without either.
pub fn smart_reading<T: ClimateEntity + Clone>(
    zone: &Zone,
    entities: &[T],
    sensor: Option<f64>,
    memories: &HashMap<String, EntityMemory>,
) -> Option<f64> {
    room_temperature(zone, &read_entities(entities, memories), sensor)
}

/// The entities read on their last tick: one that can't be read keeps its last reading
fn read_entities<T: ClimateEntity + Clone>(
    entities: &[T],
    memories: &HashMap<String, EntityMemory>,
) -> Vec<T> {
    entities
        .iter()
        .filter(|e| memories.get(e.get_entity_id()).is_some_and(|m| m.read))
        .cloned()
        .collect()
}

/// The room reading a zone's entities were last driven by (or not), whether any of them is being
/// driven harder, and the highest `max_temp` they're driven to
pub fn zone_drive(
    zone: &Zone,
    memories: &HashMap<String, EntityMemory>,
) -> (Option<f64>, bool, Option<f64>) {
    let room_c = zone
        .entity_ids
        .iter()
        .find_map(|id| memories.get(id)?.room_c);
    let driven: Vec<&EntityMemory> = zone
        .entity_ids
        .iter()
        .filter_map(|id| memories.get(id).filter(|m| m.driving))
        .collect();
    let setpoint = driven
        .iter()
        .filter_map(|m| m.drive_setpoint)
        .reduce(f64::max);
    (room_c, !driven.is_empty(), setpoint)
}

/// Read the weather and record the reading (or the error) in `status`
/// If the read fails, the last good reading is kept for up to [`KEEP_READING_FOR`], so a brief
/// Home Assistant hiccup doesn't change any targets; after that, targets aren't adjusted.
pub async fn read_weather(
    status: &WeatherState,
    source: &dyn WeatherSource,
    now: chrono::DateTime<Local>,
) -> Option<Weather> {
    let entity_id = status.read().unwrap().entity_id.clone();
    let result = source.fetch(entity_id.as_deref()).await;
    let mut status = status.write().unwrap();
    match result {
        Ok(weather) => {
            status.read_at = weather.as_ref().map(|_| now);
            status.weather = weather.clone();
            status.error = None;
            weather
        }
        Err(e) => {
            status.error = Some(e.to_string());
            let fresh = status
                .read_at
                .is_some_and(|at| now - at <= KEEP_READING_FOR);
            if fresh {
                eprintln!("Failed to read the weather, using the last reading: {}", e);
            } else {
                eprintln!("Failed to read the weather, not adjusting targets: {}", e);
                status.weather = None;
                status.read_at = None;
            }
            status.weather.clone()
        }
    }
}

/// Read the hourly forecast into `status` when the last attempt is [`FORECAST_EVERY`] old (or
/// there hasn't been one), so Home Assistant is asked at most twice an hour. A failed read keeps
/// the last forecast, minus the hours that have passed. Sunrise and sunset are read with it.
pub async fn read_forecast(
    status: &WeatherState,
    source: &dyn WeatherSource,
    now: chrono::DateTime<Local>,
) {
    let entity_id = {
        let status = status.read().unwrap();
        let due = status
            .forecast_tried_at
            .is_none_or(|at| now - at >= FORECAST_EVERY);
        match &status.entity_id {
            Some(id) if due => id.clone(),
            _ => return,
        }
    };
    let result = source.fetch_forecast(&entity_id).await;
    let daylight = source.fetch_daylight().await;
    let mut status = status.write().unwrap();
    // The entity was changed while the forecast was read: it's for the old one
    if status.entity_id.as_deref() != Some(entity_id.as_str()) {
        return;
    }
    status.forecast_tried_at = Some(now);
    match daylight {
        Ok(daylight) => status.daylight = daylight,
        Err(e) => eprintln!("Failed to read sunrise and sunset, keeping the last: {}", e),
    }
    match result {
        Ok(forecast) => {
            status.forecast = forecast;
            status.forecast_error = None;
        }
        Err(e) => {
            eprintln!("Failed to read the forecast, keeping the last one: {}", e);
            status.forecast_error = Some(e.to_string());
            let this_hour = now.to_utc() - chrono::TimeDelta::hours(1);
            status.forecast.retain(|h| h.at > this_hour);
        }
    }
}

/// The room sensors of the zones that have one, by zone id. A sensor that can't be read is left
/// out, so a shadow or smart zone uses its TRVs' readings, and no zone's TRVs are driven harder.
pub async fn read_room_sensors(
    zones: &ZonesState,
    source: &dyn WeatherSource,
) -> HashMap<Uuid, f64> {
    let sensors: Vec<(Uuid, String)> = zones
        .read()
        .unwrap()
        .zones
        .iter()
        .filter_map(|z| Some((z.id, z.room.sensor_entity_id.clone()?)))
        .collect();
    let mut readings = HashMap::new();
    for (zone_id, sensor) in sensors {
        match source.fetch_temperature(&sensor).await {
            Ok(Some(c)) => {
                readings.insert(zone_id, c);
            }
            Ok(None) => eprintln!("{sensor} has no reading; using the TRVs"),
            Err(e) => eprintln!("Failed to read {sensor}, using the TRVs: {}", e),
        }
    }
    readings
}

/// Why an entity is being set the way it is, e.g. "Lounge: 20 °C scheduled, +1.5 cold = 21.5 °C"
pub fn call_context(zone: Option<&str>, status: &ZoneStatus, boosted: bool) -> String {
    let zone = zone.unwrap_or("no zone");
    let mut why = match (&status.state, status.scheduled_target, status.target) {
        (HeatingState::On, Some(scheduled), Some(target)) => {
            let mut why = format!("{scheduled} °C scheduled");
            for reason in &status.reasons {
                let sign = if reason.delta > 0.0 { "+" } else { "-" };
                why += &format!(", {sign}{} {:?}", reason.delta.abs(), reason.cause).to_lowercase();
            }
            if target != scheduled || !status.reasons.is_empty() {
                why += &format!(" = {target} °C");
            }
            if status.held {
                why += " (kept: small change)";
            }
            why
        }
        (HeatingState::On, _, _) => "On".to_string(),
        (HeatingState::Off, _, _) => "Off".to_string(),
    };
    if let Some(early) = &status.early_start {
        why += &format!(", early start for {}", early.period_start.format("%H:%M"));
    }
    if let Some(late) = &status.late_finish {
        why += &format!(", late finish for {}", late.period_end.format("%H:%M"));
    }
    if let Some(warm) = &status.warm_up {
        why += &format!(", cold warm-up ({} °C outside)", warm.outside_c);
    }
    if boosted {
        why += ", boost";
    }
    format!("{zone}: {why}")
}

/// A smart zone on the first tick after a start, when it can't decide yet because nothing has
/// been read. Its entities are only read that tick: falling back to the schedule could turn off a
/// warm-up that's under way. From the next tick it decides, or (with no room temperature at all)
/// follows its schedule, so a radiator that never reads can't stop the zone being heated.
pub fn smart_waiting(zone: &Zone, first_tick: bool) -> bool {
    first_tick && zone.mode == crate::zones::room::ZoneMode::Smart
}

/// The decisions that heat their zones: smart zones' only, never shadow zones'
pub fn acting_decisions(
    zones: &Zones,
    ticks: &HashMap<Uuid, crate::shadow::Tick>,
) -> HashMap<Uuid, crate::smart::Decision> {
    ticks
        .iter()
        .filter(|(id, _)| {
            zones
                .get(**id)
                .is_some_and(|z| z.mode == crate::zones::room::ZoneMode::Smart)
        })
        .map(|(id, t)| (*id, t.decision.decision.clone()))
        .collect()
}

/// What a smart zone's decision asks of its entities: On at the decision's target to heat, else Off
pub fn smart_scheduled(decision: &crate::smart::Decision) -> Scheduled {
    match (decision.action, decision.target) {
        (crate::smart::Action::Heat, Some(target)) => Scheduled {
            state: HeatingState::On,
            target_temp: Some(target),
            room_c: None,
        },
        _ => Scheduled {
            state: HeatingState::Off,
            target_temp: None,
            room_c: None,
        },
    }
}

/// Why a smart zone's entity is set the way it is, e.g. "Lounge: smart, heat to 20 °C from 16:10
/// for 17:00"
pub fn smart_context(
    zone: Option<&str>,
    decision: &crate::smart::Decision,
    boosted: bool,
) -> String {
    use crate::smart::{Action, Factor};
    let comfort = decision.comfort.start.format("%H:%M");
    let setback = decision
        .reasons
        .iter()
        .any(|r| matches!(r, Factor::Setback { .. }));
    let mut why = match (decision.action, decision.target) {
        (Action::Heat, Some(c)) if setback => format!("smart, setback {c} °C"),
        (Action::Heat, Some(c)) => format!(
            "smart, heat to {c} °C from {} for {comfort}",
            decision.start_at.format("%H:%M")
        ),
        (Action::NotNeeded, _) => format!("smart, not needed for {comfort}"),
        _ => format!("smart, off until {}", decision.start_at.format("%H:%M")),
    };
    if boosted {
        why += ", boost";
    }
    format!("{}: {why}", zone.unwrap_or("no zone"))
}

/// Bring a tick's results into the live entity list: each entity's fetched state, and the end of
/// any boost the tick found expired. Entities added or removed during the tick stay that way, and a
/// boost set during the tick (the live boost no longer matches the one the tick started with) is
/// kept. `processed` and `boosts_at_start` are in the same order, as the tick copied them.
pub fn merge_tick_results<T: ClimateEntity>(
    live: &mut [T],
    processed: &[T],
    boosts_at_start: &[Option<BoostInfo>],
) {
    for (done, started_with) in processed.iter().zip(boosts_at_start) {
        let Some(entity) = live
            .iter_mut()
            .find(|e| e.get_entity_id() == done.get_entity_id())
        else {
            continue; // removed during the tick
        };
        entity.update_cached_state(done.get_cached_state().clone());
        if done.get_boosted_status() != started_with && entity.get_boosted_status() == started_with
        {
            entity.set_boost(done.get_boosted_status().clone());
        }
    }
}

/// Whether `entity_id` is still in the live list of managed entities
fn is_managed<T: ClimateEntity>(live: &RwLock<Vec<T>>, entity_id: &str) -> bool {
    live.read()
        .unwrap()
        .iter()
        .any(|e| e.get_entity_id() == entity_id)
}

/// Main scheduler loop that runs periodically and applies schedule
pub async fn run_scheduler<T: ClimateEntity + Clone>(state: SchedulerState<T>) {
    let mut interval = interval(Duration::from_secs(15));

    println!("\n=== Heating Scheduler Started ===");

    let mut first_tick = true;
    loop {
        interval.tick().await;

        let now = Local::now();

        // Clone entities to avoid holding lock across await points
        let mut entities_clone = {
            let climates = state.climate_entities.read().unwrap();
            climates.clone()
        };
        // Each entity's boost as the tick starts, to tell a boost this tick ended from one set meanwhile
        let boosts_at_start: Vec<Option<BoostInfo>> = entities_clone
            .iter()
            .map(|e| e.get_boosted_status().clone())
            .collect();

        // Read the weather once per tick; without it, targets aren't adjusted
        let weather = read_weather(&state.weather, state.weather_source.as_ref(), now).await;
        read_forecast(&state.weather, state.weather_source.as_ref(), now).await;
        let sensors = read_room_sensors(&state.zones, state.weather_source.as_ref()).await;
        let memories = state.memories.lock().unwrap().clone();

        // What each entity's zone schedule asks for right now, and why (for the dry-run log)
        // None for an entity of a smart zone whose radiators haven't been read yet (the first tick
        // after a start): it's only read, so a warm-up under way isn't turned off for one tick
        let (scheduled, contexts, default_target_temp): (Vec<Option<Scheduled>>, Vec<String>, f64) = {
            let sets = state.schedule.read().unwrap();
            let zones = state.zones.read().unwrap();
            let mut weather_status = state.weather.write().unwrap();
            let WeatherStatus { held, begun, .. } = &mut *weather_status;
            let statuses = zone_statuses(&zones, &sets, weather.as_ref(), held, begun, &now);
            // What Smart would do for shadow and smart zones: kept and logged, never sent
            let mut shadow = state.shadow.write().unwrap();
            let ticks = zones
                .zones
                .iter()
                .filter(|z| z.mode != crate::zones::room::ZoneMode::Schedule)
                // Not until its radiators have been read (the first tick after a start)
                .filter(|z| crate::shadow::zone_read(z, &entities_clone))
                .filter_map(|z| {
                    let sensor = sensors.get(&z.id).copied();
                    let room_c = smart_reading(z, &entities_clone, sensor, &memories);
                    let previous = shadow.decisions.get(&z.id).map(|d| &d.decision);
                    let tick = zone_tick(
                        z,
                        &sets,
                        statuses.get(&z.id)?,
                        weather.as_ref(),
                        &weather_status,
                        room_c,
                        previous,
                        now,
                    )?;
                    Some((z.id, tick))
                })
                .collect::<HashMap<Uuid, crate::shadow::Tick>>();
            // Smart zones are heated by their decision; shadow zones only log theirs
            let acting = acting_decisions(&zones, &ticks);
            shadow.record(ticks);
            drop(shadow);
            let no_zone = zone_status(None, &sets, weather.as_ref(), None, None, &now);
            let (scheduled, contexts) = entities_clone
                .iter()
                .map(|e| {
                    let zone = zones.zone_for(e.get_entity_id());
                    let status = zone.and_then(|z| statuses.get(&z.id)).unwrap_or(&no_zone);
                    let boosted = calculate_desired_heating_state_for_boost(e.get_boosted_status())
                        .0
                        == HeatingState::On;
                    let name = zone.map(|z| z.name.as_str());
                    let room_c = zone.and_then(|z| {
                        drive_reading(z, &entities_clone, sensors.get(&z.id).copied(), &memories)
                    });
                    // A boost still wins (in apply_schedule_to_entity), and the frost floor
                    // still applies when Smart leaves a room off
                    if let Some(decision) = zone.and_then(|z| acting.get(&z.id)) {
                        let scheduled = Scheduled {
                            room_c,
                            ..smart_scheduled(decision)
                        };
                        return (Some(scheduled), smart_context(name, decision, boosted));
                    }
                    if zone.is_some_and(|z| smart_waiting(z, first_tick)) {
                        return (
                            None,
                            format!("{}: smart, reading the room", name.unwrap_or("no zone")),
                        );
                    }
                    let scheduled = Scheduled {
                        state: status.state.clone(),
                        target_temp: status.target,
                        room_c,
                    };
                    (Some(scheduled), call_context(name, status, boosted))
                })
                .unzip();
            (scheduled, contexts, sets.default_target_temp)
        };

        // Process entities outside the lock
        for ((entity, scheduled), context) in
            entities_clone.iter_mut().zip(&scheduled).zip(contexts)
        {
            // Removed while this tick ran: it's no longer controlled, so send it nothing. Checked
            // again after its state is read, just before anything is sent.
            let id = entity.get_entity_id().to_string();
            let still_managed = || is_managed(&state.climate_entities, &id);
            if !still_managed() {
                continue;
            }
            let Some(scheduled) = scheduled else {
                let read = match entity.fetch_and_update_state(&state.api_client).await {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("Error fetching state for {}: {}", id, e);
                        false
                    }
                };
                // So the next tick's smart decision trusts this reading (see smart_reading)
                if still_managed() {
                    let mut memories = state.memories.lock().unwrap();
                    memories.entry(id).or_default().read = read;
                }
                continue;
            };
            let mut last_sent = state
                .last_sent_targets
                .lock()
                .unwrap()
                .get(&id)
                .copied()
                .flatten();
            let mut memory = state
                .memories
                .lock()
                .unwrap()
                .get(&id)
                .copied()
                .unwrap_or_default();
            CALL_CONTEXT
                .scope(
                    context,
                    apply_schedule_to_entity(
                        entity,
                        scheduled,
                        default_target_temp,
                        &state.api_client,
                        &mut last_sent,
                        &mut memory,
                        &still_managed,
                    ),
                )
                .await;
            if still_managed() {
                state.memories.lock().unwrap().insert(id.clone(), memory);
                state
                    .last_sent_targets
                    .lock()
                    .unwrap()
                    .insert(id, last_sent);
            }
        }

        // Merge what this tick learned into the live list, rather than writing the copy back over
        // entities added, removed or boosted while it was talking to Home Assistant
        if let Ok(mut climates) = state.climate_entities.write() {
            merge_tick_results(&mut climates, &entities_clone, &boosts_at_start);
        }
        first_tick = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    #[test]
    fn test_call_context() {
        use crate::weather::adjust::{Cause, Reason};
        let on = ZoneStatus {
            state: HeatingState::On,
            scheduled_target: Some(20.0),
            target: Some(21.5),
            reasons: vec![Reason {
                cause: Cause::Cold,
                delta: 1.5,
            }],
            raw_target: Some(21.5),
            held: false,
            early_start: None,
            late_finish: None,
            warm_up: None,
            room_c: None,
            driving: false,
            drive_setpoint: None,
        };
        assert_eq!(
            call_context(Some("Lounge"), &on, false),
            "Lounge: 20 °C scheduled, +1.5 cold = 21.5 °C"
        );
        let plain = ZoneStatus {
            target: Some(20.0),
            reasons: vec![],
            raw_target: None,
            ..on.clone()
        };
        assert_eq!(
            call_context(Some("Study"), &plain, false),
            "Study: 20 °C scheduled"
        );
        let off = ZoneStatus {
            state: HeatingState::Off,
            scheduled_target: None,
            target: None,
            ..plain
        };
        assert_eq!(call_context(None, &off, true), "no zone: Off, boost");
    }

    #[test]
    fn test_calculate_heating_action_no_change() {
        // When current matches desired, no change needed
        assert_eq!(
            calculate_heating_action_for_schedule(&HeatingState::On, &HeatingState::On),
            HeatingAction::NoChange
        );
        assert_eq!(
            calculate_heating_action_for_schedule(&HeatingState::Off, &HeatingState::Off),
            HeatingAction::NoChange
        );
    }

    #[test]
    fn test_calculate_heating_action_state_change() {
        // When states differ, change to desired
        assert_eq!(
            calculate_heating_action_for_schedule(&HeatingState::Off, &HeatingState::On),
            HeatingAction::TurnOn
        );
        assert_eq!(
            calculate_heating_action_for_schedule(&HeatingState::On, &HeatingState::Off),
            HeatingAction::TurnOff
        );
    }

    /// What an entity's zone asks for, without weather or holding
    fn scheduled_for(
        entity_id: &str,
        sets: &ScheduleSets,
        zones: &Zones,
        weather: Option<&Weather>,
        now: &chrono::DateTime<Local>,
    ) -> Scheduled {
        let status = zone_status(zones.zone_for(entity_id), sets, weather, None, None, now);
        Scheduled {
            state: status.state,
            target_temp: status.target,
            room_c: None,
        }
    }

    fn scheduled(state: HeatingState, target_temp: Option<f64>) -> Scheduled {
        Scheduled {
            state,
            target_temp,
            room_c: None,
        }
    }

    #[test]
    fn test_desired_target_temp() {
        let on = scheduled(HeatingState::On, Some(21.5));
        let off = scheduled(HeatingState::Off, None);

        assert_eq!(
            desired_target_temp(&HeatingState::On, &on, 20.0),
            Some(21.5)
        );
        assert_eq!(desired_target_temp(&HeatingState::Off, &off, 20.0), None);
        // Boosted On during a scheduled Off uses the default
        assert_eq!(
            desired_target_temp(&HeatingState::On, &off, 20.0),
            Some(20.0)
        );
    }

    fn fake_client() -> ApiClient {
        ApiClient::new(
            reqwest::Url::parse("http://fake").unwrap(),
            "fake_token".to_string(),
            None,
        )
    }

    fn mock(state: HeatingState) -> crate::climate::MockClimate {
        crate::climate::MockClimate::new("climate.test".to_string(), state)
    }

    /// One scheduler tick for `entity`
    async fn tick(
        entity: &mut crate::climate::MockClimate,
        scheduled: &Scheduled,
        last_sent: &mut Option<f64>,
    ) {
        apply_schedule_to_entity(
            entity,
            scheduled,
            20.0,
            &fake_client(),
            last_sent,
            &mut EntityMemory::default(),
            &|| true,
        )
        .await;
    }

    fn sent(entity: &crate::climate::MockClimate) -> Vec<f64> {
        entity.set_temperatures.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn test_turning_on_sets_target_once() {
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let on = scheduled(HeatingState::On, Some(21.5));

        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &on, &mut last).await;

        assert_eq!(sent(&entity), vec![21.5]);
    }

    #[tokio::test]
    async fn test_rounding_trv_is_not_recommanded() {
        let mut entity = mock(HeatingState::Off);
        entity.target_step = Some(0.5);
        let mut last = None;
        let on = scheduled(HeatingState::On, Some(21.3));

        for _ in 0..5 {
            tick(&mut entity, &on, &mut last).await;
        }

        // The TRV reports 21.5, but we only sent once
        assert_eq!(entity.info.as_ref().unwrap().target_temp, Some(21.5));
        assert_eq!(sent(&entity), vec![21.3]);
    }

    #[tokio::test]
    async fn test_target_changed_on_the_trv_is_put_back() {
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let on = scheduled(HeatingState::On, Some(20.0));

        tick(&mut entity, &on, &mut last).await;
        // The TRV drops to its Away preset's 5 °C
        *entity.changed_on_trv.lock().unwrap() = Some(5.0);
        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &on, &mut last).await;

        assert_eq!(sent(&entity), vec![20.0, 20.0]);
    }

    #[tokio::test]
    async fn test_trv_that_ignores_the_put_back_is_retried_every_5_minutes() {
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let mut put_back = EntityMemory::default();
        let on = scheduled(HeatingState::On, Some(20.0));
        let mut tick = async |entity: &mut crate::climate::MockClimate,
                              put_back: &mut EntityMemory| {
            // Stuck in its Away preset: it keeps reporting 5 °C whatever it's sent
            apply_schedule_to_entity(
                entity,
                &on,
                20.0,
                &fake_client(),
                &mut last,
                put_back,
                &|| true,
            )
            .await;
            *entity.changed_on_trv.lock().unwrap() = Some(5.0);
        };

        tick(&mut entity, &mut put_back).await;
        for _ in 0..3 {
            tick(&mut entity, &mut put_back).await;
        }
        assert_eq!(sent(&entity), vec![20.0, 20.0], "put back once");

        put_back.put_back_at = put_back.put_back_at.map(|at| at - PUT_BACK_EVERY);
        tick(&mut entity, &mut put_back).await;
        assert_eq!(
            sent(&entity),
            vec![20.0, 20.0, 20.0],
            "and again 5 minutes on"
        );
    }

    #[tokio::test]
    async fn test_target_change_while_on_is_sent_once() {
        let mut entity = mock(HeatingState::On);
        let mut last = None;

        tick(
            &mut entity,
            &scheduled(HeatingState::On, Some(19.0)),
            &mut last,
        )
        .await;
        tick(
            &mut entity,
            &scheduled(HeatingState::On, Some(22.0)),
            &mut last,
        )
        .await;
        tick(
            &mut entity,
            &scheduled(HeatingState::On, Some(22.0)),
            &mut last,
        )
        .await;

        assert_eq!(sent(&entity), vec![19.0, 22.0]);
    }

    #[tokio::test]
    async fn test_turning_on_again_resends_same_target() {
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let on = scheduled(HeatingState::On, Some(21.0));

        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &scheduled(HeatingState::Off, None), &mut last).await;
        tick(&mut entity, &on, &mut last).await;

        assert_eq!(sent(&entity), vec![21.0, 21.0]);
    }

    #[tokio::test]
    async fn test_failed_send_retries_once_per_tick() {
        let mut entity = mock(HeatingState::On);
        entity.fail_set_temperature = true;
        let mut last = None;
        let on = scheduled(HeatingState::On, Some(21.0));

        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &on, &mut last).await;
        assert_eq!(sent(&entity), vec![21.0, 21.0], "one attempt per tick");
        assert_eq!(last, None);

        entity.fail_set_temperature = false;
        tick(&mut entity, &on, &mut last).await;
        tick(&mut entity, &on, &mut last).await;
        assert_eq!(sent(&entity).len(), 3, "stops once a send succeeds");
        assert_eq!(last, Some(21.0));
    }

    #[tokio::test]
    async fn test_off_sets_no_temperature() {
        let mut entity = mock(HeatingState::On);
        let mut last = Some(21.0);

        tick(&mut entity, &scheduled(HeatingState::Off, None), &mut last).await;

        assert!(sent(&entity).is_empty());
        assert_eq!(last, None);
    }

    #[test]
    fn test_frost_floor() {
        use HeatingState::{Off, On};
        // Only when it would be Off
        assert!(frost_floor(&Off, 6.9, &Off, None));
        assert!(!frost_floor(&On, 6.9, &Off, None));
        assert!(!frost_floor(&Off, 7.0, &Off, None));
        // Once it's heating to the floor's target, it keeps going until the room reaches it
        assert!(frost_floor(&Off, 9.9, &On, Some(FROST_TARGET_C)));
        assert!(!frost_floor(&Off, 10.0, &On, Some(FROST_TARGET_C)));
        // Heating for another reason, or reporting Off, doesn't keep it going
        assert!(!frost_floor(&Off, 9.0, &On, Some(20.0)));
        assert!(!frost_floor(&Off, 9.0, &Off, Some(FROST_TARGET_C)));
    }

    #[tokio::test]
    async fn test_frost_floor_heats_a_cold_room_until_it_reaches_the_target() {
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let off = scheduled(HeatingState::Off, None);

        entity.current_temperature = 8.0;
        tick(&mut entity, &off, &mut last).await;
        assert!(sent(&entity).is_empty(), "above the floor, Off stays Off");

        entity.current_temperature = 6.5;
        tick(&mut entity, &off, &mut last).await;
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::On));
        assert_eq!(sent(&entity), vec![FROST_TARGET_C]);

        // Warming, but not there yet: keeps heating without resending
        entity.current_temperature = 8.5;
        tick(&mut entity, &off, &mut last).await;
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::On));
        assert_eq!(sent(&entity), vec![FROST_TARGET_C]);

        entity.current_temperature = 10.0;
        tick(&mut entity, &off, &mut last).await;
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::Off));
        assert_eq!(last, None);
    }

    fn smart_decision(action: crate::smart::Action) -> crate::smart::Decision {
        let five = NaiveTime::from_hms_opt(17, 0, 0).unwrap();
        crate::smart::Decision {
            action,
            target: (action == crate::smart::Action::Heat).then_some(20.5),
            start_at: NaiveTime::from_hms_opt(16, 10, 0).unwrap(),
            comfort: crate::smart::ComfortTime {
                start: five,
                end: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                target: 20.5,
            },
            reasons: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_a_smart_decision_heats_and_stops_and_the_frost_floor_still_applies() {
        use crate::smart::Action;
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let heat = smart_scheduled(&smart_decision(Action::Heat));
        tick(&mut entity, &heat, &mut last).await;
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::On));
        assert_eq!(sent(&entity), vec![20.5]);

        for action in [Action::NotNeeded, Action::Off] {
            let off = smart_scheduled(&smart_decision(action));
            entity.mode.lock().unwrap().replace(HeatingState::On);
            tick(&mut entity, &off, &mut last).await;
            assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::Off));
        }

        // Smart leaves it off, but the room is freezing
        entity.current_temperature = 6.0;
        tick(
            &mut entity,
            &smart_scheduled(&smart_decision(Action::Off)),
            &mut last,
        )
        .await;
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::On));
        assert_eq!(sent(&entity).last(), Some(&FROST_TARGET_C));
    }

    #[tokio::test]
    async fn test_only_smart_zones_act_on_their_decisions() {
        use crate::zones::room::ZoneMode;
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.a".to_string()]);
        zones
            .create_manual(
                "Loft",
                &["climate.a".to_string()],
                &["climate.a".to_string()],
            )
            .unwrap();
        let ids: Vec<Uuid> = zones.zones.iter().map(|z| z.id).collect();
        let zone_tick = || crate::shadow::Tick {
            decision: crate::shadow::ZoneDecision {
                at: Local::now(),
                room_c: Some(18.0),
                schedule_start: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
                decision: smart_decision(crate::smart::Action::Heat),
            },
            inside: false,
            schedule_heating: false,
        };
        let ticks: HashMap<Uuid, crate::shadow::Tick> =
            ids.iter().map(|id| (*id, zone_tick())).collect();
        let (shadow, smart) = (ids[0], ids[1]);
        zones.zones[0].mode = ZoneMode::Shadow;
        zones.zones[1].mode = ZoneMode::Smart;

        let acting = acting_decisions(&zones, &ticks);
        assert!(
            !acting.contains_key(&shadow),
            "a shadow zone never heats from its decision"
        );
        assert!(acting.contains_key(&smart));

        // A tick for each zone's TRV, the schedule Off: the shadow zone's sends nothing, though
        // its decision is Heat; the smart zone's heats
        let schedule_off = scheduled(HeatingState::Off, None);
        for (zone, heats) in [(shadow, false), (smart, true)] {
            let mut trv = mock(HeatingState::Off);
            let mut last = None;
            let wanted = acting
                .get(&zone)
                .map_or(schedule_off.clone(), smart_scheduled);
            tick(&mut trv, &wanted, &mut last).await;
            assert_eq!(!sent(&trv).is_empty(), heats);
            assert_eq!(*trv.mode.lock().unwrap() == Some(HeatingState::On), heats);
        }
    }

    #[test]
    fn test_a_smart_zone_waits_only_on_the_first_tick() {
        use crate::zones::room::ZoneMode;
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.test".to_string()]);
        let mut zone = zones.zones[0].clone();
        zone.mode = ZoneMode::Smart;
        assert!(smart_waiting(&zone, true));
        assert!(
            !smart_waiting(&zone, false),
            "after that, never: a dead TRV can't stop it"
        );
        // Schedule and shadow zones follow the schedule from the first tick
        for mode in [ZoneMode::Schedule, ZoneMode::Shadow] {
            zone.mode = mode;
            assert!(!smart_waiting(&zone, true));
        }
    }

    #[test]
    fn test_smart_context() {
        use crate::smart::{Action, Factor};
        let heat = smart_decision(Action::Heat);
        assert_eq!(
            smart_context(Some("Lounge"), &heat, false),
            "Lounge: smart, heat to 20.5 °C from 16:10 for 17:00"
        );
        let mut setback = heat.clone();
        setback.target = Some(15.0);
        setback.reasons.push(Factor::Setback { c: 15.0 });
        assert_eq!(
            smart_context(None, &setback, true),
            "no zone: smart, setback 15 °C, boost"
        );
        assert_eq!(
            smart_context(Some("Lounge"), &smart_decision(Action::NotNeeded), false),
            "Lounge: smart, not needed for 17:00"
        );
        assert_eq!(
            smart_context(Some("Lounge"), &smart_decision(Action::Off), false),
            "Lounge: smart, off until 16:10"
        );
    }

    #[tokio::test]
    async fn test_frost_floor_leaves_a_scheduled_target_alone() {
        let mut entity = mock(HeatingState::Off);
        entity.current_temperature = 5.0;
        let mut last = None;

        tick(
            &mut entity,
            &scheduled(HeatingState::On, Some(18.0)),
            &mut last,
        )
        .await;

        assert_eq!(sent(&entity), vec![18.0]);
    }

    #[tokio::test]
    async fn test_boost_with_no_max_temp_uses_the_usual_target() {
        for (scheduled, expected) in [
            (scheduled(HeatingState::Off, None), 20.0),
            (scheduled(HeatingState::On, Some(18.0)), 18.0),
        ] {
            let mut entity = mock(HeatingState::Off);
            entity.set_boost(Some(BoostInfo::starting(Local::now(), 1)));
            let mut last = None;

            tick(&mut entity, &scheduled, &mut last).await;

            assert_eq!(sent(&entity), vec![expected], "{scheduled:?}");
        }
    }

    #[test]
    fn test_scheduled_for_follows_zone_set() {
        use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
        use crate::zones::areas::Area;

        // Active set: Off all day. "Warm" set: On 21.0 all day.
        let mut sets = ScheduleSets::from_schedule(Schedule::new("Off"));
        let warm = sets.create("Warm", None).unwrap().id;
        sets.get_mut(warm).unwrap().add_entry(
            ScheduleEntry::new("All day", TimePeriod::new(0, 0, 0, 0), HeatingState::On)
                .with_target(21.0),
        );

        let managed = vec!["climate.study".to_string(), "climate.loft".to_string()];
        let mut zones = Zones::default();
        let study_area = Area {
            id: "study".to_string(),
            name: "Study".to_string(),
            entities: vec!["climate.study".to_string()],
        };
        zones.reconcile(Some(vec![study_area]), &managed);
        let study = zones.zone_for("climate.study").unwrap().id;
        let now = Local::now();

        // Every zone follows the active set until assigned one
        assert_eq!(
            scheduled_for("climate.study", &sets, &zones, None, &now),
            scheduled(HeatingState::Off, None)
        );

        zones.set_schedule_set(study, Some(warm)).unwrap();
        assert_eq!(
            scheduled_for("climate.study", &sets, &zones, None, &now),
            scheduled(HeatingState::On, Some(21.0))
        );
        assert_eq!(
            scheduled_for("climate.loft", &sets, &zones, None, &now),
            scheduled(HeatingState::Off, None)
        );
        // An entity in no zone follows the active set
        assert_eq!(
            scheduled_for("climate.unknown", &sets, &zones, None, &now),
            scheduled(HeatingState::Off, None)
        );
    }

    fn zone_with_profile(adjust: bool) -> Zone {
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &["climate.a".to_string()]);
        let mut zone = zones.zones[0].clone();
        zone.weather_adjust = adjust;
        zone.wind_exposure = crate::weather::adjust::WindExposure::High;
        zone
    }

    fn on_all_day(target: f64) -> ScheduleSets {
        use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
        let mut schedule = Schedule::new("On");
        schedule.add_entry(
            ScheduleEntry::new("All day", TimePeriod::new(0, 0, 0, 0), HeatingState::On)
                .with_target(target),
        );
        ScheduleSets::from_schedule(schedule)
    }

    fn cold_and_windy() -> Weather {
        Weather {
            temperature: Some(-5.0),
            wind_speed: Some(50.0),
            wind_bearing: None,
            cloud_coverage: Some(100.0),
        }
    }

    #[test]
    fn test_zone_status_adjusts_only_when_switched_on() {
        let sets = on_all_day(20.0);
        let now = Local::now();
        let weather = cold_and_windy();

        let off = zone_status(
            Some(&zone_with_profile(false)),
            &sets,
            Some(&weather),
            None,
            None,
            &now,
        );
        assert_eq!(off.target, Some(20.0));
        assert!(off.reasons.is_empty());

        let on = zone_status(
            Some(&zone_with_profile(true)),
            &sets,
            Some(&weather),
            None,
            None,
            &now,
        );
        assert_eq!(on.scheduled_target, Some(20.0));
        assert_eq!(on.target, Some(22.5));
        assert_eq!(on.reasons.len(), 2);

        // No weather reading: the scheduled target as is
        let unread = zone_status(
            Some(&zone_with_profile(true)),
            &sets,
            None,
            None,
            None,
            &now,
        );
        assert_eq!(unread.target, Some(20.0));
    }

    #[test]
    fn test_zone_status_never_adjusts_off() {
        use crate::schedule::Schedule;
        let sets = ScheduleSets::from_schedule(Schedule::new("Off"));
        let status = zone_status(
            Some(&zone_with_profile(true)),
            &sets,
            Some(&cold_and_windy()),
            None,
            None,
            &Local::now(),
        );
        assert_eq!(status.state, HeatingState::Off);
        assert_eq!(status.target, None);
        assert!(status.reasons.is_empty());
    }

    #[tokio::test]
    async fn test_target_clamped_to_entity_limits() {
        let mut entity = mock(HeatingState::Off);
        entity.info.as_mut().unwrap().max_temp = Some(21.0);
        let mut last = None;

        tick(
            &mut entity,
            &scheduled(HeatingState::On, Some(22.5)),
            &mut last,
        )
        .await;

        assert_eq!(sent(&entity), vec![21.0]);
    }

    struct FailingWeather;

    #[async_trait::async_trait]
    impl WeatherSource for FailingWeather {
        async fn fetch(&self, _entity_id: Option<&str>) -> anyhow::Result<Option<Weather>> {
            Err(anyhow::anyhow!("HA unreachable"))
        }
        async fn fetch_forecast(
            &self,
            _entity_id: &str,
        ) -> anyhow::Result<Vec<crate::weather::ForecastHour>> {
            Err(anyhow::anyhow!("HA unreachable"))
        }
        async fn fetch_daylight(&self) -> anyhow::Result<Option<crate::schedule::TimePeriod>> {
            anyhow::bail!("no sun")
        }
    }

    #[tokio::test]
    async fn test_forecast_is_read_twice_an_hour_and_kept_while_reads_fail() {
        let status: WeatherState = Default::default();
        let mock = crate::weather::MockWeather::default();
        *mock.weather.write().unwrap() = cold_and_windy();
        let start = Local::now();
        let at = |minutes| start + chrono::TimeDelta::minutes(minutes);
        let forecast = |status: &WeatherState| status.read().unwrap().forecast.clone();

        // No weather entity: nothing is asked
        read_forecast(&status, &mock, at(0)).await;
        assert!(forecast(&status).is_empty());
        assert_eq!(status.read().unwrap().forecast_tried_at, None);

        status.write().unwrap().entity_id = Some("weather.example".to_string());
        read_forecast(&status, &mock, at(0)).await;
        let first = forecast(&status);
        assert_eq!(first.len(), 24);
        assert_eq!(first[1].weather.temperature, Some(-5.5));
        let daylight = status.read().unwrap().daylight;
        assert!(daylight.is_some());

        // Not due again for 30 minutes, even if it would fail
        read_forecast(&status, &FailingWeather, at(29)).await;
        assert_eq!(forecast(&status), first);
        assert_eq!(status.read().unwrap().forecast_error, None);

        // Due and failing: the hours still to come are kept, with the error
        read_forecast(&status, &FailingWeather, at(150)).await;
        let kept = forecast(&status);
        assert!(kept.len() >= 21 && kept.len() < 24, "{}", kept.len());
        assert_eq!(kept.last(), first.last());
        assert!(status.read().unwrap().forecast_error.is_some());
        assert_eq!(status.read().unwrap().daylight, daylight);

        read_forecast(&status, &mock, at(180)).await;
        assert_eq!(forecast(&status).len(), 24);
        assert_eq!(status.read().unwrap().forecast_error, None);
    }

    #[tokio::test]
    async fn test_failed_read_keeps_last_reading_for_30_minutes() {
        let status: WeatherState = Default::default();
        let mock = crate::weather::MockWeather::default();
        *mock.weather.write().unwrap() = cold_and_windy();
        let start = Local::now();
        let at = |minutes| start + chrono::TimeDelta::minutes(minutes);

        assert_eq!(
            read_weather(&status, &mock, at(0)).await,
            Some(cold_and_windy())
        );

        // Within 30 minutes the last reading is still used
        assert_eq!(
            read_weather(&status, &FailingWeather, at(29)).await,
            Some(cold_and_windy())
        );
        assert!(
            status
                .read()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("unreachable")
        );

        // After that, no adjustment
        assert_eq!(read_weather(&status, &FailingWeather, at(31)).await, None);
        assert_eq!(status.read().unwrap().weather, None);

        // A good read clears the error
        assert_eq!(
            read_weather(&status, &mock, at(32)).await,
            Some(cold_and_windy())
        );
        assert_eq!(status.read().unwrap().error, None);
    }

    #[test]
    fn test_wobbling_weather_holds_the_target() {
        // Cold only: 3.4 °C gives 20.2 (rounds to 20), 3.3 °C gives 20.3 (rounds to 20.5)
        let sets = on_all_day(20.0);
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &["climate.a".to_string()]);
        zones.zones[0].weather_adjust = true;
        let id = zones.zones[0].id;
        let now = Local::now();
        let mut held = HashMap::new();
        let mut target_at = |outside: f64| {
            let weather = Weather {
                temperature: Some(outside),
                ..Default::default()
            };
            zone_statuses(
                &zones,
                &sets,
                Some(&weather),
                &mut held,
                &mut HashMap::new(),
                &now,
            )[&id]
                .clone()
        };

        assert_eq!(target_at(3.4).target, Some(20.0));
        for outside in [3.3, 3.4, 3.3, 3.2, 3.4, 3.0] {
            let status = target_at(outside);
            assert_eq!(status.target, Some(20.0), "at {} °C: {:?}", outside, status);
        }
        // Held when the rounded value differs
        assert!(target_at(3.3).held);

        // A clear move (raw 20.5, 0.25 past the boundary) changes it
        assert_eq!(target_at(1.7).target, Some(20.5));
        // ...and then the new value is held in turn
        assert_eq!(target_at(3.0).target, Some(20.5));
        // A move back below 20.0 releases it
        assert_eq!(target_at(5.0).target, Some(20.0));
    }

    #[test]
    fn test_hold_resets_when_the_schedule_changes() {
        let sets = on_all_day(20.0);
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &["climate.a".to_string()]);
        zones.zones[0].weather_adjust = true;
        let id = zones.zones[0].id;
        let weather = Weather {
            temperature: Some(3.3),
            ..Default::default()
        };
        // Held at 20.0 for a scheduled 20.0...
        let mut held = HashMap::from([(
            id,
            Held {
                scheduled: 20.0,
                target: 20.0,
            },
        )]);
        let now = Local::now();
        assert_eq!(
            zone_statuses(
                &zones,
                &sets,
                Some(&weather),
                &mut held,
                &mut HashMap::new(),
                &now
            )[&id]
                .target,
            Some(20.0)
        );
        // ...but not when the schedule now says 21.0
        let sets = on_all_day(21.0);
        assert_eq!(
            zone_statuses(
                &zones,
                &sets,
                Some(&weather),
                &mut held,
                &mut HashMap::new(),
                &now
            )[&id]
                .target,
            Some(21.5)
        );
    }

    #[tokio::test]
    async fn test_weather_hiccup_sends_nothing() {
        let sets = on_all_day(20.0);
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &["climate.test".to_string()]);
        zones.zones[0].weather_adjust = true;
        let id = zones.zones[0].id;
        let status: WeatherState = Default::default();
        let mut entity = mock(HeatingState::Off);
        let source = crate::weather::MockWeather::default();
        *source.weather.write().unwrap() = cold_and_windy();
        let mut last = None;
        let start = Local::now();

        for (minute, ok) in [(0, true), (1, false), (2, false), (3, true)] {
            let now = start + chrono::TimeDelta::minutes(minute);
            let weather = if ok {
                read_weather(&status, &source, now).await
            } else {
                read_weather(&status, &FailingWeather, now).await
            };
            let zone_status = {
                let mut status = status.write().unwrap();
                let status = &mut *status;
                zone_statuses(
                    &zones,
                    &sets,
                    weather.as_ref(),
                    &mut status.held,
                    &mut status.begun,
                    &now,
                )[&id]
                    .clone()
            };
            tick(
                &mut entity,
                &scheduled(zone_status.state, zone_status.target),
                &mut last,
            )
            .await;
        }

        assert_eq!(sent(&entity), vec![22.0], "one send, none for the hiccup");
    }

    /// Today at hh:mm, local time
    fn today_at(hour: u32, minute: u32) -> chrono::DateTime<Local> {
        use chrono::TimeZone;
        let time = chrono::NaiveTime::from_hms_opt(hour, minute, 0).unwrap();
        Local
            .from_local_datetime(&Local::now().date_naive().and_time(time))
            .earliest()
            .unwrap()
    }

    fn morning_sets() -> ScheduleSets {
        use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
        let mut schedule = Schedule::new("Mornings");
        schedule.add_entry(
            ScheduleEntry::new("Morning", TimePeriod::new(7, 0, 9, 0), HeatingState::On)
                .with_target(21.0),
        );
        ScheduleSets::from_schedule(schedule)
    }

    fn adjusting_zone() -> (Zones, Uuid) {
        let mut zones = Zones::default();
        zones.reconcile(Some(vec![]), &["climate.test".to_string()]);
        zones.zones[0].weather_adjust = true;
        let id = zones.zones[0].id;
        (zones, id)
    }

    fn freezing() -> Weather {
        Weather {
            temperature: Some(-2.0),
            wind_speed: Some(0.0),
            wind_bearing: None,
            cloud_coverage: Some(100.0),
        }
    }

    #[test]
    fn test_zone_status_starts_early_when_cold() {
        let sets = morning_sets();
        let (zones, _) = adjusting_zone();
        let zone = &zones.zones[0];

        let status = zone_status(
            Some(zone),
            &sets,
            Some(&freezing()),
            None,
            None,
            &today_at(6, 45),
        );

        assert_eq!(status.state, HeatingState::On);
        assert_eq!(status.scheduled_target, Some(21.0));
        // -2 °C also adjusts the target: +1.1 cold -> 22.1, rounded to 22.0
        assert_eq!(status.target, Some(22.0));
        let early = status.early_start.unwrap();
        assert_eq!(
            early.period_start,
            chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap()
        );
        assert_eq!(early.lead_minutes, 30);

        // Too early yet, weather adjust off, or early start disabled: still Off
        let too_early = zone_status(
            Some(zone),
            &sets,
            Some(&freezing()),
            None,
            None,
            &today_at(6, 20),
        );
        assert_eq!(too_early.state, HeatingState::Off);
        let mut off_zone = zone.clone();
        off_zone.weather_adjust = false;
        let status = zone_status(
            Some(&off_zone),
            &sets,
            Some(&freezing()),
            None,
            None,
            &today_at(6, 45),
        );
        assert_eq!(status.state, HeatingState::Off);
        let mut disabled = zone.clone();
        disabled.max_early_start_minutes = 0;
        let status = zone_status(
            Some(&disabled),
            &sets,
            Some(&freezing()),
            None,
            None,
            &today_at(6, 45),
        );
        assert_eq!(status.state, HeatingState::Off);
        assert!(status.early_start.is_none());
    }

    #[tokio::test]
    async fn test_early_start_sends_once_into_the_period() {
        let sets = morning_sets();
        let (zones, id) = adjusting_zone();
        let mut held = HashMap::new();
        let mut early = HashMap::new();
        let mut entity = mock(HeatingState::Off);
        let mut last = None;

        let mut states = Vec::new();
        for (hour, minute) in [(6, 20), (6, 35), (6, 50), (7, 0), (7, 30), (9, 0)] {
            let now = today_at(hour, minute);
            let status = zone_statuses(
                &zones,
                &sets,
                Some(&freezing()),
                &mut held,
                &mut early,
                &now,
            )[&id]
                .clone();
            states.push((status.state.clone(), status.early_start.is_some()));
            tick(
                &mut entity,
                &scheduled(status.state, status.target),
                &mut last,
            )
            .await;
        }

        use HeatingState::*;
        assert_eq!(
            states,
            vec![
                (Off, false),
                (On, true),
                (On, true),
                (On, false),
                (On, false),
                (Off, false),
            ]
        );
        // Turned On at 06:35 with 22.0, unchanged into the period, nothing sent when Off
        assert_eq!(sent(&entity), vec![22.0]);
        assert_eq!(*entity.mode.lock().unwrap(), Some(Off));
    }

    #[test]
    fn test_early_start_keeps_when_heating_began() {
        let sets = morning_sets();
        let (zones, id) = adjusting_zone();
        let (mut held, mut early) = (HashMap::new(), HashMap::new());

        // Heating begins at 06:35, 25 minutes before 07:00 (the lead allows 30)
        let minutes = [(6, 35), (6, 45), (6, 55)].map(|(hour, minute)| {
            let status = &zone_statuses(
                &zones,
                &sets,
                Some(&freezing()),
                &mut held,
                &mut early,
                &today_at(hour, minute),
            )[&id];
            let early = status.early_start.as_ref().unwrap();
            (early.lead_minutes, early.minutes_early)
        });
        assert_eq!(minutes, [(30, 25); 3]);
    }

    #[tokio::test]
    async fn test_warming_mid_lead_keeps_the_early_start() {
        let sets = morning_sets();
        let (mut zones, id) = adjusting_zone();
        let mut held = HashMap::new();
        let mut early = HashMap::new();
        let mut entity = mock(HeatingState::Off);
        let mut last = None;
        let mild = Weather {
            temperature: Some(6.0), // a 10-minute lead
            ..Default::default()
        };

        // 06:40 at -2 °C starts early; at 06:41 it's 6 °C, so a fresh check would say Off
        let mut states = Vec::new();
        for (minute, weather) in [(40, &freezing()), (41, &mild), (50, &mild), (55, &mild)] {
            let now = today_at(6, minute);
            let status = zone_statuses(&zones, &sets, Some(weather), &mut held, &mut early, &now)
                [&id]
                .clone();
            states.push(status.state.clone());
            tick(
                &mut entity,
                &scheduled(status.state, status.target),
                &mut last,
            )
            .await;
        }
        let now = today_at(7, 0);
        let status =
            zone_statuses(&zones, &sets, Some(&mild), &mut held, &mut early, &now)[&id].clone();
        states.push(status.state.clone());
        assert!(
            status.early_start.is_none(),
            "the period itself has started"
        );
        tick(
            &mut entity,
            &scheduled(status.state, status.target),
            &mut last,
        )
        .await;

        assert_eq!(states, vec![HeatingState::On; 5]);
        assert_eq!(*entity.mode.lock().unwrap(), Some(HeatingState::On));
        // One TurnOn; the target follows the weather adjustment once (22.0 -> 21.0 when mild)
        assert_eq!(sent(&entity), vec![22.0, 21.0]);

        // Switching weather adjust off ends a begun early start
        let mut early = HashMap::from([(
            id,
            Begun {
                early_start: Some(EarlyStart {
                    period_start: chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
                    lead_minutes: 30,
                    minutes_early: 30,
                    causes: vec![],
                }),
                late_finish: None,
                warm_up: None,
            },
        )]);
        zones.zones[0].weather_adjust = false;
        let status = zone_statuses(
            &zones,
            &sets,
            Some(&mild),
            &mut HashMap::new(),
            &mut early,
            &today_at(6, 45),
        )[&id]
            .clone();
        assert_eq!(status.state, HeatingState::Off);
    }

    /// The Lounge-style zone with late finishes enabled
    fn late_zone(minutes: u32) -> (Zones, Uuid) {
        let (mut zones, id) = adjusting_zone();
        zones.zones[0].max_late_finish_minutes = minutes;
        (zones, id)
    }

    /// Run ticks at the given times, returning (state, target, early start?, late finish?) each time
    async fn run_ticks(
        zones: &Zones,
        sets: &ScheduleSets,
        weather: &[Weather],
        times: &[(u32, u32)],
        entity: &mut crate::climate::MockClimate,
    ) -> Vec<(HeatingState, Option<f64>, bool, bool)> {
        let id = zones.zones[0].id;
        let (mut held, mut begun, mut last) = (HashMap::new(), HashMap::new(), None);
        let mut seen = Vec::new();
        for (i, &(hour, minute)) in times.iter().enumerate() {
            let weather = &weather[i.min(weather.len() - 1)];
            let status = zone_statuses(
                zones,
                sets,
                Some(weather),
                &mut held,
                &mut begun,
                &today_at(hour, minute),
            )[&id]
                .clone();
            seen.push((
                status.state.clone(),
                status.target,
                status.early_start.is_some(),
                status.late_finish.is_some(),
            ));
            tick(entity, &scheduled(status.state, status.target), &mut last).await;
        }
        seen
    }

    #[tokio::test]
    async fn test_late_finish_keeps_heating_then_turns_off_once() {
        use HeatingState::*;
        let sets = morning_sets();
        let (zones, _) = late_zone(30);
        let mut entity = mock(On);
        // Warming at 09:10 doesn't cut the extension short: it was decided at 09:00
        let weather = [freezing(), freezing(), freezing(), Weather::default()];
        let seen = run_ticks(
            &zones,
            &sets,
            &weather,
            &[(8, 55), (9, 0), (9, 5), (9, 10), (9, 29), (9, 30), (9, 45)],
            &mut entity,
        )
        .await;
        assert_eq!(
            seen,
            vec![
                (On, Some(22.0), false, false),
                (On, Some(22.0), false, true),
                (On, Some(22.0), false, true),
                // No weather now, so the ended period's plain target
                (On, Some(21.0), false, true),
                (On, Some(21.0), false, true),
                (Off, None, false, false),
                (Off, None, false, false),
            ]
        );
        // One send per real change, and turned off once, when the extension ended
        assert_eq!(sent(&entity), vec![22.0, 21.0]);
        assert_eq!(*entity.mode.lock().unwrap(), Some(Off));
    }

    #[tokio::test]
    async fn test_late_finish_meets_the_next_early_start() {
        use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
        use HeatingState::*;
        let mut schedule = Schedule::new("Two");
        schedule.add_entry(
            ScheduleEntry::new("Early", TimePeriod::new(6, 0, 7, 0), On).with_target(21.0),
        );
        schedule.add_entry(
            ScheduleEntry::new("Later", TimePeriod::new(7, 40, 9, 0), On).with_target(23.0),
        );
        let sets = ScheduleSets::from_schedule(schedule);
        let (zones, _) = late_zone(30);
        let mut entity = mock(On);
        // 07:00 starts a 30 minute late finish; the 07:40 period may start 30 minutes early (07:10)
        let seen = run_ticks(
            &zones,
            &sets,
            &[freezing()],
            &[(6, 55), (7, 0), (7, 5), (7, 10), (7, 20), (7, 35), (7, 40)],
            &mut entity,
        )
        .await;
        assert_eq!(
            seen,
            vec![
                (On, Some(22.0), false, false),
                (On, Some(22.0), false, true),
                (On, Some(22.0), false, true),
                // The early start takes over: no gap, and one change of target
                (On, Some(24.0), true, false),
                (On, Some(24.0), true, false),
                (On, Some(24.0), true, false),
                (On, Some(24.0), false, false),
            ]
        );
        assert_eq!(sent(&entity), vec![22.0, 24.0]);
        // It was already On, so no mode was ever commanded: in particular, never Off
        assert_eq!(*entity.mode.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn test_late_finish_needs_weather_adjust_and_a_cap() {
        use HeatingState::*;
        let sets = morning_sets();
        for (adjust, minutes) in [(false, 30), (true, 0)] {
            let (mut zones, _) = late_zone(minutes);
            zones.zones[0].weather_adjust = adjust;
            let mut entity = mock(On);
            let seen = run_ticks(
                &zones,
                &sets,
                &[freezing()],
                &[(8, 55), (9, 0)],
                &mut entity,
            )
            .await;
            assert_eq!(
                seen[1],
                (Off, None, false, false),
                "adjust {adjust}, {minutes} min"
            );
        }
    }

    /// Mornings 06:00-09:00 and evenings 17:00-22:00, so 09:00-17:00 is an 8-hour Off gap
    fn day_sets() -> ScheduleSets {
        use crate::schedule::{Schedule, ScheduleEntry, TimePeriod};
        let mut schedule = Schedule::new("Day");
        for (sh, eh) in [(6, 9), (17, 22)] {
            schedule.add_entry(
                ScheduleEntry::new("On", TimePeriod::new(sh, 0, eh, 0), HeatingState::On)
                    .with_target(21.0),
            );
        }
        ScheduleSets::from_schedule(schedule)
    }

    fn warmup_zone(enabled: bool) -> (Zones, Uuid) {
        let (mut zones, id) = adjusting_zone();
        zones.zones[0].cold_warmups.enabled = enabled;
        (zones, id)
    }

    #[tokio::test]
    async fn test_warm_ups_turn_on_and_off_once_each() {
        use HeatingState::*;
        let sets = day_sets();
        let (zones, _) = warmup_zone(true);
        let mut entity = mock(Off);
        let mild = Weather {
            temperature: Some(5.0),
            ..freezing()
        };
        // Freezing at each slot's start; milder at 12:10 doesn't end the 12:00 warm-up early
        let weather = [freezing(), freezing(), mild, freezing()];
        let seen = run_ticks(
            &zones,
            &sets,
            &weather,
            &[
                (11, 59),
                (12, 0),
                (12, 10),
                (12, 19),
                (12, 20),
                (14, 0),
                (15, 0),
                (15, 20),
            ],
            &mut entity,
        )
        .await;
        let states: Vec<_> = seen.iter().map(|(s, t, ..)| (s.clone(), *t)).collect();
        assert_eq!(
            states,
            vec![
                (Off, None),
                // The default target (20), not adjusted for the freezing weather
                (On, Some(20.0)),
                (On, Some(20.0)),
                (On, Some(20.0)),
                (Off, None),
                (Off, None),
                (On, Some(20.0)),
                (Off, None),
            ]
        );
        // One target send per warm-up (each turns On), and Off at the end
        assert_eq!(sent(&entity), vec![20.0, 20.0]);
        assert_eq!(*entity.mode.lock().unwrap(), Some(Off));
    }

    #[test]
    fn test_warm_ups_only_for_zones_that_opt_in() {
        let sets = day_sets();
        let (zones, _) = warmup_zone(false);
        let status = zone_status(
            Some(&zones.zones[0]),
            &sets,
            Some(&freezing()),
            None,
            None,
            &today_at(12, 0),
        );
        assert_eq!(status.state, HeatingState::Off);
        assert!(status.warm_up.is_none());

        // And none without a weather reading
        let (zones, _) = warmup_zone(true);
        let status = zone_status(
            Some(&zones.zones[0]),
            &sets,
            None,
            None,
            None,
            &today_at(12, 0),
        );
        assert_eq!(status.state, HeatingState::Off);
    }

    /// `h:m` UTC on `day` `month` 2026
    fn utc(month: u32, day: u32, h: u32, m: u32) -> chrono::DateTime<Local> {
        use chrono::TimeZone;
        let at = chrono::NaiveDate::from_ymd_opt(2026, month, day)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap();
        Local.from_utc_datetime(&at)
    }

    fn boost(minutes: i64) -> BoostInfo {
        BoostInfo::starting(utc(1, 15, 12, 0), minutes)
    }

    const HEATS: (HeatingState, bool) = (HeatingState::On, false);
    const ENDS: (HeatingState, bool) = (HeatingState::Off, true);

    #[test]
    fn test_boost_heats_until_its_end() {
        let b = Some(boost(60));
        assert_eq!(boost_state_at(&b, utc(1, 15, 12, 0)), HEATS);
        assert_eq!(boost_state_at(&b, utc(1, 15, 12, 59)), HEATS);
        assert_eq!(
            boost_state_at(&b, utc(1, 15, 13, 1)),
            ENDS,
            "expired boost is cleared"
        );
        assert_eq!(
            boost_state_at(&None, utc(1, 15, 12, 30)),
            (HeatingState::Off, false)
        );
    }

    #[test]
    fn test_boost_past_midnight_heats_until_its_real_end() {
        // 23:30 to 01:30
        let b = Some(BoostInfo::starting(utc(1, 15, 23, 30), 120));
        assert_eq!(boost_state_at(&b, utc(1, 15, 23, 30)), HEATS, "first tick");
        assert_eq!(
            boost_state_at(&b, utc(1, 16, 0, 0)),
            HEATS,
            "after midnight"
        );
        assert_eq!(boost_state_at(&b, utc(1, 16, 1, 29)), HEATS);
        assert_eq!(
            boost_state_at(&b, utc(1, 16, 1, 31)),
            ENDS,
            "cleared after its end"
        );
    }

    #[test]
    fn test_boost_lasts_as_long_as_asked_when_the_clocks_go_back() {
        // 25 Oct 2026: at 01:00 UTC, UK clocks go back from 02:00 BST to 01:00 GMT. A boost from
        // 01:30 BST (00:30 UTC) for an hour then sees the clock read 01:00, before its start,
        // which a boost of times of day took as over.
        let b = Some(BoostInfo::starting(utc(10, 25, 0, 30), 60));
        assert_eq!(
            boost_state_at(&b, utc(10, 25, 1, 0)),
            HEATS,
            "clocks just went back"
        );
        assert_eq!(boost_state_at(&b, utc(10, 25, 1, 29)), HEATS);
        assert_eq!(
            boost_state_at(&b, utc(10, 25, 1, 31)),
            ENDS,
            "an hour after it began"
        );
    }

    #[test]
    fn test_boost_lasts_as_long_as_asked_when_the_clocks_go_forward() {
        // 29 Mar 2026: at 01:00 UTC, UK clocks go forward from 01:00 GMT to 02:00 BST. A boost from
        // 00:50 GMT for 45 minutes then sees the clock read 02:00, past its 01:35 end, which a
        // boost of times of day took as over.
        let b = Some(BoostInfo::starting(utc(3, 29, 0, 50), 45));
        assert_eq!(
            boost_state_at(&b, utc(3, 29, 1, 0)),
            HEATS,
            "clocks just went forward"
        );
        assert_eq!(boost_state_at(&b, utc(3, 29, 1, 34)), HEATS);
        assert_eq!(
            boost_state_at(&b, utc(3, 29, 1, 36)),
            ENDS,
            "45 minutes after it began"
        );
    }

    fn named(id: &str) -> crate::climate::MockClimate {
        crate::climate::MockClimate::new(id.to_string(), HeatingState::Off)
    }

    /// What an entity's cached state says: (state, target)
    fn seen(entity: &crate::climate::MockClimate) -> Option<(HeatingState, Option<f64>)> {
        entity
            .get_cached_state()
            .as_ref()
            .map(|c| (c.state.clone(), c.target_temp))
    }

    #[test]
    fn test_entity_added_mid_tick_is_kept() {
        let mut live = vec![named("climate.a"), named("climate.b")];
        // The tick copies the list and fetches each entity's state from HA ...
        let mut processed = live.clone();
        let boosts: Vec<_> = processed
            .iter()
            .map(|e| e.get_boosted_status().clone())
            .collect();
        for entity in processed.iter_mut() {
            let mut fetched = entity.get_cached_state().clone().unwrap();
            fetched.state = HeatingState::On;
            fetched.target_temp = Some(21.5);
            entity.update_cached_state(Some(fetched));
        }
        // ... while someone adds an entity
        live.push(named("climate.c"));

        merge_tick_results(&mut live, &processed, &boosts);
        let ids: Vec<_> = live.iter().map(|e| e.get_entity_id()).collect();
        assert_eq!(ids, vec!["climate.a", "climate.b", "climate.c"]);
        // The processed entities have what the tick fetched; the new one is untouched
        let on = Some((HeatingState::On, Some(21.5)));
        assert_eq!(seen(&live[0]), on);
        assert_eq!(seen(&live[1]), on);
        assert_eq!(seen(&live[2]), Some((HeatingState::Off, None)));
    }

    #[test]
    fn test_entity_removed_mid_tick_stays_removed() {
        let mut live = vec![named("climate.a"), named("climate.b")];
        let processed = live.clone();
        let boosts = vec![None, None];
        live.retain(|e| e.get_entity_id() != "climate.a");

        merge_tick_results(&mut live, &processed, &boosts);
        let ids: Vec<_> = live.iter().map(|e| e.get_entity_id()).collect();
        assert_eq!(ids, vec!["climate.b"]);
    }

    #[test]
    fn test_an_entity_removed_mid_tick_is_not_managed() {
        let live = RwLock::new(vec![named("climate.a"), named("climate.b")]);
        assert!(is_managed(&live, "climate.a"));
        // Removed while the tick works through its copy
        live.write()
            .unwrap()
            .retain(|e| e.get_entity_id() != "climate.a");
        assert!(!is_managed(&live, "climate.a"));
        assert!(is_managed(&live, "climate.b"));
    }

    #[test]
    fn test_boosts_set_mid_tick_are_kept_and_expired_ones_end() {
        // a: boosted when the tick started, and the tick found it expired
        // b: boosted by the user while the tick ran
        // c: its boost was replaced by a new one while the tick ended the old one
        let mut a = named("climate.a");
        a.set_boost(Some(boost(30)));
        let mut c = named("climate.c");
        c.set_boost(Some(boost(30)));
        let mut live = vec![a, named("climate.b"), c];
        let mut processed = live.clone();
        let boosts: Vec<_> = processed
            .iter()
            .map(|e| e.get_boosted_status().clone())
            .collect();
        processed[0].set_boost(None);
        processed[2].set_boost(None);
        live[1].set_boost(Some(boost(45)));
        live[2].set_boost(Some(boost(60)));

        merge_tick_results(&mut live, &processed, &boosts);
        assert_eq!(live[0].get_boosted_status(), &None, "expired boost ended");
        assert_eq!(
            live[1].get_boosted_status(),
            &Some(boost(45)),
            "new boost kept"
        );
        assert_eq!(
            live[2].get_boosted_status(),
            &Some(boost(60)),
            "replacing boost kept"
        );
    }

    /// A mock TRV that takes up to `max_temp`
    fn trv(max_temp: Option<f64>) -> crate::climate::MockClimate {
        let mut entity = mock(HeatingState::Off);
        entity.info.as_mut().unwrap().max_temp = max_temp;
        entity
    }

    /// On at `target`, with the room reading `room_c`
    fn on_in_room(target: f64, room_c: Option<f64>) -> Scheduled {
        Scheduled {
            room_c,
            ..scheduled(HeatingState::On, Some(target))
        }
    }

    /// One scheduler tick for `entity`, keeping what it remembers between ticks in `memory`
    async fn drive_tick(
        entity: &mut crate::climate::MockClimate,
        scheduled: &Scheduled,
        last_sent: &mut Option<f64>,
        memory: &mut EntityMemory,
    ) {
        apply_schedule_to_entity(
            entity,
            scheduled,
            20.0,
            &fake_client(),
            last_sent,
            memory,
            &|| true,
        )
        .await;
    }

    #[tokio::test]
    async fn test_a_room_well_below_the_target_drives_the_trv_at_its_max_temp() {
        let mut entity = trv(Some(28.0));
        let (mut last, mut memory) = (None, EntityMemory::default());

        drive_tick(
            &mut entity,
            &on_in_room(20.0, Some(18.0)),
            &mut last,
            &mut memory,
        )
        .await;

        assert_eq!(sent(&entity), vec![28.0]);
        assert!(memory.driving);
        assert_eq!(memory.room_c, Some(18.0));
        assert_eq!(last, Some(28.0));
    }

    #[tokio::test]
    async fn test_driving_stops_near_the_target_and_restarts_only_well_below_it() {
        let mut entity = trv(Some(28.0));
        let (mut last, mut memory) = (None, EntityMemory::default());
        let mut at = async |room_c: f64| {
            drive_tick(
                &mut entity,
                &on_in_room(20.0, Some(room_c)),
                &mut last,
                &mut memory,
            )
            .await;
            (sent(&entity), memory.driving)
        };

        assert_eq!(at(18.0).await, (vec![28.0], true));
        assert_eq!(
            at(19.0).await,
            (vec![28.0], true),
            "1 °C below: still driving"
        );
        assert_eq!(at(19.6).await, (vec![28.0, 20.0], false), "within 0.5 °C");
        assert_eq!(
            at(19.0).await,
            (vec![28.0, 20.0], false),
            "1 °C below: not again"
        );
        assert_eq!(
            at(18.5).await,
            (vec![28.0, 20.0, 28.0], true),
            "1.5 °C below"
        );
        assert_eq!(
            at(18.5).await,
            (vec![28.0, 20.0, 28.0], true),
            "nothing resent"
        );
    }

    #[test]
    fn test_drive() {
        assert!(drive(20.0, Some(18.5), false));
        assert!(!drive(20.0, Some(18.6), false));
        assert!(drive(20.0, Some(19.4), true));
        assert!(!drive(20.0, Some(19.5), true));
        assert!(!drive(20.0, Some(21.0), true), "above the target");
        assert!(!drive(20.0, None, true), "never without a reading");
    }

    #[test]
    fn test_smart_reads_only_the_trvs_read_on_their_last_tick() {
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.a".to_string(), "climate.b".to_string()]);
        let mut zone = zones.zones.remove(0);
        zone.room.trv_offset_c = -1.5;
        // a's last fetch failed, so it still holds a frozen 15 °C
        let entities: Vec<_> = [15.0, 20.0]
            .into_iter()
            .zip(["climate.a", "climate.b"])
            .map(|(c, id)| {
                let mut e = crate::climate::MockClimate::new(id.to_string(), HeatingState::On);
                e.info.as_mut().unwrap().current_temperature = c;
                e
            })
            .collect();
        let read = |ids: &[&str]| -> HashMap<String, EntityMemory> {
            let memory = EntityMemory {
                read: true,
                ..Default::default()
            };
            ids.iter().map(|id| (id.to_string(), memory)).collect()
        };

        // Both read: their average, without the TRV offset (that's only for driving)
        let both = read(&["climate.a", "climate.b"]);
        assert_eq!(smart_reading(&zone, &entities, None, &both), Some(17.5));
        // Only the fresh TRV
        let stale_a = read(&["climate.b"]);
        assert_eq!(smart_reading(&zone, &entities, None, &stale_a), Some(20.0));
        // Every TRV stale: no reading, as when none has been read
        let none_read = HashMap::new();
        assert_eq!(smart_reading(&zone, &entities, None, &none_read), None);
        // A room sensor's reading is used whatever its TRVs
        assert_eq!(
            smart_reading(&zone, &entities, Some(18.0), &none_read),
            Some(18.0)
        );
    }

    #[test]
    fn test_a_trv_only_zone_reads_its_trvs_plus_its_offset() {
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.a".to_string(), "climate.b".to_string()]);
        let mut zone = zones.zones.remove(0);
        let entities: Vec<_> = [19.0, 20.0]
            .into_iter()
            .zip(["climate.a", "climate.b"])
            .map(|(c, id)| {
                let mut e = crate::climate::MockClimate::new(id.to_string(), HeatingState::On);
                e.info.as_mut().unwrap().current_temperature = c;
                e
            })
            .collect();

        let read = |ids: &[&str]| -> HashMap<String, EntityMemory> {
            ids.iter()
                .map(|id| {
                    let memory = EntityMemory {
                        read: true,
                        ..Default::default()
                    };
                    (id.to_string(), memory)
                })
                .collect()
        };
        let both = read(&["climate.a", "climate.b"]);

        // TRVs at 0.5 °C below the target: not far enough, unless they read warm
        assert_eq!(drive_reading(&zone, &entities, None, &both), Some(19.5));
        assert!(!drive(
            20.0,
            drive_reading(&zone, &entities, None, &both),
            false
        ));
        zone.room.trv_offset_c = -1.5;
        assert_eq!(drive_reading(&zone, &entities, None, &both), Some(18.0));
        assert!(drive(
            20.0,
            drive_reading(&zone, &entities, None, &both),
            false
        ));

        // A TRV that couldn't be read on its last tick is left out: a's frozen 15 °C would make
        // the average 17.5 and drive, but b's 20 °C alone doesn't
        zone.room.trv_offset_c = 0.0;
        let mut frozen = entities.clone();
        frozen[0].info.as_mut().unwrap().current_temperature = 15.0;
        assert_eq!(drive_reading(&zone, &frozen, None, &both), Some(17.5));
        let stale_a = read(&["climate.b"]);
        assert_eq!(drive_reading(&zone, &frozen, None, &stale_a), Some(20.0));
        assert!(!drive(
            20.0,
            drive_reading(&zone, &frozen, None, &stale_a),
            false
        ));
        // None read: nothing to drive by
        assert_eq!(drive_reading(&zone, &entities, None, &HashMap::new()), None);
        assert_eq!(
            drive_reading(
                &zone,
                &Vec::<crate::climate::MockClimate>::new(),
                None,
                &both
            ),
            None
        );

        // A zone with a room sensor reads only the sensor, as it is, without the offset
        zone.room.sensor_entity_id = Some("sensor.lounge".to_string());
        assert_eq!(
            drive_reading(&zone, &entities, Some(19.5), &both),
            Some(19.5)
        );
        // and when the sensor can't be read, doesn't fall back to its TRVs
        assert_eq!(drive_reading(&zone, &entities, None, &both), None);
    }

    #[tokio::test]
    async fn test_a_failed_fetch_marks_the_entity_unread() {
        // Nothing listens on port 1, so reading its state fails
        let unreachable = ApiClient::new(
            reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
            "fake_token".to_string(),
            None,
        );
        let mut entity = crate::climate::DefaultClimate::new("climate.test".to_string());
        let mut memory = EntityMemory {
            read: true,
            ..Default::default()
        };
        apply_schedule_to_entity(
            &mut entity,
            &on_in_room(20.0, Some(15.0)),
            20.0,
            &unreachable,
            &mut None,
            &mut memory,
            &|| true,
        )
        .await;
        assert!(!memory.read);

        let mut entity = trv(Some(28.0));
        drive_tick(&mut entity, &on_in_room(20.0, None), &mut None, &mut memory).await;
        assert!(memory.read);
    }

    #[tokio::test]
    async fn test_no_reading_or_no_max_temp_sends_exactly_the_target() {
        for (max_temp, room_c) in [(Some(28.0), None), (None, Some(15.0))] {
            let mut entity = trv(max_temp);
            let (mut last, mut memory) = (None, EntityMemory::default());
            for _ in 0..3 {
                let on = on_in_room(20.0, room_c);
                drive_tick(&mut entity, &on, &mut last, &mut memory).await;
            }
            assert_eq!(sent(&entity), vec![20.0], "{max_temp:?} {room_c:?}");
            assert!(!memory.driving);
        }
    }

    #[tokio::test]
    async fn test_the_frost_floor_is_never_driven() {
        let mut entity = trv(Some(28.0));
        entity.current_temperature = 5.0;
        let (mut last, mut memory) = (None, EntityMemory::default());
        let off = Scheduled {
            room_c: Some(4.0),
            ..scheduled(HeatingState::Off, None)
        };

        drive_tick(&mut entity, &off, &mut last, &mut memory).await;
        drive_tick(&mut entity, &off, &mut last, &mut memory).await;

        assert_eq!(sent(&entity), vec![FROST_TARGET_C]);
        assert!(!memory.driving);
    }

    #[tokio::test]
    async fn test_a_driven_trv_is_put_back_only_when_changed_by_hand() {
        let mut entity = trv(Some(28.0));
        let (mut last, mut memory) = (None, EntityMemory::default());
        let cold = on_in_room(20.0, Some(17.0));

        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        // It reports its max_temp, as it was sent: that's not a change on the TRV
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![28.0]);

        // Turned down by hand
        *entity.changed_on_trv.lock().unwrap() = Some(21.0);
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![28.0, 28.0]);
    }

    fn boosted_trv(max_temp: Option<f64>) -> crate::climate::MockClimate {
        let mut entity = trv(max_temp);
        entity.set_boost(Some(BoostInfo::starting(Local::now(), 60)));
        entity
    }

    /// A boost that ended a minute ago
    fn ended_boost() -> BoostInfo {
        BoostInfo::starting(Local::now() - chrono::TimeDelta::minutes(61), 60)
    }

    #[tokio::test]
    async fn test_a_boost_sends_the_max_temp_whatever_the_schedule() {
        for scheduled in [
            scheduled(HeatingState::Off, None),
            scheduled(HeatingState::On, Some(18.0)),
        ] {
            let mut entity = boosted_trv(Some(28.0));
            let (mut last, mut memory) = (None, EntityMemory::default());

            drive_tick(&mut entity, &scheduled, &mut last, &mut memory).await;

            assert_eq!(sent(&entity), vec![28.0], "{scheduled:?}");
            assert_eq!(last, Some(28.0));
            assert!(!memory.driving);
        }
    }

    #[tokio::test]
    async fn test_a_boost_sends_the_max_temp_once_then_the_usual_target_after() {
        let mut entity = boosted_trv(Some(28.0));
        let (mut last, mut memory) = (None, EntityMemory::default());
        let on = scheduled(HeatingState::On, Some(18.0));

        // The TRV reports the max_temp it was sent: not a change on the TRV, so not put back
        for _ in 0..3 {
            drive_tick(&mut entity, &on, &mut last, &mut memory).await;
        }
        assert_eq!(sent(&entity), vec![28.0]);
        assert_eq!(memory.put_back_at, None);

        entity.set_boost(Some(ended_boost()));
        for _ in 0..2 {
            drive_tick(&mut entity, &on, &mut last, &mut memory).await;
        }
        assert_eq!(sent(&entity), vec![28.0, 18.0]);
        assert_eq!(entity.get_boosted_status(), &None);
        assert_eq!(memory.put_back_at, None);
    }

    #[tokio::test]
    async fn test_a_boost_while_driving_sends_the_max_temp_and_driving_resumes_after() {
        let mut entity = trv(Some(28.0));
        let (mut last, mut memory) = (None, EntityMemory::default());
        let cold = on_in_room(20.0, Some(17.0));

        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        assert!(memory.driving);
        assert_eq!(memory.drive_setpoint, Some(28.0));

        // Boosted: still the max_temp, nothing resent, and it's the boost not driving
        entity.set_boost(Some(BoostInfo::starting(Local::now(), 60)));
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![28.0]);
        assert!(!memory.driving);
        assert_eq!(memory.drive_setpoint, None);

        // Ended with the room still cold: driven again, at the max_temp already sent
        entity.set_boost(Some(ended_boost()));
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![28.0]);
        assert!(memory.driving);
        assert_eq!(memory.drive_setpoint, Some(28.0));

        // Ended with the room near the target: back to the target, not driving
        entity.set_boost(Some(BoostInfo::starting(Local::now(), 60)));
        drive_tick(&mut entity, &cold, &mut last, &mut memory).await;
        entity.set_boost(Some(ended_boost()));
        let warm = on_in_room(20.0, Some(19.8));
        drive_tick(&mut entity, &warm, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![28.0, 20.0]);
        assert!(!memory.driving);
        assert_eq!(memory.drive_setpoint, None);
    }

    #[tokio::test]
    async fn test_a_boost_in_a_frosty_room_sends_the_max_temp_and_the_floor_resumes_after() {
        let mut entity = trv(Some(28.0));
        entity.current_temperature = 5.0;
        let (mut last, mut memory) = (None, EntityMemory::default());
        let off = Scheduled {
            room_c: Some(4.0),
            ..scheduled(HeatingState::Off, None)
        };

        drive_tick(&mut entity, &off, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![FROST_TARGET_C]);

        entity.set_boost(Some(BoostInfo::starting(Local::now(), 60)));
        drive_tick(&mut entity, &off, &mut last, &mut memory).await;
        drive_tick(&mut entity, &off, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![FROST_TARGET_C, 28.0]);
        assert!(!memory.driving);

        // Still below the frost floor when it ends: heated to the floor's target again
        entity.set_boost(Some(ended_boost()));
        drive_tick(&mut entity, &off, &mut last, &mut memory).await;
        assert_eq!(sent(&entity), vec![FROST_TARGET_C, 28.0, FROST_TARGET_C]);
        assert!(!memory.driving);
        assert_eq!(memory.drive_setpoint, None);
    }

    /// Reads every room sensor at 17 °C
    struct ColdRoom;

    #[async_trait::async_trait]
    impl WeatherSource for ColdRoom {
        async fn fetch(&self, _entity_id: Option<&str>) -> anyhow::Result<Option<Weather>> {
            Ok(None)
        }
        async fn fetch_forecast(
            &self,
            _entity_id: &str,
        ) -> anyhow::Result<Vec<crate::weather::ForecastHour>> {
            Ok(Vec::new())
        }
        async fn fetch_daylight(&self) -> anyhow::Result<Option<crate::schedule::TimePeriod>> {
            Ok(None)
        }
        async fn fetch_temperature(&self, _entity_id: &str) -> anyhow::Result<Option<f64>> {
            Ok(Some(17.0))
        }
    }

    #[tokio::test]
    async fn test_every_mode_drives_a_cold_room() {
        use crate::zones::room::ZoneMode;
        for mode in [ZoneMode::Schedule, ZoneMode::Shadow, ZoneMode::Smart] {
            let mut zones = Zones::default();
            zones.reconcile(None, &["climate.test".to_string()]);
            zones.zones[0].mode = mode;
            zones.zones[0].room.sensor_entity_id = Some("sensor.lounge".to_string());
            let zone_id = zones.zones[0].id;
            let zones: ZonesState = Arc::new(RwLock::new(zones));

            let sensors = read_room_sensors(&zones, &ColdRoom).await;
            assert_eq!(sensors.get(&zone_id), Some(&17.0), "{mode:?}");
            let room_c = drive_reading(
                &zones.read().unwrap().zones[0],
                &[mock(HeatingState::Off)],
                sensors.get(&zone_id).copied(),
                &HashMap::new(),
            );
            // Smart heats by its decision; Schedule and Shadow by the schedule
            let scheduled = match mode {
                ZoneMode::Smart => smart_scheduled(&smart_decision(crate::smart::Action::Heat)),
                _ => scheduled(HeatingState::On, Some(20.0)),
            };
            let scheduled = Scheduled {
                room_c,
                ..scheduled
            };

            let mut entity = trv(Some(28.0));
            let (mut last, mut memory) = (None, EntityMemory::default());
            drive_tick(&mut entity, &scheduled, &mut last, &mut memory).await;
            assert_eq!(sent(&entity), vec![28.0], "{mode:?}");
        }
    }

    #[test]
    fn test_zone_drive() {
        let mut zones = Zones::default();
        zones.reconcile(None, &["climate.a".to_string(), "climate.b".to_string()]);
        let zone = &zones.zones[0];
        let memory = |drive_setpoint: Option<f64>| EntityMemory {
            driving: drive_setpoint.is_some(),
            drive_setpoint,
            room_c: Some(17.0),
            ..Default::default()
        };
        let memories = HashMap::from([
            ("climate.a".to_string(), memory(Some(28.0))),
            ("climate.b".to_string(), memory(Some(30.0))),
        ]);
        assert_eq!(zone_drive(zone, &memories), (Some(17.0), true, Some(30.0)));
        let memories = HashMap::from([("climate.a".to_string(), memory(None))]);
        assert_eq!(zone_drive(zone, &memories), (Some(17.0), false, None));
        assert_eq!(zone_drive(zone, &HashMap::new()), (None, false, None));
    }
}
