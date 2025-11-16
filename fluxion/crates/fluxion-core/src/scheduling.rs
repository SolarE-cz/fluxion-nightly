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

use crate::components::{
    InverterOperationMode, OperationSchedule, PriceAnalysis, ScheduledMode, TimeBlockPrice,
};
use crate::resources::{ControlConfig, Currency, I18nResource};
use crate::strategy::{AdaptiveSeasonalOptimizer, EvaluationContext};
use chrono::Utc;
use fluent::fluent_args;
use tracing::{debug, info};

/// Check if debug logging is enabled based on log level
fn is_debug_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

#[cfg(test)]
use chrono::DateTime;

/// Configuration for schedule generation
#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    /// Minimum battery SOC (%) - don't discharge below this
    pub min_battery_soc: f32,

    /// Maximum battery SOC (%) - don't charge above this
    pub max_battery_soc: f32,

    /// Target inverter IDs (empty = all inverters)
    pub target_inverters: Vec<String>,

    /// Display currency for price formatting
    pub display_currency: Currency,

    /// Default battery operation mode when not force charging/discharging
    pub default_battery_mode: InverterOperationMode,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            target_inverters: Vec::new(),
            display_currency: Currency::default(),
            default_battery_mode: InverterOperationMode::SelfUse,
        }
    }
}

