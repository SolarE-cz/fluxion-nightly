// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Simulation state management for strategy testing.
//!
//! This module defines the core state structures that track
//! simulation progress, strategy results, and user overrides.

use crate::SyntheticDay;
use crate::strategies::StrategySelection;
use chrono::{DateTime, Utc};
use fluxion_core::strategy::BlockEvaluation;
use fluxion_types::inverter::InverterOperationMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Complete simulation state for a day
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationState {
    /// Unique simulation ID
    pub id: Uuid,

    /// The synthetic day being simulated
    pub day: SyntheticDay,

    /// Current block index (0-95)
    pub current_block: usize,

    /// Results for each strategy
    pub strategy_results: HashMap<String, StrategySimulationResult>,

    /// Active overrides
    pub overrides: SimulationOverrides,

    /// Simulation configuration
    pub config: SimulationConfig,

    /// When simulation was created
    pub created_at: DateTime<Utc>,

    /// When simulation was last updated
    pub last_updated: DateTime<Utc>,
}

impl SimulationState {
    /// Create a new simulation state
    pub fn new(day: SyntheticDay, config: SimulationConfig, strategy_ids: Vec<String>) -> Self {
        let initial_soc = day.initial_soc;

        let mut strategy_results = HashMap::new();
        for strategy_id in strategy_ids {
            strategy_results.insert(
                strategy_id.clone(),
                StrategySimulationResult::new(&strategy_id, initial_soc),
            );
        }

        Self {
            id: Uuid::new_v4(),
            day,
            current_block: 0,
            strategy_results,
            overrides: SimulationOverrides::default(),
            config,
            created_at: Utc::now(),
            last_updated: Utc::now(),
        }
    }

    /// Get current time as HH:MM string
    pub fn current_time_str(&self) -> String {
        let minutes = self.current_block * 15;
        let hours = minutes / 60;
        let mins = minutes % 60;
        format!("{:02}:{:02}", hours, mins)
    }

    /// Check if simulation is complete
    pub fn is_complete(&self) -> bool {
        self.current_block >= 96
    }

