// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Strategy registry and baseline implementations for simulation.
//!
//! This module provides:
//! - Strategy registry for managing V1-V5 and baseline strategies
//! - No-battery baseline (all consumption from grid)
//! - Naive self-use baseline (simple self-consumption)

use fluxion_core::strategy::{
    BlockEvaluation, EconomicStrategy, EnergyFlows, EvaluationContext, WinterAdaptiveConfig,
    WinterAdaptiveStrategy, WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
    WinterAdaptiveV3Config, WinterAdaptiveV3Strategy, WinterAdaptiveV4Config,
    WinterAdaptiveV4Strategy, WinterAdaptiveV5Config, WinterAdaptiveV5Strategy,
    WinterAdaptiveV6Config, WinterAdaptiveV6Strategy, WinterAdaptiveV7Config,
    WinterAdaptiveV7Strategy, WinterAdaptiveV8Config, WinterAdaptiveV8Strategy,
    WinterAdaptiveV9Config, WinterAdaptiveV9Strategy,
};
use fluxion_types::inverter::InverterOperationMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Strategy selection for simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySelection {
    /// Strategy identifier
    pub strategy_id: String,

    /// Whether this strategy is enabled
    pub enabled: bool,

    /// Optional config overrides (JSON)
    pub config_overrides: Option<serde_json::Value>,
}

/// Information about a strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyInfo {
    /// Unique identifier
    pub id: String,

    /// Display name
    pub name: String,

    /// Description
    pub description: String,

    /// Version tag (V1, V2, V3, V4, Baseline)
    pub version: String,

    /// Whether this is a baseline strategy
    pub is_baseline: bool,
}

/// Registry of all available strategies
pub struct StrategyRegistry {
    strategies: HashMap<String, Arc<dyn EconomicStrategy>>,
    info: Vec<StrategyInfo>,
}

