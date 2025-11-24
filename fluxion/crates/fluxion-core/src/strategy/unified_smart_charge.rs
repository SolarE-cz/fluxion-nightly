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

//! Unified Smart Charge Strategy
//!
//! This strategy merges the best features from:
//! - TimeAwareChargeStrategy (upcoming cheapest blocks)
//! - DayAheadChargePlanningStrategy (global optimization)
//! - MorningPreChargeStrategy (morning peak preparation)
//!
//! Key features:
//! - Multiple block selection methods (threshold, top-N, morning-focused)
//! - Solar awareness (avoids charging when solar coming soon)
//! - Time-based SOC targets
//! - Actual price forecasts (no heuristics)
//! - Configurable parameters

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::Timelike;
use std::collections::HashSet;

/// Expected block duration in minutes (15-minute blocks)
const EXPECTED_BLOCK_DURATION_MINUTES: u32 = 15;
/// Block duration in hours for calculations
const BLOCK_DURATION_HOURS: f32 = 0.25;
/// Opportunity value weighting factor
const OPPORTUNITY_VALUE_WEIGHT: f32 = 0.7;
/// Price threshold for gradual discharge (blocks within 90% of max price)
/// Unified Smart Charge Strategy Configuration
#[derive(Debug, Clone)]
pub struct UnifiedSmartChargeConfig {
    /// Price threshold above minimum (default: 0.05 = 5%)
    pub price_threshold_percentage: f32,
    /// Minimum price difference for morning peak (default: 0.5 CZK)
    pub min_price_difference_czk: f32,
    /// Night SOC target (default: 70%)
    pub night_target_soc: f32,
    /// Afternoon SOC target (default: 90%)
    pub afternoon_target_soc: f32,
    /// Evening SOC target (default: 100%)
    pub evening_target_soc: f32,
    /// Min solar threshold to skip charging (default: 0.5 kWh)
    pub min_solar_threshold_kwh: f32,
    /// Hours to look ahead for solar (default: 4)
    pub solar_lookahead_hours: u32,
    /// Enable morning peak optimization (default: true)
    pub enable_morning_peak_focus: bool,
    /// Morning peak window start hour (default: 6)
    pub morning_peak_window_start: u32,
    /// Morning peak window end hour (default: 10)
    pub morning_peak_window_end: u32,
}

impl Default for UnifiedSmartChargeConfig {
    fn default() -> Self {
        Self {
            price_threshold_percentage: 0.05,
            min_price_difference_czk: 0.5,
            night_target_soc: 70.0,
            afternoon_target_soc: 90.0,
            evening_target_soc: 100.0,
            min_solar_threshold_kwh: 0.5,
            solar_lookahead_hours: 4,
            enable_morning_peak_focus: true,
            morning_peak_window_start: 6,
            morning_peak_window_end: 10,
        }
    }
}

/// Unified Smart Charge Strategy
///
/// Merges three charging strategies into one intelligent, configurable strategy
/// that selects optimal charging blocks using multiple methods and considers
/// solar availability, time-of-day targets, and actual price forecasts.
#[derive(Debug, Clone)]
pub struct UnifiedSmartChargeStrategy {
    enabled: bool,
    config: UnifiedSmartChargeConfig,
}

impl UnifiedSmartChargeStrategy {
    /// Create a new Unified Smart Charge strategy
    pub fn new(enabled: bool, config: UnifiedSmartChargeConfig) -> Self {
        Self { enabled, config }
    }

