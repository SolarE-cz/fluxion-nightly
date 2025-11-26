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

/// Self-Use strategy for normal battery operation
///
/// This strategy maximizes self-consumption and minimizes grid dependency
/// while accounting for battery wear and efficiency losses.
pub struct SelfUseStrategy {
    enabled: bool,
}

impl SelfUseStrategy {
    /// Create a new Self-Use strategy
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl Default for SelfUseStrategy {
    fn default() -> Self {
        Self::new(true)
    }
}

impl EconomicStrategy for SelfUseStrategy {
    fn name(&self) -> &str {
        "Self-Use"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
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

        // Calculate energy flows for self-use mode
        let solar = context.solar_forecast_kwh;
        let consumption = context.consumption_forecast_kwh;
        let efficiency = context.control_config.battery_efficiency;

        eval.energy_flows.solar_generation_kwh = solar;
        eval.energy_flows.household_consumption_kwh = consumption;

        // Determine energy routing based on solar vs. consumption
        if solar >= consumption {
            // Excess solar available
            let excess = solar - consumption;

            // If battery has room, charge it (simplified - ignores actual SOC capacity)
            if context.current_battery_soc < context.control_config.max_battery_soc {
                // Charge battery with excess (accounting for efficiency)
                eval.energy_flows.battery_charge_kwh = excess * efficiency;

                // No grid import needed
                eval.energy_flows.grid_import_kwh = 0.0;

                // If still excess after battery charge, export
                let efficiency_loss = excess * (1.0 - efficiency);
                if excess > efficiency_loss {
                    eval.energy_flows.grid_export_kwh = 0.0; // Simplified: assume all goes to battery
                } else {
                    eval.energy_flows.grid_export_kwh = 0.0;
                }

                // Calculate costs: only battery wear
                let wear_cost = economics::battery_degradation_cost(
                    eval.energy_flows.battery_charge_kwh,
                    context.control_config.battery_wear_cost_czk_per_kwh,
                );

                eval.cost_czk = wear_cost;

                // Calculate revenue: avoided grid import for consumption
                eval.revenue_czk =
                    economics::grid_import_cost(consumption, context.price_block.price_czk_per_kwh);

                eval.reason = format!(
                    "Storing {:.2} kWh solar for later use (avoided import cost)",
                    eval.energy_flows.battery_charge_kwh
                );
            } else {
                // Battery full, export excess
                eval.energy_flows.battery_charge_kwh = 0.0;
                eval.energy_flows.grid_export_kwh = excess;
                eval.energy_flows.grid_import_kwh = 0.0;

                // Revenue from export and avoided import
                eval.revenue_czk =
                    economics::grid_export_revenue(excess, context.grid_export_price_czk_per_kwh)
                        + economics::grid_import_cost(
                            consumption,
                            context.price_block.price_czk_per_kwh,
                        );

                eval.cost_czk = 0.0;

                eval.reason = format!("Exporting {:.2} kWh excess solar (battery full)", excess);
            }
        } else {
            // Consumption exceeds solar, need additional energy
            let deficit = consumption - solar;

            // Use battery if available
            if context.current_battery_soc > context.control_config.min_battery_soc {
                // Discharge battery to meet deficit
                let battery_discharge = deficit.min(
                    context.control_config.battery_capacity_kwh
                        * (context.current_battery_soc - context.control_config.min_battery_soc)
                        / 100.0,
                );

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                // Remaining deficit from grid
                let remaining_deficit = deficit - battery_discharge;
                eval.energy_flows.grid_import_kwh = remaining_deficit;

                // Costs: battery wear + grid import
                let wear_cost = economics::battery_degradation_cost(
                    battery_discharge,
                    context.control_config.battery_wear_cost_czk_per_kwh,
                );

                let import_cost = economics::grid_import_cost(
                    remaining_deficit,
                    context.price_block.price_czk_per_kwh,
                );

                eval.cost_czk = wear_cost + import_cost;

                // Revenue: using stored energy avoids full grid import
                // We count the battery discharge as avoided import
                eval.revenue_czk = economics::grid_import_cost(
                    battery_discharge,
                    context.price_block.price_czk_per_kwh,
                );

                eval.reason = format!(
                    "Using {:.2} kWh from battery to offset grid import",
                    battery_discharge
                );
            } else {
                // Battery too low, import all deficit from grid
                eval.energy_flows.battery_discharge_kwh = 0.0;
                eval.energy_flows.grid_import_kwh = deficit;

                eval.cost_czk =
                    economics::grid_import_cost(deficit, context.price_block.price_czk_per_kwh);
                eval.revenue_czk = 0.0;

                eval.reason = format!("Importing {:.2} kWh from grid (battery low)", deficit);
            }
        }

        // First, calculate local per-block economics (used as a fallback
        // when we don't have a full price horizon available).
        eval.calculate_net_profit();

        // If we have a price horizon, override economics to represent the
        // "do nothing" baseline cost of serving expected consumption from
        // the grid (zero solar) for all remaining blocks.
        if let Some(all_blocks) = context.all_price_blocks {
            let per_block_import_kwh = context.consumption_forecast_kwh;

            if per_block_import_kwh > 0.0 && !all_blocks.is_empty() {
                let baseline_future_cost: f32 = all_blocks
                    .iter()
                    .map(|b| per_block_import_kwh * b.price_czk_per_kwh)
                    .sum();

                // Baseline self-use has no explicit revenue; it's the
                // estimated future grid cost if we do not actively
                // optimize (no proactive charging, no solar benefit).
                eval.revenue_czk = 0.0;
                eval.cost_czk = baseline_future_cost;
                eval.calculate_net_profit();

                eval.reason = format!(
                    "Baseline self-use: estimated future grid cost {:.2} CZK with {:.3} kWh/15min and no solar ({} remaining blocks)",
                    baseline_future_cost,
                    per_block_import_kwh,
                    all_blocks.len()
                );
            }
        }

        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::TimeBlockPrice;
    use crate::resources::ControlConfig;
    use chrono::Utc;

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
    fn test_self_use_with_excess_solar() {
        let strategy = SelfUseStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.50,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 2.0,
            consumption_forecast_kwh: 1.0,
            grid_export_price_czk_per_kwh: 0.40,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert!(eval.energy_flows.battery_charge_kwh > 0.0);
        assert_eq!(eval.energy_flows.grid_import_kwh, 0.0);
        assert!(eval.net_profit_czk >= 0.0);
    }

    #[test]
    fn test_self_use_with_deficit() {
        let strategy = SelfUseStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.50,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.5,
            consumption_forecast_kwh: 2.0,
            grid_export_price_czk_per_kwh: 0.40,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert!(eval.energy_flows.battery_discharge_kwh > 0.0);
        assert!(eval.energy_flows.grid_import_kwh >= 0.0);
    }

    #[test]
    fn test_self_use_strategy_name() {
        let strategy = SelfUseStrategy::default();
        assert_eq!(strategy.name(), "Self-Use");
    }
}