/// Generate an operation schedule using the economic optimizer
///
/// This function uses the economic optimization architecture where multiple strategies
/// compete based on their expected profit/cost for each time block.
///
/// # Arguments
/// * `time_block_prices` - Price data for each 15-minute block
/// * `control_config` - Battery parameters and control configuration
/// * `schedule_config` - Schedule generation configuration
/// * `current_battery_soc` - Current battery state of charge (%)
/// * `solar_forecast` - Optional solar generation forecast per block (kWh)
/// * `consumption_forecast` - Optional consumption forecast per block (kWh)
/// * `export_price_multiplier` - Export price as fraction of import price (default: 0.8)
/// * `strategies_config` - Optional strategies configuration (uses defaults if None)
///
/// # Returns
/// Complete `OperationSchedule` with economically optimized mode assignments
#[expect(clippy::too_many_arguments)]
pub fn generate_schedule_with_optimizer(
    time_block_prices: &[TimeBlockPrice],
    control_config: &ControlConfig,
    schedule_config: &ScheduleConfig,
    current_battery_soc: f32,
    solar_forecast: Option<&[f32]>,
    consumption_forecast: Option<&[f32]>,
    export_price_multiplier: f32,
    strategies_config: Option<&crate::strategy::SeasonalStrategiesConfig>,
) -> OperationSchedule {
    if time_block_prices.is_empty() {
        info!("Cannot generate schedule from empty price data");
        return OperationSchedule::default();
    }

    let optimizer = if let Some(config) = strategies_config {
        AdaptiveSeasonalOptimizer::with_config(config)
    } else {
        AdaptiveSeasonalOptimizer::with_defaults()
    };
    let mut scheduled_blocks = Vec::new();
    let mut total_profit = 0.0;

    // Track predicted SOC throughout schedule for realistic future evaluations
    let now = Utc::now();

    // IMPORTANT: Filter price blocks to only include current and future blocks
    // This prevents the scheduler from using current SOC for past blocks when regenerating mid-day,
    // which would cause incorrect SOC predictions for the rest of today.
    let relevant_blocks: Vec<(usize, &TimeBlockPrice)> = time_block_prices
        .iter()
        .enumerate()
        .filter(|(_, block)| {
            block.block_start
                >= now
                    .checked_sub_signed(chrono::Duration::minutes(15))
                    .unwrap_or(now)
        })
        .collect();

    if relevant_blocks.is_empty() {
        info!("No current or future price blocks available for scheduling");
        return OperationSchedule::default();
    }

    info!(
        "Scheduling for {} blocks (filtered {} past blocks, current SOC: {:.1}%)",
        relevant_blocks.len(),
        time_block_prices.len() - relevant_blocks.len(),
        current_battery_soc
    );

    // DISABLED: Automatic charge block reservation
    // The TimeAwareChargeStrategy now handles all charging decisions dynamically
    // based on real-time evaluation of current SOC, remaining cheap blocks, and time of day.
    // Pre-reserving blocks was causing charging at non-optimal times.

    // FIRST PASS: Simulate schedule to predict SOC at each block
    // This allows discharge planning to use realistic future SOC
    let mut soc_predictions: Vec<f32> = Vec::with_capacity(relevant_blocks.len());
    let mut temp_predicted_soc = current_battery_soc;

    for (local_idx, (original_idx, price_block)) in relevant_blocks.iter().enumerate() {
        soc_predictions.push(temp_predicted_soc);

        let solar_kwh = solar_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.0);
        let consumption_kwh = consumption_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.25);
        let export_price = price_block.price_czk_per_kwh * export_price_multiplier;

        let remaining_blocks: Vec<TimeBlockPrice> = relevant_blocks
            .iter()
            .skip(local_idx)
            .map(|(_, block)| (*block).clone())
            .collect();

        let context = EvaluationContext {
            price_block,
            control_config,
            current_battery_soc: temp_predicted_soc,
            solar_forecast_kwh: solar_kwh,
            consumption_forecast_kwh: consumption_kwh,
            grid_export_price_czk_per_kwh: export_price,
            all_price_blocks: Some(&remaining_blocks),
        };

        let evaluation = optimizer.evaluate(&context);
        temp_predicted_soc = update_soc_prediction(
            temp_predicted_soc,
            &evaluation,
            control_config,
            solar_kwh,
            consumption_kwh,
        );
    }

    // GLOBAL DISCHARGE PLANNING: Find peak SOC in future and use it for discharge planning
    // This ensures we consider the battery will be charged before discharge time
    let peak_future_soc = soc_predictions
        .iter()
        .copied()
        .fold(current_battery_soc, f32::max);
    debug!(
        "Discharge planning using peak future SOC: {:.1}% (current: {:.1}%)",
        peak_future_soc, current_battery_soc
    );

    optimizer.plan_discharge_blocks(time_block_prices, peak_future_soc, control_config);

    // Initialize predicted SOC for actual scheduling pass
    let mut predicted_soc = current_battery_soc;

    for (local_idx, (original_idx, price_block)) in relevant_blocks.iter().enumerate() {
        // Get forecasts for this block (use defaults if not provided)
        let solar_kwh = solar_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.0);
        let consumption_kwh = consumption_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.25); // Default: ~1 kWh/hour

        let export_price = price_block.price_czk_per_kwh * export_price_multiplier;

        // For filtered blocks, all are current or future, so we use predicted SOC
        // (starting with current SOC for the first block)
        let soc_for_evaluation = predicted_soc;

        // Collect remaining blocks (from this block onwards) for strategy analysis
        // This ensures strategies only consider upcoming blocks when making decisions
        let remaining_blocks: Vec<TimeBlockPrice> = relevant_blocks
            .iter()
            .skip(local_idx)
            .map(|(_, block)| (*block).clone())
            .collect();

        // Create evaluation context with appropriate SOC
        let context = EvaluationContext {
            price_block,
            control_config,
            current_battery_soc: soc_for_evaluation,
            solar_forecast_kwh: solar_kwh,
            consumption_forecast_kwh: consumption_kwh,
            grid_export_price_czk_per_kwh: export_price,
            all_price_blocks: Some(&remaining_blocks),
        };

        // Optimize and get best strategy evaluation
        // Capture debug info if debug logging is enabled
        let capture_debug = is_debug_enabled();
        let evaluation = optimizer.evaluate_with_debug(&context, capture_debug);

        // Update predicted SOC for next block
        // Since all blocks in relevant_blocks are current/future, always update prediction
        predicted_soc = update_soc_prediction(
            predicted_soc,
            &evaluation,
            control_config,
            solar_kwh,
            consumption_kwh,
        );

        debug!(
            "Block {} (orig {}): {} - {} (profit: {:.3} CZK, SOC: {:.1}%)",
            local_idx,
            original_idx,
            evaluation.strategy_name,
            evaluation.reason,
            evaluation.net_profit_czk,
            predicted_soc
        );

        total_profit += evaluation.net_profit_czk;

        scheduled_blocks.push(ScheduledMode {
            block_start: evaluation.block_start,
            duration_minutes: evaluation.duration_minutes,
            target_inverters: if schedule_config.target_inverters.is_empty() {
                None
            } else {
                Some(schedule_config.target_inverters.clone())
            },
            mode: evaluation.mode,
            reason: format!(
                "{} - {} (expected profit: {:.2} CZK)",
                evaluation.strategy_name, evaluation.reason, evaluation.net_profit_czk
            ),
            debug_info: evaluation.debug_info,
        });
    }

    let mut schedule = OperationSchedule {
        scheduled_blocks,
        generated_at: Utc::now(),
        based_on_price_version: Utc::now(), // Current time as no pre-analysis
    };

    // DISABLED: Charge block consolidation
    // This was adding extra charge blocks that the optimizer didn't select.
    // All charging decisions should come from the strategy optimizer only.
    // consolidate_charge_blocks(&mut schedule, time_block_prices);

    // Count modes
    let charge_count = schedule
        .scheduled_blocks
        .iter()
        .filter(|b| b.mode == InverterOperationMode::ForceCharge)
        .count();
    let discharge_count = schedule
        .scheduled_blocks
        .iter()
        .filter(|b| b.mode == InverterOperationMode::ForceDischarge)
        .count();
    let self_use_count = schedule
        .scheduled_blocks
        .iter()
        .filter(|b| b.mode == InverterOperationMode::SelfUse)
        .count();

    info!(
        "Generated economic schedule: {} blocks, {} charge, {} discharge, {} self-use, total expected profit: {:.2} CZK",
        schedule.scheduled_blocks.len(),
        charge_count,
        discharge_count,
        self_use_count,
        total_profit
    );

    // Post-process to reduce mode switches and protect inverter EEPROM
    reduce_mode_switches(&mut schedule, control_config, &soc_predictions);

    schedule
}