    /// Get the best performing strategy
    pub fn best_strategy(&self) -> Option<(&String, &StrategySimulationResult)> {
        self.strategy_results.iter().min_by(|(_, a), (_, b)| {
            a.net_cost_czk
                .partial_cmp(&b.net_cost_czk)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Get strategies sorted by net cost (best first)
    pub fn ranked_strategies(&self) -> Vec<(&String, &StrategySimulationResult)> {
        let mut results: Vec<_> = self.strategy_results.iter().collect();
        results.sort_by(|(_, a), (_, b)| {
            a.net_cost_czk
                .partial_cmp(&b.net_cost_czk)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }
}

/// Result for a single strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySimulationResult {
    /// Strategy identifier
    pub strategy_id: String,

    /// Strategy display name
    pub strategy_name: String,

    /// SOC at each block (grows as simulation progresses)
    pub soc_history: Vec<f32>,

    /// Block evaluations (decisions made)
    pub evaluations: Vec<BlockEvaluation>,

    /// Running cost totals (grows as simulation progresses)
    pub cumulative_cost_czk: Vec<f32>,

    /// Total grid import (kWh)
    pub total_grid_import_kwh: f32,

    /// Total grid export (kWh)
    pub total_grid_export_kwh: f32,

    /// Total battery charge (kWh)
    pub total_battery_charge_kwh: f32,

    /// Total battery discharge (kWh)
    pub total_battery_discharge_kwh: f32,

    /// Total import cost (CZK)
    pub total_import_cost_czk: f32,

    /// Total export revenue (CZK)
    pub total_export_revenue_czk: f32,

    /// Net cost (import cost - export revenue) (CZK)
    pub net_cost_czk: f32,

    /// Current battery SOC (%)
    pub current_soc: f32,

    /// Current operation mode
    pub current_mode: InverterOperationMode,

    /// Last decision reason
    pub last_reason: String,
}

impl StrategySimulationResult {
    /// Create a new result tracker for a strategy
    pub fn new(strategy_id: &str, initial_soc: f32) -> Self {
        Self {
            strategy_id: strategy_id.to_string(),
            strategy_name: strategy_id.to_string(), // Will be updated with display name
            soc_history: vec![initial_soc],
            evaluations: Vec::new(),
            cumulative_cost_czk: vec![0.0],
            total_grid_import_kwh: 0.0,
            total_grid_export_kwh: 0.0,
            total_battery_charge_kwh: 0.0,
            total_battery_discharge_kwh: 0.0,
            total_import_cost_czk: 0.0,
            total_export_revenue_czk: 0.0,
            net_cost_czk: 0.0,
            current_soc: initial_soc,
            current_mode: InverterOperationMode::SelfUse,
            last_reason: String::new(),
        }
    }

    /// Calculate battery cycles (charge + discharge / 2 / capacity)
    pub fn battery_cycles(&self, capacity_kwh: f32) -> f32 {
        if capacity_kwh <= 0.0 {
            return 0.0;
        }
        (self.total_battery_charge_kwh + self.total_battery_discharge_kwh) / 2.0 / capacity_kwh
    }

    /// Reset result to initial state (for re-simulation from override)
    pub fn reset(&mut self, initial_soc: f32) {
        self.soc_history = vec![initial_soc];
        self.evaluations.clear();
        self.cumulative_cost_czk = vec![0.0];
        self.total_grid_import_kwh = 0.0;
        self.total_grid_export_kwh = 0.0;
        self.total_battery_charge_kwh = 0.0;
        self.total_battery_discharge_kwh = 0.0;
        self.total_import_cost_czk = 0.0;
        self.total_export_revenue_czk = 0.0;
        self.net_cost_czk = 0.0;
        self.current_soc = initial_soc;
        self.current_mode = InverterOperationMode::SelfUse;
        self.last_reason.clear();
    }
}

/// Runtime overrides for simulation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimulationOverrides {
    /// Override SOC at specific block
    pub soc_override: Option<SocOverride>,

    /// Override load at specific blocks (block_index -> kWh)
    pub load_overrides: HashMap<usize, f32>,

    /// Override price at specific blocks (block_index -> CZK/kWh)
    pub price_overrides: HashMap<usize, f32>,

    /// Force mode for specific strategy at specific blocks
    /// Key: (strategy_id, block_index), Value: mode
    pub mode_overrides: HashMap<(String, usize), InverterOperationMode>,
}

impl SimulationOverrides {
    /// Clear all overrides
    pub fn clear(&mut self) {
        self.soc_override = None;
        self.load_overrides.clear();
        self.price_overrides.clear();
        self.mode_overrides.clear();
    }

    /// Check if any overrides are active
    pub fn has_overrides(&self) -> bool {
        self.soc_override.is_some()
            || !self.load_overrides.is_empty()
            || !self.price_overrides.is_empty()
            || !self.mode_overrides.is_empty()
    }
}

/// SOC override specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocOverride {
    /// Block index where SOC is overridden
    pub block_index: usize,

    /// New SOC value (0-100%)
    pub soc_percent: f32,

    /// Apply to specific strategies, or None for all
    pub strategy_ids: Option<Vec<String>>,
}

/// Simulation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    /// Strategies to simulate
    pub strategies: Vec<StrategySelection>,

    /// Include no-battery baseline
    pub include_no_battery: bool,

    /// Include naive self-use baseline
    pub include_naive: bool,

    /// Battery capacity (kWh)
    pub battery_capacity_kwh: f32,

    /// Maximum charge/discharge rate (kW)
    pub max_charge_rate_kw: f32,

    /// Battery round-trip efficiency (0-1)
    pub battery_efficiency: f32,

    /// Battery wear cost (CZK/kWh cycled)
    pub battery_wear_cost_czk_per_kwh: f32,

    /// Minimum SOC (%)
    pub min_soc: f32,

    /// Maximum SOC (%)
    pub max_soc: f32,

    /// HDO low tariff grid fee (CZK/kWh)
    pub hdo_low_tariff_czk: f32,

    /// HDO high tariff grid fee (CZK/kWh)
    pub hdo_high_tariff_czk: f32,

    /// Export price ratio (fraction of import price)
    pub export_price_ratio: f32,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            strategies: vec![StrategySelection {
                strategy_id: "winter_adaptive_v4".to_string(),
                enabled: true,
                config_overrides: None,
            }],
            include_no_battery: true,
            include_naive: true,
            battery_capacity_kwh: 10.0,
            max_charge_rate_kw: 3.5,
            battery_efficiency: 0.95,
            battery_wear_cost_czk_per_kwh: 0.125,
            min_soc: 10.0,
            max_soc: 100.0,
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
            export_price_ratio: 0.80,
        }
    }
}

/// Summary of simulation results for API response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResultsSummary {
    /// Whether simulation is complete
    pub completed: bool,

    /// Total blocks simulated
    pub total_blocks: usize,

    /// Current block
    pub current_block: usize,

    /// Results per strategy
    pub strategies: Vec<StrategyResultSummary>,

    /// Strategy IDs ranked by net cost (best first)
    pub ranking: Vec<String>,

    /// Savings analysis
    pub savings_analysis: SavingsAnalysis,
}

