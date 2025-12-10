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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use chrono::Utc;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};

use crate::{
    components::*,
    debug::DebugModeConfig,
    execution::*,
    traits::{InverterDataSource, PriceDataSource},
};
use std::time::Instant;

/// Wrapper resource for the price data source
#[derive(Resource)]
pub struct PriceDataSourceResource(pub Arc<dyn PriceDataSource>);

/// Wrapper resource for the inverter data source
#[derive(Resource)]
pub struct InverterDataSourceResource(pub Arc<dyn InverterDataSource>);

/// Resource to track last battery SOC history fetch time
#[derive(Resource)]
pub struct BatteryHistoryFetchTimer {
    pub last_fetch: Instant,
    pub interval_secs: u64,
}

impl Default for BatteryHistoryFetchTimer {
    fn default() -> Self {
        Self {
            last_fetch: Instant::now(),
            interval_secs: 5 * 60, // Fetch history every 5 minutes
        }
    }
}

/// System that executes scheduled mode changes
/// Runs every update cycle to check if mode changes are needed
///
/// NOTE: SOC check temporarily removed to avoid blocking.
/// Future: Implement battery SOC validation via channels or ECS components before mode changes.
/// This would prevent mode changes when battery status is unavailable or out-of-date.
pub fn schedule_execution_system(
    schedule_query: Query<&OperationSchedule>,
    async_writer: Res<crate::resources::AsyncInverterWriter>,
    mut current_mode_query: Query<(&mut CurrentMode, &Inverter, Option<&BatteryStatus>)>,
    config: Res<ExecutionConfig>,
    debug: Res<DebugModeConfig>,
    system_config: Res<crate::resources::SystemConfig>,
) {
    let now = Utc::now();

    // Get the current schedule
    let schedule = match schedule_query.single() {
        Ok(s) => s,
        Err(_) => {
            trace!("No schedule available yet");
            return;
        }
    };

    // Process each inverter
    for (mut current_mode, inverter, battery_status) in current_mode_query.iter_mut() {
        // Find inverter config
        let inverter_config = system_config.inverters.iter().find(|i| i.id == inverter.id);

        if let Some(inv_cfg) = inverter_config {
            // Check if this inverter should receive commands based on topology
            match &inv_cfg.topology {
                crate::resources::InverterTopology::Slave { master_id } => {
                    trace!(
                        "Skipping slave inverter {} (controlled by {})",
                        inverter.id, master_id
                    );
                    continue;
                }
                crate::resources::InverterTopology::Master { slave_ids } => {
                    trace!(
                        "Processing master inverter {} (controls {} slaves)",
                        inverter.id,
                        slave_ids.len()
                    );
                }
                crate::resources::InverterTopology::Independent => {
                    trace!("Processing independent inverter {}", inverter.id);
                }
            }

            // Get current scheduled mode
            if let Some(scheduled_mode) = schedule.get_current_mode(now) {
                // Check if this scheduled mode applies to this inverter
                if !should_execute_for_inverter(scheduled_mode, &inverter.id) {
                    continue;
                }

                // Check if mode change is needed
                if scheduled_mode.mode != current_mode.mode {
                    // Check minimum interval
                    if !can_change_mode(&current_mode, &config, now) {
                        debug!(
                            "Skipping mode change for {}: too soon since last change",
                            inverter.id
                        );
                        continue;
                    }

                    // Check battery SOC constraints for charge/discharge operations
                    // SAFETY: Require battery status to prevent charging at 100% or discharging at min SOC
                    let Some(battery) = battery_status else {
                        warn!(
                            "Skipping mode change for {}: no battery status available (safety constraint)",
                            inverter.id
                        );
                        continue;
                    };

                    let soc = battery.soc_percent as f32;

                    // Skip force-charge if battery is already at max SOC
                    if scheduled_mode.mode == InverterOperationMode::ForceCharge
                        && soc >= system_config.control_config.max_battery_soc
                    {
                        debug!(
                            "Skipping force-charge for {}: SOC ({:.1}%) >= max ({:.1}%)",
                            inverter.id, soc, system_config.control_config.max_battery_soc
                        );
                        continue;
                    }

                    // Skip force-discharge if battery is at or below min SOC
                    if scheduled_mode.mode == InverterOperationMode::ForceDischarge
                        && soc <= system_config.control_config.min_battery_soc
                    {
                        debug!(
                            "Skipping force-discharge for {}: SOC ({:.1}%) <= min ({:.1}%)",
                            inverter.id, soc, system_config.control_config.min_battery_soc
                        );
                        continue;
                    }

                    // Execute mode change
                    if debug.enabled {
                        info!(
                            "🔧 [DEBUG] Would change {} from {:?} to {:?}: {}",
                            inverter.id,
                            current_mode.mode,
                            scheduled_mode.mode,
                            scheduled_mode.reason
                        );
                        // In debug mode, update the mode immediately for testing
                        current_mode.mode = scheduled_mode.mode;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    } else {
                        // Send command using direct async writer (fire-and-forget)
                        let command = InverterCommand::SetMode(scheduled_mode.mode);

                        info!(
                            "📤 Sending command to change {} to {:?}: {}",
                            inverter.id, scheduled_mode.mode, scheduled_mode.reason
                        );

                        // Use async fire-and-forget for mode changes (don't block the ECS system)
                        async_writer.write_command_async(inverter.id.clone(), command);

                        // Update current mode immediately (optimistic)
                        current_mode.mode = scheduled_mode.mode;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    }
                }
            }
        }
    }
}

