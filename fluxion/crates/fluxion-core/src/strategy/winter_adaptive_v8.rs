// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! # Winter Adaptive V8 Strategy - Top-N Peak Discharge Optimizer
//!
//! **Status:** Production - Aggressive peak-hour discharge maximization
//!
//! ## Overview
//!
//! V8 is designed to maximize savings by aggressively discharging during the highest price peaks
//! while ensuring the battery has sufficient capacity to reach those peaks.
//!
//! ## Key Design Principles
//!
//! 1. **Top-N Peak Targeting**
//!    - User configurable number of top price blocks (default: 8 blocks = 2 hours)
//!    - Only discharge during these absolute highest price periods
//!    - Ensures concentrated discharge when prices are maximum
//!
//! 2. **Predictive Battery Management**
//!    - Simulates battery SOC throughout the planning horizon
//!    - Only schedules discharge if battery will have capacity during peaks
//!    - Prevents early depletion before afternoon/evening peaks
//!
//! 3. **Minimum Spread Requirement**
//!    - Discharge only if (sell_price - buy_price) >= 3 CZK (configurable)
//!    - Uses effective prices (including grid fees and round-trip efficiency)
//!    - Ensures every discharge cycle is meaningfully profitable
//!
//! 4. **Smart Charging**
//!    - Charges in cheapest available blocks
//!    - Reserves battery capacity specifically for top peak discharge
//!    - Avoids unnecessary charging if peaks aren't profitable enough
//!
//! ## Algorithm
//!
//! ### Phase 1: Price Analysis
//!
//! - Find cheapest blocks for charging (bottom 25%)
//! - Find top N most expensive blocks for potential discharge
//! - Calculate average charge price and peak discharge price
//!
//! ### Phase 2: Profitability Check
//!
//! - Calculate spread: avg_peak_price - avg_charge_price
//! - Apply round-trip efficiency adjustment
//! - Only proceed if spread >= min_discharge_spread_czk (default 3.0)
//!
//! ### Phase 3: Battery SOC Prediction
//!
//! - Simulate battery state through each block
//! - Account for consumption, charging, and discharging
//! - Verify battery will have capacity during peak blocks
//!
//! ### Phase 4: Schedule Generation
//!
//! - Charge in cheapest blocks up to target SOC
//! - Discharge ONLY in top N peak blocks
//! - Self-use for all other blocks

use serde::{Deserialize, Serialize};

use crate::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive V8 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV8Config {
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    /// Target battery SOC (%) - charge up to this level
    pub target_battery_soc: f32,

    /// Hardware minimum battery SOC (%) - never discharge below this
    pub min_discharge_soc: f32,

    // === Top-N Peak Discharge ===
    /// Number of top price blocks to target for discharge
    /// Default: 8 blocks = 2 hours of discharge during absolute peaks
    pub top_discharge_blocks_count: usize,

    /// Minimum price spread (CZK) between discharge and charge for profitability
    /// This is: (avg_peak_price - avg_charge_price) × efficiency >= this value
    /// Default: 3.0 CZK
    pub min_discharge_spread_czk: f32,

    /// Round-trip battery efficiency (charge × discharge efficiency)
    /// Default: 0.90 (90% round-trip efficiency)
    pub battery_round_trip_efficiency: f32,

    // === Charging Configuration ===
    /// Percentile for cheap blocks (0.0-1.0) - charge in bottom X%
    /// Default: 0.25 (bottom 25%)
    pub cheap_block_percentile: f32,

    // === Consumption Prediction ===
    /// Average household consumption (kWh) per 15-minute block
    /// Used to predict SOC changes throughout the day
    /// Default: 0.25 kWh (1 kW average load)
    pub avg_consumption_per_block_kwh: f32,

    // === Export Policy ===
    /// Minimum price spread (CZK) to allow grid export instead of home use
    /// Default: 5.0 CZK (must be significantly more profitable than home use)
    pub min_export_spread_czk: f32,

    /// Minimum predicted SOC (%) after discharge to allow grid export
    /// Default: 50% (ensures battery reserve for home consumption)
    pub min_soc_after_export: f32,

    // === Safety ===
    /// Enable negative price handling (charge when getting paid)
    pub negative_price_handling_enabled: bool,

    // === Solar-Aware Charging ===
    /// Enable solar-aware charge reduction
    /// When enabled, reduces grid charging based on expected solar production
    /// Default: true
    pub solar_aware_charging_enabled: bool,

    /// Minimum number of grid charge blocks to always schedule (safety margin)
    /// Even with high solar forecast, keep this many charge blocks as backup
    /// Default: 2 (30 minutes of grid charging)
    pub min_grid_charge_blocks: usize,

    /// Price threshold (CZK/kWh) below which we always charge from grid
    /// Opportunistic charging regardless of solar forecast - very cheap power is worth grabbing
    /// Default: 1.5 CZK/kWh
    pub opportunistic_charge_threshold_czk: f32,

    /// Factor for how much battery capacity to reserve for solar (0.0-1.0)
    /// E.g., 0.8 means if 8 kWh solar expected and 10 kWh battery available, reserve 6.4 kWh
    /// Lower values are more conservative (more grid charging as backup)
    /// Default: 0.7 (reserve 70% of expected solar capacity)
    pub solar_capacity_reservation_factor: f32,

    /// Minimum solar forecast (kWh) to trigger charge reduction
    /// Below this threshold, charge normally from grid
    /// Default: 2.0 kWh
    pub min_solar_for_reduction_kwh: f32,
}

