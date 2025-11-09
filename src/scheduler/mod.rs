use crate::api_client::ApiClient;
use crate::climate::ClimateEntity;
use crate::schedule::HeatingState;
use crate::ScheduleState;
use chrono::Local;
use std::time::Duration;
use tokio::time::interval;

pub struct SchedulerState {
    pub api_client: ApiClient,
    pub schedule: ScheduleState,
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
    entity: &ClimateEntity,
    action: HeatingAction,
    api_client: &ApiClient,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        HeatingAction::TurnOn => {
            println!("  → Turning ON: {}", entity.entity_id);
            entity.turn_on(api_client).await?;
        }
        HeatingAction::TurnOff => {
            println!("  → Turning OFF: {}", entity.entity_id);
            entity.turn_off(api_client).await?;
        }
        HeatingAction::NoChange => {
            println!("  ✓ No change needed: {}", entity.entity_id);
        }
    }
    Ok(())
}

/// Main scheduler loop that runs periodically and applies schedule
pub async fn run_scheduler(state: SchedulerState) {
    let mut interval = interval(Duration::from_secs(5));
    let mut last_state: Option<HeatingState> = None;

    let mut office_trv = ClimateEntity::new("climate.office_trv".to_string());
    office_trv.get_state(&state.api_client).await.unwrap();


    println!("\n=== Heating Scheduler Started ===");
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
            "{} Action determined {:?}",
            now.format("%Y-%m-%d %H:%M:%S"),
            action
        );

        //todo turning off triggered twice, it retriggered the turning on.. // first check of last state should be a value from an entity
        // Only log and apply changes when action is needed
        if action != HeatingAction::NoChange {
            println!(
                "[{}] Schedule change detected: {:?} → {:?}",
                now.format("%Y-%m-%d %H:%M:%S"),
                last_state,
                desired_state
            );

            // Apply to all climate entities
            // for entity_id in &climate_entities {
            //     let entity = ClimateEntity::new(entity_id.clone());

            if let Err(e) = apply_heating_action(&office_trv, action.clone(), &state.api_client).await
            {
                eprintln!("  ✗ Error applying action to {}: {}", &office_trv.entity_id, e);
            } else {
                // Update last state only if successful
                last_state = Some(desired_state.clone());
            }
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