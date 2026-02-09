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

//! Winter Adaptive Strategy V3
//!
//! A simplified battery optimization strategy using centralized price calculation.
//!
//! ## Key Features
//!
//! 1. **Centralized Pricing**: Uses pre-calculated effective prices (spot + grid fees) from scheduler
//! 2. **Winter Discharge Restriction**: SOC >= 50% AND top 4 expensive blocks today
//! 3. **Arbitrage Efficiency Check**: Only discharge when price exceeds median + buffer
//! 4. **Simplified Algorithm**: No arbitrage, P90, spikes, or feed-in complexity

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
    locking::{LockedBlock, ScheduleLockState},
    seasonal::SeasonalMode,
};
use chrono::Utc;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for Winter Adaptive V3 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV3Config {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Priority for conflict resolution (0-100, higher wins)
    pub priority: u8,

    /// Target battery SOC for charging (default: 90%)
    pub daily_charging_target_soc: f32,

    /// Minimum SOC for winter discharge (default: 50%)
    pub winter_discharge_min_soc: f32,

    /// Number of top expensive blocks per day to allow discharge (default: 4)
    pub top_discharge_blocks_per_day: usize,

    /// Minimum arbitrage buffer above median+high_grid_fee for discharge to be worthwhile
    /// Default: 1.0 CZK (use 0.05 for EUR)
    /// Discharge only allowed if: effective_price > (median_spot + high_grid_fee + buffer)
    pub discharge_arbitrage_buffer: f32,

    /// Minimum consecutive charge blocks (default: 2)
    pub min_consecutive_charge_blocks: usize,

    /// Price tolerance for charge block selection (default: 15%)
    pub charge_price_tolerance_percent: f32,

    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,

    /// Current seasonal mode
    pub seasonal_mode: SeasonalMode,
}

impl Default for WinterAdaptiveV3Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 100,
            daily_charging_target_soc: 90.0,
            winter_discharge_min_soc: 50.0,
            top_discharge_blocks_per_day: 4,
            discharge_arbitrage_buffer: 1.0, // 1.0 CZK, use 0.05 for EUR
            min_consecutive_charge_blocks: 2,
            charge_price_tolerance_percent: 15.0,
            negative_price_handling_enabled: true,
            seasonal_mode: SeasonalMode::Winter,
        }
    }
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug)]
pub struct WinterAdaptiveV3Strategy {
    config: WinterAdaptiveV3Config,
    lock_state: RwLock<ScheduleLockState>,
}

impl WinterAdaptiveV3Strategy {
    pub fn new(config: WinterAdaptiveV3Config) -> Self {
        Self {
            config,
            lock_state: RwLock::new(ScheduleLockState::default()),
        }
    }

    /// Check if discharge is allowed based on SOC, block ranking, and arbitrage efficiency
    fn is_discharge_allowed(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
    ) -> (bool, String) {
        // Summer mode: no restrictions
        if self.config.seasonal_mode == SeasonalMode::Summer {
            return (true, "Summer mode - no restrictions".to_string());
        }

        // Winter: Check SOC >= min threshold
        if context.current_battery_soc < self.config.winter_discharge_min_soc {
            return (
                false,
                format!(
                    "SOC {:.1}% < {:.1}% minimum",
                    context.current_battery_soc, self.config.winter_discharge_min_soc
                ),
            );
        }

        // Winter: Check if in top N expensive blocks TODAY
        let current_block = &all_blocks[current_idx];
        let today = current_block.block_start.date_naive();

        // Filter blocks for today using pre-calculated effective prices
        let mut today_blocks: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.block_start.date_naive() == today)
            .map(|(idx, b)| (idx, b.effective_price_czk_per_kwh))
            .collect();

        // Sort by effective price descending (most expensive first)
        today_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Get top N indices
        let top_n: Vec<usize> = today_blocks
            .iter()
            .take(self.config.top_discharge_blocks_per_day)
            .map(|(idx, _)| *idx)
            .collect();

        let current_effective_price = current_block.effective_price_czk_per_kwh;

