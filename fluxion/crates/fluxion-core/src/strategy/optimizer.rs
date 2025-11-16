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

use crate::strategy::{BlockEvaluation, EconomicStrategy, EvaluationContext};
use std::sync::Arc;
use tracing::{debug, info};

/// Economic optimizer that selects the best strategy for each time block
pub struct EconomicOptimizer {
    /// List of strategies to evaluate (in order of evaluation)
    strategies: Vec<Arc<dyn EconomicStrategy>>,
}

impl EconomicOptimizer {
    /// Create a new economic optimizer with a set of strategies
    pub fn new(strategies: Vec<Arc<dyn EconomicStrategy>>) -> Self {
        Self { strategies }
    }

    /// Create a default optimizer with standard strategies
    pub fn with_default_strategies() -> Self {
        use crate::strategy::{
            MorningPreChargeStrategy, PriceArbitrageStrategy, SelfUseStrategy, SolarFirstStrategy,
            TimeAwareChargeStrategy,
        };

        let strategies: Vec<Arc<dyn EconomicStrategy>> = vec![
            Arc::new(MorningPreChargeStrategy::default()), // Night charging before morning peak
            Arc::new(TimeAwareChargeStrategy::default()),  // Time-based charging before evening
            Arc::new(PriceArbitrageStrategy::default()),
            Arc::new(SolarFirstStrategy::default()),
            Arc::new(SelfUseStrategy::default()), // Fallback strategy, always applicable
        ];

        info!(
            "Initialized EconomicOptimizer with {} strategies",
            strategies.len()
        );
        for strategy in &strategies {
            info!(
                "  - {} (enabled: {})",
                strategy.name(),
                strategy.is_enabled()
            );
        }

        Self::new(strategies)
    }

    /// Evaluate all strategies and select the best one for this time block
    ///
    /// Returns the evaluation from the strategy with the highest net profit.
    /// If all strategies return negative infinity (not applicable), returns
    /// the Self-Use strategy as a safe fallback.
    pub fn optimize(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut best_evaluation: Option<BlockEvaluation> = None;
        let mut best_profit = f32::NEG_INFINITY;

        debug!(
            "Evaluating {} strategies for block at {}",
            self.strategies.len(),
            context.price_block.block_start
        );

        // Evaluate each enabled strategy
        for strategy in &self.strategies {
            if !strategy.is_enabled() {
                debug!("  Skipping disabled strategy: {}", strategy.name());
                continue;
            }

            let evaluation = strategy.evaluate(context);

            debug!(
                "  Strategy '{}': mode={:?}, profit={:.3} CZK, reason={}",
                evaluation.strategy_name,
                evaluation.mode,
                evaluation.net_profit_czk,
                evaluation.reason
            );

            // Update best if this strategy has higher profit
            if evaluation.net_profit_czk > best_profit {
                best_profit = evaluation.net_profit_czk;
                best_evaluation = Some(evaluation);
            }
        }

        // Return the best evaluation, or create a fallback if all strategies were not viable
        match best_evaluation {
            Some(eval) => {
                debug!(
                    "Selected strategy '{}' with profit {:.3} CZK for block at {}",
                    eval.strategy_name, eval.net_profit_czk, eval.block_start
                );
                eval
            }
            None => {
                // This should rarely happen if Self-Use is included, but handle gracefully
                debug!("No strategy applicable, using safe Self-Use fallback");
                use crate::components::InverterOperationMode;
                use crate::strategy::{Assumptions, BlockEvaluation};

                let mut fallback = BlockEvaluation::new(
                    context.price_block.block_start,
                    context.price_block.duration_minutes,
                    InverterOperationMode::SelfUse,
                    "Fallback".to_string(),
                );

                fallback.assumptions = Assumptions {
                    solar_forecast_kwh: context.solar_forecast_kwh,
                    consumption_forecast_kwh: context.consumption_forecast_kwh,
                    current_battery_soc: context.current_battery_soc,
                    battery_efficiency: context.control_config.battery_efficiency,
                    battery_wear_cost_czk_per_kwh: context
                        .control_config
                        .battery_wear_cost_czk_per_kwh,
                    grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
                    grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
                };

                fallback.reason = "No strategy applicable, using safe Self-Use mode".to_string();
                fallback
            }
        }
    }

    /// Add a strategy to the optimizer
    pub fn add_strategy(&mut self, strategy: Arc<dyn EconomicStrategy>) {
        info!("Adding strategy: {}", strategy.name());
        self.strategies.push(strategy);
    }

    /// Get the number of strategies registered
    pub fn strategy_count(&self) -> usize {
        self.strategies.len()
    }

