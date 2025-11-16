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

use crate::components::InverterOperationMode;
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::Timelike;

/// Day-Ahead Charge Planning Strategy
///
/// This strategy performs multi-period optimization to determine optimal charging
/// times across the entire price horizon (typically 24-48 hours).
///
/// Key features:
/// - Looks ahead at all available price blocks
/// - Calculates total energy needed for evening/night consumption
/// - Compares cost of charging at different times
/// - Prefers charging at absolute cheapest times even if they're hours away
/// - Avoids charging at "relatively cheap" mid-day prices when night prices are lower
///
/// Example scenario:
/// - Night price: 1.12 CZK/kWh
/// - Afternoon price: 1.92 CZK/kWh (within 10% of local minimum, but much higher than night)
/// - Strategy: Charge more at night to avoid afternoon charging
#[derive(Debug, Clone)]
pub struct DayAheadChargePlanningStrategy {
    enabled: bool,
}

impl DayAheadChargePlanningStrategy {
    /// Create a new Day-Ahead Charge Planning strategy
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Find the cheapest charging blocks across the entire price horizon
    /// Returns a sorted list of (block_index, price) tuples
    fn find_cheapest_charging_windows(
        &self,
        all_blocks: &[crate::components::TimeBlockPrice],
        current_block_start: chrono::DateTime<chrono::Utc>,
        num_blocks_needed: usize,
    ) -> Vec<(usize, f32)> {
        let mut upcoming_blocks: Vec<(usize, f32, chrono::DateTime<chrono::Utc>)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.block_start >= current_block_start)
            .map(|(idx, b)| (idx, b.price_czk_per_kwh, b.block_start))
            .collect();

        // Sort by price (cheapest first)
        upcoming_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take the N cheapest blocks
        upcoming_blocks
            .into_iter()
            .take(num_blocks_needed)
            .map(|(idx, price, _)| (idx, price))
            .collect()
    }

    /// Calculate if we should charge now based on day-ahead planning
    fn should_charge_now(
        &self,
        context: &EvaluationContext,
        all_blocks: &[crate::components::TimeBlockPrice],
    ) -> (bool, String) {
        // Find current block index
        let current_block_idx = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start);

        let Some(current_idx) = current_block_idx else {
            return (
                false,
                "Could not find current block in price data".to_string(),
            );
        };

        let current_price = context.price_block.price_czk_per_kwh;

        // Calculate how many blocks we need to charge to prepare for evening
        // Estimate: need to reach ~90% SOC by evening
        let target_soc = 90.0;
        let energy_needed = context.control_config.battery_capacity_kwh
            * (target_soc - context.current_battery_soc)
            / 100.0;

        if energy_needed <= 0.0 {
            return (
                false,
                format!(
                    "Battery already at {:.1}% (target: {}%)",
                    context.current_battery_soc, target_soc
                ),
            );
        }

        let blocks_needed = (energy_needed
            / (context.control_config.max_battery_charge_rate_kw * 0.25))
            .ceil() as usize;

        // Find the cheapest blocks in the day
        let cheapest_blocks = self.find_cheapest_charging_windows(
            all_blocks,
            context.price_block.block_start,
            blocks_needed,
        );

        // Check if current block is in the cheapest set
        let is_in_cheapest = cheapest_blocks.iter().any(|(idx, _)| *idx == current_idx);

        if !is_in_cheapest {
            // Find the price of the most expensive block in the cheapest set
            let max_cheap_price = cheapest_blocks
                .iter()
                .map(|(_, price)| *price)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.0);

            return (
                false,
                format!(
                    "Not in day-ahead cheapest blocks (current: {:.3} CZK, max cheap: {:.3} CZK, need {} blocks)",
                    current_price, max_cheap_price, blocks_needed
                ),
            );
        }

        (
            true,
            format!(
                "In day-ahead cheapest blocks ({:.3} CZK, {} of {} blocks planned)",
                current_price,
                cheapest_blocks.len(),
                blocks_needed
            ),
        )
    }
}

impl Default for DayAheadChargePlanningStrategy {
    fn default() -> Self {
        Self::new(true)
    }
}

impl EconomicStrategy for DayAheadChargePlanningStrategy {
    fn name(&self) -> &str {
        "Day-Ahead-Planning"
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
            eval.reason = "No price data available for day-ahead planning".to_string();
            return eval;
        };

        // Check if we should charge based on day-ahead optimization
        let (should_charge, reason) = self.should_charge_now(context, all_blocks);

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

        // Calculate charge amount
        let target_soc = 90.0;
        let max_charge_this_block = context.control_config.max_battery_charge_rate_kw * 0.25;
        let energy_needed_to_target = context.control_config.battery_capacity_kwh
            * (target_soc - context.current_battery_soc)
            / 100.0;

        let charge_kwh = max_charge_this_block.min(energy_needed_to_target);

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

        // Revenue: Avoided import during evening peak
        // Find the average price of evening blocks (17:00-23:00) to use as assumed evening price
        let evening_price = all_blocks
            .iter()
            .filter(|b| {
                let hour = b.block_start.time().hour();
                (17..23).contains(&hour)
            })
            .map(|b| b.price_czk_per_kwh)
            .sum::<f32>()
            / all_blocks
                .iter()
                .filter(|b| {
                    let hour = b.block_start.time().hour();
                    (17..23).contains(&hour)
                })
                .count() as f32;

        let assumed_evening_price = if evening_price > 0.0 {
            evening_price
        } else {
            // Fallback if no evening data available
            context.price_block.price_czk_per_kwh * 1.5
        };

        eval.revenue_czk = economics::grid_import_cost(
            eval.energy_flows.battery_charge_kwh,
            assumed_evening_price,
        );

        eval.calculate_net_profit();
        eval.reason = format!("Charging {:.2} kWh ({})", charge_kwh, reason);

        eval
    }
}