impl StrategyRegistry {
    /// Create registry with all V1-V5 strategies plus baselines
    pub fn new_with_defaults() -> Self {
        let mut strategies: HashMap<String, Arc<dyn EconomicStrategy>> = HashMap::new();

        // V1 - Winter Adaptive
        strategies.insert(
            "winter_adaptive_v1".to_string(),
            Arc::new(WinterAdaptiveStrategy::new(WinterAdaptiveConfig::default())),
        );

        // V2 - Winter Adaptive V2
        strategies.insert(
            "winter_adaptive_v2".to_string(),
            Arc::new(WinterAdaptiveV2Strategy::new(
                WinterAdaptiveV2Config::default(),
            )),
        );

        // V3 - Winter Adaptive V3
        strategies.insert(
            "winter_adaptive_v3".to_string(),
            Arc::new(WinterAdaptiveV3Strategy::new(
                WinterAdaptiveV3Config::default(),
            )),
        );

        // V4 - Winter Adaptive V4
        strategies.insert(
            "winter_adaptive_v4".to_string(),
            Arc::new(WinterAdaptiveV4Strategy::new(
                WinterAdaptiveV4Config::default(),
            )),
        );

        // V5 - Winter Adaptive V5
        strategies.insert(
            "winter_adaptive_v5".to_string(),
            Arc::new(WinterAdaptiveV5Strategy::new(
                WinterAdaptiveV5Config::default(),
            )),
        );

        // V6 - Winter Adaptive V6 (Adaptive Hybrid Optimizer)
        strategies.insert(
            "winter_adaptive_v6".to_string(),
            Arc::new(WinterAdaptiveV6Strategy::new(
                WinterAdaptiveV6Config::default(),
            )),
        );

        // V7 - Winter Adaptive V7 (Unconstrained Multi-Cycle Arbitrage Optimizer)
        strategies.insert(
            "winter_adaptive_v7".to_string(),
            Arc::new(WinterAdaptiveV7Strategy::new(
                WinterAdaptiveV7Config::default(),
            )),
        );

        // V8 - Winter Adaptive V8 (Solar-Aware Multi-Cycle Optimizer)
        strategies.insert(
            "winter_adaptive_v8".to_string(),
            Arc::new(WinterAdaptiveV8Strategy::new(
                WinterAdaptiveV8Config::default(),
            )),
        );

        // V9 - Winter Adaptive V9 (Solar-Aware Morning Peak Optimizer)
        strategies.insert(
            "winter_adaptive_v9".to_string(),
            Arc::new(WinterAdaptiveV9Strategy::new(
                WinterAdaptiveV9Config::default(),
            )),
        );

        // Baselines
        strategies.insert("no_battery".to_string(), Arc::new(NoBatteryBaseline));

        strategies.insert("naive".to_string(), Arc::new(NaiveSelfUseStrategy));

        let info = vec![
            StrategyInfo {
                id: "winter_adaptive_v1".to_string(),
                name: "Winter Adaptive V1".to_string(),
                description: "Original winter strategy with forward-looking optimization and EMA forecasting".to_string(),
                version: "V1".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v2".to_string(),
                name: "Winter Adaptive V2".to_string(),
                description: "Advanced strategy with arbitrage window detection and P90 validation".to_string(),
                version: "V2".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v3".to_string(),
                name: "Winter Adaptive V3".to_string(),
                description: "HDO tariff integration for accurate Czech grid fee calculation".to_string(),
                version: "V3".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v4".to_string(),
                name: "Winter Adaptive V4".to_string(),
                description: "Global price optimization - ranks ALL blocks for optimal scheduling".to_string(),
                version: "V4".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v5".to_string(),
                name: "Winter Adaptive V5".to_string(),
                description: "Enhanced global price optimization with improved arbitrage detection".to_string(),
                version: "V5".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v6".to_string(),
                name: "Winter Adaptive V6".to_string(),
                description: "Adaptive Hybrid Optimizer - combines V3/V4/V5 with pattern detection".to_string(),
                version: "V6".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v7".to_string(),
                name: "Winter Adaptive V7".to_string(),
                description: "Unconstrained Multi-Cycle Arbitrage Optimizer - maximum cost savings with no artificial limits".to_string(),
                version: "V7".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v8".to_string(),
                name: "Winter Adaptive V8".to_string(),
                description: "Solar-Aware Multi-Cycle Optimizer - V7 enhanced with solar forecast integration".to_string(),
                version: "V8".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "winter_adaptive_v9".to_string(),
                name: "Winter Adaptive V9".to_string(),
                description: "Solar-Aware Morning Peak Optimizer - minimal grid charging on sunny days, covers morning peak only".to_string(),
                version: "V9".to_string(),
                is_baseline: false,
            },
            StrategyInfo {
                id: "no_battery".to_string(),
                name: "No Battery".to_string(),
                description: "Baseline without battery storage - all consumption from grid".to_string(),
                version: "Baseline".to_string(),
                is_baseline: true,
            },
            StrategyInfo {
                id: "naive".to_string(),
                name: "Naive Self-Use".to_string(),
                description: "Simple self-consumption without price optimization".to_string(),
                version: "Baseline".to_string(),
                is_baseline: true,
            },
        ];

        Self { strategies, info }
    }

    /// Get a strategy by ID
    pub fn get(&self, id: &str) -> Option<Arc<dyn EconomicStrategy>> {
        self.strategies.get(id).cloned()
    }

    /// List all available strategies
    pub fn list_strategies(&self) -> &[StrategyInfo] {
        &self.info
    }

    /// Get strategy IDs
    pub fn strategy_ids(&self) -> Vec<String> {
        self.strategies.keys().cloned().collect()
    }

    /// Check if a strategy exists
    pub fn contains(&self, id: &str) -> bool {
        self.strategies.contains_key(id)
    }