    /// Get the names of all registered strategies
    pub fn strategy_names(&self) -> Vec<String> {
        self.strategies
            .iter()
            .map(|s| s.name().to_string())
            .collect()
    }

    /// Evaluate all strategies and return all evaluations (for dashboard/debugging)
    ///
    /// This is useful for showing users why a particular strategy was chosen
    /// and what other options were considered.
    pub fn evaluate_all(&self, context: &EvaluationContext) -> Vec<BlockEvaluation> {
        self.strategies
            .iter()
            .filter(|s| s.is_enabled())
            .map(|strategy| strategy.evaluate(context))
            .collect()
    }
}

impl Default for EconomicOptimizer {
    fn default() -> Self {
        Self::with_default_strategies()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::TimeBlockPrice;
    use crate::resources::ControlConfig;
    use crate::strategy::{PriceArbitrageStrategy, SelfUseStrategy, SolarFirstStrategy};
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
    fn test_optimizer_initialization() {
        let optimizer = EconomicOptimizer::with_default_strategies();
        assert_eq!(optimizer.strategy_count(), 5);

        let names = optimizer.strategy_names();
        assert!(names.contains(&"Morning-Pre-Charge".to_string()));
        assert!(names.contains(&"Time-Aware-Charge".to_string()));
        assert!(names.contains(&"Price-Arbitrage".to_string()));
        assert!(names.contains(&"Solar-First".to_string()));
        assert!(names.contains(&"Self-Use".to_string()));
    }

    #[test]
    fn test_optimizer_selects_best_strategy() {
        let optimizer = EconomicOptimizer::with_default_strategies();
        let config = create_test_config();
        let now = Utc::now();

        // Low price scenario - should favor charging or self-use
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

        let result = optimizer.optimize(&context);

        // Should select some strategy
        assert!(!result.strategy_name.is_empty());
        assert!(!result.reason.is_empty());
    }

    #[test]
    fn test_optimizer_with_solar() {
        let optimizer = EconomicOptimizer::with_default_strategies();
        let config = create_test_config();
        let now = Utc::now();

        // Scenario with abundant solar and low export price
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
            grid_export_price_czk_per_kwh: 0.25, // Low export price
            all_price_blocks: None,
        };

        let result = optimizer.optimize(&context);

        // Solar-First or Self-Use should be selected
        assert!(result.strategy_name == "Solar-First" || result.strategy_name == "Self-Use");
    }

    #[test]
    fn test_optimizer_evaluate_all() {
        let optimizer = EconomicOptimizer::with_default_strategies();
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.40,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 1.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.30,
            all_price_blocks: None,
        };

        let all_evals = optimizer.evaluate_all(&context);

        // Should return evaluations from all enabled strategies
        assert!(all_evals.len() >= 2); // At least Self-Use and one other
    }

    #[test]
    fn test_add_custom_strategy() {
        let mut optimizer = EconomicOptimizer::new(vec![]);
        assert_eq!(optimizer.strategy_count(), 0);

        optimizer.add_strategy(Arc::new(SelfUseStrategy::default()));
        assert_eq!(optimizer.strategy_count(), 1);

        optimizer.add_strategy(Arc::new(PriceArbitrageStrategy::default()));
        assert_eq!(optimizer.strategy_count(), 2);
    }

    #[test]
    fn test_optimizer_with_disabled_strategies() {
        let strategies: Vec<Arc<dyn EconomicStrategy>> = vec![
            Arc::new(PriceArbitrageStrategy::new(false, 0.15)), // Disabled
            Arc::new(SelfUseStrategy::new(true)),               // Enabled
        ];

        let optimizer = EconomicOptimizer::new(strategies);
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
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.15,
            all_price_blocks: None,
        };

        let result = optimizer.optimize(&context);

        // Should only use enabled strategy (Self-Use)
        assert_eq!(result.strategy_name, "Self-Use");
    }

    #[test]
    fn test_optimizer_fallback_when_none_applicable() {
        // Create optimizer with only strategies that might not apply
        let strategies: Vec<Arc<dyn EconomicStrategy>> = vec![
            Arc::new(SolarFirstStrategy::new(true, 1.5)), // Only applies with solar
        ];

        let optimizer = EconomicOptimizer::new(strategies);
        let config = create_test_config();
        let now = Utc::now();

        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 0.40,
        };

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0, // No solar, so Solar-First won't apply
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.30,
            all_price_blocks: None,
        };

        let result = optimizer.optimize(&context);

        // Should return fallback
        assert!(!result.reason.is_empty());
    }
}
