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

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::Timelike;

/// Morning Pre-Charge strategy for avoiding expensive morning grid imports
#[derive(Debug, Clone)]
pub struct MorningPreChargeStrategy {
    enabled: bool,
}

impl MorningPreChargeStrategy {
    /// Create a new Morning Pre-Charge strategy
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Identify morning peak price (00:00-10:00)
    fn find_morning_peak_price(&self, all_blocks: &[TimeBlockPrice]) -> Option<f32> {
        let morning_prices: Vec<f32> = all_blocks
            .iter()
            .filter(|b| {
                let hour = b.block_start.time().hour();
                (0..10).contains(&hour)
            })
            .map(|b| b.price_czk_per_kwh)
            .collect();

        if morning_prices.is_empty() {
            return None;
        }

        // Return the maximum price during morning window
        morning_prices
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .copied()
    }

    /// Find the cheapest 3-4 blocks during night time (22:00-08:00)
    fn find_cheapest_night_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
    ) -> Vec<(usize, chrono::DateTime<chrono::Utc>, f32)> {
        // Filter to night blocks (22:00-08:00) that are upcoming
        let mut night_blocks: Vec<(usize, chrono::DateTime<chrono::Utc>, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                let hour = b.block_start.time().hour();
                !(8..22).contains(&hour) && b.block_start >= current_block_start
            })
            .map(|(idx, b)| (idx, b.block_start, b.price_czk_per_kwh))
            .collect();

        // Sort by price (cheapest first)
        night_blocks.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

        // Take 3-4 cheapest blocks
        night_blocks.into_iter().take(4).collect()
    }

    /// Check if we should charge based on morning pre-charge logic
    fn should_charge_now(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
    ) -> (bool, String, f32) {
        // Find current block index
        let current_block_idx = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start);

        let Some(current_idx) = current_block_idx else {
            return (
                false,
                "Could not find current block in price data".to_string(),
                0.0,
            );
        };

        let current_hour = context.price_block.block_start.time().hour();

        // Only operate during night hours (22:00-08:00)
        if (8..22).contains(&current_hour) {
            return (
                false,
                format!("Not in night window (current hour: {current_hour})"),
                0.0,
            );
        }

        // Find morning peak price
        let Some(morning_peak_price) = self.find_morning_peak_price(all_blocks) else {
            return (
                false,
                "No morning peak price data available".to_string(),
                0.0,
            );
        };

        // Find cheapest night blocks
        let cheapest_night_blocks =
            self.find_cheapest_night_blocks(all_blocks, context.price_block.block_start);

        if cheapest_night_blocks.is_empty() {
            return (false, "No night blocks available".to_string(), 0.0);
        }

        let min_night_price = cheapest_night_blocks[0].2;
        let price_diff = morning_peak_price - min_night_price;

        // Check if price difference is significant (>0.5 CZK)
        if price_diff <= 0.5 {
            return (
                false,
                format!(
                    "Price difference too small (morning peak: {:.3} CZK, night min: {:.3} CZK, diff: {:.3} CZK)",
                    morning_peak_price, min_night_price, price_diff
                ),
                0.0,
            );
        }

        // Check if current block is in the cheapest night blocks
        let is_in_cheapest = cheapest_night_blocks
            .iter()
            .any(|(idx, _, _)| *idx == current_idx);

        if !is_in_cheapest {
            let max_cheap_price = cheapest_night_blocks
                .iter()
                .map(|(_, _, price)| *price)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.0);

            return (
                false,
                format!(
                    "Not in cheapest night blocks (current: {:.3} CZK, max cheap: {:.3} CZK, {} blocks)",
                    context.price_block.price_czk_per_kwh,
                    max_cheap_price,
                    cheapest_night_blocks.len()
                ),
                0.0,
            );
        }

        let remaining_cheap_blocks = cheapest_night_blocks
            .iter()
            .filter(|(_, start, _)| *start >= context.price_block.block_start)
            .count();

        (
            true,
            format!(
                "Charging during cheap night block ({:.3} CZK, {} of {} blocks, morning peak: {:.3} CZK)",
                context.price_block.price_czk_per_kwh,
                remaining_cheap_blocks,
                cheapest_night_blocks.len(),
                morning_peak_price
            ),
            morning_peak_price,
        )
    }
}

impl Default for MorningPreChargeStrategy {
    fn default() -> Self {
        Self::new(true)
    }
}

impl EconomicStrategy for MorningPreChargeStrategy {
    fn name(&self) -> &str {
        "Morning-Pre-Charge"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse, // Default to self-use
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

        // Early exit if no all_price_blocks provided
        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available for morning pre-charge planning".to_string();
            return eval;
        };

        // Check if we should charge based on morning pre-charge logic
        let (should_charge, reason, morning_peak_price) =
            self.should_charge_now(context, all_blocks);

        if !should_charge {
            // Self-use mode
            eval.mode = InverterOperationMode::SelfUse;
            eval.reason = reason;

            // Calculate self-use economics
            let solar = context.solar_forecast_kwh;
            let consumption = context.consumption_forecast_kwh;

            eval.energy_flows.solar_generation_kwh = solar;
            eval.energy_flows.household_consumption_kwh = consumption;

            if solar >= consumption {
                eval.energy_flows.grid_import_kwh = 0.0;
                eval.revenue_czk =
                    economics::grid_import_cost(consumption, context.price_block.price_czk_per_kwh);
                eval.cost_czk = 0.0;
            } else {
                let deficit = consumption - solar;
                eval.energy_flows.grid_import_kwh = deficit;
                eval.cost_czk =
                    economics::grid_import_cost(deficit, context.price_block.price_czk_per_kwh);
                eval.revenue_czk = 0.0;
            }

            eval.calculate_net_profit();
            return eval;
        }

        // CHARGE MODE
        eval.mode = InverterOperationMode::ForceCharge;

        // Target 60% SOC for morning readiness
        let target_soc = if context.current_battery_soc < 50.0 {
            60.0
        } else {
            50.0
        };

        let max_charge_this_block = context.control_config.max_battery_charge_rate_kw * 0.25;
        let energy_needed_to_target = context.control_config.battery_capacity_kwh
            * (target_soc - context.current_battery_soc)
            / 100.0;

        let charge_kwh = max_charge_this_block.min(energy_needed_to_target).max(0.0);

        eval.energy_flows.solar_generation_kwh = context.solar_forecast_kwh;
        eval.energy_flows.household_consumption_kwh = context.consumption_forecast_kwh;
        eval.energy_flows.battery_charge_kwh =
            charge_kwh * context.control_config.battery_efficiency;

        // Grid import = charge + consumption - solar
        eval.energy_flows.grid_import_kwh =
            charge_kwh + context.consumption_forecast_kwh - context.solar_forecast_kwh.max(0.0);

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

        // Revenue: Avoided import during morning peak
        eval.revenue_czk =
            economics::grid_import_cost(eval.energy_flows.battery_charge_kwh, morning_peak_price);

        eval.calculate_net_profit();
        eval.reason = format!("Charging {:.2} kWh - {}", charge_kwh, reason);

        eval
    }
}
