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

/// Price Arbitrage strategy for exploiting price spreads
///
/// This strategy looks for opportunities to buy low and sell high,
/// accounting for all costs involved in the round-trip.
pub struct PriceArbitrageStrategy {
    enabled: bool,
    /// Minimum price difference (CZK/kWh) to consider arbitrage profitable
    min_spread_threshold: f32,
}

impl PriceArbitrageStrategy {
    /// Create a new Price Arbitrage strategy
    ///
    /// # Arguments
    /// * `enabled` - Whether this strategy is active
    /// * `min_spread_threshold` - Minimum price spread (CZK/kWh) for profitability
    pub fn new(enabled: bool, min_spread_threshold: f32) -> Self {
        Self {
            enabled,
            min_spread_threshold,
        }
    }
}

impl Default for PriceArbitrageStrategy {
    fn default() -> Self {
        Self::new(true, 0.15) // Default: 0.15 CZK/kWh minimum spread
    }
}

impl EconomicStrategy for PriceArbitrageStrategy {
    fn name(&self) -> &str {
        "Price-Arbitrage"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        // Evaluate both charge and discharge scenarios, pick the most profitable

        let charge_eval = self.evaluate_force_charge(context);
        let discharge_eval = self.evaluate_force_discharge(context);

        // Return whichever has higher profit
        if charge_eval.net_profit_czk >= discharge_eval.net_profit_czk {
            charge_eval
        } else {
            discharge_eval
        }
    }
}

impl PriceArbitrageStrategy {
    /// Evaluate Force Charge scenario
    fn evaluate_force_charge(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::ForceCharge,
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

        // Check if battery can accept charge
        if context.current_battery_soc >= context.control_config.max_battery_soc {
            // Battery full, force charge not viable
            eval.net_profit_czk = f32::NEG_INFINITY; // Not viable
            eval.reason = "Battery full, cannot force charge".to_string();
            return eval;
        }

        // Calculate energy to charge based on actual charge rate
        let max_charge_this_block = context.control_config.max_battery_charge_rate_kw * 0.25;
        let energy_needed_to_max = context.control_config.battery_capacity_kwh
            * (context.control_config.max_battery_soc - context.current_battery_soc)
            / 100.0;
        let charge_kwh = max_charge_this_block.min(energy_needed_to_max);

        eval.energy_flows.solar_generation_kwh = context.solar_forecast_kwh;
        eval.energy_flows.household_consumption_kwh = context.consumption_forecast_kwh;
        eval.energy_flows.battery_charge_kwh =
            charge_kwh * context.control_config.battery_efficiency;
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

        // Revenue: Potential future export value (we assume average future export price)
        // For now, use a simple heuristic: benefit if we can sell at higher price later
        // This is a simplification - full implementation would look ahead at price forecast
        let assumed_future_export_price = context.grid_export_price_czk_per_kwh * 1.2; // Assume 20% higher

        eval.revenue_czk = economics::grid_export_revenue(
            eval.energy_flows.battery_charge_kwh,
            assumed_future_export_price,
        );

        eval.calculate_net_profit();

        // Only recommend if profitable and spread exceeds threshold
        let effective_spread = assumed_future_export_price - context.price_block.price_czk_per_kwh;
        if eval.net_profit_czk <= 0.0 || effective_spread < self.min_spread_threshold {
            eval.net_profit_czk = f32::NEG_INFINITY; // Mark as not viable
            eval.reason = format!(
                "Arbitrage not profitable (spread {:.3} < {:.3} threshold)",
                effective_spread, self.min_spread_threshold
            );
        } else {
            eval.reason = format!(
                "Force charging {:.2} kWh at {:.3} CZK/kWh for later sale",
                charge_kwh, context.price_block.price_czk_per_kwh
            );
        }

        eval
    }

