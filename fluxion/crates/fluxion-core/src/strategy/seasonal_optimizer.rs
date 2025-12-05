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

use crate::strategy::winter_adaptive::{WinterAdaptiveConfig, WinterAdaptiveStrategy};
use crate::strategy::{BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::config::StrategiesConfigCore;

/// Strategies configuration for optimizer construction
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SeasonalStrategiesConfig {
    pub winter_adaptive_enabled: bool,
    pub winter_adaptive_ema_period_days: usize,
    pub winter_adaptive_min_solar_percentage: f32,
    pub winter_adaptive_target_battery_soc: f32,
    pub winter_adaptive_top_expensive_blocks: usize,
    pub winter_adaptive_tomorrow_preservation_threshold: f32,
    pub winter_adaptive_grid_export_price_threshold: f32,
    pub winter_adaptive_min_soc_for_export: f32,
    pub winter_adaptive_export_trigger_multiplier: f32,
    pub winter_adaptive_negative_price_handling_enabled: bool,
    pub winter_adaptive_charge_on_negative_even_if_full: bool,
}

impl Default for SeasonalStrategiesConfig {
    fn default() -> Self {
        Self {
            winter_adaptive_enabled: true,
            winter_adaptive_ema_period_days: 7,
            winter_adaptive_min_solar_percentage: 0.10,
            winter_adaptive_target_battery_soc: 90.0,
            winter_adaptive_top_expensive_blocks: 12,
            winter_adaptive_tomorrow_preservation_threshold: 1.2,
            winter_adaptive_grid_export_price_threshold: 8.0,
            winter_adaptive_min_soc_for_export: 50.0,
            winter_adaptive_export_trigger_multiplier: 2.5,
            winter_adaptive_negative_price_handling_enabled: true,
            winter_adaptive_charge_on_negative_even_if_full: false,
        }
    }
}

impl From<&StrategiesConfigCore> for SeasonalStrategiesConfig {
    fn from(config: &StrategiesConfigCore) -> Self {
        Self {
            winter_adaptive_enabled: config.winter_adaptive.enabled,
            winter_adaptive_ema_period_days: config.winter_adaptive.ema_period_days,
            winter_adaptive_min_solar_percentage: config.winter_adaptive.min_solar_percentage,
            winter_adaptive_target_battery_soc: config.winter_adaptive.target_battery_soc,
            winter_adaptive_top_expensive_blocks: config.winter_adaptive.top_expensive_blocks,
            winter_adaptive_tomorrow_preservation_threshold: config
                .winter_adaptive
                .tomorrow_preservation_threshold,
            winter_adaptive_grid_export_price_threshold: config
                .winter_adaptive
                .grid_export_price_threshold,
            winter_adaptive_min_soc_for_export: config.winter_adaptive.min_soc_for_export,
            winter_adaptive_export_trigger_multiplier: config
                .winter_adaptive
                .export_trigger_multiplier,
            winter_adaptive_negative_price_handling_enabled: config
                .winter_adaptive
                .negative_price_handling_enabled,
            winter_adaptive_charge_on_negative_even_if_full: config
                .winter_adaptive
                .charge_on_negative_even_if_full,
        }
    }
}

pub struct AdaptiveSeasonalOptimizer {
    winter_adaptive: WinterAdaptiveStrategy,
}

impl AdaptiveSeasonalOptimizer {
    #[must_use]
    pub fn new(winter_adaptive: WinterAdaptiveStrategy) -> Self {
        Self { winter_adaptive }
    }

    /// Construct optimizer with sensible defaults
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::with_config(&SeasonalStrategiesConfig::default())
    }

    /// Construct optimizer with custom configuration
    #[must_use]
    pub fn with_config(config: &SeasonalStrategiesConfig) -> Self {
        let winter_adaptive_config = WinterAdaptiveConfig {
            enabled: config.winter_adaptive_enabled,
            ema_period_days: config.winter_adaptive_ema_period_days,
            min_solar_percentage: config.winter_adaptive_min_solar_percentage,
            target_battery_soc: config.winter_adaptive_target_battery_soc,
            top_expensive_blocks: config.winter_adaptive_top_expensive_blocks,
            tomorrow_preservation_threshold: config.winter_adaptive_tomorrow_preservation_threshold,
            grid_export_price_threshold: config.winter_adaptive_grid_export_price_threshold,
            min_soc_for_export: config.winter_adaptive_min_soc_for_export,
            export_trigger_multiplier: config.winter_adaptive_export_trigger_multiplier,
            negative_price_handling_enabled: config.winter_adaptive_negative_price_handling_enabled,
            charge_on_negative_even_if_full: config.winter_adaptive_charge_on_negative_even_if_full,
            ..Default::default()
        };

        Self {
            winter_adaptive: WinterAdaptiveStrategy::new(winter_adaptive_config),
        }
    }

    /// Evaluate strategies and pick the best
    /// Currently only supports WinterAdaptiveStrategy
    #[must_use]
    pub fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        self.evaluate_with_debug(context, false)
    }

    /// Evaluate with optional debug information capture
    #[must_use]
    pub fn evaluate_with_debug(
        &self,
        context: &EvaluationContext,
        capture_debug: bool,
    ) -> BlockEvaluation {
        // Only one strategy to evaluate
        let mut eval = self.winter_adaptive.evaluate(context);

        // Capture debug info if requested
        if capture_debug {
            use crate::strategy::{BlockDebugInfo, StrategyEvaluation};

            let strategy_eval = StrategyEvaluation {
                strategy_name: eval.strategy_name.clone(),
                mode: eval.mode,
                net_profit_czk: eval.net_profit_czk,
                reason: eval.reason.clone(),
            };

            let winning_reason = format!("{} is the only active strategy", eval.strategy_name);

            // Collect key conditions from evaluation context
            let conditions = vec![
                format!("SOC: {:.1}%", context.current_battery_soc),
                format!(
                    "Price: {:.4} CZK/kWh",
                    context.price_block.price_czk_per_kwh
                ),
                format!("Solar forecast: {:.2} kWh", context.solar_forecast_kwh),
                format!(
                    "Consumption forecast: {:.2} kWh",
                    context.consumption_forecast_kwh
                ),
                format!(
                    "Export price: {:.4} CZK/kWh",
                    context.grid_export_price_czk_per_kwh
                ),
            ];

            eval.debug_info = Some(BlockDebugInfo {
                evaluated_strategies: vec![strategy_eval],
                winning_reason,
                conditions,
            });
        }

        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use fluxion_types::config::ControlConfig;
    use fluxion_types::pricing::TimeBlockPrice;

    fn cfg() -> ControlConfig {
        ControlConfig {
            force_charge_hours: 8,
            force_discharge_hours: 2,
            min_battery_soc: 50.0,
            max_battery_soc: 100.0,
            maximum_export_power_w: 9500,
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
    fn test_optimizer_winter_route() {
        let optimizer = AdaptiveSeasonalOptimizer::with_defaults();
        let now = chrono::DateTime::parse_from_rfc3339("2025-10-14T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 9.53,
        };
        let all = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 2.40,
        }];
        let ctx = EvaluationContext {
            price_block: &price_block,
            control_config: &cfg(),
            current_battery_soc: 100.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 9.53,
            all_price_blocks: Some(&all),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None, // Not needed in test
        };
        let eval = optimizer.evaluate(&ctx);
        assert!(!eval.strategy_name.is_empty());
    }
}
