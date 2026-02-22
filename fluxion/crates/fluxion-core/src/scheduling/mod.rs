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

use crate::strategy::BlockEvaluation;
use chrono::Utc;
use fluent::fluent_args;
use fluxion_i18n::I18n;
use fluxion_plugins::{
    BatteryState, BlockDecision, EvaluationRequest, ForecastData, HistoricalData, OperationMode,
    PluginManager, PriceBlock,
};
use fluxion_types::UserControlState;
use fluxion_types::config::{ControlConfig, Currency, PricingConfig};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::{PriceAnalysis, TimeBlockPrice};
use fluxion_types::scheduling::{OperationSchedule, ScheduledMode};
use tracing::{debug, info, warn};

/// Check if debug logging is enabled based on log level
fn is_debug_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

/// Calculate yesterday's average effective price for initial battery cost basis.
///
/// This provides a realistic starting cost for energy already in the battery at day start.
/// Falls back to today's average if no yesterday data is available.
///
/// # Arguments
/// * `time_block_prices` - All available price blocks (may include yesterday and today)
///
/// # Returns
/// Average effective price (CZK/kWh) to use as initial battery cost basis
fn calculate_initial_battery_price(time_block_prices: &[TimeBlockPrice]) -> f32 {
    let today = Utc::now().date_naive();
    let yesterday = today - chrono::Duration::days(1);

    // Try to find yesterday's blocks
    let yesterday_blocks: Vec<&TimeBlockPrice> = time_block_prices
        .iter()
        .filter(|b| b.block_start.date_naive() == yesterday)
        .collect();

    if !yesterday_blocks.is_empty() {
        let avg: f32 = yesterday_blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .sum::<f32>()
            / yesterday_blocks.len() as f32;
        debug!(
            "Using yesterday's average price for initial battery cost: {:.3} CZK/kWh ({} blocks)",
            avg,
            yesterday_blocks.len()
        );
        return avg;
    }

    // Fall back to today's average
    if !time_block_prices.is_empty() {
        let avg: f32 = time_block_prices
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .sum::<f32>()
            / time_block_prices.len() as f32;
        debug!(
            "No yesterday data available, using today's average for initial battery cost: {:.3} CZK/kWh",
            avg
        );
        return avg;
    }

    // Ultimate fallback
    3.0 // Reasonable default ~3 CZK/kWh
}

