use crate::api_client::ApiClient;
use crate::climate::{BoostInfo, ClimateEntity};
use crate::schedule::HeatingState;
use crate::ScheduleState;
use chrono::Local;
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
pub fn calculate_heating_action_for_schedule(
    current_state: &HeatingState,
    desired_state: &HeatingState,
) -> HeatingAction {
    match (current_state, desired_state) {
        // If current state matches desired, no change needed
        (HeatingState::On, HeatingState::On) => HeatingAction::NoChange,
        (HeatingState::Off, HeatingState::Off) => HeatingAction::NoChange,

        // If states differ, change to desired state
        (HeatingState::Off, HeatingState::On) => HeatingAction::TurnOn,
        (HeatingState::On, HeatingState::Off) => HeatingAction::TurnOff,
    }
}
pub fn calculate_desired_heating_state_for_boost(
    boost_info: &BoostInfo,
) -> HeatingState {
    if !boost_info.boosted {
        HeatingState::Off
    } else {
        // Validate that current time is inside the boosted time period
        let now = Local::now().time();

        if let (Some(start), Some(end)) = (boost_info.boost_start, boost_info.boost_end) {
            // Check if current time is within boost period
            if now >= start && now <= end {
                return HeatingState::On;
            }
        }

        HeatingState::Off
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

/// Main scheduler loop that runs periodically and applies schedule
pub async fn run_scheduler<T: ClimateEntity + Clone>(state: SchedulerState<T>) {
    let mut interval = interval(Duration::from_secs(15));

    println!("\n=== Heating Scheduler Started ===");

    loop {
        interval.tick().await;

        let now = Local::now();

        // Get current scheduled state
        let desired_state = {
            let schedule = state.schedule.read().unwrap();
            schedule.get_current_state(&now)
        };

        // Clone entities to avoid holding lock across await points
        let mut entities_clone = {
            let climates = state.climate_entities.read().unwrap();
            climates.clone()
        };

        // Process entities outside the lock
        for entity in entities_clone.iter_mut() {
            entity.fetch_and_update_state(&state.api_client).await.unwrap();
            let boosted_state = calculate_desired_heating_state_for_boost(entity.get_boosted_status());
            let final_desired_state = final_desired_heating_state(&desired_state, &boosted_state);

            let heating_state = entity.get_cached_state().clone().unwrap().state;

            let action = calculate_heating_action_for_schedule(&heating_state, &final_desired_state);

            println!(
                "[{}] Action: {:?}",
                now.format("%Y-%m-%d %H:%M:%S"),
                action
            );

            // Only apply changes when action is needed
            if action != HeatingAction::NoChange {
                println!(
                    "  Schedule change: {:?} → {:?}",
                    heating_state,
                    desired_state
                );

                if let Err(e) = apply_heating_action(entity, action.clone(), &state.api_client).await {
                    eprintln!("  ✗ Error applying action: {}", e);
                }
            }
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
}