impl Default for WinterAdaptiveV8Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 8,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            top_discharge_blocks_count: 8, // 2 hours of peak discharge
            min_discharge_spread_czk: 3.0,
            battery_round_trip_efficiency: 0.90,
            cheap_block_percentile: 0.25,
            avg_consumption_per_block_kwh: 0.25,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            negative_price_handling_enabled: true,
            // Solar-aware charging defaults
            solar_aware_charging_enabled: true,
            min_grid_charge_blocks: 2,
            opportunistic_charge_threshold_czk: 1.5,
            solar_capacity_reservation_factor: 0.7,
            min_solar_for_reduction_kwh: 2.0,
        }
    }
}

/// Scheduled action for a specific block
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScheduledAction {
    /// Charge during this block
    Charge,
    /// Discharge during this block (in top N peaks)
    Discharge,
    /// No scheduled action - use self-use mode
    SelfUse,
}

/// Solar-aware charging calculation result
#[derive(Debug, Clone)]
struct SolarChargeAdjustment {
    /// How much energy (kWh) we expect solar to contribute
    expected_solar_contribution_kwh: f32,
    /// How much grid charging (kWh) we still need
    #[allow(dead_code)]
    required_grid_charge_kwh: f32,
    /// Number of grid charge blocks needed (adjusted for solar)
    required_grid_charge_blocks: usize,
    /// Original number of charge blocks (without solar adjustment)
    #[allow(dead_code)]
    original_charge_blocks: usize,
    /// Blocks saved due to solar forecast
    blocks_saved: usize,
}

/// Planning result for the entire horizon
#[derive(Debug)]
struct DischargePlan {
    /// Indices of blocks where we should charge
    charge_blocks: Vec<usize>,
    /// Indices of top N blocks where we should discharge
    discharge_blocks: Vec<usize>,
    /// Average charge price
    avg_charge_price: f32,
    /// Average discharge price
    #[allow(dead_code)]
    avg_discharge_price: f32,
    /// Net profit per kWh (after efficiency)
    profit_per_kwh: f32,
    /// Whether discharge is profitable enough
    is_profitable: bool,
    /// Solar adjustment info (if solar-aware charging enabled)
    solar_adjustment: Option<SolarChargeAdjustment>,
}

pub struct WinterAdaptiveV8Strategy {
    config: WinterAdaptiveV8Config,
}

impl WinterAdaptiveV8Strategy {
    pub fn new(config: WinterAdaptiveV8Config) -> Self {
        Self { config }
    }

