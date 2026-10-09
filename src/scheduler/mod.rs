use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::HeatingState;
use crate::schedule::sets::ScheduleSets;
use crate::weather::adjust::{Reason, adjust_target};
use crate::weather::{Weather, WeatherSource};
use crate::zones::{Zone, Zones};
use crate::{ScheduleState, WeatherState, ZonesState};
use chrono::Local;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::interval;

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

/// What the schedule asks of one entity at `time`: its zone's schedule set, or the active set
/// when the zone follows the active set, the zone's set no longer exists, or it has no zone
pub fn scheduled_for(
    entity_id: &str,
    sets: &ScheduleSets,
    zones: &Zones,
    weather: Option<&Weather>,
    time: &chrono::DateTime<Local>,
) -> Scheduled {
    let status = zone_status(zones.zone_for(entity_id), sets, weather, time);
    Scheduled {
        state: status.state,
        target_temp: status.target,
    }
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
}

/// A zone's state and target at `time`: from its schedule set (or the active set when it follows
/// the active set, its set no longer exists, or there is no zone), adjusted for the weather when
/// the zone has weather adjust on and is scheduled On. Off is never adjusted.
pub fn zone_status(
    zone: Option<&Zone>,
    sets: &ScheduleSets,
    weather: Option<&Weather>,
    time: &chrono::DateTime<Local>,
) -> ZoneStatus {
    let schedule = zone
        .and_then(|z| z.schedule_set_id)
        .and_then(|id| sets.get(id))
        .unwrap_or_else(|| sets.active());
    let entry = schedule.get_active_entry(time);
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
    ZoneStatus {
        state,
        scheduled_target,
        target: adjusted.as_ref().map(|a| a.target).or(scheduled_target),
        reasons: adjusted.map(|a| a.reasons).unwrap_or_default(),
    }
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
pub async fn read_weather(status: &WeatherState, source: &dyn WeatherSource) -> Option<Weather> {
    let entity_id = status.read().unwrap().entity_id.clone();
    let result = source.fetch(entity_id.as_deref()).await;
    let mut status = status.write().unwrap();
    match result {
        Ok(weather) => {
            status.weather = weather.clone();
            status.error = None;
            weather
        }
        Err(e) => {
            eprintln!("Failed to read the weather, not adjusting targets: {}", e);
            status.weather = None;
            status.error = Some(e.to_string());
            None
        }
    }
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
        let weather = read_weather(&state.weather, state.weather_source.as_ref()).await;

        // What each entity's zone schedule asks for right now
        let (scheduled, default_target_temp): (Vec<Scheduled>, f64) = {
            let sets = state.schedule.read().unwrap();
            let zones = state.zones.read().unwrap();
            let scheduled = entities_clone
                .iter()
                .map(|e| scheduled_for(e.get_entity_id(), &sets, &zones, weather.as_ref(), &now))
                .collect();
            (scheduled, sets.default_target_temp)
        };

        // Process entities outside the lock
        for (entity, scheduled) in entities_clone.iter_mut().zip(&scheduled) {
            let last_sent = last_sent_targets
                .entry(entity.get_entity_id().to_string())
                .or_default();
            apply_schedule_to_entity(
                entity,
                scheduled,
                default_target_temp,
                &state.api_client,
                last_sent,
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

        let off = zone_status(Some(&zone_with_profile(false)), &sets, Some(&weather), &now);
        assert_eq!(off.target, Some(20.0));
        assert!(off.reasons.is_empty());

        let on = zone_status(Some(&zone_with_profile(true)), &sets, Some(&weather), &now);
        assert_eq!(on.scheduled_target, Some(20.0));
        assert_eq!(on.target, Some(22.5));
        assert_eq!(on.reasons.len(), 2);

        // No weather reading: the scheduled target as is
        let unread = zone_status(Some(&zone_with_profile(true)), &sets, None, &now);
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
    async fn test_read_weather_records_reading_and_failure() {
        let status: WeatherState = Default::default();
        let mock = crate::weather::MockWeather::default();
        *mock.weather.write().unwrap() = cold_and_windy();

        assert_eq!(read_weather(&status, &mock).await, Some(cold_and_windy()));
        assert_eq!(status.read().unwrap().weather, Some(cold_and_windy()));

        assert_eq!(read_weather(&status, &FailingWeather).await, None);
        let status = status.read().unwrap();
        assert_eq!(status.weather, None);
        assert!(status.error.as_deref().unwrap().contains("unreachable"));
    }
}