/// Calculate effective prices (spot price + grid fees) for all time blocks
///
/// Uses the GlobalHdoCache to determine whether each block is in low or high tariff period,
/// then adds the appropriate grid fee from PricingConfig to the spot price.
///
/// # Arguments
/// * `time_block_prices` - Mutable slice of price blocks to update
/// * `hdo_cache` - Optional global HDO cache for tariff period lookup
/// * `pricing_config` - Pricing configuration with grid fee amounts
///
/// # Note
/// If HDO cache is not available or returns None, falls back to spot_buy_fee_czk
pub fn calculate_effective_prices(
    time_block_prices: &mut [TimeBlockPrice],
    hdo_cache: Option<&crate::resources::GlobalHdoCache>,
    pricing_config: &PricingConfig,
) {
    for block in time_block_prices.iter_mut() {
        let grid_fee = if let Some(cache) = hdo_cache {
            // Try to get HDO tariff period from cache
            match cache.is_low_tariff(block.block_start) {
                Some(true) => {
                    // Low tariff period
                    pricing_config.hdo_low_tariff_czk
                }
                Some(false) => {
                    // High tariff period
                    pricing_config.hdo_high_tariff_czk
                }
                None => {
                    // HDO data not available for this time, use fallback
                    warn!(
                        "HDO tariff data not available for {}, using fallback spot_buy_fee",
                        block.block_start
                    );
                    pricing_config.spot_buy_fee_czk
                }
            }
        } else {
            // No HDO cache available, use fallback
            pricing_config.spot_buy_fee_czk
        };

        // Calculate effective price = spot price + grid fee
        block.effective_price_czk_per_kwh = block.price_czk_per_kwh + grid_fee;
    }

    debug!(
        "Calculated effective prices for {} blocks using HDO tariff data",
        time_block_prices.len()
    );
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
/// * `backup_discharge_min_soc` - Minimum SOC from HA sensor (backup_discharge_min_soc)
/// * `grid_import_today_kwh` - Optional grid import energy consumed today (kWh)
/// * `plugin_manager` - The shared plugin manager with registered strategies
/// * `solar_forecast_total_today_kwh` - Total solar forecast for today (kWh)
/// * `solar_forecast_remaining_today_kwh` - Remaining solar forecast for today (kWh)
/// * `solar_forecast_tomorrow_kwh` - Solar forecast for tomorrow (kWh)
/// * `user_control` - Optional user control state for overrides and restrictions
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
    backup_discharge_min_soc: f32,
    grid_import_today_kwh: Option<f32>,
    plugin_manager: &PluginManager,
    hdo_raw_data: Option<String>,
    solar_forecast_total_today_kwh: f32,
    solar_forecast_remaining_today_kwh: f32,
    solar_forecast_tomorrow_kwh: f32,
    user_control: Option<&UserControlState>,
    hourly_consumption_profile: Option<&[f32; 24]>,
) -> OperationSchedule {
    if time_block_prices.is_empty() {
        info!("Cannot generate schedule from empty price data");
        return OperationSchedule::default();
    }
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

    // Initialize battery cost tracking for arbitrage profit calculation
    // battery_energy_kwh: Current energy stored in battery
    // battery_total_cost_czk: Total cost of energy currently in battery
    let initial_battery_price = calculate_initial_battery_price(time_block_prices);
    let battery_capacity = control_config.battery_capacity_kwh;
    let mut battery_energy_kwh = (current_battery_soc / 100.0) * battery_capacity;
    let mut battery_total_cost_czk = battery_energy_kwh * initial_battery_price;

    debug!(
        "Scheduling for {} blocks (filtered {} past blocks, current SOC: {:.1}%)",
        relevant_blocks.len(),
        time_block_prices.len() - relevant_blocks.len(),
        current_battery_soc
    );
    debug!(
        "Initial battery state: {:.2} kWh @ {:.3} CZK/kWh avg (total cost: {:.2} CZK)",
        battery_energy_kwh, initial_battery_price, battery_total_cost_czk
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

        // Calculate current average charge price for arbitrage tracking
        let avg_charge_price = if battery_energy_kwh > 0.0 {
            battery_total_cost_czk / battery_energy_kwh
        } else {
            initial_battery_price
        };

        let request = create_evaluation_request(
            price_block,
            &remaining_blocks,
            control_config,
            temp_predicted_soc,
            solar_kwh,
            consumption_kwh,
            export_price,
            backup_discharge_min_soc,
            grid_import_today_kwh,
            hdo_raw_data.clone(),
            solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh,
            avg_charge_price,
            hourly_consumption_profile,
        );

        let decision = plugin_manager.evaluate(&request);
        let evaluation = convert_decision_to_evaluation(&decision, &request);
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

        // Calculate current average charge price for arbitrage tracking
        let avg_charge_price = if battery_energy_kwh > 0.0 {
            battery_total_cost_czk / battery_energy_kwh
        } else {
            initial_battery_price
        };

        // Check for user-defined fixed time slot FIRST
        // If a fixed slot covers this block, use it instead of strategy evaluation
        if let Some(uc) = user_control
            && let Some(fixed_slot) = uc.get_fixed_slot_at(price_block.block_start)
        {
            // User override takes precedence - skip normal evaluation
            debug!(
                "Block {}: Using user override slot {} ({:?})",
                local_idx, fixed_slot.id, fixed_slot.mode
            );

            // Still update predicted SOC based on the fixed mode
            let mut fixed_evaluation = BlockEvaluation::new(
                price_block.block_start,
                price_block.duration_minutes,
                fixed_slot.mode,
                "User Override".to_string(),
            );
            fixed_evaluation.reason = fixed_slot
                .note
                .clone()
                .unwrap_or_else(|| "User-locked time slot".to_string());
            fixed_evaluation.decision_uid = Some(format!("user_override:{}", fixed_slot.id));

            predicted_soc = update_soc_prediction(
                predicted_soc,
                &fixed_evaluation,
                control_config,
                solar_kwh,
                consumption_kwh,
            );

            scheduled_blocks.push(ScheduledMode {
                block_start: fixed_evaluation.block_start,
                duration_minutes: fixed_evaluation.duration_minutes,
                target_inverters: if schedule_config.target_inverters.is_empty() {
                    None
                } else {
                    Some(schedule_config.target_inverters.clone())
                },
                mode: fixed_evaluation.mode,
                reason: format!("User Override - {}", fixed_evaluation.reason),
                decision_uid: fixed_evaluation.decision_uid.clone(),
                debug_info: None,
            });

            continue; // Skip normal evaluation for this block
        }

        // Create evaluation request for plugin manager
        let request = create_evaluation_request(
            price_block,
            &remaining_blocks,
            control_config,
            soc_for_evaluation,
            solar_kwh,
            consumption_kwh,
            export_price,
            backup_discharge_min_soc,
            grid_import_today_kwh,
            hdo_raw_data.clone(),
            solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh,
            avg_charge_price,
            hourly_consumption_profile,
        );

        // Get decision from plugin manager
        let decision = plugin_manager.evaluate(&request);
        let mut evaluation = convert_decision_to_evaluation(&decision, &request);

        // Apply user control restrictions (disallow charge/discharge)
        if let Some(uc) = user_control
            && !uc.is_mode_allowed(evaluation.mode)
        {
            let original_mode = evaluation.mode;
            let original_reason = evaluation.reason.clone();

            // Convert disallowed mode to SelfUse (default safe mode)
            evaluation.mode = schedule_config.default_battery_mode;
            evaluation.reason = format!(
                "{} (converted from {:?} - user restriction: {})",
                original_reason,
                original_mode,
                if uc.disallow_charge && original_mode == InverterOperationMode::ForceCharge {
                    "charge disallowed"
                } else {
                    "discharge disallowed"
                }
            );

            debug!(
                "Block {}: Mode {:?} restricted by user, using {:?} instead",
                local_idx, original_mode, evaluation.mode
            );
        }

        // Update battery cost tracking based on the decision
        let current_price = price_block.effective_price_czk_per_kwh;
        match evaluation.mode {
            InverterOperationMode::ForceCharge => {
                // Energy charged = max charge rate * 0.25 hours (15 min block)
                let charge_kwh = control_config.max_battery_charge_rate_kw * 0.25;
                let charge_cost = charge_kwh * current_price;
                battery_energy_kwh += charge_kwh * control_config.battery_efficiency;
                battery_total_cost_czk += charge_cost;
            }
            InverterOperationMode::ForceDischarge | InverterOperationMode::SelfUse => {
                // Estimate discharge based on mode
                let discharge_kwh = if evaluation.mode == InverterOperationMode::ForceDischarge {
                    // Force discharge at export rate
                    (control_config.maximum_export_power_w as f32 / 1000.0) * 0.25
                } else {
                    // Self-use: discharge equals net consumption (consumption - solar)
                    (consumption_kwh - solar_kwh).max(0.0)
                };

                if discharge_kwh > 0.0 && battery_energy_kwh > 0.0 {
                    // Remove proportional cost when discharging
                    let discharge_ratio = (discharge_kwh / battery_energy_kwh).min(1.0);
                    battery_total_cost_czk *= 1.0 - discharge_ratio;
                    battery_energy_kwh = (battery_energy_kwh - discharge_kwh).max(0.0);
                }
            }
            InverterOperationMode::BackUpMode => {
                // Similar to SelfUse but more conservative
                let discharge_kwh = (consumption_kwh - solar_kwh).max(0.0);
                if discharge_kwh > 0.0 && battery_energy_kwh > 0.0 {
                    let discharge_ratio = (discharge_kwh / battery_energy_kwh).min(1.0);
                    battery_total_cost_czk *= 1.0 - discharge_ratio;
                    battery_energy_kwh = (battery_energy_kwh - discharge_kwh).max(0.0);
                }
            }
        }

        // Update predicted SOC for next block
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
            decision_uid: evaluation.decision_uid.clone(),
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

    debug!(
        "Generated economic schedule: {} blocks, {} charge, {} discharge, {} self-use, total expected profit: {:.2} CZK",
        schedule.scheduled_blocks.len(),
        charge_count,
        discharge_count,
        self_use_count,
        total_profit
    );

    // Post-process to ensure minimum consecutive charge blocks
    // Simply remove any force-charge sequences shorter than the minimum required
    remove_short_force_sequences(&mut schedule, control_config);

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
            decision_uid: None, // Legacy scheduler doesn't generate decision UIDs
            debug_info: None,   // Legacy scheduler doesn't generate debug info
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

/// Remove force-charge/discharge sequences that are shorter than the minimum required
/// consecutive blocks. This ensures we respect the user's minimum duration setting
/// without extending sequences into expensive periods.
///
/// # Arguments
/// * `schedule` - Mutable schedule to process
/// * `config` - Control configuration (includes min_consecutive_force_blocks)
fn remove_short_force_sequences(schedule: &mut OperationSchedule, config: &ControlConfig) {
    if schedule.scheduled_blocks.is_empty() {
        return;
    }

    let mut changes = 0;
    let min_consecutive = config.min_consecutive_force_blocks;

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

        // If sequence is too short, convert to self-use
        if consecutive < min_consecutive {
            let mode_name = if mode == InverterOperationMode::ForceCharge {
                "Force-Charge"
            } else {
                "Force-Discharge"
            };

            for j in 0..consecutive {
                schedule.scheduled_blocks[i + j].mode = config.default_battery_mode;
                schedule.scheduled_blocks[i + j].reason = format!(
                    "Converted from {} to Self-Use ({}-block sequence < {} min required)",
                    mode_name, consecutive, min_consecutive
                );
            }
            changes += consecutive;
        }

        i += consecutive;
    }

    if changes > 0 {
        debug!(
            "Removed {} short force operation blocks (< {} consecutive blocks required)",
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

/// Create an evaluation request for the plugin manager
#[expect(clippy::too_many_arguments)]
fn create_evaluation_request(
    price_block: &TimeBlockPrice,
    remaining_blocks: &[TimeBlockPrice],
    control_config: &ControlConfig,
    current_soc: f32,
    solar_kwh: f32,
    consumption_kwh: f32,
    export_price: f32,
    backup_discharge_min_soc: f32,
    grid_import_today_kwh: Option<f32>,
    hdo_raw_data: Option<String>,
    solar_forecast_total_today_kwh: f32,
    solar_forecast_remaining_today_kwh: f32,
    solar_forecast_tomorrow_kwh: f32,
    battery_avg_charge_price_czk_per_kwh: f32,
    hourly_consumption_profile: Option<&[f32; 24]>,
) -> EvaluationRequest {
    EvaluationRequest {
        block: PriceBlock {
            block_start: price_block.block_start,
            duration_minutes: price_block.duration_minutes,
            price_czk_per_kwh: price_block.price_czk_per_kwh,
            effective_price_czk_per_kwh: price_block.effective_price_czk_per_kwh,
        },
        battery: BatteryState {
            current_soc_percent: current_soc,
            capacity_kwh: control_config.battery_capacity_kwh,
            max_charge_rate_kw: control_config.max_battery_charge_rate_kw,
            min_soc_percent: control_config.hardware_min_battery_soc,
            max_soc_percent: 100.0,
            efficiency: control_config.battery_efficiency,
            wear_cost_czk_per_kwh: control_config.battery_wear_cost_czk_per_kwh,
        },
        forecast: ForecastData {
            solar_kwh,
            consumption_kwh,
            grid_export_price_czk_per_kwh: export_price,
        },
        all_blocks: remaining_blocks
            .iter()
            .map(|b| PriceBlock {
                block_start: b.block_start,
                duration_minutes: b.duration_minutes,
                price_czk_per_kwh: b.price_czk_per_kwh,
                effective_price_czk_per_kwh: b.effective_price_czk_per_kwh,
            })
            .collect(),
        historical: HistoricalData {
            grid_import_today_kwh,
            consumption_today_kwh: None, // TODO: Track actual consumption
            hourly_consumption_profile: hourly_consumption_profile.map(|p| p.to_vec()),
        },
        backup_discharge_min_soc,
        hdo_raw_data,
        solar_forecast_total_today_kwh,
        solar_forecast_remaining_today_kwh,
        solar_forecast_tomorrow_kwh,
        battery_avg_charge_price_czk_per_kwh,
    }
}

/// Convert a plugin decision back to a BlockEvaluation for compatibility
fn convert_decision_to_evaluation(
    decision: &BlockDecision,
    request: &EvaluationRequest,
) -> BlockEvaluation {
    use crate::strategy::{Assumptions, EnergyFlows};
    use fluxion_types::scheduling::{BlockDebugInfo, StrategyEvaluation};

    let mode = match decision.mode {
        OperationMode::SelfUse => InverterOperationMode::SelfUse,
        OperationMode::ForceCharge => InverterOperationMode::ForceCharge,
        OperationMode::ForceDischarge => InverterOperationMode::ForceDischarge,
        OperationMode::BackUpMode => InverterOperationMode::BackUpMode,
    };

    let net_profit = decision.expected_profit_czk.unwrap_or(0.0);

    // Use actual strategy name from decision, falling back to priority-based name
    let strategy_name = decision
        .strategy_name
        .clone()
        .unwrap_or_else(|| format!("Plugin (priority {})", decision.priority));

    BlockEvaluation {
        block_start: decision.block_start,
        duration_minutes: decision.duration_minutes,
        mode,
        revenue_czk: if net_profit > 0.0 { net_profit } else { 0.0 },
        cost_czk: if net_profit < 0.0 { -net_profit } else { 0.0 },
        net_profit_czk: net_profit,
        energy_flows: EnergyFlows::default(),
        assumptions: Assumptions {
            solar_forecast_kwh: request.forecast.solar_kwh,
            consumption_forecast_kwh: request.forecast.consumption_kwh,
            current_battery_soc: request.battery.current_soc_percent,
            battery_efficiency: request.battery.efficiency,
            battery_wear_cost_czk_per_kwh: request.battery.wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: request.block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: request.forecast.grid_export_price_czk_per_kwh,
        },
        reason: decision.reason.clone(),
        strategy_name: strategy_name.clone(),
        decision_uid: decision.decision_uid.clone(),
        debug_info: if is_debug_enabled() {
            Some(BlockDebugInfo {
                evaluated_strategies: vec![StrategyEvaluation {
                    strategy_name,
                    mode,
                    net_profit_czk: net_profit,
                    reason: decision.reason.clone(),
                }],
                winning_reason: decision.reason.clone(),
                conditions: vec![
                    format!("SOC: {:.1}%", request.battery.current_soc_percent),
                    format!("Price: {:.3} CZK/kWh", request.block.price_czk_per_kwh),
                ],
            })
        } else {
            None
        },
        arbitrage_profit_czk: 0.0, // Calculated by strategy if discharge occurs
    }
}