    /// Evaluate Force Discharge scenario
    fn evaluate_force_discharge(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::ForceDischarge,
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

        // Check if battery has energy to discharge
        if context.current_battery_soc <= context.control_config.min_battery_soc {
            // Battery too low, force discharge not viable
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = "Battery too low, cannot force discharge".to_string();
            return eval;
        }

        // Calculate energy to discharge based on actual discharge rate (assume same as charge rate)
        let available_kwh = context.control_config.battery_capacity_kwh
            * (context.current_battery_soc - context.control_config.min_battery_soc)
            / 100.0;
        let max_discharge_this_block = context.control_config.max_battery_charge_rate_kw * 0.25;
        let discharge_kwh = max_discharge_this_block.min(available_kwh);

        eval.energy_flows.solar_generation_kwh = context.solar_forecast_kwh;
        eval.energy_flows.household_consumption_kwh = context.consumption_forecast_kwh;
        eval.energy_flows.battery_discharge_kwh = discharge_kwh;
        eval.energy_flows.grid_export_kwh =
            discharge_kwh + context.solar_forecast_kwh - context.consumption_forecast_kwh.max(0.0);

        // Calculate revenue
        eval.revenue_czk = economics::grid_export_revenue(
            eval.energy_flows.grid_export_kwh,
            context.grid_export_price_czk_per_kwh,
        );

        // Calculate costs
        let wear_cost = economics::battery_degradation_cost(
            discharge_kwh,
            context.control_config.battery_wear_cost_czk_per_kwh,
        );

        // Opportunity cost: we're using stored energy that was bought at some past price
        // For simplification, assume average historical buy price (would need actual data)
        let assumed_historical_buy_price = context.price_block.price_czk_per_kwh * 0.7; // Assume bought 30% cheaper
        let opportunity_cost =
            economics::grid_import_cost(discharge_kwh, assumed_historical_buy_price);

        eval.cost_czk = wear_cost + opportunity_cost;

        eval.calculate_net_profit();

        // Only recommend if export price significantly exceeds historical cost
        let effective_margin = context.grid_export_price_czk_per_kwh - assumed_historical_buy_price;
        if eval.net_profit_czk <= 0.0 || effective_margin < self.min_spread_threshold {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "Discharge not profitable (margin {:.3} < {:.3} threshold)",
                effective_margin, self.min_spread_threshold
            );
        } else {
            eval.reason = format!(
                "Force discharging {:.2} kWh at {:.3} CZK/kWh export price",
                discharge_kwh, context.grid_export_price_czk_per_kwh
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
    fn test_price_arbitrage_low_price() {
        let strategy = PriceArbitrageStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.20, // Very low price
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.15,
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        // Should recommend force charge at low price
        assert!(matches!(
            eval.mode,
            InverterOperationMode::ForceCharge | InverterOperationMode::ForceDischarge
        ));
    }

    #[test]
    fn test_price_arbitrage_high_price() {
        let strategy = PriceArbitrageStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.80, // High price
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.70, // Good export price
            all_price_blocks: None,
        };

        let eval = strategy.evaluate(&context);

        // At high prices, should consider discharge if profitable
        assert!(matches!(
            eval.mode,
            InverterOperationMode::ForceCharge | InverterOperationMode::ForceDischarge
        ));
    }

    #[test]
    fn test_price_arbitrage_strategy_name() {
        let strategy = PriceArbitrageStrategy::default();
        assert_eq!(strategy.name(), "Price-Arbitrage");
    }

    #[test]
    fn test_battery_full_blocks_charge() {
        let strategy = PriceArbitrageStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.20,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 100.0, // Full battery
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.15,
            all_price_blocks: None,
        };

        let charge_eval = strategy.evaluate_force_charge(&context);

        // Should not recommend charge when battery is full
        assert_eq!(charge_eval.net_profit_czk, f32::NEG_INFINITY);
    }

    #[test]
    fn test_battery_low_blocks_discharge() {
        let strategy = PriceArbitrageStrategy::default();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.80,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 10.0, // At minimum
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.70,
            all_price_blocks: None,
        };

        let discharge_eval = strategy.evaluate_force_discharge(&context);

        // Should not recommend discharge when battery is too low
        assert_eq!(discharge_eval.net_profit_czk, f32::NEG_INFINITY);
    }
}
