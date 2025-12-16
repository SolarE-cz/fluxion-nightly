// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided "AS IS", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

use crate::components::*;
use crate::debug::DebugModeConfig;
use crate::debug_execute;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use tracing::{error, info};

/// Configuration for schedule execution
#[derive(Resource, Debug, Clone)]
pub struct ExecutionConfig {
    /// Minimum time between mode changes (seconds) to prevent rapid switching
    pub min_mode_change_interval_secs: u64,
}

impl ExecutionConfig {
    /// Create a new ExecutionConfig with the specified interval
    pub fn new(min_mode_change_interval_secs: u64) -> Self {
        Self {
            min_mode_change_interval_secs,
        }
    }
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            min_mode_change_interval_secs: 60, // Default: 1 minute between changes
        }
    }
}

/// Get the current active mode for the current time block
/// Returns None if no schedule exists or current time is outside scheduled range
pub fn get_current_scheduled_mode(
    schedule: &OperationSchedule,
    now: DateTime<Utc>,
) -> Option<&ScheduledMode> {
    schedule.scheduled_blocks.iter().find(|block| {
        let block_end =
            block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
        now >= block.block_start && now < block_end
    })
}

/// Check if a mode change is needed
/// Returns true if the schedule mode differs from the current mode
pub fn should_change_mode(
    schedule: &OperationSchedule,
    current_mode: &CurrentMode,
    now: DateTime<Utc>,
) -> bool {
    if let Some(scheduled_mode) = get_current_scheduled_mode(schedule, now) {
        scheduled_mode.mode != current_mode.mode
    } else {
        false
    }
}

/// Create a pending command for a mode change
pub fn create_mode_change_command(
    schedule: &OperationSchedule,
    now: DateTime<Utc>,
) -> Option<InverterCommand> {
    get_current_scheduled_mode(schedule, now)
        .map(|scheduled_mode| InverterCommand::SetMode(scheduled_mode.mode))
}

/// Check if enough time has passed since the last mode change
pub fn can_change_mode(
    current_mode: &CurrentMode,
    config: &ExecutionConfig,
    now: DateTime<Utc>,
) -> bool {
    let elapsed = (now - current_mode.set_at).num_seconds();
    elapsed >= config.min_mode_change_interval_secs as i64
}

/// Execute a mode change command
/// This should be called from a system that has access to the data source
/// Returns true if the mode was changed (or simulated in debug mode)
pub fn execute_mode_change(
    current_mode: &mut CurrentMode,
    new_mode: InverterOperationMode,
    reason: String,
    now: DateTime<Utc>,
    debug_mode: &DebugModeConfig,
) -> bool {
    let result = debug_execute!(
        debug_mode,
        &format!(
            "Change mode from {:?} to {:?}: {}",
            current_mode.mode, new_mode, reason
        ),
        {
            // In production, this would actually execute via data source
            // But that happens in the system, not here
            use crate::debug::DebugExecutionResult;
            DebugExecutionResult::Executed
        }
    );

    if result.is_success() {
        current_mode.mode = new_mode;
        current_mode.set_at = now;
        current_mode.reason = reason;
        info!("✅ Mode changed to {:?}", new_mode);
        true
    } else {
        error!("❌ Failed to change mode (debug logged only)");
        false
    }
}

/// Determine if an inverter should receive commands based on topology
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InverterControlTopology {
    /// Inverter is controlled directly by FluxION
    Independent,
    /// Inverter is a master that controls other inverters
    Master { slave_ids: Vec<String> },
    /// Inverter is controlled by a master inverter
    Slave { master_id: String },
}

impl InverterControlTopology {
    /// Check if this inverter should receive direct commands
    pub fn should_receive_commands(&self) -> bool {
        match self {
            InverterControlTopology::Independent | InverterControlTopology::Master { .. } => true,
            InverterControlTopology::Slave { .. } => false,
        }
    }

    /// Check if this is a master inverter
    pub fn is_master(&self) -> bool {
        matches!(self, InverterControlTopology::Master { .. })
    }

    /// Check if this is a slave inverter
    pub fn is_slave(&self) -> bool {
        matches!(self, InverterControlTopology::Slave { .. })
    }
}

