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

/// Resource to track if initial mode sync has been completed for each inverter
/// This allows bypassing the debounce on the first mode change after startup
#[derive(Resource, Default)]
pub struct InitialModeSyncTracker {
    /// Set of inverter IDs that have completed initial sync
    pub synced_inverters: std::collections::HashSet<String>,
}

/// System that executes scheduled mode changes
/// Runs every update cycle to check if mode changes are needed
///
/// On initial startup, compares against ACTUAL inverter mode and applies immediately.
/// After initial sync, uses debounce interval between mode changes.
#[expect(clippy::too_many_arguments)]
pub fn schedule_execution_system(
    schedule_query: Query<&OperationSchedule>,
    async_writer: Res<crate::resources::AsyncInverterWriter>,
    mut current_mode_query: Query<(
        &mut CurrentMode,
        &Inverter,
        Option<&BatteryStatus>,
        Option<&RawInverterState>,
    )>,
    config: Res<ExecutionConfig>,
    debug: Res<DebugModeConfig>,
    system_config: Res<crate::resources::SystemConfig>,
    mut sync_tracker: ResMut<InitialModeSyncTracker>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
) {
    let now = Utc::now();

    // Check if FluxION is disabled by user
    // When disabled: set all inverters to SelfUse and stop sending mode commands
    if let Some(ref uc) = user_control
        && !uc.state.enabled
    {
        for (mut current_mode, inverter, _, _) in current_mode_query.iter_mut() {
            if current_mode.mode != InverterOperationMode::SelfUse {
                if debug.enabled {
                    info!(
                        "🔧 [DEBUG] FluxION disabled - would set {} to SelfUse (currently {:?})",
                        inverter.id, current_mode.mode
                    );
                } else {
                    info!(
                        "⏸️ FluxION disabled by user - setting {} to SelfUse",
                        inverter.id
                    );
                    let command = InverterCommand::SetMode(InverterOperationMode::SelfUse);
                    async_writer.write_command_async(inverter.id.clone(), command);
                }
                current_mode.mode = InverterOperationMode::SelfUse;
                current_mode.set_at = now;
                current_mode.reason = "FluxION disabled by user".to_string();
            }
        }
        return; // Stop - no more mode commands while disabled
    }

    // Get the current schedule
    let schedule = match schedule_query.single() {
        Ok(s) => s,
        Err(_) => {
            trace!("No schedule available yet");
            return;
        }
    };

    // Process each inverter
    for (mut current_mode, inverter, battery_status, raw_state) in current_mode_query.iter_mut() {
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
                // Clone the scheduled mode so we can potentially modify it
                let mut effective_mode = scheduled_mode.clone();

                // Check if user has a fixed slot override active RIGHT NOW
                // This ensures newly created slots take effect immediately
                if let Some(ref uc) = user_control
                    && let Some(fixed_slot) = uc.state.get_fixed_slot_at(now)
                    && effective_mode.mode != fixed_slot.mode
                {
                    info!(
                        "🔒 Fixed slot override active: {} -> {:?} (slot: {})",
                        effective_mode.mode, fixed_slot.mode, fixed_slot.id
                    );
                    effective_mode.mode = fixed_slot.mode;
                    effective_mode.reason = format!(
                        "Fixed slot: {}",
                        fixed_slot.note.as_deref().unwrap_or("User override")
                    );
                }

                // Check if this scheduled mode applies to this inverter
                if !should_execute_for_inverter(&effective_mode, &inverter.id) {
                    continue;
                }

                // Use effective_mode (with potential fixed slot override) for the rest
                let scheduled_mode = &effective_mode;

                // Check if this is the initial sync for this inverter
                let is_initial_sync = !sync_tracker.synced_inverters.contains(&inverter.id);

                // Get actual inverter mode from hardware (if available)
                let actual_mode = raw_state.map(|r| r.state.work_mode);

                // Determine which mode to compare against:
                // - On initial sync: use actual inverter mode (if available) to detect mismatch
                // - After initial sync: use CurrentMode (our internal tracking)
                let effective_current_mode = if is_initial_sync {
                    actual_mode.unwrap_or(current_mode.mode)
                } else {
                    current_mode.mode
                };

                // Check if mode change is needed
                if scheduled_mode.mode != effective_current_mode {
                    // On initial sync: bypass debounce to apply plan immediately
                    // After initial sync: check minimum interval
                    if !is_initial_sync && !can_change_mode(&current_mode, &config, now) {
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
                        // Mark as synced even if skipped due to SOC (schedule is correct, just not needed)
                        if is_initial_sync {
                            sync_tracker.synced_inverters.insert(inverter.id.clone());
                            info!(
                                "🔄 Initial sync for {}: skipped (SOC at max), mode already aligned",
                                inverter.id
                            );
                        }
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
                        // Mark as synced even if skipped due to SOC
                        if is_initial_sync {
                            sync_tracker.synced_inverters.insert(inverter.id.clone());
                            info!(
                                "🔄 Initial sync for {}: skipped (SOC at min), mode already aligned",
                                inverter.id
                            );
                        }
                        continue;
                    }

                    // Execute mode change
                    if debug.enabled {
                        info!(
                            "🔧 [DEBUG] Would change {} from {:?} to {:?}: {}{}",
                            inverter.id,
                            effective_current_mode,
                            scheduled_mode.mode,
                            scheduled_mode.reason,
                            if is_initial_sync {
                                " (initial sync)"
                            } else {
                                ""
                            }
                        );
                        // In debug mode, update the mode immediately for testing
                        current_mode.mode = scheduled_mode.mode;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    } else {
                        // Send command using direct async writer (fire-and-forget)
                        let command = InverterCommand::SetMode(scheduled_mode.mode);

                        info!(
                            "📤 Sending command to change {} to {:?}: {}{}",
                            inverter.id,
                            scheduled_mode.mode,
                            scheduled_mode.reason,
                            if is_initial_sync {
                                " (initial sync - bypassing debounce)"
                            } else {
                                ""
                            }
                        );

                        // Use async fire-and-forget for mode changes (don't block the ECS system)
                        async_writer.write_command_async(inverter.id.clone(), command);

                        // Update current mode immediately (optimistic)
                        current_mode.mode = scheduled_mode.mode;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    }

                    // Mark inverter as synced after first mode change
                    if is_initial_sync {
                        sync_tracker.synced_inverters.insert(inverter.id.clone());
                        info!("🔄 Initial sync completed for {}", inverter.id);
                    }
                } else {
                    // Mode matches schedule - mark as synced
                    if is_initial_sync {
                        sync_tracker.synced_inverters.insert(inverter.id.clone());
                        info!(
                            "🔄 Initial sync for {}: mode already matches schedule ({:?})",
                            inverter.id, scheduled_mode.mode
                        );
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

/// Keep the scheduler's HDO cache in sync with the HA HDO sensor and timezone.
/// Runs before `update_prices_system`, so prices are computed with the current tariff.
pub fn sync_hdo_cache_system(
    hdo_data: Option<Res<crate::async_systems::HdoScheduleData>>,
    hdo_cache: Option<Res<crate::resources::GlobalHdoCache>>,
    timezone: Option<Res<crate::resources::TimezoneConfig>>,
) {
    let (Some(hdo), Some(cache)) = (hdo_data, hdo_cache) else {
        return;
    };
    let timezone_changed = timezone.as_ref().is_some_and(|tz| tz.is_changed());
    if !hdo.is_changed() && !timezone_changed {
        return;
    }

    if let Some(tz) = timezone.as_ref().and_then(|tz| tz.tz) {
        cache.set_timezone(tz);
    }

    if hdo.is_changed()
        && let Some(raw) = hdo.raw_data.as_deref()
    {
        let schedules = crate::strategy::pricing::parse_hdo_sensor_data(raw);
        if !schedules.is_empty() {
            debug!("HDO cache updated with {} day schedules", schedules.len());
            cache.update(schedules);
        }
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
            // Initialize mode sync tracker for initial sync bypass
            .init_resource::<InitialModeSyncTracker>()
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
                    crate::async_systems::user_control_event_handler,
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
                    sync_hdo_cache_system.before(crate::async_systems::update_prices_system),
                    crate::async_systems::update_prices_system,
                ),
            );
    }
}

#[cfg(test)]
mod hdo_cache_sync_tests {
    use super::*;
    use crate::async_systems::HdoScheduleData;
    use crate::resources::{GlobalHdoCache, TimezoneConfig};
    use crate::scheduling::calculate_effective_prices;
    use chrono::{TimeZone, Utc};
    use fluxion_types::config::{PriceSchedule, PricingConfig};
    use fluxion_types::pricing::TimeBlockPrice;

    fn pricing() -> PricingConfig {
        PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: PriceSchedule::Flat(0.0),
            fixed_sell_price_czk: PriceSchedule::Flat(0.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        }
    }

    fn block(utc_hour: u32, spot: f32) -> TimeBlockPrice {
        TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 14, utc_hour, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: spot,
            effective_price_czk_per_kwh: spot,
            spot_sell_price_czk_per_kwh: None,
        }
    }

    #[test]
    fn test_scheduler_effective_price_uses_hdo_schedule_data() {
        let raw = r#"{"data": {"signals": [{"signal": "EVV1", "datum": "14.01.2026",
            "casy": "00:00-06:00; 07:00-09:00"}]}}"#;

        let mut world = World::new();
        world.insert_resource(HdoScheduleData {
            raw_data: Some(raw.to_string()),
            ..Default::default()
        });
        world.insert_resource(GlobalHdoCache::default());
        world.insert_resource(TimezoneConfig::new(Some("Europe/Prague".to_string())));

        let mut schedule = Schedule::default();
        schedule.add_systems(sync_hdo_cache_system);
        schedule.run(&mut world);

        let cache = world.resource::<GlobalHdoCache>();
        // 02:00 UTC = 03:00 Prague (low window), 09:00 UTC = 10:00 Prague (high)
        let mut blocks = vec![block(2, 2.0), block(9, 2.0)];
        calculate_effective_prices(&mut blocks, Some(cache), &pricing());

        // spot + low/high HDO fee + operator buy fee
        assert!((blocks[0].effective_price_czk_per_kwh - 3.00).abs() < 0.001);
        assert!((blocks[1].effective_price_czk_per_kwh - 4.30).abs() < 0.001);
    }
}