    /// METHOD A: Threshold-based block selection
    /// Selects all upcoming blocks within threshold% of global minimum price
    fn method_a_threshold_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
    ) -> HashSet<chrono::DateTime<chrono::Utc>> {
        let upcoming_blocks: Vec<(chrono::DateTime<chrono::Utc>, f32)> = all_blocks
            .iter()
            .filter(|b| b.block_start >= current_block_start)
            .map(|b| (b.block_start, b.price_czk_per_kwh))
            .collect();

        if upcoming_blocks.is_empty() {
            return HashSet::new();
        }

        // Use minimum price from ALL blocks (not just upcoming)
        // This prevents treating expensive blocks as "cheap" just because they're the only upcoming ones
        let min_price = all_blocks
            .iter()
            .map(|b| b.price_czk_per_kwh)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0.0);

        let threshold = min_price * (1.0 + self.config.price_threshold_percentage);

        upcoming_blocks
            .iter()
            .filter(|(_, price)| *price <= threshold)
            .map(|(time, _)| *time)
            .collect()
    }

    /// METHOD B: Top-N block selection
    /// Selects N cheapest blocks where N is based on energy needed
    /// Only selects blocks that are actually economically viable (within reasonable threshold)
    fn method_b_topn_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
        context: &EvaluationContext,
    ) -> HashSet<chrono::DateTime<chrono::Utc>> {
        // Calculate target SOC based on time
        let target_soc = self.get_target_soc_for_time(current_block_start);

        // Energy needed to reach target
        let energy_needed = context.control_config.battery_capacity_kwh
            * (target_soc - context.current_battery_soc)
            / 100.0;

        if energy_needed <= 0.0 {
            return HashSet::new();
        }

        // Blocks needed
        let blocks_needed = (energy_needed
            / (context.control_config.max_battery_charge_rate_kw * BLOCK_DURATION_HOURS))
            .ceil() as usize;

        // Get upcoming blocks sorted by price
        let mut upcoming_blocks: Vec<(chrono::DateTime<chrono::Utc>, f32)> = all_blocks
            .iter()
            .filter(|b| b.block_start >= current_block_start)
            .map(|b| (b.block_start, b.price_czk_per_kwh))
            .collect();

        if upcoming_blocks.is_empty() {
            return HashSet::new();
        }

        upcoming_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Calculate threshold based on minimum price + threshold percentage
        // This ensures we don't charge at expensive blocks just because they're the "cheapest" available
        let min_price = upcoming_blocks[0].1;
        let threshold = min_price * (1.0 + self.config.price_threshold_percentage);

        // Take top N cheapest blocks, but only if they're within the threshold
        upcoming_blocks
            .into_iter()
            .filter(|(_, price)| *price <= threshold)
            .take(blocks_needed)
            .map(|(time, _)| time)
            .collect()
    }

    /// METHOD C: Morning-focused block selection
    /// Selects 3-4 cheapest night blocks if morning peak justifies it
    fn method_c_morning_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
    ) -> HashSet<chrono::DateTime<chrono::Utc>> {
        if !self.config.enable_morning_peak_focus {
            return HashSet::new();
        }

        // Find morning peak price
        let morning_peak_price = all_blocks
            .iter()
            .filter(|b| {
                let hour = b.block_start.hour();
                (self.config.morning_peak_window_start..self.config.morning_peak_window_end)
                    .contains(&hour)
            })
            .map(|b| b.price_czk_per_kwh)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let Some(morning_peak) = morning_peak_price else {
            return HashSet::new();
        };

        // Get night blocks (22:00-08:00)
        let mut night_blocks: Vec<(chrono::DateTime<chrono::Utc>, f32)> = all_blocks
            .iter()
            .filter(|b| {
                let hour = b.block_start.hour();
                !(8..22).contains(&hour) && b.block_start >= current_block_start
            })
            .map(|b| (b.block_start, b.price_czk_per_kwh))
            .collect();

        if night_blocks.is_empty() {
            return HashSet::new();
        }

        // Sort by price
        night_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let min_night_price = night_blocks[0].1;
        let price_diff = morning_peak - min_night_price;

        // Only activate if price difference is significant
        if price_diff <= self.config.min_price_difference_czk {
            return HashSet::new();
        }

        // Take 3-4 cheapest night blocks
        night_blocks
            .into_iter()
            .take(4)
            .map(|(time, _)| time)
            .collect()
    }

    /// Combine all three methods into unified cheap blocks set
    fn identify_cheap_charging_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
        context: &EvaluationContext,
    ) -> HashSet<chrono::DateTime<chrono::Utc>> {
        let threshold_blocks = self.method_a_threshold_blocks(all_blocks, current_block_start);
        let topn_blocks = self.method_b_topn_blocks(all_blocks, current_block_start, context);
        let morning_blocks = self.method_c_morning_blocks(all_blocks, current_block_start);

        // Intersection of A & B ensures we only charge in truly cheap blocks
        let core_cheap: HashSet<_> = threshold_blocks
            .intersection(&topn_blocks)
            .cloned()
            .collect();

        // Union with C adds morning-focused blocks
        core_cheap.union(&morning_blocks).cloned().collect()
    }

    /// Check if significant solar is coming soon
    fn solar_coming_soon(
        &self,
        context: &EvaluationContext,
        _all_blocks: &[TimeBlockPrice],
    ) -> bool {
        // For now, use simple heuristic
        // Better implementation would sum solar forecast for next N hours
        let current_hour = context.price_block.block_start.hour();
        let _lookahead_end = context.price_block.block_start
            + chrono::Duration::hours(self.config.solar_lookahead_hours as i64);

        // Check if we're approaching daytime (simplified)
        let approaching_daytime = (6..10).contains(&current_hour);

        // If already midday, solar is here or has passed
        if (10..16).contains(&current_hour) {
            return context.solar_forecast_kwh > self.config.min_solar_threshold_kwh;
        }

        // If approaching morning with significant solar forecast, skip charging
        // Return true to skip charging when solar IS coming (high forecast)
        // This is a simplified check - production code should use actual solar forecast data
        approaching_daytime && context.solar_forecast_kwh > self.config.min_solar_threshold_kwh
    }

    /// Get target SOC based on time of day
    fn get_target_soc_for_time(&self, time: chrono::DateTime<chrono::Utc>) -> f32 {
        let hour = time.hour();

        if hour < 12 {
            self.config.night_target_soc
        } else if hour < 17 {
            self.config.afternoon_target_soc
        } else {
            self.config.evening_target_soc
        }
    }

    /// Check if we should charge now
    fn should_charge_now(
        &self,
        context: &EvaluationContext,
        cheap_blocks: &HashSet<chrono::DateTime<chrono::Utc>>,
        all_blocks: &[TimeBlockPrice],
    ) -> (bool, String, f32) {
        // Solar awareness check
        if self.solar_coming_soon(context, all_blocks) {
            return (
                false,
                "Solar generation expected soon, skipping grid charge".to_string(),
                0.0,
            );
        }

        // Check if current block is in cheap blocks set
        if !cheap_blocks.contains(&context.price_block.block_start) {
            return (
                false,
                format!(
                    "Not in unified cheap blocks (price: {:.3} CZK)",
                    context.price_block.price_czk_per_kwh
                ),
                0.0,
            );
        }

        // Calculate target SOC
        let target_soc = self.get_target_soc_for_time(context.price_block.block_start);

        // Check if we need to charge
        if context.current_battery_soc >= target_soc {
            return (
                false,
                format!(
                    "SOC {:.1}% >= target {:.0}%",
                    context.current_battery_soc, target_soc
                ),
                0.0,
            );
        }

        (
            true,
            format!(
                "Charging during unified cheap block (price: {:.3} CZK, target SOC: {:.0}%)",
                context.price_block.price_czk_per_kwh, target_soc
            ),
            target_soc,
        )
    }

    /// Calculate average evening price
    fn calculate_evening_price(&self, all_blocks: &[TimeBlockPrice]) -> f32 {
        let evening_prices: Vec<f32> = all_blocks
            .iter()
            .filter(|b| {
                let hour = b.block_start.hour();
                (17..23).contains(&hour)
            })
            .map(|b| b.price_czk_per_kwh)
            .collect();

        if evening_prices.is_empty() {
            return 0.0;
        }

        evening_prices.iter().sum::<f32>() / evening_prices.len() as f32
    }
}