    /// Get display name for a strategy
    pub fn display_name(&self, id: &str) -> String {
        self.info
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

/// No-battery baseline strategy
///
/// Simulates a system without battery storage:
/// - All consumption comes directly from grid
/// - Solar excess is exported to grid
/// - No price optimization possible
pub struct NoBatteryBaseline;

impl EconomicStrategy for NoBatteryBaseline {
    fn name(&self) -> &str {
        "No Battery"
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse, // Conceptually self-use (no battery to control)
            self.name().to_string(),
        );

        // Net consumption = load - solar
        let net_consumption = context.consumption_forecast_kwh - context.solar_forecast_kwh;

        if net_consumption > 0.0 {
            // Need to import from grid
            eval.cost_czk = net_consumption * context.price_block.price_czk_per_kwh;
            eval.energy_flows.grid_import_kwh = net_consumption;
            eval.reason = format!(
                "No battery - grid import {:.2} kWh @ {:.2} CZK/kWh",
                net_consumption, context.price_block.price_czk_per_kwh
            );
        } else {
            // Export excess solar to grid
            let excess = -net_consumption;
            eval.revenue_czk = excess * context.grid_export_price_czk_per_kwh;
            eval.energy_flows.grid_export_kwh = excess;
            eval.reason = format!(
                "No battery - grid export {:.2} kWh @ {:.2} CZK/kWh",
                excess, context.grid_export_price_czk_per_kwh
            );
        }

        eval.energy_flows.solar_generation_kwh = context.solar_forecast_kwh;
        eval.energy_flows.household_consumption_kwh = context.consumption_forecast_kwh;

        eval.net_profit_czk = eval.revenue_czk - eval.cost_czk;

        eval
    }
}

/// Naive self-use strategy
///
/// Simple self-consumption without price optimization:
/// - Battery charges from solar excess
/// - Battery discharges to cover load when needed
/// - No grid charging, no price-based decisions
pub struct NaiveSelfUseStrategy;

impl EconomicStrategy for NaiveSelfUseStrategy {
    fn name(&self) -> &str {
        "Naive Self-Use"
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse,
            self.name().to_string(),
        );

        let solar = context.solar_forecast_kwh;
        let consumption = context.consumption_forecast_kwh;
        let battery_soc = context.current_battery_soc;
        let efficiency = context.control_config.battery_efficiency;

        // Battery capacity in kWh for this 15-min block
        let max_charge = context.control_config.max_battery_charge_rate_kw * 0.25;
        let max_discharge = context.control_config.max_battery_charge_rate_kw * 0.25;

        let battery_capacity = context.control_config.battery_capacity_kwh;
        let current_energy = battery_capacity * (battery_soc / 100.0);
        let available_discharge = (current_energy
            - battery_capacity * (context.control_config.min_battery_soc / 100.0))
            .max(0.0);
        let available_charge =
            (battery_capacity * (context.control_config.max_battery_soc / 100.0) - current_energy)
                .max(0.0);

        let net_power = solar - consumption;

        let mut energy_flows = EnergyFlows {
            solar_generation_kwh: solar,
            household_consumption_kwh: consumption,
            ..Default::default()
        };

        if net_power > 0.0 {
            // Solar excess - charge battery first, then export
            let charge_amount = net_power.min(max_charge).min(available_charge);
            let excess_after_charge = net_power - charge_amount;

            energy_flows.battery_charge_kwh = charge_amount;

            if excess_after_charge > 0.0 {
                energy_flows.grid_export_kwh = excess_after_charge;
                eval.revenue_czk = excess_after_charge * context.grid_export_price_czk_per_kwh;
            }

            eval.reason = format!(
                "Self-use: solar excess {:.2} kWh, charged {:.2} kWh, exported {:.2} kWh",
                net_power, charge_amount, excess_after_charge
            );
        } else {
            // Solar deficit - discharge battery first, then import
            let deficit = -net_power;
            let discharge_amount = deficit
                .min(max_discharge)
                .min(available_discharge)
                .min(deficit / efficiency); // Account for efficiency

            let import_needed = deficit - (discharge_amount * efficiency);

            energy_flows.battery_discharge_kwh = discharge_amount;

            if import_needed > 0.0 {
                energy_flows.grid_import_kwh = import_needed;
                eval.cost_czk = import_needed * context.price_block.price_czk_per_kwh;
            }

            eval.reason = format!(
                "Self-use: deficit {:.2} kWh, discharged {:.2} kWh, imported {:.2} kWh",
                deficit, discharge_amount, import_needed
            );
        }

        eval.energy_flows = energy_flows;
        eval.net_profit_czk = eval.revenue_czk - eval.cost_czk;