/// Generate an operation schedule from price analysis (legacy method)
///
/// This is the original simple scheduling method that uses pre-computed price analysis
/// to determine charge/discharge blocks based on cheapest/most expensive hours.
///
/// For economic optimization with multiple strategies, use `generate_schedule_with_optimizer`.
///
/// # Arguments
/// * `time_block_prices` - Price data for each 15-minute block
/// * `analysis` - Pre-computed price analysis with charge/discharge blocks
/// * `config` - Schedule generation configuration
/// * `i18n` - I18n resource for translated reason strings (optional, falls back to English)
///
/// # Returns
/// Complete `OperationSchedule` with block-by-block mode assignments
pub fn generate_schedule(
    time_block_prices: &[TimeBlockPrice],
    analysis: &PriceAnalysis,
    config: &ScheduleConfig,
    i18n: Option<&I18nResource>,
) -> OperationSchedule {
    if time_block_prices.is_empty() {
        info!("Cannot generate schedule from empty price data");
        return OperationSchedule::default();
    }

    let mut scheduled_blocks = Vec::new();

    for (idx, price_block) in time_block_prices.iter().enumerate() {
        // Determine mode for this block based on analysis
        let currency_symbol = config.display_currency.symbol();
        let (mode, reason) = if analysis.charge_blocks.contains(&idx) {
            let reason = if let Some(i18n_res) = i18n {
                i18n_res
                    .inner()
                    .format(
                        "reason-cheapest-block",
                        Some(&fluent_args![
                            "price" => price_block.price_czk_per_kwh,
                            "currency" => currency_symbol
                        ]),
                    )
                    .unwrap_or_else(|_| {
                        format!(
                            "Cheapest block ({}{:.3}/kWh)",
                            currency_symbol, price_block.price_czk_per_kwh
                        )
                    })
            } else {
                format!(
                    "Cheapest block ({}{:.3}/kWh)",
                    currency_symbol, price_block.price_czk_per_kwh
                )
            };
            (InverterOperationMode::ForceCharge, reason)
        } else if analysis.discharge_blocks.contains(&idx) {
            let reason = if let Some(i18n_res) = i18n {
                i18n_res
                    .inner()
                    .format(
                        "reason-peak-price",
                        Some(&fluent_args![
                            "price" => price_block.price_czk_per_kwh,
                            "currency" => currency_symbol
                        ]),
                    )
                    .unwrap_or_else(|_| {
                        format!(
                            "Peak price ({}{:.3}/kWh)",
                            currency_symbol, price_block.price_czk_per_kwh
                        )
                    })
            } else {
                format!(
                    "Peak price ({}{:.3}/kWh)",
                    currency_symbol, price_block.price_czk_per_kwh
                )
            };
            (InverterOperationMode::ForceDischarge, reason)
        } else {
            let reason = if let Some(i18n_res) = i18n {
                i18n_res
                    .inner()
                    .format(
                        "reason-normal-operation",
                        Some(&fluent_args![
                            "price" => price_block.price_czk_per_kwh,
                            "currency" => currency_symbol
                        ]),
                    )
                    .unwrap_or_else(|_| {
                        format!(
                            "Normal operation ({}{:.3}/kWh)",
                            currency_symbol, price_block.price_czk_per_kwh
                        )
                    })
            } else {
                format!(
                    "Normal operation ({}{:.3}/kWh)",
                    currency_symbol, price_block.price_czk_per_kwh
                )
            };
            (config.default_battery_mode, reason)
        };

        scheduled_blocks.push(ScheduledMode {
            block_start: price_block.block_start,
            duration_minutes: price_block.duration_minutes,
            target_inverters: if config.target_inverters.is_empty() {
                None
            } else {
                Some(config.target_inverters.clone())
            },
            mode,
            reason,
            debug_info: None, // Legacy scheduler doesn't generate debug info
        });
    }

    let schedule = OperationSchedule {
        scheduled_blocks,
        generated_at: Utc::now(),
        based_on_price_version: analysis.analyzed_at,
    };

    // DISABLED: Charge block consolidation
    // This was extending charge blocks to neighbors, causing charging outside the cheapest windows.
    // The TimeAwareChargeStrategy now makes precise decisions about which blocks to charge.
    // consolidate_charge_blocks(&mut schedule, time_block_prices);

    info!(
        "Generated schedule: {} blocks, {} charge, {} discharge, {} self-use",
        schedule.scheduled_blocks.len(),
        analysis.charge_blocks.len(),
        analysis.discharge_blocks.len(),
        schedule.scheduled_blocks.len()
            - analysis.charge_blocks.len()
            - analysis.discharge_blocks.len()
    );

    debug!(
        "Schedule based on price data analyzed at: {}",
        analysis.analyzed_at
    );

    schedule
}

