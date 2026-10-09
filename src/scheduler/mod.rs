use crate::ScheduleState;
use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::HeatingState;
use chrono::Local;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::interval;

pub struct SchedulerState<T: ClimateEntity + Clone> {
    pub api_client: ApiClient,
    pub schedule: ScheduleState,
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
    if action == HeatingAction::TurnOn || *last_sent != Some(target) {
        match entity.set_temperature(api_client, target).await {
            Ok(()) => *last_sent = Some(target),
            Err(e) => eprintln!("  ✗ Error setting temperature: {}", e),
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

        // Get current scheduled state and target
        let (scheduled, default_target_temp) = {
            let sets = state.schedule.read().unwrap();
            let entry = sets.active().get_active_entry(&now);
            let scheduled = Scheduled {
                state: entry
                    .map(|e| e.heating_state.clone())
                    .unwrap_or(HeatingState::Off),
                target_temp: entry.and_then(|e| e.target_temp),
            };
            (scheduled, sets.default_target_temp)
        };

        // Clone entities to avoid holding lock across await points
        let mut entities_clone = {
            let climates = state.climate_entities.read().unwrap();
            climates.clone()
        };

        // Process entities outside the lock
        for entity in entities_clone.iter_mut() {
            let last_sent = last_sent_targets
                .entry(entity.get_entity_id().to_string())
                .or_default();
            apply_schedule_to_entity(
                entity,
                &scheduled,
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
        // HA reports heat once turned on; the mock doesn't change its own mode
        if scheduled.state == HeatingState::On {
            entity.info.as_mut().unwrap().state = HeatingState::On;
        }
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
        entity.info.as_mut().unwrap().state = HeatingState::Off;
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
}
