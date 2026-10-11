use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateEntity};
use crate::dry_run::CALL_CONTEXT;
use crate::schedule::HeatingState;
use crate::schedule::TARGET_TEMP_RANGE;
use crate::schedule::sets::ScheduleSets;
use crate::weather::Begun;
use crate::weather::adjust::{Reason, adjust_target};
use crate::weather::early::{EarlyStart, continue_early_start, early_start};
use crate::weather::late::{LateFinish, continue_late_finish, late_finish};
use crate::weather::{Held, KEEP_READING_FOR, Weather, WeatherSource, WeatherStatus};
use crate::zones::{Zone, Zones};
use crate::{ScheduleState, WeatherState, ZonesState};
use chrono::Local;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

pub struct SchedulerState<T: ClimateEntity + Clone> {
    pub api_client: ApiClient,
    pub schedule: ScheduleState,
    pub zones: ZonesState,
    pub weather: WeatherState,
    pub weather_source: Arc<dyn WeatherSource>,
    pub climate_entities: Arc<RwLock<Vec<T>>>,
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
    if let Some(boosted) = boost_info {
        // Validate that current time is inside the boosted time period
        let now = Local::now().time();
        // Check if current time is within boost period
        if now >= boosted.boost_start && now <= boosted.boost_end {
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

    let state = entry
        .map(|e| e.heating_state.clone())
        .unwrap_or(HeatingState::Off);
    let scheduled_target = entry.and_then(|e| e.target_temp);

    let adjusted = match (zone, weather, &state, scheduled_target) {
        (Some(zone), Some(weather), HeatingState::On, Some(target)) if zone.weather_adjust => {
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
        if status.early_start.is_some() || status.late_finish.is_some() {
            begun.insert(
                *id,
                Begun {
                    early_start: status.early_start.clone(),
                    late_finish: status.late_finish.clone(),
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

/// Fetch one entity's state and bring it in line with the schedule and any boost.
/// Turning On sets the mode, then the target temperature.
///
/// `last_sent` is the target last sent to this entity successfully. The target is only sent
/// when it differs from that, or right after turning On, never because the entity reports a
/// different value: a TRV that rounds or clamps the target would otherwise be re-commanded
/// every tick. A failed send leaves `last_sent` unchanged, so it is retried on the next tick.
pub async fn apply_schedule_to_entity<T: ClimateEntity>(
    entity: &mut T,
    scheduled: &Scheduled,
    default_target_temp: f64,
    api_client: &ApiClient,
    last_sent: &mut Option<f64>,
) {
    let now = Local::now();
    if let Err(e) = entity.fetch_and_update_state(api_client).await {
        eprintln!(
            "[{}] Error fetching state for {}: {}",
            now.format("%Y-%m-%d %H:%M:%S"),
            entity.get_entity_id(),
            e
        );
        return;
    }
    let (boosted_state, should_update) =
        calculate_desired_heating_state_for_boost(entity.get_boosted_status());
    if should_update {
        entity.set_boost(None);
    }
    let final_desired_state = final_desired_heating_state(&scheduled.state, &boosted_state);

    let cached = entity.get_cached_state().clone().unwrap();
    let heating_state = cached.state;

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

    let Some(target) = desired_target_temp(&final_desired_state, scheduled, default_target_temp)
    else {
        *last_sent = None;
        return;
    };
    // Keep within what the entity accepts
    let target = target
        .max(cached.min_temp.unwrap_or(f64::MIN))
        .min(cached.max_temp.unwrap_or(f64::MAX));
    if action == HeatingAction::TurnOn || *last_sent != Some(target) {
        match entity.set_temperature(api_client, target).await {
            Ok(()) => *last_sent = Some(target),
            Err(e) => eprintln!("  ✗ Error setting temperature: {}", e),
        }
    }
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
    if boosted {
        why += ", boost";
    }
    format!("{zone}: {why}")
}

/// Main scheduler loop that runs periodically and applies schedule
pub async fn run_scheduler<T: ClimateEntity + Clone>(state: SchedulerState<T>) {
    let mut interval = interval(Duration::from_secs(15));

    println!("\n=== Heating Scheduler Started ===");

    // Last target temperature sent to each entity, by entity id
    let mut last_sent_targets: HashMap<String, Option<f64>> = HashMap::new();

    loop {
        interval.tick().await;

        let now = Local::now();

        // Clone entities to avoid holding lock across await points
        let mut entities_clone = {
            let climates = state.climate_entities.read().unwrap();
            climates.clone()
        };

        // Read the weather once per tick; without it, targets aren't adjusted
        let weather = read_weather(&state.weather, state.weather_source.as_ref(), now).await;

        // What each entity's zone schedule asks for right now, and why (for the dry-run log)
        let (scheduled, contexts, default_target_temp): (Vec<Scheduled>, Vec<String>, f64) = {
            let sets = state.schedule.read().unwrap();
            let zones = state.zones.read().unwrap();
            let mut weather_status = state.weather.write().unwrap();
            let WeatherStatus { held, begun, .. } = &mut *weather_status;
            let statuses = zone_statuses(&zones, &sets, weather.as_ref(), held, begun, &now);
            let no_zone = zone_status(None, &sets, weather.as_ref(), None, None, &now);
            let (scheduled, contexts) = entities_clone
                .iter()
                .map(|e| {
                    let zone = zones.zone_for(e.get_entity_id());
                    let status = zone.and_then(|z| statuses.get(&z.id)).unwrap_or(&no_zone);
                    let boosted = calculate_desired_heating_state_for_boost(e.get_boosted_status())
                        .0
                        == HeatingState::On;
                    let scheduled = Scheduled {
                        state: status.state.clone(),
                        target_temp: status.target,
                    };
                    (
                        scheduled,
                        call_context(zone.map(|z| z.name.as_str()), status, boosted),
                    )
                })
                .unzip();
            (scheduled, contexts, sets.default_target_temp)
        };

        // Process entities outside the lock
        for ((entity, scheduled), context) in
            entities_clone.iter_mut().zip(&scheduled).zip(contexts)
        {
            let last_sent = last_sent_targets
                .entry(entity.get_entity_id().to_string())
                .or_default();
            CALL_CONTEXT
                .scope(
                    context,
                    apply_schedule_to_entity(
                        entity,
                        scheduled,
                        default_target_temp,
                        &state.api_client,
                        last_sent,
                    ),
                )
                .await;
        }

        // Update the shared state with processed entities
        if let Ok(mut climates) = state.climate_entities.write() {
            *climates = entities_clone;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    fn scheduled(state: HeatingState, target_temp: Option<f64>) -> Scheduled {
        Scheduled { state, target_temp }
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
        apply_schedule_to_entity(entity, scheduled, 20.0, &fake_client(), last_sent).await;
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

    #[tokio::test]
    async fn test_boost_during_off_uses_default_target() {
        let mut entity = mock(HeatingState::Off);
        let now = Local::now().time();
        let boost_end = now + chrono::Duration::minutes(1);
        if boost_end < now {
            return; // boost windows don't wrap midnight; skip in the last minute of the day
        }
        entity.set_boost(Some(BoostInfo {
            boost_start: now,
            boost_end,
        }));
        let mut last = None;

        tick(&mut entity, &scheduled(HeatingState::Off, None), &mut last).await;

        assert_eq!(sent(&entity), vec![20.0]);
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
}