/// Filter scheduled mode to check if it targets a specific inverter
pub fn should_execute_for_inverter(scheduled_mode: &ScheduledMode, inverter_id: &str) -> bool {
    match &scheduled_mode.target_inverters {
        None => true, // No target specified = all inverters
        Some(targets) => targets.contains(&inverter_id.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_schedule() -> OperationSchedule {
        let now = Utc::now();
        OperationSchedule {
            scheduled_blocks: vec![
                ScheduledMode {
                    block_start: now - chrono::Duration::minutes(30),
                    duration_minutes: 60,
                    target_inverters: None,
                    mode: InverterOperationMode::ForceCharge,
                    reason: "Test charge".to_string(),
                    debug_info: None,
                },
                ScheduledMode {
                    block_start: now + chrono::Duration::minutes(30),
                    duration_minutes: 60,
                    target_inverters: None,
                    mode: InverterOperationMode::ForceDischarge,
                    reason: "Test discharge".to_string(),
                    debug_info: None,
                },
            ],
            generated_at: now,
            based_on_price_version: now,
        }
    }

    #[test]
    fn test_get_current_scheduled_mode() {
        let schedule = create_test_schedule();
        let now = Utc::now();

        // Should find the first block (current time is within it)
        let current = get_current_scheduled_mode(&schedule, now);
        assert!(current.is_some());
        assert_eq!(current.unwrap().mode, InverterOperationMode::ForceCharge);
    }

    #[test]
    fn test_get_current_scheduled_mode_future() {
        let schedule = create_test_schedule();
        let future_time = Utc::now() + chrono::Duration::minutes(45);

        // Should find the second block
        let current = get_current_scheduled_mode(&schedule, future_time);
        assert!(current.is_some());
        assert_eq!(current.unwrap().mode, InverterOperationMode::ForceDischarge);
    }

    #[test]
    fn test_get_current_scheduled_mode_none() {
        let schedule = create_test_schedule();
        let past_time = Utc::now() - chrono::Duration::hours(2);

        // Should not find any block
        let current = get_current_scheduled_mode(&schedule, past_time);
        assert!(current.is_none());
    }

    #[test]
    fn test_should_change_mode_true() {
        let schedule = create_test_schedule();
        let current_mode = CurrentMode {
            mode: InverterOperationMode::SelfUse,
            set_at: Utc::now(),
            reason: "Test".to_string(),
        };

        let should_change = should_change_mode(&schedule, &current_mode, Utc::now());
        assert!(should_change); // ForceCharge != SelfUse
    }

    #[test]
    fn test_should_change_mode_false() {
        let schedule = create_test_schedule();
        let current_mode = CurrentMode {
            mode: InverterOperationMode::ForceCharge,
            set_at: Utc::now(),
            reason: "Test".to_string(),
        };

        let should_change = should_change_mode(&schedule, &current_mode, Utc::now());
        assert!(!should_change); // ForceCharge == ForceCharge
    }

    #[test]
    fn test_can_change_mode_fresh_start() {
        let now = Utc::now();
        let current_mode = CurrentMode {
            mode: InverterOperationMode::SelfUse,
            set_at: now - chrono::Duration::seconds(120),
            reason: "Test".to_string(),
        };
        let config = ExecutionConfig::default();

        assert!(can_change_mode(&current_mode, &config, now));
    }

    #[test]
    fn test_can_change_mode_too_soon() {
        let now = Utc::now();
        let current_mode = CurrentMode {
            mode: InverterOperationMode::SelfUse,
            set_at: now - chrono::Duration::seconds(30),
            reason: "Test".to_string(),
        };
        let config = ExecutionConfig::default(); // 60 second minimum

        assert!(!can_change_mode(&current_mode, &config, now));
    }

    #[test]
    fn test_can_change_mode_enough_time() {
        let now = Utc::now();
        let current_mode = CurrentMode {
            mode: InverterOperationMode::SelfUse,
            set_at: now - chrono::Duration::seconds(120),
            reason: "Test".to_string(),
        };
        let config = ExecutionConfig::default(); // 60 second minimum

        assert!(can_change_mode(&current_mode, &config, now));
    }

    #[test]
    fn test_inverter_topology_independent() {
        let topology = InverterControlTopology::Independent;
        assert!(topology.should_receive_commands());
        assert!(!topology.is_master());
        assert!(!topology.is_slave());
    }

    #[test]
    fn test_inverter_topology_master() {
        let topology = InverterControlTopology::Master {
            slave_ids: vec!["slave1".to_string(), "slave2".to_string()],
        };
        assert!(topology.should_receive_commands());
        assert!(topology.is_master());
        assert!(!topology.is_slave());
    }

    #[test]
    fn test_inverter_topology_slave() {
        let topology = InverterControlTopology::Slave {
            master_id: "master1".to_string(),
        };
        assert!(!topology.should_receive_commands());
        assert!(!topology.is_master());
        assert!(topology.is_slave());
    }

    #[test]
    fn test_should_execute_for_inverter_all() {
        let scheduled_mode = ScheduledMode {
            block_start: Utc::now(),
            duration_minutes: 60,
            target_inverters: None,
            mode: InverterOperationMode::ForceCharge,
            reason: "Test".to_string(),
            debug_info: None,
        };

        assert!(should_execute_for_inverter(&scheduled_mode, "inv1"));
        assert!(should_execute_for_inverter(&scheduled_mode, "inv2"));
    }

    #[test]
    fn test_should_execute_for_inverter_targeted() {
        let scheduled_mode = ScheduledMode {
            block_start: Utc::now(),
            duration_minutes: 60,
            target_inverters: Some(vec!["inv1".to_string(), "inv2".to_string()]),
            mode: InverterOperationMode::ForceCharge,
            reason: "Test".to_string(),
            debug_info: None,
        };

        assert!(should_execute_for_inverter(&scheduled_mode, "inv1"));
        assert!(should_execute_for_inverter(&scheduled_mode, "inv2"));
        assert!(!should_execute_for_inverter(&scheduled_mode, "inv3"));
    }

    #[test]
    fn test_create_mode_change_command() {
        let schedule = create_test_schedule();
        let command = create_mode_change_command(&schedule, Utc::now());

        assert!(command.is_some());
        match command.unwrap() {
            InverterCommand::SetMode(mode) => {
                assert_eq!(mode, InverterOperationMode::ForceCharge);
            }
            _ => panic!("Expected SetMode command"),
        }
    }
}