    /// Find the top N most expensive blocks for discharge
    fn find_top_n_expensive_blocks(&self, blocks: &[TimeBlockPrice], n: usize) -> Vec<usize> {
        if blocks.is_empty() || n == 0 {
            return Vec::new();
        }

        // Create (index, price) pairs
        let mut indexed_blocks: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        // Sort by price descending (highest first)
        indexed_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take top N and return their indices
        indexed_blocks
            .into_iter()
            .take(n)
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Find cheapest blocks for charging
    fn find_cheapest_blocks(&self, blocks: &[TimeBlockPrice]) -> Vec<usize> {
        if blocks.is_empty() {
            return Vec::new();
        }

        let n = blocks.len();
        let cheap_count = ((n as f32) * self.config.cheap_block_percentile).ceil() as usize;

        let mut indexed_blocks: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        // Sort by price ascending (cheapest first)
        indexed_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        indexed_blocks
            .into_iter()
            .take(cheap_count)
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Calculate average price for a set of blocks
    fn calculate_avg_price(&self, blocks: &[TimeBlockPrice], indices: &[usize]) -> f32 {
        if indices.is_empty() {
            return 0.0;
        }

        let sum: f32 = indices
            .iter()
            .filter_map(|&i| blocks.get(i))
            .map(|b| b.effective_price_czk_per_kwh)
            .sum();

        sum / indices.len() as f32
    }

    /// Predict battery SOC at each block, considering charge/discharge schedule
    /// Returns the SOC at the START of each block (before any operations)
    fn predict_battery_soc(
        &self,
        blocks: &[TimeBlockPrice],
        charge_blocks: &[usize],
        discharge_blocks: &[usize],
        initial_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
    ) -> Vec<f32> {
        let mut soc_predictions = vec![initial_soc; blocks.len()];
        let mut current_soc = initial_soc;

        #[expect(clippy::needless_range_loop)]
        for i in 0..blocks.len() {
            // Save SOC at START of this block
            soc_predictions[i] = current_soc;

            // Now apply operations for this block
            if charge_blocks.contains(&i) && current_soc < self.config.target_battery_soc {
                let charge_kwh = max_charge_rate_kw * 0.25; // 15-minute block
                let charge_soc_delta = (charge_kwh / battery_capacity_kwh)
                    * 100.0
                    * self.config.battery_round_trip_efficiency;
                current_soc = (current_soc + charge_soc_delta).min(self.config.target_battery_soc);
                // During charge, grid also covers consumption, so no SOC impact
            } else if discharge_blocks.contains(&i) && current_soc > self.config.min_discharge_soc {
                // During discharge blocks, we discharge more than just consumption
                let discharge_kwh = max_charge_rate_kw * 0.25; // 15-minute block
                let discharge_soc_delta = (discharge_kwh / battery_capacity_kwh) * 100.0;
                current_soc =
                    (current_soc - discharge_soc_delta).max(self.config.min_discharge_soc);
            } else {
                // Self-use: battery covers consumption naturally
                let consumption_kwh = self.config.avg_consumption_per_block_kwh;
                let consumption_soc_delta = (consumption_kwh / battery_capacity_kwh) * 100.0;
                current_soc =
                    (current_soc - consumption_soc_delta).max(self.config.min_discharge_soc);
            }
        }

        soc_predictions
    }

    /// Generate the discharge plan for the planning horizon
    ///
    /// With solar-aware charging:
    /// 1. Find ALL cheapest blocks (percentile-based)
    /// 2. Find opportunistic blocks (very cheap, always charge)
    /// 3. Calculate solar adjustment to reduce grid charging
    /// 4. Select final charge blocks: opportunistic + enough cheap blocks to meet adjusted need
    fn generate_discharge_plan(
        &self,
        blocks: &[TimeBlockPrice],
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
        solar_remaining_today_kwh: f32,
    ) -> DischargePlan {
        // Step 1: Find ALL cheapest blocks for potential charging (before solar adjustment)
        let all_cheap_blocks = self.find_cheapest_blocks(blocks);
        let original_charge_count = all_cheap_blocks.len();

        // Step 2: Find opportunistic charge blocks (very cheap prices - always charge)
        let opportunistic_blocks = self.find_opportunistic_charge_blocks(blocks);

        // Step 3: Calculate solar adjustment
        let solar_adjustment = self.calculate_solar_charge_adjustment(
            current_soc,
            battery_capacity_kwh,
            max_charge_rate_kw,
            solar_remaining_today_kwh,
            original_charge_count,
        );

        // Step 4: Build final charge block list
        // Start with opportunistic blocks (always included)
        let mut charge_blocks: Vec<usize> = opportunistic_blocks.clone();

        // Add cheapest blocks up to the solar-adjusted need
        // Sort cheap blocks by price to pick the absolute cheapest
        let mut sorted_cheap: Vec<(usize, f32)> = all_cheap_blocks
            .iter()
            .filter(|idx| !charge_blocks.contains(idx)) // Don't double-count opportunistic
            .filter_map(|&idx| {
                blocks
                    .get(idx)
                    .map(|b| (idx, b.effective_price_czk_per_kwh))
            })
            .collect();
        sorted_cheap.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Add cheapest blocks until we reach the adjusted need
        let blocks_still_needed = solar_adjustment
            .required_grid_charge_blocks
            .saturating_sub(charge_blocks.len());

        for (idx, _) in sorted_cheap.into_iter().take(blocks_still_needed) {
            charge_blocks.push(idx);
        }

        // Ensure we have at least min_grid_charge_blocks
        if charge_blocks.len() < self.config.min_grid_charge_blocks {
            // Add more cheap blocks if needed
            for &idx in &all_cheap_blocks {
                if !charge_blocks.contains(&idx) {
                    charge_blocks.push(idx);
                    if charge_blocks.len() >= self.config.min_grid_charge_blocks {
                        break;
                    }
                }
            }
        }

        // Sort charge blocks by index for consistent ordering
        charge_blocks.sort();
        charge_blocks.dedup();

        // Step 5: Find top N most expensive blocks for discharge
        let top_n = self.config.top_discharge_blocks_count.min(blocks.len());
        let discharge_blocks = self.find_top_n_expensive_blocks(blocks, top_n);

        // Step 6: Calculate average prices
        let avg_charge_price = self.calculate_avg_price(blocks, &charge_blocks);
        let avg_discharge_price = self.calculate_avg_price(blocks, &discharge_blocks);

        // Step 7: Check profitability
        let gross_spread = avg_discharge_price - avg_charge_price;
        let profit_per_kwh = gross_spread * self.config.battery_round_trip_efficiency;
        let is_profitable = profit_per_kwh >= self.config.min_discharge_spread_czk;

        // Step 8: Predict SOC to verify battery will have capacity during peaks
        let soc_predictions = self.predict_battery_soc(
            blocks,
            &charge_blocks,
            &discharge_blocks,
            current_soc,
            battery_capacity_kwh,
            max_charge_rate_kw,
        );

        // Step 9: Verify we have battery capacity during discharge blocks
        let has_capacity_for_discharge = discharge_blocks.iter().all(|&idx| {
            soc_predictions.get(idx).copied().unwrap_or(0.0) > self.config.min_discharge_soc + 5.0
        });

        DischargePlan {
            charge_blocks,
            discharge_blocks: if is_profitable && has_capacity_for_discharge {
                discharge_blocks
            } else {
                Vec::new() // Don't discharge if not profitable or no capacity
            },
            avg_charge_price,
            avg_discharge_price,
            profit_per_kwh,
            is_profitable: is_profitable && has_capacity_for_discharge,
            solar_adjustment: Some(solar_adjustment),
        }
    }

    /// Generate the schedule for all blocks
    fn generate_schedule(
        &self,
        blocks: &[TimeBlockPrice],
        plan: &DischargePlan,
    ) -> Vec<ScheduledAction> {
        let mut schedule = vec![ScheduledAction::SelfUse; blocks.len()];

        for &block_idx in &plan.charge_blocks {
            if block_idx < schedule.len() {
                schedule[block_idx] = ScheduledAction::Charge;
            }
        }

        for &block_idx in &plan.discharge_blocks {
            if block_idx < schedule.len() {
                schedule[block_idx] = ScheduledAction::Discharge;
            }
        }

        schedule
    }

    /// Decide whether discharge should go to grid or home
    fn should_export_to_grid(
        &self,
        current_soc: f32,
        discharge_price: f32,
        min_price_in_horizon: f32,
        predicted_consumption_kwh: f32,
        battery_capacity_kwh: f32,
    ) -> bool {
        // Calculate spread
        let spread = discharge_price - min_price_in_horizon;

        // Check if spread is large enough
        if spread < self.config.min_export_spread_czk {
            return false;
        }

        // Predict SOC after discharge (assume we discharge one block's worth)
        let discharge_kwh = battery_capacity_kwh * 0.1; // ~10% per block at max rate
        let predicted_soc_after = current_soc
            - (discharge_kwh / battery_capacity_kwh * 100.0)
            - (predicted_consumption_kwh / battery_capacity_kwh * 100.0);

        // Only export if predicted SOC stays above threshold
        predicted_soc_after >= self.config.min_soc_after_export
    }

    /// Calculate minimum price in horizon
    fn min_price(&self, blocks: &[TimeBlockPrice]) -> f32 {
        blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .fold(f32::INFINITY, f32::min)
    }

    /// Calculate solar-adjusted charging needs
    ///
    /// This is the core of solar-aware optimization:
    /// 1. Calculate how much battery capacity needs to be filled
    /// 2. Estimate how much solar can contribute (with safety factor)
    /// 3. Determine remaining grid charge need
    /// 4. Return the adjusted number of charge blocks needed
    fn calculate_solar_charge_adjustment(
        &self,
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
        solar_remaining_today_kwh: f32,
        original_charge_blocks: usize,
    ) -> SolarChargeAdjustment {
        // Calculate battery capacity that needs filling
        let soc_gap = (self.config.target_battery_soc - current_soc).max(0.0);
        let capacity_to_fill_kwh = (soc_gap / 100.0) * battery_capacity_kwh;

        // If solar forecast is below threshold, don't adjust
        if !self.config.solar_aware_charging_enabled
            || solar_remaining_today_kwh < self.config.min_solar_for_reduction_kwh
        {
            return SolarChargeAdjustment {
                expected_solar_contribution_kwh: 0.0,
                required_grid_charge_kwh: capacity_to_fill_kwh,
                required_grid_charge_blocks: original_charge_blocks,
                original_charge_blocks,
                blocks_saved: 0,
            };
        }

        // Calculate expected solar contribution (with reservation factor for safety)
        // We don't trust the forecast 100% - apply the reservation factor
        let expected_solar_contribution = (solar_remaining_today_kwh
            * self.config.solar_capacity_reservation_factor)
            .min(capacity_to_fill_kwh); // Can't contribute more than we need

        // Calculate remaining grid charge need
        let required_grid_charge_kwh =
            (capacity_to_fill_kwh - expected_solar_contribution).max(0.0);

        // Convert to number of blocks (each block charges at max_charge_rate_kw for 15 min = 0.25 hours)
        let charge_per_block_kwh =
            max_charge_rate_kw * 0.25 * self.config.battery_round_trip_efficiency;
        let calculated_blocks = if charge_per_block_kwh > 0.0 {
            (required_grid_charge_kwh / charge_per_block_kwh).ceil() as usize
        } else {
            original_charge_blocks
        };

        // Apply minimum safety margin
        let required_grid_charge_blocks = calculated_blocks.max(self.config.min_grid_charge_blocks);

        // Calculate blocks saved
        let blocks_saved = original_charge_blocks.saturating_sub(required_grid_charge_blocks);

        SolarChargeAdjustment {
            expected_solar_contribution_kwh: expected_solar_contribution,
            required_grid_charge_kwh,
            required_grid_charge_blocks,
            original_charge_blocks,
            blocks_saved,
        }
    }

    /// Find blocks with prices below the opportunistic threshold
    /// These blocks should ALWAYS be included in charging, regardless of solar
    fn find_opportunistic_charge_blocks(&self, blocks: &[TimeBlockPrice]) -> Vec<usize> {
        blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.effective_price_czk_per_kwh < self.config.opportunistic_charge_threshold_czk
                    || b.effective_price_czk_per_kwh < 0.0 // Always charge on negative prices
            })
            .map(|(i, _)| i)
            .collect()
    }
}