        // Check if in top N expensive blocks
        if !top_n.contains(&current_idx) {
            return (
                false,
                format!(
                    "Not in top {} expensive blocks, eff price {:.3} CZK",
                    self.config.top_discharge_blocks_per_day, current_effective_price
                ),
            );
        }

        // =====================================================================
        // Arbitrage efficiency check
        // Only discharge if: effective_price > (median_effective_price + buffer)
        // This ensures discharge is worthwhile vs. just using grid directly
        // =====================================================================
        let today_effective_prices: Vec<f32> = all_blocks
            .iter()
            .filter(|b| b.block_start.date_naive() == today)
            .map(|b| b.effective_price_czk_per_kwh)
            .collect();

        let median_effective_price = Self::calculate_median(&today_effective_prices);
        let arbitrage_threshold = median_effective_price + self.config.discharge_arbitrage_buffer;

        if current_effective_price <= arbitrage_threshold {
            return (
                false,
                format!(
                    "Arbitrage not efficient: eff {:.3} <= threshold {:.3} (median {:.3} + buffer {:.3})",
                    current_effective_price,
                    arbitrage_threshold,
                    median_effective_price,
                    self.config.discharge_arbitrage_buffer
                ),
            );
        }

        let rank = top_n
            .iter()
            .position(|&idx| idx == current_idx)
            .unwrap_or(0)
            + 1;
        (
            true,
            format!(
                "Top {} block (#{} of {}), eff {:.3} > threshold {:.3} CZK",
                self.config.top_discharge_blocks_per_day,
                rank,
                today_blocks.len(),
                current_effective_price,
                arbitrage_threshold
            ),
        )
    }

    /// Calculate median of a slice of f32 values
    fn calculate_median(values: &[f32]) -> f32 {
        if values.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let len = sorted.len();
        if len.is_multiple_of(2) {
            (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
        } else {
            sorted[len / 2]
        }
    }

    /// Select cheapest consecutive blocks for charging using "super block" approach
    ///
    /// Algorithm:
    /// 1. Create overlapping "super blocks" of N consecutive blocks (N = min_consecutive_charge_blocks)
    /// 2. Calculate average effective price for each super block
    /// 3. Sort by average price (cheapest first)
    /// 4. Select cheapest super blocks until we have enough blocks for target SOC
    fn select_charge_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
        blocks_needed: usize,
    ) -> Vec<usize> {
        let min_consecutive = self.config.min_consecutive_charge_blocks;

        // Step 1: Use pre-calculated effective prices for all remaining blocks
        let blocks_with_prices: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .skip(current_idx)
            .map(|(idx, b)| (idx, b.effective_price_czk_per_kwh))
            .collect();

        if blocks_with_prices.len() < min_consecutive {
            return Vec::new();
        }

        // Step 2: Create overlapping "super blocks" of N consecutive blocks
        // Each super block is (window_start_in_vec, avg_effective_price, block_indices)
        let mut super_blocks: Vec<(usize, f32, Vec<usize>)> = Vec::new();

        for i in 0..=(blocks_with_prices.len() - min_consecutive) {
            let window: Vec<(usize, f32)> = blocks_with_prices[i..i + min_consecutive].to_vec();
            let avg_price: f32 =
                window.iter().map(|(_, p)| *p).sum::<f32>() / min_consecutive as f32;
            let indices: Vec<usize> = window.iter().map(|(idx, _)| *idx).collect();
            super_blocks.push((i, avg_price, indices));
        }

        // Step 3: Sort by average effective price (cheapest first)
        super_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Step 4: Select cheapest super blocks until we have enough blocks
        let mut selected_indices: Vec<usize> = Vec::new();

        for (_, avg_price, indices) in &super_blocks {
            if selected_indices.len() >= blocks_needed {
                break;
            }

            // Add all indices from this super block (overlapping is allowed)
            for &idx in indices {
                if !selected_indices.contains(&idx) && selected_indices.len() < blocks_needed {
                    selected_indices.push(idx);
                }
            }

            tracing::debug!(
                "V3: Selected super block with avg price {:.3} CZK, indices {:?}",
                avg_price,
                indices
            );
        }

        // Sort by index for consistent ordering
        selected_indices.sort();

        tracing::debug!(
            "V3: Final charge schedule: {} blocks, indices {:?}",
            selected_indices.len(),
            selected_indices
        );

        selected_indices
    }

    /// Main decision logic
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
    ) -> (InverterOperationMode, String, String) {
        let current_price = context.price_block.price_czk_per_kwh; // For display/logging
        let current_block_start = context.price_block.block_start;
        let effective_price = context.price_block.effective_price_czk_per_kwh;

        // =====================================================================
        // PRIORITY 0: Check locked blocks
        // =====================================================================
        {
            let lock_state = self.lock_state.read().unwrap();
            if let Some((locked_mode, locked_reason)) =
                lock_state.get_locked_mode(current_block_start)
            {
                tracing::debug!(
                    "V3: Block {} is locked to {:?}",
                    current_block_start,
                    locked_mode
                );
                return (
                    locked_mode,
                    locked_reason,
                    "winter_adaptive_v3:locked_block".to_string(),
                );
            }
        }

        // =====================================================================
        // PRIORITY 1: Negative prices - always charge (free energy!)
        // =====================================================================
        if self.config.negative_price_handling_enabled && current_price < 0.0 {
            if context.current_battery_soc < 100.0 {
                return (
                    InverterOperationMode::ForceCharge,
                    format!("Negative price: {:.3} CZK/kWh", current_price),
                    "winter_adaptive_v3:negative_price_charge".to_string(),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price, battery full: {:.3} CZK/kWh", current_price),
                "winter_adaptive_v3:negative_price_full".to_string(),
            );
        }

        // =====================================================================
        // STEP 2: Calculate charge blocks needed
        // =====================================================================
        let battery_capacity_kwh = context.control_config.battery_capacity_kwh;
        let charge_per_block_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
        let target_soc = self.config.daily_charging_target_soc;
        let soc_deficit = (target_soc - context.current_battery_soc).max(0.0);
        let energy_needed_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;
        let blocks_for_soc = (energy_needed_kwh / charge_per_block_kwh).ceil() as usize;

        let config_charge_blocks = context.control_config.force_charge_hours * 4;
        let blocks_needed = config_charge_blocks.max(blocks_for_soc);

        tracing::debug!(
            "V3: SOC {:.1}% -> {:.1}% target, need {} blocks (config min: {})",
            context.current_battery_soc,
            target_soc,
            blocks_for_soc,
            config_charge_blocks
        );

        // =====================================================================
        // STEP 3: Select cheapest charge blocks by EFFECTIVE price
        // =====================================================================
        let charge_schedule =
            self.select_charge_blocks(all_blocks, current_block_index, blocks_needed);

        // =====================================================================
        // STEP 4: Check if current block should charge
        // =====================================================================
        let should_charge = charge_schedule.contains(&current_block_index);

        if should_charge && context.current_battery_soc < target_soc {
            // Lock next blocks to prevent oscillation
            self.lock_upcoming_blocks(
                all_blocks,
                current_block_index,
                &charge_schedule,
                context.control_config.min_consecutive_force_blocks,
            );

            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "Scheduled charge: spot {:.3} + grid {:.3} = {:.3} CZK/kWh",
                    current_price,
                    effective_price - current_price,
                    effective_price
                ),
                "winter_adaptive_v3:scheduled_charge".to_string(),
            );
        }

        // =====================================================================
        // STEP 5: Check discharge restriction (Winter mode)
        // =====================================================================
        let (discharge_allowed, discharge_reason) =
            self.is_discharge_allowed(context, all_blocks, current_block_index);

        if discharge_allowed {
            return (
                InverterOperationMode::SelfUse,
                format!("Discharge allowed: {}", discharge_reason),
                "winter_adaptive_v3:discharge_allowed".to_string(),
            );
        }

        // =====================================================================
        // STEP 6: Default - SelfUse with discharge prevention
        // =====================================================================
        (
            InverterOperationMode::SelfUse,
            format!(
                "Hold: {} (eff {:.3} CZK)",
                discharge_reason, effective_price
            ),
            "winter_adaptive_v3:hold".to_string(),
        )
    }

    /// Lock upcoming blocks to prevent oscillation
    fn lock_upcoming_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
        charge_schedule: &[usize],
        min_consecutive: usize,
    ) {
        let now = Utc::now();
        let current_block_start = all_blocks[current_idx].block_start;
        let block_age_seconds = (now - current_block_start).num_seconds();

        // Only lock if evaluating current block
        let is_current_block = (0..900).contains(&block_age_seconds);
        if !is_current_block {
            return;
        }

        let mut lock_state = self.lock_state.write().unwrap();
        let mut blocks_to_lock = Vec::new();

        for i in 0..min_consecutive {
            let block_idx = current_idx + i;
            if block_idx < all_blocks.len() {
                let block = &all_blocks[block_idx];
                let block_mode = if charge_schedule.contains(&block_idx) {
                    InverterOperationMode::ForceCharge
                } else {
                    InverterOperationMode::SelfUse
                };
                let block_reason = if charge_schedule.contains(&block_idx) {
                    format!("Scheduled charge: {:.3} CZK", block.price_czk_per_kwh)
                } else {
                    format!("Hold: {:.3} CZK", block.price_czk_per_kwh)
                };

                blocks_to_lock.push(LockedBlock {
                    block_start: block.block_start,
                    mode: block_mode,
                    reason: block_reason,
                });
            }
        }

        let blocks_locked = blocks_to_lock.len();
        lock_state.lock_blocks(blocks_to_lock);
        tracing::debug!(
            "V3: Locked {} blocks starting at {}",
            blocks_locked,
            current_block_start
        );
    }
}

