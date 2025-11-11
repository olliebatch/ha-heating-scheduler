use crate::api_client::ApiClient;
use crate::climate::ClimateEntity;
use crate::schedule::HeatingState;
use crate::ScheduleState;
use chrono::Local;
use std::time::Duration;
use tokio::time::interval;

pub struct SchedulerState<T: ClimateEntity> {
    pub api_client: ApiClient,
    pub schedule: ScheduleState,
    pub climate_entity: T,
}

/// Represents an action to be taken on a climate entity
#[derive(Debug, Clone, PartialEq)]
pub enum HeatingAction {
    TurnOn,
    TurnOff,
    NoChange,
}

/// Calculate what heating action should be taken based on the schedule
pub fn calculate_heating_action(
    current_state: Option<&HeatingState>,
    desired_state: &HeatingState,
) -> HeatingAction {
    match (current_state, desired_state) {
        // If we don't know current state, apply desired state
        (None, HeatingState::On) => HeatingAction::TurnOn,
        (None, HeatingState::Off) => HeatingAction::TurnOff,

        // If current state matches desired, no change needed
        (Some(HeatingState::On), HeatingState::On) => HeatingAction::NoChange,
        (Some(HeatingState::Off), HeatingState::Off) => HeatingAction::NoChange,

        // If states differ, change to desired state
        (Some(HeatingState::Off), HeatingState::On) => HeatingAction::TurnOn,
        (Some(HeatingState::On), HeatingState::Off) => HeatingAction::TurnOff,
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
pub async fn run_scheduler<T: ClimateEntity>(mut state: SchedulerState<T>) {
    let mut interval = interval(Duration::from_secs(5));

    // Fetch initial state from the entity
    println!("\n=== Heating Scheduler Started ===");
    println!("Fetching initial state for {}...", state.climate_entity.get_entity_id());

    if let Err(e) = state.climate_entity.fetch_and_update_state(&state.api_client).await {
        eprintln!("Warning: Failed to fetch initial state: {}", e);
    }

    let mut last_state = state
        .climate_entity
        .get_cached_state()
        .as_ref()
        .map(|info| info.state.clone());

    println!("Initial state: {:?}", last_state);
    println!("Checking schedule every 5 seconds...\n");

    loop {
        interval.tick().await;

        let now = Local::now();

        // Get current scheduled state
        let desired_state = {
            let schedule = state.schedule.read().unwrap();
            schedule.get_current_state(&now)
        };

        // Calculate what action to take
        let action = calculate_heating_action(last_state.as_ref(), &desired_state);

        println!(
            "[{}] Action: {:?}",
            now.format("%Y-%m-%d %H:%M:%S"),
            action
        );

        // Only apply changes when action is needed
        if action != HeatingAction::NoChange {
            println!(
                "  Schedule change: {:?} → {:?}",
                last_state,
                desired_state
            );

            if let Err(e) = apply_heating_action(&state.climate_entity, action.clone(), &state.api_client).await {
                eprintln!("  ✗ Error applying action: {}", e);
            } else {
                // Update last state only if successful
                last_state = Some(desired_state.clone());
            }

            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_heating_action_initial_state() {
        // When we don't know the current state, apply the desired state
        assert_eq!(
            calculate_heating_action(None, &HeatingState::On),
            HeatingAction::TurnOn
        );
        assert_eq!(
            calculate_heating_action(None, &HeatingState::Off),
            HeatingAction::TurnOff
        );
    }

    #[test]
    fn test_calculate_heating_action_no_change() {
        // When current matches desired, no change needed
        assert_eq!(
            calculate_heating_action(Some(&HeatingState::On), &HeatingState::On),
            HeatingAction::NoChange
        );
        assert_eq!(
            calculate_heating_action(Some(&HeatingState::Off), &HeatingState::Off),
            HeatingAction::NoChange
        );
    }

    #[test]
    fn test_calculate_heating_action_state_change() {
        // When states differ, change to desired
        assert_eq!(
            calculate_heating_action(Some(&HeatingState::Off), &HeatingState::On),
            HeatingAction::TurnOn
        );
        assert_eq!(
            calculate_heating_action(Some(&HeatingState::On), &HeatingState::Off),
            HeatingAction::TurnOff
        );
    }
}