impl SimulationResultsSummary {
    /// Create summary from simulation state
    pub fn from_state(state: &SimulationState) -> Self {
        let ranked = state.ranked_strategies();

        let strategies: Vec<StrategyResultSummary> = ranked
            .iter()
            .map(|(id, result)| StrategyResultSummary {
                strategy_id: (*id).clone(),
                strategy_name: result.strategy_name.clone(),
                net_cost_czk: result.net_cost_czk,
                grid_import_kwh: result.total_grid_import_kwh,
                grid_export_kwh: result.total_grid_export_kwh,
                battery_cycles: result.battery_cycles(state.config.battery_capacity_kwh),
                final_soc: result.current_soc,
                savings_vs_no_battery: 0.0, // Calculated below
                savings_vs_naive: 0.0,      // Calculated below
            })
            .collect();

        let ranking: Vec<String> = ranked.iter().map(|(id, _)| (*id).clone()).collect();

        // Calculate savings
        let no_battery_cost = state
            .strategy_results
            .get("no_battery")
            .map(|r| r.net_cost_czk)
            .unwrap_or(0.0);

        let naive_cost = state
            .strategy_results
            .get("naive")
            .map(|r| r.net_cost_czk)
            .unwrap_or(0.0);

        let strategies_with_savings: Vec<StrategyResultSummary> = strategies
            .into_iter()
            .map(|mut s| {
                s.savings_vs_no_battery = no_battery_cost - s.net_cost_czk;
                s.savings_vs_naive = naive_cost - s.net_cost_czk;
                s
            })
            .collect();

        let best_strategy = ranking.first().cloned().unwrap_or_default();
        let worst_strategy = ranking.last().cloned().unwrap_or_default();

        let best_cost = strategies_with_savings
            .first()
            .map(|s| s.net_cost_czk)
            .unwrap_or(0.0);
        let worst_cost = strategies_with_savings
            .last()
            .map(|s| s.net_cost_czk)
            .unwrap_or(0.0);

        let savings_analysis = SavingsAnalysis {
            best_strategy,
            worst_strategy,
            savings_range_czk: worst_cost - best_cost,
            no_battery_cost_czk: no_battery_cost,
            best_savings_percent: if no_battery_cost > 0.0 {
                ((no_battery_cost - best_cost) / no_battery_cost) * 100.0
            } else {
                0.0
            },
        };

        Self {
            completed: state.is_complete(),
            total_blocks: 96,
            current_block: state.current_block,
            strategies: strategies_with_savings,
            ranking,
            savings_analysis,
        }
    }
}

/// Summary for a single strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyResultSummary {
    /// Strategy identifier
    pub strategy_id: String,

    /// Strategy display name
    pub strategy_name: String,

    /// Net cost (CZK)
    pub net_cost_czk: f32,

    /// Grid import (kWh)
    pub grid_import_kwh: f32,

    /// Grid export (kWh)
    pub grid_export_kwh: f32,

    /// Battery cycles
    pub battery_cycles: f32,

    /// Final SOC (%)
    pub final_soc: f32,

    /// Savings vs no-battery baseline (CZK)
    pub savings_vs_no_battery: f32,

    /// Savings vs naive strategy (CZK)
    pub savings_vs_naive: f32,
}

/// Savings analysis across all strategies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavingsAnalysis {
    /// Best performing strategy ID
    pub best_strategy: String,

    /// Worst performing strategy ID
    pub worst_strategy: String,

    /// Range between best and worst (CZK)
    pub savings_range_czk: f32,

    /// No-battery baseline cost (CZK)
    pub no_battery_cost_czk: f32,

    /// Best strategy's savings vs no-battery (%)
    pub best_savings_percent: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_result_initialization() {
        let result = StrategySimulationResult::new("test_strategy", 50.0);

        assert_eq!(result.strategy_id, "test_strategy");
        assert_eq!(result.current_soc, 50.0);
        assert_eq!(result.soc_history.len(), 1);
        assert_eq!(result.net_cost_czk, 0.0);
    }

    #[test]
    fn test_battery_cycles_calculation() {
        let mut result = StrategySimulationResult::new("test", 50.0);
        result.total_battery_charge_kwh = 10.0;
        result.total_battery_discharge_kwh = 10.0;

        // 10 + 10 / 2 / 10 = 1.0 cycle
        assert!((result.battery_cycles(10.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_current_time_str() {
        let day = crate::SyntheticDay {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            blocks: Vec::new(),
            price_scenario_name: "test".to_string(),
            total_consumption_kwh: 0.0,
            total_solar_kwh: 0.0,
            initial_soc: 50.0,
            battery_capacity_kwh: 10.0,
        };

        let mut state = SimulationState::new(day, SimulationConfig::default(), vec![]);

        state.current_block = 0;
        assert_eq!(state.current_time_str(), "00:00");

        state.current_block = 28; // 7:00
        assert_eq!(state.current_time_str(), "07:00");

        state.current_block = 95; // 23:45
        assert_eq!(state.current_time_str(), "23:45");
    }
}