        // Track assumptions
        eval.assumptions.solar_forecast_kwh = solar;
        eval.assumptions.consumption_forecast_kwh = consumption;
        eval.assumptions.current_battery_soc = battery_soc;
        eval.assumptions.battery_efficiency = efficiency;
        eval.assumptions.grid_import_price_czk_per_kwh = context.price_block.price_czk_per_kwh;
        eval.assumptions.grid_export_price_czk_per_kwh = context.grid_export_price_czk_per_kwh;

        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use fluxion_types::config::ControlConfig;
    use fluxion_types::pricing::TimeBlockPrice;

    fn create_test_context<'a>(
        price_block: &'a TimeBlockPrice,
        control_config: &'a ControlConfig,
        battery_soc: f32,
        solar: f32,
        consumption: f32,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            price_block,
            control_config,
            current_battery_soc: battery_soc,
            solar_forecast_kwh: solar,
            consumption_forecast_kwh: consumption,
            grid_export_price_czk_per_kwh: price_block.price_czk_per_kwh * 0.8,
            all_price_blocks: None,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
        }
    }

    #[test]
    fn test_no_battery_imports_when_no_solar() {
        let strategy = NoBatteryBaseline;
        let price_block = TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
            effective_price_czk_per_kwh: 3.0,
        };
        let control_config = ControlConfig::default();

        let context = create_test_context(&price_block, &control_config, 50.0, 0.0, 1.0);
        let eval = strategy.evaluate(&context);

        assert!(eval.energy_flows.grid_import_kwh > 0.0);
        assert!(eval.cost_czk > 0.0);
        assert_eq!(eval.energy_flows.battery_charge_kwh, 0.0);
    }

    #[test]
    fn test_no_battery_exports_when_solar_excess() {
        let strategy = NoBatteryBaseline;
        let price_block = TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
            effective_price_czk_per_kwh: 3.0,
        };
        let control_config = ControlConfig::default();

        let context = create_test_context(&price_block, &control_config, 50.0, 2.0, 0.5);
        let eval = strategy.evaluate(&context);

        assert!(eval.energy_flows.grid_export_kwh > 0.0);
        assert!(eval.revenue_czk > 0.0);
    }

    #[test]
    fn test_naive_uses_battery_for_deficit() {
        let strategy = NaiveSelfUseStrategy;
        let price_block = TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 15, 18, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: 4.0,
            effective_price_czk_per_kwh: 4.0,
        };
        let control_config = ControlConfig {
            battery_capacity_kwh: 10.0,
            max_battery_charge_rate_kw: 3.5,
            min_battery_soc: 10.0,
            ..Default::default()
        };

        // 50% SOC = 5 kWh available (above 10% min)
        let context = create_test_context(&price_block, &control_config, 50.0, 0.0, 0.5);
        let eval = strategy.evaluate(&context);

        // Should discharge battery to cover deficit
        assert!(
            eval.energy_flows.battery_discharge_kwh > 0.0,
            "Should discharge battery: {:?}",
            eval.energy_flows
        );
    }

    #[test]
    fn test_naive_charges_from_solar_excess() {
        let strategy = NaiveSelfUseStrategy;
        let price_block = TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
        };
        let control_config = ControlConfig {
            battery_capacity_kwh: 10.0,
            max_battery_charge_rate_kw: 3.5,
            max_battery_soc: 100.0,
            ..Default::default()
        };

        // 30% SOC = 7 kWh capacity available for charging
        let context = create_test_context(&price_block, &control_config, 30.0, 2.0, 0.5);
        let eval = strategy.evaluate(&context);

        // Should charge battery from solar excess
        assert!(
            eval.energy_flows.battery_charge_kwh > 0.0,
            "Should charge battery: {:?}",
            eval.energy_flows
        );
    }

    #[test]
    fn test_registry_contains_all_strategies() {
        let registry = StrategyRegistry::new_with_defaults();

        assert!(registry.contains("winter_adaptive_v1"));
        assert!(registry.contains("winter_adaptive_v2"));
        assert!(registry.contains("winter_adaptive_v3"));
        assert!(registry.contains("winter_adaptive_v4"));
        assert!(registry.contains("no_battery"));
        assert!(registry.contains("naive"));
    }
}