/// System for initializing inverter entities on startup
pub fn initialize_inverters_system(
    mut commands: Commands,
    system_config: Res<crate::resources::SystemConfig>,
    query: Query<&Inverter>,
) {
    // Only run once at startup
    if !query.is_empty() {
        return;
    }

    info!("Initializing inverter entities...");

    for inv_config in &system_config.inverters {
        info!(
            "Creating entity for inverter: {} ({})",
            inv_config.id, inv_config.inverter_type
        );

        commands.spawn((
            Inverter {
                id: inv_config.id.clone(),
                inverter_type: inv_config.inverter_type,
            },
            CurrentMode::default(),
            // Add other required components
            PowerGeneration::default(),
            GridPower::default(),
            BatteryStatus::default(),
            InverterStatus::default(),
        ));
    }
}

/// Marker to track if we've fetched initial battery history from HA
#[derive(Resource, Default)]
pub struct BatteryHistoryInitialized(pub bool);

// TODO: Implement system to sync ConsumptionHistory data to WinterAdaptiveStrategy config
// The strategy config is boxed inside AdaptiveSeasonalOptimizer, making direct updates complex.
// Possible approaches:
// 1. Pass ConsumptionHistory as a parameter to strategy evaluation (requires trait changes)
// 2. Store strategy configs in a separate ECS component that can be mutated
// 3. Rebuild the optimizer when consumption history updates (expensive)
//
// For now, the strategy uses fallback consumption estimates (per-block × 96).

/// System to fetch initial battery history from Home Assistant on startup
/// This populates the BatteryHistory with the last 48 hours of data from HA
pub fn fetch_initial_battery_history_system(
    mut initialized: ResMut<BatteryHistoryInitialized>,
    system_config: Res<crate::resources::SystemConfig>,
    _battery_history: Res<BatteryHistory>,
) {
    // Only run once
    if initialized.0 {
        return;
    }

    initialized.0 = true;

    // Get the battery entity from first inverter config
    let battery_entity = system_config.inverters.first().map(|inv| {
        format!(
            "sensor.{}_battery_capacity",
            inv.entity_prefix.replace(".", "_")
        )
    });

    if let Some(entity) = battery_entity {
        info!(
            "📊 Fetching initial battery history from HA for entity: {}",
            entity
        );

        // Future: Implement initial history fetch from Home Assistant
        // Would require:
        // 1. Pass HA client to this system via resource
        // 2. Call HA history API to fetch last 48h of battery SOC data
        // 3. Populate BatteryHistory with the fetched data
        // For now, history is populated as new data arrives (every 15 min)

        debug!("Note: Initial history fetch not yet implemented");
        debug!("History will be populated as new inverter data arrives (every 15 min)");
    }
}

/// System to trigger battery SOC history fetch from HA
/// Runs periodically (every 5 minutes) to fetch and update historical battery SOC data
pub fn trigger_battery_history_fetch_system(
    mut timer: ResMut<BatteryHistoryFetchTimer>,
    system_config: Res<crate::resources::SystemConfig>,
) {
    let now = Instant::now();
    let elapsed = now.duration_since(timer.last_fetch).as_secs();

    // Only fetch if interval has elapsed
    if elapsed < timer.interval_secs {
        return;
    }

    // Update timer
    timer.last_fetch = now;

    // Get the battery entity from first inverter config
    let battery_entity = system_config.inverters.first().map(|inv| {
        format!(
            "sensor.{}_battery_capacity",
            inv.entity_prefix.replace(".", "_")
        )
    });

    if let Some(entity) = battery_entity {
        debug!(
            "Battery history fetch interval elapsed for entity: {}",
            entity
        );
        // Actual fetch will be done via web query or separate channel
    } else {
        debug!("No inverter configured for battery history fetch");
    }
}

/// Plugin to register all continuous operation systems
pub struct ContinuousSystemsPlugin;

impl Plugin for ContinuousSystemsPlugin {
    fn build(&self, app: &mut App) {
        app
            // Initialize battery history resources
            .init_resource::<BatteryHistory>()
            .init_resource::<BatteryHistoryFetchTimer>()
            .init_resource::<BatteryHistoryInitialized>()
            // Initialize PV generation history resources
            .init_resource::<PvHistory>()
            // Initialize consumption history for winter adaptive strategy
            .init_resource::<crate::components::ConsumptionHistory>()
            .add_systems(
                Startup,
                (
                    initialize_inverters_system,
                    crate::async_systems::setup_async_workers,
                    fetch_initial_battery_history_system,
                )
                    .chain(), // Ensure inverters are created before async workers
            )
            .add_systems(Startup, crate::async_systems::setup_price_cache)
            .add_systems(
                Update,
                (
                    crate::async_systems::poll_consumption_history_channel,
                    crate::async_systems::config_event_handler,
                    crate::async_systems::check_health_system,
                    // poll_command_results removed - using direct AsyncInverterWriter
                    crate::async_systems::read_inverter_states_system,
                    // Decompose RawInverterState into individual components
                    crate::async_systems::decompose_inverter_state,
                    // Keep schedule execution but update to use channels
                    schedule_execution_system,
                    // Trigger battery history fetch periodically
                    trigger_battery_history_fetch_system,
                    // Process web queries via message passing (ECS -> Web)
                    crate::web_bridge::web_query_system,
                    // New channel-based systems (non-blocking)
                    crate::async_systems::update_prices_system,
                ),
            );
    }
}