/// Check if current battery SOC allows the planned mode
pub fn check_soc_constraints(
    planned_mode: InverterOperationMode,
    current_soc: f32,
    config: &ScheduleConfig,
) -> bool {
    match planned_mode {
        InverterOperationMode::ForceCharge => {
            // Don't charge if already at max
            if current_soc >= config.max_battery_soc {
                debug!(
                    "Skipping charge: SOC {:.1}% >= max {:.1}%",
                    current_soc, config.max_battery_soc
                );
                return false;
            }
        }
        InverterOperationMode::ForceDischarge => {
            // Don't discharge if below min
            if current_soc <= config.min_battery_soc {
                debug!(
                    "Skipping discharge: SOC {:.1}% <= min {:.1}%",
                    current_soc, config.min_battery_soc
                );
                return false;
            }
        }
        InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
            // Self-use and Backup Mode are always allowed
        }
    }

    true
}

/// Reduce unnecessary mode switches to protect inverter EEPROM
///
/// This function prevents short-duration force-charge/discharge operations that:
/// - Have fewer than the configured minimum consecutive blocks
/// - Have high SOC (>90%) for force-charge
/// - Have negligible economic benefit
///
/// Short mode switches cause inverter EEPROM writes which degrade the hardware.
/// This consolidation ensures minimum duration for force operations (default: 2 blocks = 30 min),
/// or reverts to self-use if the benefit is too small.
///
/// # Arguments
/// * `schedule` - Mutable schedule to optimize
/// * `config` - Control configuration (includes min_consecutive_force_blocks)
/// * `soc_predictions` - Predicted SOC at each block for high-SOC filtering
fn reduce_mode_switches(
    schedule: &mut OperationSchedule,
    config: &ControlConfig,
    soc_predictions: &[f32],
) {
    if schedule.scheduled_blocks.is_empty() {
        return;
    }

    let mut changes = 0;
    let high_soc_threshold = 90.0;
    let min_consecutive = config.min_consecutive_force_blocks;

    // CRITICAL: If initial SOC is very low (<20%), skip consolidation entirely
    // to allow all charge blocks through (prevents expensive morning peak imports)
    let initial_soc = soc_predictions.first().copied().unwrap_or(50.0);
    if initial_soc < 20.0 {
        info!(
            "Skipping mode switch reduction due to CRITICAL low SOC ({:.1}%) - allowing all charge blocks",
            initial_soc
        );
        return;
    }

    let mut i = 0;
    while i < schedule.scheduled_blocks.len() {
        let block = &schedule.scheduled_blocks[i];
        let mode = block.mode;

        // Only process force operations
        if mode != InverterOperationMode::ForceCharge
            && mode != InverterOperationMode::ForceDischarge
        {
            i += 1;
            continue;
        }

        // Count consecutive blocks with same mode
        let mut consecutive = 1;
        while i + consecutive < schedule.scheduled_blocks.len()
            && schedule.scheduled_blocks[i + consecutive].mode == mode
        {
            consecutive += 1;
        }

        // Check if this sequence is too short (less than minimum required)
        if consecutive < min_consecutive {
            let predicted_soc = soc_predictions.get(i).copied().unwrap_or(50.0);

            // Determine if we should convert to self-use
            let should_convert = if mode == InverterOperationMode::ForceCharge {
                // Don't force-charge for short sequences if SOC > 90%
                // The battery won't accept much energy anyway
                if predicted_soc > high_soc_threshold {
                    debug!(
                        "Removing {}-block force-charge at {} (SOC: {:.1}% > {}%, min required: {})",
                        consecutive,
                        block.block_start.format("%H:%M"),
                        predicted_soc,
                        high_soc_threshold,
                        min_consecutive
                    );
                    true
                } else {
                    // For lower SOC, still avoid short sequences
                    debug!(
                        "Removing isolated {}-block force-charge at {} (< {} blocks required for EEPROM protection)",
                        consecutive,
                        block.block_start.format("%H:%M"),
                        min_consecutive
                    );
                    true
                }
            } else {
                // ForceDischarge: short sequences rarely make sense
                debug!(
                    "Removing isolated {}-block force-discharge at {} (< {} blocks required for EEPROM protection)",
                    consecutive,
                    block.block_start.format("%H:%M"),
                    min_consecutive
                );
                true
            };

            if should_convert {
                // Convert all blocks in this short sequence to default mode
                for j in 0..consecutive {
                    schedule.scheduled_blocks[i + j].mode = config.default_battery_mode;
                    schedule.scheduled_blocks[i + j].reason = format!(
                        "Converted from {} to Self-Use ({}-block sequence < {} min required, SOC: {:.1}%)",
                        if mode == InverterOperationMode::ForceCharge {
                            "Force-Charge"
                        } else {
                            "Force-Discharge"
                        },
                        consecutive,
                        min_consecutive,
                        predicted_soc
                    );
                }
                changes += consecutive;
            }
        }

        i += consecutive;
    }

    if changes > 0 {
        info!(
            "Mode switch reduction: converted {} short-duration force operation blocks to Self-Use (< {} blocks required, EEPROM protection)",
            changes, min_consecutive
        );
    }
}

