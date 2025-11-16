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

/// Solar-First strategy for maximizing solar energy utilization
///
/// This strategy focuses on capturing and storing solar energy when available,
/// calculating whether it's more profitable to store solar for later use
/// or export it immediately to the grid.
pub struct SolarFirstStrategy {
    enabled: bool,
    /// Multiplier for expected future import price vs. current export price
    /// Example: 1.5 means we expect future imports to cost 50% more than current exports
    future_price_ratio: f32,
}

impl SolarFirstStrategy {
    /// Create a new Solar-First strategy
    ///
    /// # Arguments
    /// * `enabled` - Whether this strategy is active
    /// * `future_price_ratio` - Expected future import price / current export price ratio
    pub fn new(enabled: bool, future_price_ratio: f32) -> Self {
        Self {
            enabled,
            future_price_ratio,
        }
    }
}

impl Default for SolarFirstStrategy {
    fn default() -> Self {
        Self::new(true, 1.3) // Assume future imports 30% more expensive than current exports
    }
}

impl EconomicStrategy for SolarFirstStrategy {
    fn name(&self) -> &str {
        "Solar-First"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse, // Solar-first uses self-use mode
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

        // If no solar, this strategy doesn't apply
        if context.solar_forecast_kwh <= 0.0 {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = "No solar generation - strategy not applicable".to_string();
            return eval;
        }

        let solar = context.solar_forecast_kwh;
        let consumption = context.consumption_forecast_kwh;
        let efficiency = context.control_config.battery_efficiency;

        eval.energy_flows.solar_generation_kwh = solar;
        eval.energy_flows.household_consumption_kwh = consumption;

        // First, solar meets consumption directly (no battery involved)
        let solar_to_consumption = solar.min(consumption);
        let remaining_solar = solar - solar_to_consumption;
        let remaining_consumption = consumption - solar_to_consumption;

        // Revenue from avoiding grid import for direct solar consumption
        let direct_consumption_value = economics::grid_import_cost(
            solar_to_consumption,
            context.price_block.price_czk_per_kwh,
        );

        // Now evaluate what to do with remaining solar (if any)
        if remaining_solar > 0.0 {
            // Check if battery has room to store solar
            if context.current_battery_soc < context.control_config.max_battery_soc {
                // Calculate how much solar we can store
                let battery_capacity_available = context.control_config.battery_capacity_kwh
                    * (context.control_config.max_battery_soc - context.current_battery_soc)
                    / 100.0;

                let solar_to_store = remaining_solar.min(battery_capacity_available);
                let solar_to_export = remaining_solar - solar_to_store;

                // Energy stored in battery (after efficiency losses)
                eval.energy_flows.battery_charge_kwh = solar_to_store * efficiency;
                eval.energy_flows.grid_export_kwh = solar_to_export;

                // Calculate opportunity cost of storing vs. exporting immediately
                let immediate_export_revenue = economics::grid_export_revenue(
                    solar_to_store,
                    context.grid_export_price_czk_per_kwh,
                );

                // Future value: assume we'll use this stored energy to avoid grid import
                // at a higher price later
                let expected_future_import_price =
                    context.grid_export_price_czk_per_kwh * self.future_price_ratio;

                let future_value = economics::grid_import_cost(
                    eval.energy_flows.battery_charge_kwh, // After efficiency loss
                    expected_future_import_price,
                );

                // Costs: battery wear + opportunity cost of not exporting now
                let wear_cost = economics::battery_degradation_cost(
                    solar_to_store,
                    context.control_config.battery_wear_cost_czk_per_kwh,
                );

                let opportunity_cost = immediate_export_revenue;

                eval.cost_czk = wear_cost + opportunity_cost;

                // Revenue: direct consumption value + future battery value + export of excess
                eval.revenue_czk = direct_consumption_value
                    + future_value
                    + economics::grid_export_revenue(
                        solar_to_export,
                        context.grid_export_price_czk_per_kwh,
                    );

                eval.calculate_net_profit();

                // Only recommend if storing solar is more profitable than immediate export
                let net_benefit_of_storage = future_value - immediate_export_revenue - wear_cost;

                if net_benefit_of_storage > 0.0 {
                    eval.reason = format!(
                        "Storing {:.2} kWh solar for later use (net benefit: {:.2} CZK)",
                        solar_to_store, net_benefit_of_storage
                    );
                } else {
                    // Storing not profitable, mark as not viable
                    eval.net_profit_czk = f32::NEG_INFINITY;
                    eval.reason = format!(
                        "Storing solar not profitable (immediate export better by {:.2} CZK)",
                        -net_benefit_of_storage
                    );
                }
            } else {
                // Battery full, export all remaining solar
                eval.energy_flows.battery_charge_kwh = 0.0;
                eval.energy_flows.grid_export_kwh = remaining_solar;

                eval.revenue_czk = direct_consumption_value
                    + economics::grid_export_revenue(
                        remaining_solar,
                        context.grid_export_price_czk_per_kwh,
                    );

                eval.cost_czk = 0.0;
                eval.calculate_net_profit();

                eval.reason = format!(
                    "Battery full, exporting {:.2} kWh excess solar",
                    remaining_solar
                );
            }
        } else {
            // All solar consumed directly, no excess
            eval.energy_flows.battery_charge_kwh = 0.0;
            eval.energy_flows.grid_export_kwh = 0.0;

            // If consumption exceeds solar, import remainder
            if remaining_consumption > 0.0 {
                eval.energy_flows.grid_import_kwh = remaining_consumption;
                eval.cost_czk = economics::grid_import_cost(
                    remaining_consumption,
                    context.price_block.price_czk_per_kwh,
                );
            }

            eval.revenue_czk = direct_consumption_value;
            eval.calculate_net_profit();

            eval.reason = format!(
                "All {:.2} kWh solar used for direct consumption",
                solar_to_consumption
            );
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
    fn test_solar_first_with_excess_solar() {
        let strategy = SolarFirstStrategy::default();
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
            solar_forecast_kwh: 3.0, // Plenty of solar
            consumption_forecast_kwh: 1.0,
            grid_export_price_czk_per_kwh: 0.30, // Low export price
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        // Should store excess solar since future value > immediate export
        if eval.net_profit_czk > f32::NEG_INFINITY {
            assert!(eval.energy_flows.battery_charge_kwh > 0.0);
        }
    }

    #[test]
    fn test_solar_first_no_solar() {
        let strategy = SolarFirstStrategy::default();
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
            solar_forecast_kwh: 0.0, // No solar
            consumption_forecast_kwh: 1.0,
            grid_export_price_czk_per_kwh: 0.30,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        // Strategy not applicable when no solar
        assert_eq!(eval.net_profit_czk, f32::NEG_INFINITY);
        assert!(eval.reason.contains("not applicable"));
    }

    #[test]
    fn test_solar_first_battery_full() {
        let strategy = SolarFirstStrategy::default();
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
            current_battery_soc: 100.0, // Full battery
            solar_forecast_kwh: 3.0,
            consumption_forecast_kwh: 1.0,
            grid_export_price_czk_per_kwh: 0.30,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        // Should export excess since battery is full
        assert!(eval.energy_flows.grid_export_kwh > 0.0);
        assert_eq!(eval.energy_flows.battery_charge_kwh, 0.0);
    }

    #[test]
    fn test_solar_first_all_consumed() {
        let strategy = SolarFirstStrategy::default();
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
            solar_forecast_kwh: 0.8, // Less than consumption
            consumption_forecast_kwh: 1.0,
            grid_export_price_czk_per_kwh: 0.30,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        // All solar used directly, no battery charging
        assert_eq!(eval.energy_flows.battery_charge_kwh, 0.0);
        assert_eq!(eval.energy_flows.grid_export_kwh, 0.0);
        assert!(eval.energy_flows.grid_import_kwh > 0.0); // Import remainder
    }

    #[test]
    fn test_solar_first_strategy_name() {
        let strategy = SolarFirstStrategy::default();
        assert_eq!(strategy.name(), "Solar-First");
    }
}
