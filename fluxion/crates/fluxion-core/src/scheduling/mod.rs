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

use crate::strategy::{
    AdaptiveSeasonalOptimizer, BlockEvaluation, EvaluationContext, SeasonalStrategiesConfig,
};
use chrono::Utc;
use fluent::fluent_args;
use fluxion_i18n::I18n;
use fluxion_types::config::{ControlConfig, Currency, StrategiesConfigCore};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::{PriceAnalysis, TimeBlockPrice};
use fluxion_types::scheduling::{OperationSchedule, ScheduledMode};
use tracing::{debug, info};

/// Check if debug logging is enabled based on log level
fn is_debug_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

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
/// * Export price is taken from control_config.grid_export_fee_czk_per_kwh (fixed fee)
/// * `strategies_config` - Optional strategies configuration (uses defaults if None)
/// * `backup_discharge_min_soc` - Minimum SOC from HA sensor (backup_discharge_min_soc)
/// * `grid_import_today_kwh` - Optional grid import energy consumed today (kWh)
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
    strategies_config: Option<&StrategiesConfigCore>,
    backup_discharge_min_soc: f32,
    grid_import_today_kwh: Option<f32>,
) -> OperationSchedule {
    if time_block_prices.is_empty() {
        info!("Cannot generate schedule from empty price data");
        return OperationSchedule::default();
    }

    let optimizer = if let Some(config) = strategies_config {
        let seasonal_config = SeasonalStrategiesConfig::from(config);
        AdaptiveSeasonalOptimizer::with_config(&seasonal_config)
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

    // The TimeAwareChargeStrategy now handles all charging decisions dynamically
    // based on real-time evaluation of current SOC, remaining cheap blocks, and time of day.

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
        let export_price = control_config.grid_export_fee_czk_per_kwh;

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
            backup_discharge_min_soc,
            grid_import_today_kwh,
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
    // NOTE: plan_discharge_blocks was removed from AdaptiveSeasonalOptimizer in simplification
    // If WinterAdaptiveStrategy handles discharge internally, this might not be needed or needs adjustment.
    // However, WinterAdaptiveStrategy handles discharge via evaluate().
    // The previous code called optimizer.plan_discharge_blocks which delegated to WinterPeakDischargeStrategy.
    // Since we removed WinterPeakDischargeStrategy from optimizer, this call is invalid.
    // We should remove this block.

    /*
    let peak_future_soc = soc_predictions
        .iter()
        .copied()
        .fold(current_battery_soc, f32::max);
    debug!(
        "Discharge planning using peak future SOC: {:.1}% (current: {:.1}%)",
        peak_future_soc, current_battery_soc
    );

    optimizer.plan_discharge_blocks(time_block_prices, peak_future_soc, control_config);
    */

    // Initialize predicted SOC for actual scheduling pass
    let mut predicted_soc = current_battery_soc;

    // SECOND PASS: Generate actual schedule decisions for current and future blocks
    for (local_idx, (original_idx, price_block)) in relevant_blocks.iter().enumerate() {
        // Get forecasts for this block (use defaults if not provided)
        let solar_kwh = solar_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.0);
        let consumption_kwh = consumption_forecast
            .and_then(|f| f.get(*original_idx).copied())
            .unwrap_or(0.25); // Default: ~1 kWh/hour

        let export_price = control_config.grid_export_fee_czk_per_kwh;

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
            backup_discharge_min_soc,
            grid_import_today_kwh,
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
    // Step 1: Merge nearby charge blocks separated by small gaps (handles Charge-Backup-Charge patterns)
    // Use min_consecutive_force_blocks as the max gap (default 2 = fill gaps up to 30 min)
    merge_nearby_charge_blocks(
        &mut schedule,
        control_config,
        control_config.min_consecutive_force_blocks,
    );

    // Step 2: Extend or remove short sequences
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
    i18n: Option<&I18n>,
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

/// Merge nearby charge blocks that are separated by small gaps to prevent oscillation.
///
/// This function handles patterns like: Charge -> Backup -> Charge -> Backup
/// by filling in the gaps when the gap is small (1-2 blocks).
///
/// # Arguments
/// * `schedule` - Mutable schedule to optimize
/// * `config` - Control configuration
/// * `max_gap` - Maximum gap size to fill (default: 2 blocks = 30 minutes)
fn merge_nearby_charge_blocks(
    schedule: &mut OperationSchedule,
    _config: &ControlConfig,
    max_gap: usize,
) {
    if schedule.scheduled_blocks.len() < 3 {
        return;
    }

    let mut changes = 0;
    let mut i = 0;

    while i < schedule.scheduled_blocks.len() {
        let mode = schedule.scheduled_blocks[i].mode;

        // Only process ForceCharge blocks
        if mode != InverterOperationMode::ForceCharge {
            i += 1;
            continue;
        }

        // Find the end of this charge sequence
        let mut charge_end = i;
        while charge_end + 1 < schedule.scheduled_blocks.len()
            && schedule.scheduled_blocks[charge_end + 1].mode == InverterOperationMode::ForceCharge
        {
            charge_end += 1;
        }

        // Look for another charge block after a gap
        let gap_start = charge_end + 1;
        if gap_start >= schedule.scheduled_blocks.len() {
            i = gap_start;
            continue;
        }

        // Count the gap (blocks that are not ForceCharge)
        let mut gap_end = gap_start;
        while gap_end < schedule.scheduled_blocks.len()
            && schedule.scheduled_blocks[gap_end].mode != InverterOperationMode::ForceCharge
        {
            gap_end += 1;
        }

        let gap_size = gap_end - gap_start;

        // If there's a charge block after the gap and the gap is small, fill it
        if gap_end < schedule.scheduled_blocks.len()
            && schedule.scheduled_blocks[gap_end].mode == InverterOperationMode::ForceCharge
            && gap_size <= max_gap
        {
            // Only fill gaps that are BackUpMode or SelfUse (not ForceDischarge)
            let gap_is_safe = (gap_start..gap_end).all(|j| {
                matches!(
                    schedule.scheduled_blocks[j].mode,
                    InverterOperationMode::BackUpMode | InverterOperationMode::SelfUse
                )
            });

            if gap_is_safe {
                for j in gap_start..gap_end {
                    schedule.scheduled_blocks[j].mode = InverterOperationMode::ForceCharge;
                    schedule.scheduled_blocks[j].reason = format!(
                        "Merged with adjacent charge blocks (gap {}/{} filled for EEPROM protection)",
                        gap_size, max_gap
                    );
                    changes += 1;
                }

                // Continue from after the merged section
                i = gap_end;
                continue;
            }
        }

        i = gap_start;
    }

    if changes > 0 {
        info!(
            "Merged {} gap blocks between charge sequences (max gap: {} blocks)",
            changes, max_gap
        );
    }
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

    // Initial SOC could be used for diagnostics in the future, but we
    // intentionally don't branch on it now to keep behavior simple and
    // predictable.

    let mut i = 0;
    while i < schedule.scheduled_blocks.len() {
        let mode = schedule.scheduled_blocks[i].mode;

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

            // STRATEGY: Prefer to *extend* short force sequences to reach
            // min_consecutive_force_blocks instead of dropping them. This
            // both respects inverter EEPROM limits (fewer switches) and
            // preserves the optimizer's decision to charge/discharge.
            let needed = min_consecutive - consecutive;
            let mut extended = 0usize;

            // Helper for logging mode name
            let mode_name = if mode == InverterOperationMode::ForceCharge {
                "Force-Charge"
            } else {
                "Force-Discharge"
            };

            // For very high SOC and ForceCharge, we still avoid extending
            // sequences: we don't want to push more charge when battery is
            // effectively full.
            let can_extend =
                !(mode == InverterOperationMode::ForceCharge && predicted_soc > high_soc_threshold);

            if can_extend {
                // First, try to extend to the *right* into default-mode blocks
                let mut j = i + consecutive;
                while j < schedule.scheduled_blocks.len()
                    && extended < needed
                    && schedule.scheduled_blocks[j].mode == config.default_battery_mode
                {
                    schedule.scheduled_blocks[j].mode = mode;
                    schedule.scheduled_blocks[j].reason = format!(
                        "Extended {} sequence for EEPROM protection (min {} blocks)",
                        mode_name, min_consecutive
                    );
                    extended += 1;
                    j += 1;
                }

                // If still not enough, try to extend to the *left* into
                // default-mode blocks directly before the sequence.
                if extended < needed {
                    let mut k = i;
                    while k > 0
                        && extended < needed
                        && schedule.scheduled_blocks[k - 1].mode == config.default_battery_mode
                    {
                        schedule.scheduled_blocks[k - 1].mode = mode;
                        schedule.scheduled_blocks[k - 1].reason = format!(
                            "Extended {} sequence for EEPROM protection (min {} blocks)",
                            mode_name, min_consecutive
                        );
                        extended += 1;
                        k -= 1;
                        // Also update i so outer loop doesn't skip the new start
                        i = k;
                    }
                }
            }

            if extended >= needed {
                let ts = schedule.scheduled_blocks[i].block_start;
                debug!(
                    "Extended {} sequence at {} from {} to {} blocks (SOC: {:.1}%, min required: {})",
                    mode_name,
                    ts.format("%H:%M"),
                    consecutive,
                    consecutive + extended,
                    predicted_soc,
                    min_consecutive
                );
                changes += extended;
            } else {
                // If we couldn't safely extend (e.g. high SOC or no adjacent
                // default blocks), fall back to converting this short
                // sequence to default mode to avoid rapid mode flips.
                let should_convert = if mode == InverterOperationMode::ForceCharge {
                    // Don't force-charge for short sequences if SOC > 90%
                    // The battery won't accept much energy anyway
                    if predicted_soc > high_soc_threshold {
                        let ts = schedule.scheduled_blocks[i].block_start;
                        debug!(
                            "Removing {}-block force-charge at {} (SOC: {:.1}% > {}%, min required: {})",
                            consecutive,
                            ts.format("%H:%M"),
                            predicted_soc,
                            high_soc_threshold,
                            min_consecutive
                        );
                        true
                    } else {
                        // For lower SOC, still avoid isolated short sequences
                        let ts = schedule.scheduled_blocks[i].block_start;
                        debug!(
                            "Removing isolated {}-block force-charge at {} (< {} blocks required for EEPROM protection, no room to extend)",
                            consecutive,
                            ts.format("%H:%M"),
                            min_consecutive
                        );
                        true
                    }
                } else {
                    // ForceDischarge: short sequences rarely make sense
                    let ts = schedule.scheduled_blocks[i].block_start;
                    debug!(
                        "Removing isolated {}-block force-discharge at {} (< {} blocks required for EEPROM protection, no room to extend)",
                        consecutive,
                        ts.format("%H:%M"),
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
                            mode_name, consecutive, min_consecutive, predicted_soc
                        );
                    }
                    changes += consecutive;
                }
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
    evaluation: &BlockEvaluation,
    config: &ControlConfig,
    solar_kwh: f32,
    consumption_kwh: f32,
) -> f32 {
    let battery_capacity = config.battery_capacity_kwh;
    if battery_capacity <= 0.0 {
        return current_soc;
    }

    let mut new_soc = current_soc;

    match evaluation.mode {
        InverterOperationMode::ForceCharge => {
            // Energy charged in 15 minutes (0.25 hours)
            let energy_kwh = config.max_battery_charge_rate_kw * 0.25;
            let soc_increase =
                crate::components::calculate_soc_change(energy_kwh, battery_capacity);
            new_soc += soc_increase;
        }
        InverterOperationMode::ForceDischarge => {
            // Assume discharging at max rate (or export limit) for 15 mins
            let max_export_kw = config.maximum_export_power_w as f32 / 1000.0;
            let energy_kwh = max_export_kw * 0.25;
            let soc_decrease =
                crate::components::calculate_soc_change(energy_kwh, battery_capacity);
            new_soc -= soc_decrease;
        }
        InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
            // Calculate net load (already in kWh for the block)
            let net_load = consumption_kwh - solar_kwh;

            if net_load > 0.0 {
                // Deficit: discharge battery to cover load
                let soc_decrease =
                    crate::components::calculate_soc_change(net_load, battery_capacity);
                new_soc -= soc_decrease;
            } else {
                // Surplus: charge battery with excess solar
                let energy_kwh = -net_load;
                let soc_increase =
                    crate::components::calculate_soc_change(energy_kwh, battery_capacity);
                new_soc += soc_increase;
            }
        }
    }

    // Clamp SOC to hardware limits
    new_soc.clamp(config.hardware_min_battery_soc, 100.0)
}