/// Update predicted SOC based on evaluation decision
///
/// This tracks how SOC changes through the schedule based on:
/// - ForceCharge: Adds energy to battery
/// - ForceDischarge: Removes energy from battery  
/// - SelfUse: Net change from solar and consumption
///
/// # Arguments
/// * `current_soc` - Current predicted SOC (%)
/// * `evaluation` - Block evaluation with energy flows
/// * `config` - Control configuration with battery parameters
/// * `solar_kwh` - Solar generation forecast for this block
/// * `consumption_kwh` - Consumption forecast for this block
///
/// # Returns
/// Updated SOC (%) clamped to min/max limits
fn update_soc_prediction(
    current_soc: f32,
    evaluation: &crate::strategy::BlockEvaluation,
    config: &ControlConfig,
    solar_kwh: f32,
    consumption_kwh: f32,
) -> f32 {
    use crate::components::InverterOperationMode;

    let mut new_soc = current_soc;

    match evaluation.mode {
        InverterOperationMode::ForceCharge => {
            // Charge adds energy (account for efficiency)
            let charge_kwh = evaluation.energy_flows.battery_charge_kwh;
            if charge_kwh > 0.0 {
                // Use actual energy from evaluation if available
                new_soc += (charge_kwh / config.battery_capacity_kwh) * 100.0;
            } else {
                // Fallback: estimate charge per 15-minute block
                // Formula: max_charge_rate_kw / battery_capacity_kwh * 100 / 4
                // Example: 10 kW / 23 kWh * 100 / 4 = ~10.87% per block
                let charge_per_block =
                    config.max_battery_charge_rate_kw / config.battery_capacity_kwh * 100.0 / 4.0;
                new_soc += charge_per_block * config.battery_efficiency;
            }
        }
        InverterOperationMode::ForceDischarge => {
            // Discharge removes energy
            let discharge_kwh = evaluation.energy_flows.battery_discharge_kwh;
            if discharge_kwh > 0.0 {
                new_soc -= (discharge_kwh / config.battery_capacity_kwh) * 100.0;
            }
        }
        InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
            // Net change: solar adds, consumption removes
            // If solar > consumption, battery charges
            // If consumption > solar, battery discharges (or grid imports)
            // Backup mode behaves the same as self-use for SOC prediction purposes
            let net_kwh = solar_kwh - consumption_kwh;
            new_soc += (net_kwh / config.battery_capacity_kwh) * 100.0;
        }
    }

    // Clamp to configured limits
    new_soc.clamp(config.min_battery_soc, config.max_battery_soc)
}