impl Default for UnifiedSmartChargeStrategy {
    fn default() -> Self {
        Self::new(true, UnifiedSmartChargeConfig::default())
    }
}

impl EconomicStrategy for UnifiedSmartChargeStrategy {
    fn name(&self) -> &str {
        "Unified-Smart-Charge"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        // Validate block duration assumption
        if context.price_block.duration_minutes != EXPECTED_BLOCK_DURATION_MINUTES {
            tracing::warn!(
                "Block duration {} minutes doesn't match expected {} minutes - calculations may be incorrect",
                context.price_block.duration_minutes,
                EXPECTED_BLOCK_DURATION_MINUTES
            );
        }

        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse,
            self.name().to_string(),
        );

        // Fill in assumptions
        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        // Need price data for optimization
        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available for smart charge planning".to_string();
            return eval;
        };

        // Identify cheap charging blocks
        let cheap_blocks = self.identify_cheap_charging_blocks(
            all_blocks,
            context.price_block.block_start,
            context,
        );

        tracing::debug!(
            "Unified Smart Charge: Identified {} cheap blocks for charging",
            cheap_blocks.len()
        );

        // Should we charge now?
        let (should_charge, reason, target_soc) =
            self.should_charge_now(context, &cheap_blocks, all_blocks);

        if !should_charge {
            // Self-use mode
            eval.mode = InverterOperationMode::SelfUse;
            eval.reason = reason;

            let solar = context.solar_forecast_kwh;
            let consumption = context.consumption_forecast_kwh;

            eval.energy_flows.solar_generation_kwh = solar;
            eval.energy_flows.household_consumption_kwh = consumption;

            if solar >= consumption {
                // Solar covers all consumption
                eval.energy_flows.grid_import_kwh = 0.0;
                eval.energy_flows.grid_export_kwh = solar - consumption; // Excess solar exported

                // Revenue from export
                eval.revenue_czk = economics::grid_export_revenue(
                    eval.energy_flows.grid_export_kwh,
                    context.grid_export_price_czk_per_kwh,
                );
                eval.cost_czk = 0.0;
            } else {
                // Deficit needs to be covered by Battery or Grid
                let deficit = consumption - solar;

                // Check battery availability
                let battery_kwh_available = context.control_config.battery_capacity_kwh
                    * (context.current_battery_soc - context.control_config.min_battery_soc)
                        .max(0.0)
                    / 100.0;

                // We can discharge to cover deficit, limited by max discharge rate
                let max_discharge =
                    context.control_config.max_battery_charge_rate_kw * BLOCK_DURATION_HOURS;
                let discharge_kwh = deficit.min(battery_kwh_available).min(max_discharge);

                if discharge_kwh > 0.0 {
                    eval.energy_flows.battery_discharge_kwh = discharge_kwh;

                    // Remaining deficit covered by grid
                    let remaining_deficit = deficit - discharge_kwh;
                    eval.energy_flows.grid_import_kwh = remaining_deficit;

                    // Costs: Grid Import + Battery Wear
                    let import_cost = economics::grid_import_cost(
                        remaining_deficit,
                        context.price_block.price_czk_per_kwh,
                    );
                    let wear_cost = economics::battery_degradation_cost(
                        discharge_kwh,
                        context.control_config.battery_wear_cost_czk_per_kwh,
                    );

                    eval.cost_czk = import_cost + wear_cost;
                } else {
                    // No battery available, full grid import
                    eval.energy_flows.grid_import_kwh = deficit;
                    eval.cost_czk =
                        economics::grid_import_cost(deficit, context.price_block.price_czk_per_kwh);
                }

                eval.revenue_czk = 0.0;
            }

            eval.calculate_net_profit();
            return eval;
        }

        // CHARGE MODE
        eval.mode = InverterOperationMode::ForceCharge;

        // Calculate charge amount
        let max_charge_this_block =
            context.control_config.max_battery_charge_rate_kw * BLOCK_DURATION_HOURS;
        let energy_needed_to_target = context.control_config.battery_capacity_kwh
            * (target_soc - context.current_battery_soc)
            / 100.0;
        let charge_kwh = max_charge_this_block.min(energy_needed_to_target);

        eval.energy_flows.solar_generation_kwh = context.solar_forecast_kwh;
        eval.energy_flows.household_consumption_kwh = context.consumption_forecast_kwh;
        eval.energy_flows.battery_charge_kwh =
            charge_kwh * context.control_config.battery_efficiency;

        // Grid import = charge + consumption - solar (clamped to minimum 0.0)
        eval.energy_flows.grid_import_kwh =
            (charge_kwh + context.consumption_forecast_kwh - context.solar_forecast_kwh).max(0.0);

        // Calculate costs
        let import_cost = economics::grid_import_cost(
            eval.energy_flows.grid_import_kwh,
            context.price_block.price_czk_per_kwh,
        );

        let wear_cost = economics::battery_degradation_cost(
            charge_kwh,
            context.control_config.battery_wear_cost_czk_per_kwh,
        );

        let efficiency_loss_cost = economics::grid_import_cost(
            economics::efficiency_loss(charge_kwh, context.control_config.battery_efficiency),
            context.price_block.price_czk_per_kwh,
        );

        eval.cost_czk = import_cost + wear_cost + efficiency_loss_cost;

        // Revenue: Use actual evening price from data
        let evening_price = self.calculate_evening_price(all_blocks);
        let assumed_evening_price = if evening_price > 0.0 {
            evening_price
        } else {
            context.price_block.price_czk_per_kwh * 1.5 // Fallback
        };

        eval.revenue_czk = economics::grid_import_cost(
            eval.energy_flows.battery_charge_kwh,
            assumed_evening_price,
        );

        // Opportunity boost: account for missing this cheap window
        if target_soc > context.current_battery_soc {
            let upcoming_blocks: Vec<f32> = all_blocks
                .iter()
                .filter(|b| b.block_start > context.price_block.block_start)
                .map(|b| b.price_czk_per_kwh)
                .collect();

            if !upcoming_blocks.is_empty() {
                let remaining_avg_price =
                    upcoming_blocks.iter().sum::<f32>() / upcoming_blocks.len() as f32;
                let current_price = context.price_block.price_czk_per_kwh;

                let energy_still_needed = context.control_config.battery_capacity_kwh
                    * (target_soc - context.current_battery_soc)
                    / 100.0;

                let opportunity_value =
                    energy_still_needed * (remaining_avg_price - current_price).max(0.0);

                // Boost profit by weighting factor of opportunity value
                eval.revenue_czk += opportunity_value * OPPORTUNITY_VALUE_WEIGHT;

                tracing::debug!(
                    "Opportunity boost: +{:.2} CZK (still need {:.2} kWh, current {:.3} vs avg {:.3})",
                    opportunity_value * OPPORTUNITY_VALUE_WEIGHT,
                    energy_still_needed,
                    current_price,
                    remaining_avg_price
                );
            }
        }

        eval.calculate_net_profit();
        eval.reason = format!("Charging {:.2} kWh - {}", charge_kwh, reason);

        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::TimeBlockPrice;
    use crate::resources::ControlConfig;
    use chrono::{TimeZone, Utc};

    fn create_test_config() -> ControlConfig {
        ControlConfig {
            force_charge_hours: 4,
            force_discharge_hours: 2,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            maximum_export_power_w: 5000,
            battery_capacity_kwh: 23.0,
            battery_wear_cost_czk_per_kwh: 0.125,
            battery_efficiency: 0.95,
            min_mode_change_interval_secs: 300,
            average_household_load_kw: 0.5,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            ..Default::default()
        }
    }

    #[test]
    fn test_unified_smart_charge_selects_cheap_blocks() {
        let strategy = UnifiedSmartChargeStrategy::default();
        let config = create_test_config();
        let now = Utc.with_ymd_and_hms(2025, 1, 12, 3, 0, 0).unwrap();

        let all_blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 0.30, // Cheap - current
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::hours(1),
                duration_minutes: 15,
                price_czk_per_kwh: 0.32, // Cheap
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::hours(6),
                duration_minutes: 15,
                price_czk_per_kwh: 0.80, // Expensive
            },
        ];

        let price_block = &all_blocks[0];

        let context = EvaluationContext {
            price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.1,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.25,
            all_price_blocks: Some(&all_blocks),
        };

        let eval = strategy.evaluate(&context);

        // Should charge at cheap price
        assert_eq!(eval.mode, InverterOperationMode::ForceCharge);
        assert!(eval.energy_flows.battery_charge_kwh > 0.0);
    }

    #[test]
    fn test_unified_smart_charge_skips_expensive_blocks() {
        let strategy = UnifiedSmartChargeStrategy::default();
        let config = create_test_config();
        let now = Utc.with_ymd_and_hms(2025, 1, 12, 10, 0, 0).unwrap();

        let all_blocks = vec![
            TimeBlockPrice {
                block_start: now - chrono::Duration::hours(2),
                duration_minutes: 15,
                price_czk_per_kwh: 0.30, // Cheap (past)
            },
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 0.80, // Expensive - current
            },
        ];

        let price_block = &all_blocks[1];

        let context = EvaluationContext {
            price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.1,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.25,
            all_price_blocks: Some(&all_blocks),
        };

        let eval = strategy.evaluate(&context);

        // Should NOT charge at expensive price
        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
    }

    #[test]
    fn test_unified_smart_charge_respects_soc_targets() {
        let strategy = UnifiedSmartChargeStrategy::default();
        let config = create_test_config();
        let now = Utc.with_ymd_and_hms(2025, 1, 12, 3, 0, 0).unwrap();

        let all_blocks = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.30,
        }];

        let price_block = &all_blocks[0];

        let context = EvaluationContext {
            price_block,
            control_config: &config,
            current_battery_soc: 85.0, // Already above night target
            solar_forecast_kwh: 0.1,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.25,
            all_price_blocks: Some(&all_blocks),
        };

        let eval = strategy.evaluate(&context);

        // Should NOT charge when already above target
        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
    }

    #[test]
    fn test_strategy_name() {
        let strategy = UnifiedSmartChargeStrategy::default();
        assert_eq!(strategy.name(), "Unified-Smart-Charge");
    }
}
