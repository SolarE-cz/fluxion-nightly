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
// futures_timer::Delay no longer needed - removed channel polling
// std::time::Duration removed - no longer needed for Delay
use tracing::{debug, error, info, trace};

use crate::{
    PluginManagerResource, PriceDataSourceResource,
    components::*,
    pricing::analyze_prices,
    resources::SystemConfig,
    scheduling::{ScheduleConfig, generate_schedule_with_optimizer},
};
use fluxion_types::config::ControlConfig;

use super::BackupDischargeMinSoc;

/// Generates consumption forecast for scheduling using priority-based data sources
/// Priority: 1) Historical data (EMA), 2) Current consumption, 3) Configured fallback
fn generate_consumption_forecast(
    consumption_history: &ConsumptionHistory,
    current_inverter_raw_state: Option<&RawInverterState>,
    control_config: &ControlConfig,
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

/// Initialize price cache resource with configured data source
/// Replaces the complex channel spawning with a simple resource
pub fn setup_price_cache(mut commands: Commands, price_source: Res<PriceDataSourceResource>) {
    let price_cache = crate::resources::PriceCache::new(
        price_source.0.clone(),
        3, // 5 minutes fetch interval (was 5 seconds in the old system)
    );

    commands.insert_resource(price_cache);
    info!("✅ Price cache initialized with 5-minute fetch interval");
}

/// Simplified system that updates prices using direct cache access
/// Replaces the complex channel polling with on-demand fetching
#[allow(clippy::too_many_arguments)]
pub fn update_prices_system(
    price_cache: Res<crate::resources::PriceCache>,
    mut commands: Commands,
    mut price_data_query: Query<(Entity, &mut SpotPriceData)>,
    mut price_analysis_query: Query<(Entity, &mut PriceAnalysis)>,
    mut schedule_query: Query<&mut OperationSchedule>,
    _battery_query: Query<&BatteryStatus>,
    config: Res<SystemConfig>,
    backup_soc: Option<Res<BackupDischargeMinSoc>>,
    consumption_history: Res<crate::components::ConsumptionHistory>,
    inverter_raw_state_query: Query<&RawInverterState>,
    plugin_manager_res: Res<PluginManagerResource>,
) {
    // Only fetch if cache is stale (non-blocking check)
    if !price_cache.is_stale() {
        return;
    }

    debug!("💰 Checking for updated price data...");

    // Fetch prices using the cache (will return cached data if fresh)
    let new_prices = match price_cache.get_or_fetch() {
        Ok(prices) => prices,
        Err(e) => {
            error!("❌ Failed to fetch prices: {}", e);
            return;
        }
    };

    let new_block_count = new_prices.time_block_prices.len();
    let new_hours = new_block_count as f32 / 4.0;

    // Detect if we got significantly more data (day-ahead prices arrived)
    let old_block_count = price_data_query
        .single()
        .ok()
        .map(|(_, data)| data.time_block_prices.len())
        .unwrap_or(0);

    let is_day_ahead_arrival = new_block_count > old_block_count + 10;

    if is_day_ahead_arrival {
        info!(
            "📊 Day-ahead prices arrived! Old: {} blocks ({:.1}h), New: {} blocks ({:.1}h). Will recalculate schedule.",
            old_block_count,
            old_block_count as f32 / 4.0,
            new_block_count,
            new_hours
        );
    } else {
        debug!(
            "📊 Received price data update: {} blocks ({:.1} hours)",
            new_block_count, new_hours
        );
    }

    // Update price data entity or create if doesn't exist
    if let Ok((_, mut price_data)) = price_data_query.single_mut() {
        *price_data = new_prices.clone();
    } else {
        commands.spawn(new_prices.clone());
    }

    debug!(
        "🔄 Regenerating schedule due to: {}",
        if is_day_ahead_arrival {
            "day-ahead prices arrival"
        } else {
            "price data update"
        }
    );

    // Regenerate schedule based on new prices
    let analysis = analyze_prices(
        &new_prices.time_block_prices,
        config.control_config.force_charge_hours,
        config.control_config.force_discharge_hours,
        config.pricing_config.use_spot_prices_to_buy,
        config.pricing_config.use_spot_prices_to_sell,
        config.control_config.min_consecutive_force_blocks,
    );

    let schedule_config = ScheduleConfig {
        min_battery_soc: config.control_config.min_battery_soc,
        max_battery_soc: config.control_config.max_battery_soc,
        target_inverters: config.inverters.iter().map(|i| i.id.clone()).collect(),
        display_currency: config.system_config.display_currency,
        default_battery_mode: config.control_config.default_battery_mode,
    };

    // Skip scheduling if no inverter state is available yet (startup race condition)
    if inverter_raw_state_query.iter().count() == 0 {
        debug!("⏳ Skipping schedule generation - waiting for inverter state data");
        return;
    }

    // Get current battery SOC from raw inverter state (more reliable than BatteryStatus component)
    let current_soc = inverter_raw_state_query
        .iter()
        .map(|raw| raw.state.battery_soc)
        .sum::<f32>()
        / inverter_raw_state_query.iter().count().max(1) as f32;

    // Get backup_discharge_min_soc from HA sensor (via BackupDischargeMinSoc resource)
    let backup_discharge_min_soc = backup_soc
        .as_ref()
        .map(|s| s.value)
        .unwrap_or(config.control_config.hardware_min_battery_soc);

    // Generate consumption forecast from available data
    let consumption_forecast = generate_consumption_forecast(
        &consumption_history,
        inverter_raw_state_query.iter().next(),
        &config.control_config,
        new_prices.time_block_prices.len(),
    );

    // Get today's grid import energy from inverter state (sensor.<prefix>_today_s_import_energy)
    // Fallback to consumption history if sensor is unavailable
    let grid_import_today_kwh = inverter_raw_state_query
        .iter()
        .next()
        .and_then(|raw_state| raw_state.state.grid_import_today_kwh)
        .or_else(|| {
            // Fallback: Get today's consumption from history (most recent day)
            consumption_history
                .summaries()
                .front()
                .filter(|s| {
                    // Only use if it's actually today's data (within last 24 hours)
                    let age = chrono::Utc::now().signed_duration_since(s.date);
                    age < chrono::Duration::hours(24)
                })
                .map(|s| s.grid_import_kwh)
        });

    // Use economic optimizer for schedule generation with shared plugin manager
    let plugin_manager = plugin_manager_res.0.read();
    let new_schedule = generate_schedule_with_optimizer(
        &new_prices.time_block_prices,
        &config.control_config,
        &schedule_config,
        current_soc,
        None, // Future: Add solar forecast integration (Solcast/Forecast.Solar API)
        consumption_forecast.as_deref(), // Enhanced consumption forecast
        backup_discharge_min_soc,
        grid_import_today_kwh,
        &plugin_manager,
    );

    // Update or create PriceAnalysis entity
    if let Ok((_, mut price_analysis)) = price_analysis_query.single_mut() {
        *price_analysis = analysis;
    } else {
        commands.spawn(analysis);
    }

    // Update schedule or create if doesn't exist
    if let Ok(mut schedule) = schedule_query.single_mut() {
        *schedule = new_schedule;
        debug!("✅ Schedule regenerated based on new price data");
    } else {
        commands.spawn(new_schedule);
        info!("✅ Initial schedule created");
    }
}