impl EconomicStrategy for WinterAdaptiveV3Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V3"
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
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data".to_string();
            return eval;
        };

        let current_block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let (mode, reason, decision_uid) =
            self.decide_mode(context, all_blocks, current_block_index);

        eval.mode = mode;
        eval.reason = reason;
        eval.decision_uid = Some(decision_uid);

        // Calculate energy flows based on mode
        match mode {
            InverterOperationMode::ForceCharge => {
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                // Use pre-calculated effective price for cost calculation
                let effective_price = context.price_block.effective_price_czk_per_kwh;
                eval.cost_czk = economics::grid_import_cost(charge_kwh, effective_price);
            }
            InverterOperationMode::ForceDischarge => {
                let discharge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                eval.energy_flows.grid_export_kwh = discharge_kwh;
                eval.revenue_czk = economics::grid_export_revenue(
                    discharge_kwh,
                    context.grid_export_price_czk_per_kwh,
                );
            }
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                let usable_battery_kwh = ((context.current_battery_soc
                    - context.control_config.hardware_min_battery_soc)
                    .max(0.0)
                    / 100.0)
                    * context.control_config.battery_capacity_kwh;

                let effective_price = context.price_block.effective_price_czk_per_kwh;

                // Calculate how much battery will discharge to cover load
                let battery_discharge = usable_battery_kwh.min(context.consumption_forecast_kwh);

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                if battery_discharge >= context.consumption_forecast_kwh {
                    // Battery fully covers load
                    eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                } else {
                    // Battery partially covers load, rest from grid
                    eval.revenue_czk = battery_discharge * effective_price;
                    eval.cost_czk =
                        (context.consumption_forecast_kwh - battery_discharge) * effective_price;
                    eval.energy_flows.grid_import_kwh =
                        context.consumption_forecast_kwh - battery_discharge;
                }
            }
        }

        eval.calculate_net_profit();
        eval
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // NOTE: HDO parsing tests are in crate::strategy::pricing::tests

    #[test]
    fn test_seasonal_mode_from_date() {
        // Winter months
        let winter_date = Utc.with_ymd_and_hms(2026, 1, 14, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(winter_date), SeasonalMode::Winter);

        let winter_date2 = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(winter_date2), SeasonalMode::Winter);

        // Summer months
        let summer_date = Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(summer_date), SeasonalMode::Summer);

        let summer_date2 = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(summer_date2), SeasonalMode::Summer);
    }

    #[test]
    fn test_calculate_median() {
        // Odd number of elements
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[1.0, 3.0, 2.0]),
            2.0
        );
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[5.0, 1.0, 9.0, 3.0, 7.0]),
            5.0
        );

        // Even number of elements
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[1.0, 2.0, 3.0, 4.0]),
            2.5
        );
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[1.0, 2.0]), 1.5);

        // Empty
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[]), 0.0);

        // Single element
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[42.0]), 42.0);
    }

    #[test]
    fn test_arbitrage_threshold_calculation() {
        // Given:
        // - median_effective_price = 3.8 CZK/kWh (spot + grid fees pre-calculated)
        // - buffer = 1.0 CZK
        // Threshold = 3.8 + 1.0 = 4.8 CZK/kWh
        //
        // For discharge to be efficient:
        // - effective_price must be > 4.8

        let config = WinterAdaptiveV3Config {
            enabled: true,
            discharge_arbitrage_buffer: 1.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV3Strategy::new(config);

        // Test median calculation for threshold using pre-calculated effective prices
        let effective_prices = vec![3.0, 3.8, 4.5]; // median = 3.8
        let median = WinterAdaptiveV3Strategy::calculate_median(&effective_prices);
        assert_eq!(median, 3.8);

        let threshold = median + strategy.config.discharge_arbitrage_buffer;
        assert!((threshold - 4.8).abs() < 0.001);

        // Effective price of 5.0 should pass (5.0 > 4.8)
        // Effective price of 4.8 should fail (4.8 <= 4.8)
        // Effective price of 4.0 should fail (4.0 <= 4.8)
    }

    #[test]
    fn test_super_block_selection() {
        // Simulates the export scenario:
        // Early blocks (indices 0-3): expensive ~2.64 CZK
        // Later blocks (indices 4-7): cheap ~2.29 CZK
        // Algorithm should select the cheaper blocks

        // Create mock effective prices (spot + grid fee + buy fee)
        let prices: Vec<(usize, f32)> = vec![
            // Early expensive blocks
            (0, 2.48),
            (1, 2.77),
            (2, 2.67),
            (3, 2.49),
            // Later cheap blocks
            (4, 2.25),
            (5, 2.30),
            (6, 2.31),
            (7, 2.35),
        ];

        let min_consecutive = 2;

        // Create overlapping super blocks
        let mut super_blocks: Vec<(f32, Vec<usize>)> = Vec::new();
        for i in 0..=(prices.len() - min_consecutive) {
            let window: Vec<(usize, f32)> = prices[i..i + min_consecutive].to_vec();
            let avg_price: f32 =
                window.iter().map(|(_, p)| *p).sum::<f32>() / min_consecutive as f32;
            let indices: Vec<usize> = window.iter().map(|(idx, _)| *idx).collect();
            super_blocks.push((avg_price, indices));
        }

        // Sort by price (cheapest first)
        super_blocks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Cheapest super block should be from the cheap region (indices 4-7)
        let (cheapest_avg, cheapest_indices) = &super_blocks[0];
        assert!(
            *cheapest_avg < 2.35,
            "Cheapest avg should be < 2.35, got {:.3}",
            cheapest_avg
        );
        assert!(
            cheapest_indices.iter().all(|&i| i >= 4),
            "Cheapest block indices should be >= 4, got {:?}",
            cheapest_indices
        );

        // Verify the expensive blocks are sorted later
        let (most_expensive_avg, _) = &super_blocks[super_blocks.len() - 1];
        assert!(
            *most_expensive_avg > 2.55,
            "Most expensive avg should be > 2.55, got {:.3}",
            most_expensive_avg
        );
    }
}