impl EconomicStrategy for WinterAdaptiveV8Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V8"
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse,
            self.name().to_string(),
        );

        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: 0.0, // V8 doesn't use wear cost (provable data only)
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available".to_string();
            return eval;
        };

        // Phase 1: Generate discharge plan (with solar-aware charging)
        let plan = self.generate_discharge_plan(
            all_blocks,
            context.current_battery_soc,
            context.control_config.battery_capacity_kwh,
            context.control_config.max_battery_charge_rate_kw,
            context.solar_forecast_remaining_today_kwh,
        );

        // Phase 2: Generate schedule
        let schedule = self.generate_schedule(all_blocks, &plan);

        // Find current block index
        let block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let current_action = schedule
            .get(block_index)
            .copied()
            .unwrap_or(ScheduledAction::SelfUse);
        let effective_price = context.price_block.effective_price_czk_per_kwh;
        let min_price = self.min_price(all_blocks);

        // Build summary with solar adjustment info
        let solar_info = if let Some(ref adj) = plan.solar_adjustment {
            if adj.blocks_saved > 0 {
                format!(
                    ", SOLAR: {:.1}kWh expected, {} blocks saved",
                    adj.expected_solar_contribution_kwh, adj.blocks_saved
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let summary = if plan.is_profitable {
            format!(
                "TOP-{} PEAKS: {:.2} CZK/kWh profit, {} charge/{} discharge blocks{}",
                self.config.top_discharge_blocks_count,
                plan.profit_per_kwh,
                plan.charge_blocks.len(),
                plan.discharge_blocks.len(),
                solar_info
            )
        } else {
            format!(
                "NO DISCHARGE: spread {:.2} < {:.2} CZK or insufficient capacity{}",
                plan.profit_per_kwh, self.config.min_discharge_spread_czk, solar_info
            )
        };

        // Phase 3: Execute based on schedule
        match current_action {
            ScheduledAction::Charge => {
                // Check for negative prices first (always charge if getting paid)
                if self.config.negative_price_handling_enabled && effective_price < 0.0 {
                    eval.mode = InverterOperationMode::ForceCharge;
                    eval.reason = format!(
                        "NEGATIVE PRICE CHARGE: {:.3} CZK/kWh (getting paid!) [{}]",
                        effective_price, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v8:negative_price".to_string());
                } else if context.current_battery_soc < self.config.target_battery_soc {
                    eval.mode = InverterOperationMode::ForceCharge;
                    eval.reason = format!(
                        "CHARGE: {:.3} CZK/kWh (avg: {:.2}) [{}]",
                        effective_price, plan.avg_charge_price, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v8:charge".to_string());

                    // Calculate energy flows accounting for available excess power
                    // Negative consumption means excess power is available (e.g., solar production)
                    let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                    let available_excess =
                        context.solar_forecast_kwh + (-context.consumption_forecast_kwh).max(0.0);
                    let grid_charge_needed = (charge_kwh - available_excess).max(0.0);

                    eval.energy_flows.battery_charge_kwh = charge_kwh;
                    eval.energy_flows.grid_import_kwh = grid_charge_needed;
                    eval.cost_czk = grid_charge_needed * effective_price;

                    // Export any excess we don't use for charging
                    let excess_after_charge = (available_excess - charge_kwh).max(0.0);
                    if excess_after_charge > 0.0 {
                        eval.energy_flows.grid_export_kwh = excess_after_charge;
                        eval.revenue_czk =
                            excess_after_charge * context.grid_export_price_czk_per_kwh;
                    }
                } else {
                    // Already at target SOC, use self-use
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: Battery at target SOC ({:.1}%) [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v8:self_use".to_string());
                }
            }

            ScheduledAction::Discharge => {
                // Check if we should discharge
                if context.current_battery_soc > self.config.min_discharge_soc {
                    // Determine if we should export to grid or use for home
                    let should_export = self.should_export_to_grid(
                        context.current_battery_soc,
                        effective_price,
                        min_price,
                        context.consumption_forecast_kwh,
                        context.control_config.battery_capacity_kwh,
                    );

                    if should_export {
                        // Export to grid - aggressive discharge
                        eval.mode = InverterOperationMode::ForceDischarge;
                        eval.reason = format!(
                            "PEAK DISCHARGE→GRID: {:.3} CZK/kWh (profit: {:.2}) [{}]",
                            effective_price, plan.profit_per_kwh, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v8:discharge_grid".to_string());

                        let discharge_kwh =
                            context.control_config.max_battery_charge_rate_kw * 0.25;
                        eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                        eval.energy_flows.grid_export_kwh = discharge_kwh;
                        eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
                    } else {
                        // Home use - let battery cover consumption
                        eval.mode = InverterOperationMode::SelfUse;
                        eval.reason = format!(
                            "PEAK DISCHARGE→HOME: {:.3} CZK/kWh (profit: {:.2}) [{}]",
                            effective_price, plan.profit_per_kwh, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v8:discharge_home".to_string());

                        // Calculate net consumption accounting for solar
                        // Negative consumption means excess power (solar > load)
                        let net_consumption =
                            context.consumption_forecast_kwh - context.solar_forecast_kwh;

                        if net_consumption > 0.0 {
                            // Battery covers home consumption
                            let usable_battery_kwh = ((context.current_battery_soc
                                - self.config.min_discharge_soc)
                                .max(0.0)
                                / 100.0)
                                * context.control_config.battery_capacity_kwh;

                            let battery_discharge = usable_battery_kwh.min(net_consumption);
                            eval.energy_flows.battery_discharge_kwh = battery_discharge;

                            if battery_discharge >= net_consumption {
                                // Battery fully covers load - revenue is avoided grid cost
                                eval.revenue_czk = net_consumption * effective_price;
                            } else {
                                // Partial coverage
                                eval.revenue_czk = battery_discharge * effective_price;
                                let grid_needed = net_consumption - battery_discharge;
                                eval.cost_czk = grid_needed * effective_price;
                                eval.energy_flows.grid_import_kwh = grid_needed;
                            }
                        } else {
                            // Excess power available - charge battery and export
                            let excess = -net_consumption;
                            let battery_capacity = context.control_config.battery_capacity_kwh;
                            let available_charge_capacity = (battery_capacity
                                * (context.control_config.max_battery_soc / 100.0)
                                - battery_capacity * (context.current_battery_soc / 100.0))
                                .max(0.0);
                            let max_charge_rate =
                                context.control_config.max_battery_charge_rate_kw * 0.25;
                            let charge_amount =
                                excess.min(available_charge_capacity).min(max_charge_rate);

                            eval.energy_flows.battery_charge_kwh = charge_amount;

                            let export_amount = excess - charge_amount;
                            if export_amount > 0.0 {
                                eval.energy_flows.grid_export_kwh = export_amount;
                                eval.revenue_czk =
                                    export_amount * context.grid_export_price_czk_per_kwh;
                            }
                        }
                    }
                }
            }

            ScheduledAction::SelfUse => {
                // PRIORITY: Check for negative prices (always charge if getting paid)
                if self.config.negative_price_handling_enabled && effective_price < 0.0 {
                    if context.current_battery_soc < self.config.target_battery_soc {
                        eval.mode = InverterOperationMode::ForceCharge;
                        eval.reason = format!(
                            "NEGATIVE PRICE: {:.3} CZK/kWh (getting paid!) [{}]",
                            effective_price, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v8:negative_price".to_string());

                        // Calculate energy flows accounting for available excess power
                        let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                        let available_excess = context.solar_forecast_kwh
                            + (-context.consumption_forecast_kwh).max(0.0);
                        let grid_charge_needed = (charge_kwh - available_excess).max(0.0);

                        eval.energy_flows.battery_charge_kwh = charge_kwh;
                        eval.energy_flows.grid_import_kwh = grid_charge_needed;
                        eval.cost_czk = grid_charge_needed * effective_price; // Negative = revenue!

                        // Export any excess we don't use for charging
                        let excess_after_charge = (available_excess - charge_kwh).max(0.0);
                        if excess_after_charge > 0.0 {
                            eval.energy_flows.grid_export_kwh = excess_after_charge;
                            eval.revenue_czk =
                                excess_after_charge * context.grid_export_price_czk_per_kwh;
                        }
                    }
                } else {
                    // Self-use mode - handle both consumption and excess power scenarios
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!("SELF-USE: {:.3} CZK/kWh [{}]", effective_price, summary);
                    eval.decision_uid = Some("winter_adaptive_v8:self_use".to_string());

                    // Calculate net consumption accounting for solar
                    // Negative consumption means excess power (solar > load)
                    let net_consumption =
                        context.consumption_forecast_kwh - context.solar_forecast_kwh;

                    if net_consumption > 0.0 {
                        // Deficit: need to cover consumption from battery or grid
                        let usable_battery_kwh = ((context.current_battery_soc
                            - context.control_config.hardware_min_battery_soc)
                            .max(0.0)
                            / 100.0)
                            * context.control_config.battery_capacity_kwh;

                        let battery_discharge = usable_battery_kwh.min(net_consumption);
                        eval.energy_flows.battery_discharge_kwh = battery_discharge;

                        if battery_discharge >= net_consumption {
                            // Battery fully covers deficit - avoided grid cost is revenue
                            eval.revenue_czk = net_consumption * effective_price;
                        } else {
                            // Partial coverage - need grid import for remainder
                            eval.revenue_czk = battery_discharge * effective_price;
                            let grid_needed = net_consumption - battery_discharge;
                            eval.cost_czk = grid_needed * effective_price;
                            eval.energy_flows.grid_import_kwh = grid_needed;
                        }
                    } else {
                        // Excess power available (solar > consumption)
                        let excess = -net_consumption;

                        // Charge battery with excess, up to available capacity
                        let battery_capacity = context.control_config.battery_capacity_kwh;
                        let available_charge_capacity = (battery_capacity
                            * (context.control_config.max_battery_soc / 100.0)
                            - battery_capacity * (context.current_battery_soc / 100.0))
                            .max(0.0);
                        let max_charge_rate =
                            context.control_config.max_battery_charge_rate_kw * 0.25;
                        let charge_amount =
                            excess.min(available_charge_capacity).min(max_charge_rate);

                        eval.energy_flows.battery_charge_kwh = charge_amount;

                        // Export any remaining excess
                        let export_amount = excess - charge_amount;
                        if export_amount > 0.0 {
                            eval.energy_flows.grid_export_kwh = export_amount;
                            eval.revenue_czk =
                                export_amount * context.grid_export_price_czk_per_kwh;
                        }
                    }
                }
            }
        }

        eval.calculate_net_profit();
        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn create_test_blocks_with_afternoon_peaks() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 19, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        let mut blocks = Vec::new();

        // Overnight cheap prices (00:00-07:00) - 28 blocks
        for hour in 0..7 {
            for quarter in 0..4 {
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: 1.0,
                    effective_price_czk_per_kwh: 1.0 + grid_fee,
                });
            }
        }

        // Morning moderate (07:00-14:00) - 28 blocks
        for hour in 7..14 {
            for quarter in 0..4 {
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: 3.0,
                    effective_price_czk_per_kwh: 3.0 + grid_fee,
                });
            }
        }

        // AFTERNOON PEAK (14:00-16:00) - 8 blocks - HIGHEST PRICES
        for hour in 14..16 {
            for quarter in 0..4 {
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: 6.0,
                    effective_price_czk_per_kwh: 6.0 + grid_fee,
                });
            }
        }

        // Evening moderate (16:00-24:00) - 32 blocks
        for hour in 16..24 {
            for quarter in 0..4 {
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: 4.0,
                    effective_price_czk_per_kwh: 4.0 + grid_fee,
                });
            }
        }

        blocks
    }

    #[test]
    fn test_finds_top_n_peaks() {
        let config = WinterAdaptiveV8Config::default();
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        let top_8 = strategy.find_top_n_expensive_blocks(&blocks, 8);

        assert_eq!(top_8.len(), 8, "Should find exactly 8 top blocks");

        // All top blocks should be the afternoon peak blocks (14:00-16:00)
        for &idx in &top_8 {
            let price = blocks[idx].effective_price_czk_per_kwh;
            assert!(
                price >= 7.0,
                "Top block should be from afternoon peak (6.0 + 1.8 grid fee)"
            );
        }
    }

    #[test]
    fn test_discharge_plan_profitable() {
        let config = WinterAdaptiveV8Config {
            top_discharge_blocks_count: 6,
            min_discharge_spread_czk: 3.0,
            avg_consumption_per_block_kwh: 0.1, // Lower consumption for test
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        let plan = strategy.generate_discharge_plan(&blocks, 50.0, 10.0, 3.0, 0.0);

        assert!(plan.is_profitable, "Plan should be profitable");
        assert_eq!(
            plan.discharge_blocks.len(),
            6,
            "Should discharge in 6 top blocks"
        );
        assert!(
            plan.profit_per_kwh >= 3.0,
            "Profit should meet minimum threshold"
        );
    }

    #[test]
    fn test_discharge_plan_not_profitable() {
        let config = WinterAdaptiveV8Config {
            top_discharge_blocks_count: 8,
            min_discharge_spread_czk: 10.0, // Set impossibly high threshold
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        let plan = strategy.generate_discharge_plan(&blocks, 50.0, 10.0, 3.0, 0.0);

        assert!(
            !plan.is_profitable,
            "Plan should not be profitable with high threshold"
        );
        assert_eq!(
            plan.discharge_blocks.len(),
            0,
            "Should not schedule discharge when not profitable"
        );
    }

    #[test]
    fn test_schedule_generation() {
        let config = WinterAdaptiveV8Config {
            top_discharge_blocks_count: 6,
            avg_consumption_per_block_kwh: 0.1, // Lower consumption for test
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        let plan = strategy.generate_discharge_plan(&blocks, 50.0, 10.0, 3.0, 0.0);
        let schedule = strategy.generate_schedule(&blocks, &plan);

        assert_eq!(schedule.len(), blocks.len());

        // Count charge and discharge blocks
        let charge_count = schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge))
            .count();
        let discharge_count = schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Discharge))
            .count();

        assert!(charge_count > 0, "Should have charge blocks");
        assert_eq!(discharge_count, 6, "Should have 6 discharge blocks");
    }

    // === Solar-Aware Charging Tests ===

    #[test]
    fn test_solar_adjustment_reduces_charge_blocks() {
        let min_blocks = 2;
        let config = WinterAdaptiveV8Config {
            solar_aware_charging_enabled: true,
            min_grid_charge_blocks: min_blocks,
            solar_capacity_reservation_factor: 0.7,
            min_solar_for_reduction_kwh: 2.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        // Test with no solar
        let plan_no_solar = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 0.0);

        // Test with significant solar (8 kWh expected)
        let plan_with_solar = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 8.0);

        // With solar, we should have fewer charge blocks
        assert!(
            plan_with_solar.charge_blocks.len() < plan_no_solar.charge_blocks.len(),
            "Solar should reduce charge blocks: {} with solar vs {} without",
            plan_with_solar.charge_blocks.len(),
            plan_no_solar.charge_blocks.len()
        );

        // But still have minimum safety blocks
        assert!(
            plan_with_solar.charge_blocks.len() >= min_blocks,
            "Should maintain minimum {} charge blocks, got {}",
            min_blocks,
            plan_with_solar.charge_blocks.len()
        );

        // Solar adjustment should be populated
        let adj = plan_with_solar.solar_adjustment.as_ref().unwrap();
        assert!(adj.blocks_saved > 0, "Should report blocks saved");
        assert!(
            adj.expected_solar_contribution_kwh > 0.0,
            "Should report expected solar contribution"
        );
    }

    #[test]
    fn test_solar_below_threshold_no_adjustment() {
        let config = WinterAdaptiveV8Config {
            solar_aware_charging_enabled: true,
            min_solar_for_reduction_kwh: 2.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        // Test with solar below threshold (1.5 kWh < 2.0 threshold)
        let plan_low_solar = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 1.5);

        // Test with no solar
        let plan_no_solar = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 0.0);

        // Should have same number of charge blocks (no adjustment)
        assert_eq!(
            plan_low_solar.charge_blocks.len(),
            plan_no_solar.charge_blocks.len(),
            "Low solar below threshold should not reduce charge blocks"
        );
    }

    #[test]
    fn test_solar_disabled_no_adjustment() {
        let config = WinterAdaptiveV8Config {
            solar_aware_charging_enabled: false, // Disabled
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        // Test with significant solar but feature disabled
        let plan_solar_disabled = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 8.0);

        // Test with no solar
        let plan_no_solar = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 0.0);

        // Should have same number of charge blocks (feature disabled)
        assert_eq!(
            plan_solar_disabled.charge_blocks.len(),
            plan_no_solar.charge_blocks.len(),
            "Disabled solar-aware charging should not reduce charge blocks"
        );
    }

    #[test]
    fn test_opportunistic_charging_always_included() {
        let config = WinterAdaptiveV8Config {
            solar_aware_charging_enabled: true,
            opportunistic_charge_threshold_czk: 3.0, // Any price below 3 CZK is opportunistic
            min_grid_charge_blocks: 1,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);
        let blocks = create_test_blocks_with_afternoon_peaks();

        // Find opportunistic blocks (overnight blocks at 2.80 CZK should qualify)
        let opportunistic = strategy.find_opportunistic_charge_blocks(&blocks);

        // Even with massive solar forecast, opportunistic blocks should be included
        let plan = strategy.generate_discharge_plan(&blocks, 30.0, 10.0, 3.0, 20.0); // 20 kWh solar

        // All opportunistic blocks should be in charge blocks
        for &opp_idx in &opportunistic {
            assert!(
                plan.charge_blocks.contains(&opp_idx),
                "Opportunistic block {} should always be included in charging",
                opp_idx
            );
        }
    }

    #[test]
    fn test_solar_adjustment_calculation() {
        let config = WinterAdaptiveV8Config {
            target_battery_soc: 95.0,
            solar_aware_charging_enabled: true,
            solar_capacity_reservation_factor: 0.7,
            min_grid_charge_blocks: 2,
            min_solar_for_reduction_kwh: 2.0,
            battery_round_trip_efficiency: 0.9,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV8Strategy::new(config);

        // Battery at 30% SOC, 10 kWh capacity, 3 kW charge rate
        // Need to fill: (95 - 30) / 100 * 10 = 6.5 kWh
        // Solar expected: 8 kWh * 0.7 factor = 5.6 kWh contribution
        // Grid need: 6.5 - 5.6 = 0.9 kWh
        // Blocks needed: 0.9 / (3.0 * 0.25 * 0.9) ≈ 1.33 -> 2 (ceil)
        // But minimum is 2, so expect 2 blocks

        let adjustment = strategy.calculate_solar_charge_adjustment(
            30.0, // current SOC
            10.0, // battery capacity
            3.0,  // max charge rate
            8.0,  // solar remaining
            24,   // original charge blocks (25% of 96 blocks)
        );

        assert!(
            adjustment.expected_solar_contribution_kwh > 5.0,
            "Expected ~5.6 kWh solar contribution, got {}",
            adjustment.expected_solar_contribution_kwh
        );
        assert!(
            adjustment.required_grid_charge_blocks <= 3,
            "Should need few grid charge blocks, got {}",
            adjustment.required_grid_charge_blocks
        );
        assert!(
            adjustment.blocks_saved > 15,
            "Should save many blocks, got {}",
            adjustment.blocks_saved
        );
    }
}
