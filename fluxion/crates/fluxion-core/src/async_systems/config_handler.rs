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

use bevy_ecs::prelude::*;
use tracing::{error, info, trace};

use crate::{
    components::*,
    config_events::ConfigSection,
    debug::DebugModeConfig,
    pricing::analyze_prices,
    resources::SystemConfig,
    scheduling::{ScheduleConfig, generate_schedule_with_optimizer},
    web_bridge::ConfigUpdateChannel,
};

use super::BackupDischargeMinSoc;

/// Generates consumption forecast for scheduling using priority-based data sources
/// Priority: 1) Historical data (EMA), 2) Current consumption, 3) Configured fallback
fn generate_consumption_forecast(
    consumption_history: &ConsumptionHistory,
    current_inverter_raw_state: Option<&RawInverterState>,
    control_config: &fluxion_types::config::ControlConfig,
    num_blocks: usize,
) -> Option<Vec<f32>> {
    // Calculate daily consumption estimate
    let daily_consumption_kwh = if !consumption_history.summaries().is_empty() {
        // Priority 1: Use historical EMA data (most accurate)
        let history: Vec<f32> = consumption_history.consumption_values();

        if let Some(ema) = calculate_ema(&history, 7) {
            trace!("📊 Using historical EMA consumption: {:.2} kWh/day", ema);
            ema
        } else {
            control_config.average_household_load_kw * 24.0
        }
    } else if let Some(raw_state) = current_inverter_raw_state {
        // Priority 2: Use current consumption data (extrapolated)
        if let Some(current_load_w) = raw_state.state.house_load_w {
            let current_load_kw = current_load_w / 1000.0;
            // Apply dampening factor since instantaneous readings can be high
            let estimated_daily = current_load_kw * 24.0 * 0.9;
            trace!(
                "📊 Using current consumption: {:.0}W → {:.2} kWh/day",
                current_load_w, estimated_daily
            );
            estimated_daily
        } else {
            control_config.average_household_load_kw * 24.0
        }
    } else {
        // Priority 3: Use configured fallback
        trace!(
            "📊 Using fallback consumption: {:.2} kWh/day",
            control_config.average_household_load_kw * 24.0
        );
        control_config.average_household_load_kw * 24.0
    };

    // Convert daily to per-block (15-minute blocks)
    let consumption_per_block_kwh = daily_consumption_kwh / 96.0; // 96 blocks per day

    // Generate forecast array
    let forecast = vec![consumption_per_block_kwh; num_blocks];

    trace!(
        "📊 Generated consumption forecast: {:.3} kWh per 15-min block for {} blocks",
        consumption_per_block_kwh, num_blocks
    );

    Some(forecast)
}

/// Helper function to calculate Exponential Moving Average (from fluxion-strategy crate)
fn calculate_ema(values: &[f32], period: usize) -> Option<f32> {
    if values.is_empty() || period == 0 {
        return None;
    }

    let alpha = 2.0 / (period as f32 + 1.0);
    let mut ema = values[0];

    for &value in values.iter().skip(1) {
        ema = alpha * value + (1.0 - alpha) * ema;
    }

    Some(ema)
}

/// System parameters for config_event_handler
#[derive(bevy_ecs::system::SystemParam)]
pub struct ConfigEventParams<'w, 's> {
    config_channel: ResMut<'w, ConfigUpdateChannel>,
    system_config: ResMut<'w, SystemConfig>,
    debug_config: ResMut<'w, DebugModeConfig>,
    schedule_query: Query<'w, 's, &'static mut OperationSchedule>,
    price_data_query: Query<'w, 's, &'static SpotPriceData>,
    _battery_query: Query<'w, 's, &'static BatteryStatus>,
    commands: Commands<'w, 's>,
    backup_soc: Option<Res<'w, BackupDischargeMinSoc>>,
    consumption_history: Res<'w, crate::components::ConsumptionHistory>,
    inverter_raw_state_query: Query<'w, 's, &'static RawInverterState>,
}