/// Generate a simple schedule for testing (no price analysis)
#[cfg(test)]
pub fn generate_simple_schedule(
    start_time: DateTime<Utc>,
    block_count: usize,
    mode: InverterOperationMode,
) -> OperationSchedule {
    let mut scheduled_blocks = Vec::new();

    for i in 0..block_count {
        scheduled_blocks.push(ScheduledMode {
            block_start: start_time + chrono::Duration::minutes(i as i64 * 15),
            duration_minutes: 15,
            target_inverters: None,
            mode,
            reason: "Test schedule".to_string(),
            debug_info: None,
        });
    }

    OperationSchedule {
        scheduled_blocks,
        generated_at: Utc::now(),
        based_on_price_version: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::PriceRange;
    use crate::strategy::BlockEvaluation;

    #[test]
    fn test_schedule_config_default() {
        let config = ScheduleConfig::default();
        assert_eq!(config.min_battery_soc, 10.0);
        assert_eq!(config.max_battery_soc, 100.0);
        assert_eq!(config.target_inverters.len(), 0);
    }

    #[test]
    fn test_generate_schedule_with_target_inverters() {
        let now = Utc::now();
        let price_blocks = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.40,
        }];

        let analysis = PriceAnalysis {
            charge_blocks: vec![0],
            discharge_blocks: Vec::new(),
            price_range: PriceRange::default(),
            analyzed_at: now,
        };

        let config = ScheduleConfig {
            target_inverters: vec!["inv1".to_string(), "inv2".to_string()],
            ..Default::default()
        };

        let schedule = generate_schedule(&price_blocks, &analysis, &config, None);

        assert_eq!(schedule.scheduled_blocks.len(), 1);
        assert!(schedule.scheduled_blocks[0].target_inverters.is_some());
        assert_eq!(
            schedule.scheduled_blocks[0]
                .target_inverters
                .as_ref()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn test_generate_schedule_empty_data() {
        let analysis = PriceAnalysis::default();
        let config = ScheduleConfig::default();

        let schedule = generate_schedule(&[], &analysis, &config, None);

        assert_eq!(schedule.scheduled_blocks.len(), 0);
    }

    #[test]
    fn test_check_soc_constraints_charge() {
        let config = ScheduleConfig::default();

        // Can charge at low SOC
        assert!(check_soc_constraints(
            InverterOperationMode::ForceCharge,
            50.0,
            &config
        ));

        // Cannot charge at max SOC
        assert!(!check_soc_constraints(
            InverterOperationMode::ForceCharge,
            100.0,
            &config
        ));
    }

    #[test]
    fn test_check_soc_constraints_discharge() {
        let config = ScheduleConfig::default();

        // Can discharge at high SOC
        assert!(check_soc_constraints(
            InverterOperationMode::ForceDischarge,
            50.0,
            &config
        ));

        // Cannot discharge at min SOC
        assert!(!check_soc_constraints(
            InverterOperationMode::ForceDischarge,
            10.0,
            &config
        ));
    }

    #[test]
    fn test_check_soc_constraints_self_use() {
        let config = ScheduleConfig::default();

        // Self-use always allowed
        assert!(check_soc_constraints(
            InverterOperationMode::SelfUse,
            0.0,
            &config
        ));
        assert!(check_soc_constraints(
            InverterOperationMode::SelfUse,
            100.0,
            &config
        ));
    }

    #[test]
    fn test_generate_simple_schedule() {
        let now = Utc::now();
        let schedule = generate_simple_schedule(now, 4, InverterOperationMode::ForceCharge);

        assert_eq!(schedule.scheduled_blocks.len(), 4);
        assert_eq!(
            schedule.scheduled_blocks[0].mode,
            InverterOperationMode::ForceCharge
        );
        assert_eq!(schedule.scheduled_blocks[0].duration_minutes, 15);
        assert_eq!(
            schedule.scheduled_blocks[1].block_start,
            now + chrono::Duration::minutes(15)
        );
    }

    #[test]
    fn test_schedule_with_24h_pattern() {
        let now = Utc::now();
        let mut price_blocks = Vec::new();

        // Create 24 hours (96 blocks)
        for i in 0..96 {
            price_blocks.push(TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 0.40 + (i as f32 * 0.001),
            });
        }

        // Mark first 4 blocks for charging, last 4 for discharging
        let analysis = PriceAnalysis {
            charge_blocks: vec![0, 1, 2, 3],
            discharge_blocks: vec![92, 93, 94, 95],
            price_range: PriceRange::default(),
            analyzed_at: now,
        };

        let config = ScheduleConfig::default();
        let schedule = generate_schedule(&price_blocks, &analysis, &config, None);

        assert_eq!(schedule.scheduled_blocks.len(), 96);

        // Check first 4 are charging
        for i in 0..4 {
            assert_eq!(
                schedule.scheduled_blocks[i].mode,
                InverterOperationMode::ForceCharge
            );
        }

        // Check last 4 are discharging
        for i in 92..96 {
            assert_eq!(
                schedule.scheduled_blocks[i].mode,
                InverterOperationMode::ForceDischarge
            );
        }

        // Check middle is self-use
        assert_eq!(
            schedule.scheduled_blocks[50].mode,
            InverterOperationMode::SelfUse
        );
    }

    #[test]
    fn test_soc_prediction_charge() {
        let config = ControlConfig {
            battery_capacity_kwh: 20.0,
            battery_efficiency: 0.95,
            average_household_load_kw: 0.5,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            ..Default::default()
        };

        // Starting at 50% SOC, charge for 15 minutes at 10 kW
        // Energy: 10 kW * 0.25 h * 0.95 efficiency = 2.375 kWh charged
        let charge_kwh = 2.375;
        let consumption_kwh = 0.125; // 0.5 kW * 0.25 h
        let solar_kwh = 0.0;

        let mut evaluation = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::ForceCharge,
            "Test".to_string(),
        );
        evaluation.energy_flows.battery_charge_kwh = charge_kwh;
        evaluation.energy_flows.battery_discharge_kwh = 0.0;
        evaluation.energy_flows.grid_import_kwh = 0.0;
        evaluation.energy_flows.grid_export_kwh = 0.0;
        evaluation.energy_flows.solar_generation_kwh = 0.0;
        evaluation.energy_flows.household_consumption_kwh = 0.0;

        let new_soc = update_soc_prediction(50.0, &evaluation, &config, solar_kwh, consumption_kwh);

        // SOC increase: 2.375 / 20.0 * 100 = 11.875%
        // New SOC: 50% + 11.875% = 61.875%
        assert!(
            (new_soc - 61.875).abs() < 0.5,
            "Expected ~61.875%, got {new_soc}"
        );
    }

    #[test]
    fn test_soc_prediction_discharge() {
        let config = ControlConfig {
            battery_capacity_kwh: 20.0,
            battery_efficiency: 0.95,
            average_household_load_kw: 2.0,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            ..Default::default()
        };

        // Starting at 80% SOC, discharge for 15 minutes
        // Assume 1.25 kWh discharge from battery
        let discharge_kwh = 1.25;
        let mut evaluation = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::ForceDischarge,
            "Test".to_string(),
        );
        evaluation.energy_flows.battery_charge_kwh = 0.0;
        evaluation.energy_flows.battery_discharge_kwh = discharge_kwh;
        evaluation.energy_flows.grid_import_kwh = 0.0;
        evaluation.energy_flows.grid_export_kwh = 0.0;
        evaluation.energy_flows.solar_generation_kwh = 0.0;
        evaluation.energy_flows.household_consumption_kwh = 0.0;

        let new_soc = update_soc_prediction(
            80.0,
            &evaluation,
            &config,
            0.0, // solar_kwh
            0.0, // consumption_kwh (already accounted in discharge)
        );

        // SOC decrease: 1.25 / 20.0 * 100 = 6.25%
        // New SOC: 80% - 6.25% = 73.75%
        assert!(new_soc < 80.0, "SOC should decrease during discharge");
        assert!(new_soc > 70.0, "SOC should not drop too much in 15 min");
    }

    #[test]
    fn test_soc_prediction_self_use() {
        let config = ControlConfig {
            battery_capacity_kwh: 20.0,
            battery_efficiency: 0.95,
            average_household_load_kw: 1.0,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            ..Default::default()
        };

        // Starting at 60% SOC, self-use for 15 minutes
        // Solar 0 kWh, consumption 0.25 kWh -> net -0.25 kWh from battery
        let solar_kwh = 0.0;
        let consumption_kwh = 0.25; // 1.0 kW * 0.25 h

        let mut evaluation = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::SelfUse,
            "Test".to_string(),
        );
        evaluation.energy_flows.battery_charge_kwh = 0.0;
        evaluation.energy_flows.battery_discharge_kwh = 0.0;
        evaluation.energy_flows.grid_import_kwh = 0.0;
        evaluation.energy_flows.grid_export_kwh = 0.0;
        evaluation.energy_flows.solar_generation_kwh = 0.0;
        evaluation.energy_flows.household_consumption_kwh = 0.0;

        let new_soc = update_soc_prediction(60.0, &evaluation, &config, solar_kwh, consumption_kwh);

        // SOC decrease: 0.25 / 20.0 * 100 = 1.25%
        // New SOC: 60% - 1.25% = 58.75%
        assert!(
            (new_soc - 58.75).abs() < 0.1,
            "Expected ~58.75%, got {new_soc}"
        );
    }

    #[test]
    fn test_soc_prediction_clamping() {
        let config = ControlConfig {
            battery_capacity_kwh: 20.0,
            battery_efficiency: 0.95,
            average_household_load_kw: 0.5,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            ..Default::default()
        };

        // Test upper clamping: start at 98%, charge 5 kWh
        let mut eval_charge = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::ForceCharge,
            "Test".to_string(),
        );
        eval_charge.energy_flows.battery_charge_kwh = 5.0;

        let new_soc = update_soc_prediction(98.0, &eval_charge, &config, 0.0, 0.0);
        assert!(new_soc <= 100.0, "SOC should be clamped at max");

        // Test lower clamping: start at 12%, heavy discharge 5 kWh
        let mut eval_discharge = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::ForceDischarge,
            "Test".to_string(),
        );
        eval_discharge.energy_flows.battery_discharge_kwh = 5.0;

        let new_soc = update_soc_prediction(12.0, &eval_discharge, &config, 0.0, 0.0);
        assert!(new_soc >= 10.0, "SOC should be clamped at min");
    }

    #[test]
    fn test_schedule_with_soc_prediction() {
        // This test verifies the main scheduling loop properly tracks SOC
        // Use future time to ensure blocks don't get filtered out
        let now = Utc::now();
        let future_start = now + chrono::Duration::hours(1);

        // Create price blocks: cheap at start, expensive later
        let price_blocks = vec![
            TimeBlockPrice {
                block_start: future_start,
                duration_minutes: 15,
                price_czk_per_kwh: 1.8, // Cheap - should charge
            },
            TimeBlockPrice {
                block_start: future_start + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 1.9,
            },
            TimeBlockPrice {
                block_start: future_start + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0, // Expensive
            },
        ];

        let config = ControlConfig {
            battery_capacity_kwh: 20.0,
            battery_efficiency: 0.95,
            average_household_load_kw: 0.5,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            force_charge_hours: 2,
            force_discharge_hours: 1,
            battery_wear_cost_czk_per_kwh: 0.125,
            maximum_export_power_w: 10000,
            min_mode_change_interval_secs: 300,
            min_consecutive_force_blocks: 2,
            default_battery_mode: Default::default(),
        };

        let schedule_config = ScheduleConfig {
            min_battery_soc: config.min_battery_soc,
            max_battery_soc: config.max_battery_soc,
            target_inverters: vec![],
            display_currency: Currency::CZK,
            default_battery_mode: Default::default(),
        };

        let schedule = generate_schedule_with_optimizer(
            &price_blocks,
            &config,
            &schedule_config,
            30.0, // current_battery_soc: start at 30%
            None, // solar_forecast
            None, // consumption_forecast
            0.8,  // export_price_multiplier
            None, // strategies_config: use defaults
        );

        assert_eq!(schedule.scheduled_blocks.len(), 3);

        // First blocks should be charge since we're low on SOC and have cheap prices
        // The exact modes depend on strategy evaluation, but we should see some charging
        let charge_count = schedule
            .scheduled_blocks
            .iter()
            .filter(|b| b.mode == InverterOperationMode::ForceCharge)
            .count();
        assert!(
            charge_count >= 1,
            "Should schedule at least one charge block when SOC is low"
        );
    }
}