/// System that processes config update events from the web UI
/// Updates SystemConfig and triggers schedule recalculation when needed
pub fn config_event_handler(mut params: ConfigEventParams) {
    // Process all pending config update events
    while let Ok(event) = params.config_channel.receiver.try_recv() {
        info!(
            "🔄 Processing config update event with {} changed sections",
            event.changed_sections.len()
        );

        // Merge the partial config update with the existing config
        // First, serialize the current config to JSON
        let mut current_config_json = match serde_json::to_value(&*params.system_config) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize current config: {e}");
                continue;
            }
        };

        // Merge the incoming changes
        if let (Some(current_obj), Some(new_obj)) = (
            current_config_json.as_object_mut(),
            event.new_config.as_object(),
        ) {
            for (key, value) in new_obj {
                current_obj.insert(key.clone(), value.clone());
            }
        }

        // Try to deserialize the merged config
        let new_config: SystemConfig = match serde_json::from_value(current_config_json.clone()) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to deserialize merged config update: {e}");
                // Debug: print the problematic JSON on error
                if let Ok(json_str) = serde_json::to_string_pretty(&current_config_json) {
                    let truncated = if json_str.len() > 2000 {
                        &json_str[..2000]
                    } else {
                        &json_str
                    };
                    error!("Problematic JSON: {truncated}");
                }
                continue;
            }
        };

        // Store old config for comparison logging
        let old_config = params.system_config.clone();
        *params.system_config = new_config;

        // Sync DebugModeConfig resource with SystemConfig.system_config.debug_mode
        if old_config.system_config.debug_mode != params.system_config.system_config.debug_mode {
            params.debug_config.enabled = params.system_config.system_config.debug_mode;
            if params.system_config.system_config.debug_mode {
                info!("🔍 Debug mode ENABLED - system will log actions but not execute them");
            } else {
                info!("⚠️ Debug mode DISABLED - system will execute REAL commands!");
                DebugModeConfig::warn_production_mode();
            }
        }

        info!("✅ SystemConfig updated from web UI");

        // Log which sections changed
        for section in &event.changed_sections {
            match section {
                ConfigSection::System => info!("  - System configuration updated"),
                ConfigSection::Inverters => info!("  - Inverter configuration updated"),
                ConfigSection::Pricing => info!("  - Pricing configuration updated"),
                ConfigSection::Control => info!("  - Control parameters updated"),
                ConfigSection::Strategies => info!("  - Strategy configuration updated"),
            }
        }

        // Check if we need to recalculate schedule
        let needs_schedule_recalc = event.section_changed(ConfigSection::Control)
            || event.section_changed(ConfigSection::Pricing)
            || event.section_changed(ConfigSection::Strategies);

        if needs_schedule_recalc {
            info!("🔄 Triggering schedule recalculation due to config changes");

            // Get current price data
            let Some(price_data) = params.price_data_query.single().ok() else {
                info!("⚠️ No price data available, skipping schedule recalculation");
                continue;
            };

            // Regenerate price analysis
            let _analysis = analyze_prices(
                &price_data.time_block_prices,
                params.system_config.control_config.force_charge_hours,
                params.system_config.control_config.force_discharge_hours,
                params.system_config.pricing_config.use_spot_prices_to_buy,
                params.system_config.pricing_config.use_spot_prices_to_sell,
            );

            // Create schedule config
            let schedule_config = ScheduleConfig {
                min_battery_soc: params.system_config.control_config.min_battery_soc,
                max_battery_soc: params.system_config.control_config.max_battery_soc,
                target_inverters: params
                    .system_config
                    .inverters
                    .iter()
                    .map(|i| i.id.clone())
                    .collect(),
                display_currency: params.system_config.system_config.display_currency,
                default_battery_mode: params.system_config.control_config.default_battery_mode,
            };

            // Get current battery SOC from raw inverter state (more reliable than BatteryStatus component)
            let current_soc = params
                .inverter_raw_state_query
                .iter()
                .map(|raw| raw.state.battery_soc)
                .sum::<f32>()
                / params.inverter_raw_state_query.iter().count().max(1) as f32;
            let current_soc = if current_soc > 0.0 { current_soc } else { 50.0 };

            // Get backup_discharge_min_soc from HA sensor (via BackupDischargeMinSoc resource)
            let backup_discharge_min_soc = params
                .backup_soc
                .as_ref()
                .map(|s| s.value)
                .unwrap_or(params.system_config.control_config.hardware_min_battery_soc);

            // Generate consumption forecast from available data
            let consumption_forecast = generate_consumption_forecast(
                &params.consumption_history,
                params.inverter_raw_state_query.iter().next(),
                &params.system_config.control_config,
                price_data.time_block_prices.len(),
            );

            // Get today's grid import energy from inverter state (sensor.<prefix>_today_s_import_energy)
            // Fallback to consumption history if sensor is unavailable
            let grid_import_today_kwh = params
                .inverter_raw_state_query
                .iter()
                .next()
                .and_then(|raw_state| raw_state.state.grid_import_today_kwh)
                .or_else(|| {
                    // Fallback: Get today's consumption from history (most recent day)
                    params
                        .consumption_history
                        .summaries()
                        .front()
                        .filter(|s| {
                            // Only use if it's actually today's data (within last 24 hours)
                            let age = chrono::Utc::now().signed_duration_since(s.date);
                            age < chrono::Duration::hours(24)
                        })
                        .map(|s| s.grid_import_kwh)
                });

            // Generate new schedule with updated config
            let new_schedule = generate_schedule_with_optimizer(
                &price_data.time_block_prices,
                &params.system_config.control_config,
                &schedule_config,
                current_soc,
                None,                            // Future: Solar forecast
                consumption_forecast.as_deref(), // Enhanced consumption forecast
                Some(&params.system_config.strategies_config),
                backup_discharge_min_soc,
                grid_import_today_kwh,
            );

            // Update schedule
            if let Ok(mut schedule) = params.schedule_query.single_mut() {
                *schedule = new_schedule;
                info!("✅ Schedule recalculated with new configuration");
            } else {
                params.commands.spawn(new_schedule);
                info!("✅ Initial schedule created with new configuration");
            }

            // Log significant parameter changes
            if old_config.control_config.min_battery_soc
                != params.system_config.control_config.min_battery_soc
                || old_config.control_config.max_battery_soc
                    != params.system_config.control_config.max_battery_soc
            {
                info!(
                    "  Battery SOC limits: {}%-{}% (was {}%-{}%)",
                    params.system_config.control_config.min_battery_soc,
                    params.system_config.control_config.max_battery_soc,
                    old_config.control_config.min_battery_soc,
                    old_config.control_config.max_battery_soc
                );
            }

            if old_config.control_config.force_charge_hours
                != params.system_config.control_config.force_charge_hours
                || old_config.control_config.force_discharge_hours
                    != params.system_config.control_config.force_discharge_hours
            {
                info!(
                    "  Force hours: charge={}, discharge={} (was charge={}, discharge={})",
                    params.system_config.control_config.force_charge_hours,
                    params.system_config.control_config.force_discharge_hours,
                    old_config.control_config.force_charge_hours,
                    old_config.control_config.force_discharge_hours
                );
            }
        }
    }
}
