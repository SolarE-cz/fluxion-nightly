// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Simulation engine for running strategy evaluations.
//!
//! This module provides the core simulation loop that:
//! - Steps through 15-minute blocks
//! - Evaluates each strategy for each block
//! - Tracks SOC, costs, and energy flows
//! - Handles overrides and re-simulation

use crate::state::{SimulationConfig, SimulationState, SocOverride};
use crate::strategies::StrategyRegistry;
use crate::synthetic_data::{SyntheticDay, SyntheticDayConfig, SyntheticDayGenerator};
use anyhow::Result;
use chrono::Utc;
use fluxion_core::strategy::EvaluationContext;
use fluxion_types::config::ControlConfig;
use fluxion_types::pricing::TimeBlockPrice;
use std::sync::Arc;

/// Simulation engine for multi-strategy evaluation
pub struct SimulationEngine {
    /// Strategy registry
    registry: Arc<StrategyRegistry>,
}

impl Default for SimulationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulationEngine {
    /// Create a new simulation engine with default strategies
    pub fn new() -> Self {
        Self {
            registry: Arc::new(StrategyRegistry::new_with_defaults()),
        }
    }

    /// Create engine with custom registry
    pub fn with_registry(registry: Arc<StrategyRegistry>) -> Self {
        Self { registry }
    }

    /// Get the strategy registry
    pub fn registry(&self) -> &StrategyRegistry {
        &self.registry
    }

    /// Create a new simulation from configuration
    pub fn create_simulation(
        &self,
        day_config: SyntheticDayConfig,
        sim_config: SimulationConfig,
    ) -> Result<SimulationState> {
        // Generate synthetic day
        let day = SyntheticDayGenerator::generate(&day_config)?;
        self.create_simulation_from_day(day, sim_config)
    }

    /// Create a new simulation from a pre-generated SyntheticDay
    pub fn create_simulation_from_day(
        &self,
        day: SyntheticDay,
        sim_config: SimulationConfig,
    ) -> Result<SimulationState> {
        // Collect enabled strategy IDs
        let mut strategy_ids: Vec<String> = sim_config
            .strategies
            .iter()
            .filter(|s| s.enabled && self.registry.contains(&s.strategy_id))
            .map(|s| s.strategy_id.clone())
            .collect();

        // Add baselines if requested
        if sim_config.include_no_battery {
            strategy_ids.push("no_battery".to_string());
        }
        if sim_config.include_naive {
            strategy_ids.push("naive".to_string());
        }

        // Create state
        let mut state = SimulationState::new(day, sim_config, strategy_ids);

        // Update strategy display names
        for (id, result) in state.strategy_results.iter_mut() {
            result.strategy_name = self.registry.display_name(id);
        }

        Ok(state)
    }

    /// Step simulation forward by N blocks
    pub fn step(&self, state: &mut SimulationState, blocks: usize) -> Result<()> {
        for _ in 0..blocks {
            if state.current_block >= 96 {
                break; // Day complete
            }

            self.evaluate_block(state)?;
            state.current_block += 1;
        }

        state.last_updated = Utc::now();
        Ok(())
    }

    /// Run simulation to completion
    pub fn run_to_completion(&self, state: &mut SimulationState) -> Result<()> {
        while state.current_block < 96 {
            self.evaluate_block(state)?;
            state.current_block += 1;
        }

        state.last_updated = Utc::now();
        Ok(())
    }

    /// Reset simulation to a specific block (for re-simulation after override)
    pub fn reset_to_block(&self, state: &mut SimulationState, block: usize) -> Result<()> {
        let initial_soc = state.day.initial_soc;

        // Reset all strategy results
        for result in state.strategy_results.values_mut() {
            result.reset(initial_soc);
        }

        state.current_block = 0;

        // Re-run to specified block
        if block > 0 {
            self.step(state, block)?;
        }

        Ok(())
    }

    /// Apply SOC override and re-simulate from that point
    pub fn apply_soc_override(
        &self,
        state: &mut SimulationState,
        override_spec: SocOverride,
    ) -> Result<()> {
        state.overrides.soc_override = Some(override_spec.clone());

        // Reset and re-simulate to override point
        self.reset_to_block(state, override_spec.block_index)?;

        // Apply the SOC override to affected strategies
        for (id, result) in state.strategy_results.iter_mut() {
            if override_spec.strategy_ids.is_none()
                || override_spec
                    .strategy_ids
                    .as_ref()
                    .is_some_and(|ids| ids.contains(id))
            {
                result.current_soc = override_spec.soc_percent;
                if let Some(last) = result.soc_history.last_mut() {
                    *last = override_spec.soc_percent;
                }
            }
        }

        Ok(())
    }

    /// Apply load override for specific blocks
    pub fn apply_load_override(
        &self,
        state: &mut SimulationState,
        block_overrides: Vec<(usize, f32)>,
    ) -> Result<()> {
        // Find earliest override block
        let earliest = block_overrides
            .iter()
            .map(|(b, _)| *b)
            .min()
            .unwrap_or(state.current_block);

        // Apply overrides
        for (block, load_kwh) in block_overrides {
            state.overrides.load_overrides.insert(block, load_kwh);
        }

        // Re-simulate from earliest override
        if earliest < state.current_block {
            self.reset_to_block(state, earliest)?;
        }

        Ok(())
    }

    /// Apply price override for specific blocks
    pub fn apply_price_override(
        &self,
        state: &mut SimulationState,
        block_overrides: Vec<(usize, f32)>,
    ) -> Result<()> {
        // Find earliest override block
        let earliest = block_overrides
            .iter()
            .map(|(b, _)| *b)
            .min()
            .unwrap_or(state.current_block);

        // Apply overrides
        for (block, price) in block_overrides {
            state.overrides.price_overrides.insert(block, price);
        }

        // Re-simulate from earliest override
        if earliest < state.current_block {
            self.reset_to_block(state, earliest)?;
        }

        Ok(())
    }

    /// Clear all overrides and reset simulation
    pub fn clear_overrides(&self, state: &mut SimulationState) -> Result<()> {
        state.overrides.clear();
        self.reset_to_block(state, 0)
    }

    /// Evaluate a single block for all strategies
    fn evaluate_block(&self, state: &mut SimulationState) -> Result<()> {
        let block_idx = state.current_block;
        let block = &state.day.blocks[block_idx];

        // Get consumption (possibly overridden)
        let consumption = state
            .overrides
            .load_overrides
            .get(&block_idx)
            .copied()
            .unwrap_or(block.consumption_kwh);

        // Get price (possibly overridden)
        let price = state
            .overrides
            .price_overrides
            .get(&block_idx)
            .copied()
            .unwrap_or(block.price_czk_per_kwh);

        // Build price blocks for strategy context (all 96 with overrides applied)
        let all_price_blocks: Vec<TimeBlockPrice> = state
            .day
            .blocks
            .iter()
            .map(|b| TimeBlockPrice {
                block_start: b.timestamp,
                duration_minutes: 15,
                price_czk_per_kwh: b.price_czk_per_kwh,
                effective_price_czk_per_kwh: b.price_czk_per_kwh,
            })
            .collect();

        let current_price_block = TimeBlockPrice {
            block_start: block.timestamp,
            duration_minutes: 15,
            price_czk_per_kwh: price,
            effective_price_czk_per_kwh: price,
        };

        // Build control config
        let control_config = ControlConfig {
            battery_capacity_kwh: state.config.battery_capacity_kwh,
            max_battery_charge_rate_kw: state.config.max_charge_rate_kw,
            battery_efficiency: state.config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: state.config.battery_wear_cost_czk_per_kwh,
            min_battery_soc: state.config.min_soc,
            max_battery_soc: state.config.max_soc,
            ..ControlConfig::default()
        };

        // Calculate export price
        let export_price = price * state.config.export_price_ratio;

        // Calculate solar forecast values
        let solar_forecast_total_today_kwh: f32 =
            state.day.blocks.iter().map(|b| b.solar_kwh).sum();
        let solar_forecast_remaining_today_kwh: f32 = state
            .day
            .blocks
            .iter()
            .skip(block_idx)
            .map(|b| b.solar_kwh)
            .sum();

        // Get strategy IDs to iterate (clone to avoid borrow issues)
        let strategy_ids: Vec<String> = state.strategy_results.keys().cloned().collect();

        // Evaluate each strategy
        for strategy_id in strategy_ids {
            // Check for SOC override at this block
            if let Some(ref soc_override) = state.overrides.soc_override
                && soc_override.block_index == block_idx
                && let Some(result) = state.strategy_results.get_mut(&strategy_id)
                && (soc_override.strategy_ids.is_none()
                    || soc_override
                        .strategy_ids
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&strategy_id)))
            {
                result.current_soc = soc_override.soc_percent;
            }

            // Get current SOC for this strategy
            let current_soc = state
                .strategy_results
                .get(&strategy_id)
                .map(|r| r.current_soc)
                .unwrap_or(50.0);

            // Build evaluation context
            let context = EvaluationContext {
                price_block: &current_price_block,
                control_config: &control_config,
                current_battery_soc: current_soc,
                solar_forecast_kwh: block.solar_kwh,
                consumption_forecast_kwh: consumption,
                grid_export_price_czk_per_kwh: export_price,
                all_price_blocks: Some(&all_price_blocks),
                backup_discharge_min_soc: state.config.min_soc,
                grid_import_today_kwh: state
                    .strategy_results
                    .get(&strategy_id)
                    .map(|r| r.total_grid_import_kwh),
                consumption_today_kwh: state
                    .strategy_results
                    .get(&strategy_id)
                    .map(|r| r.total_grid_import_kwh + r.total_battery_discharge_kwh),
                solar_forecast_total_today_kwh,
                solar_forecast_remaining_today_kwh,
                solar_forecast_tomorrow_kwh: 0.0, // Single-day simulation, no tomorrow data
                battery_avg_charge_price_czk_per_kwh: 0.0, // Not tracked in simulator
            };

            // Get strategy and evaluate
            if let Some(strategy) = self.registry.get(&strategy_id) {
                let eval = strategy.evaluate(&context);

                // Calculate new SOC based on mode and energy flows
                let new_soc = self.calculate_new_soc(current_soc, &eval, &state.config);

                // Calculate costs from energy flows (centralized cost calculation)
                // This ensures all strategies are evaluated with identical cost logic.
                // Only real measurable costs: grid import/export at spot/tariff prices
                let import_cost = eval.energy_flows.grid_import_kwh * price;
                let export_revenue = eval.energy_flows.grid_export_kwh * export_price;

                // Update result
                if let Some(result) = state.strategy_results.get_mut(&strategy_id) {
                    result.soc_history.push(new_soc);
                    result.current_soc = new_soc;
                    result.current_mode = eval.mode;
                    result.last_reason = eval.reason.clone();

                    // Update energy totals
                    result.total_grid_import_kwh += eval.energy_flows.grid_import_kwh;
                    result.total_grid_export_kwh += eval.energy_flows.grid_export_kwh;
                    result.total_battery_charge_kwh += eval.energy_flows.battery_charge_kwh;
                    result.total_battery_discharge_kwh += eval.energy_flows.battery_discharge_kwh;

                    // Update cost totals (using engine-calculated costs, not strategy-provided)
                    // Only real measurable costs (grid import/export)
                    result.total_import_cost_czk += import_cost;
                    result.total_export_revenue_czk += export_revenue;
                    result.net_cost_czk =
                        result.total_import_cost_czk - result.total_export_revenue_czk;

                    result.cumulative_cost_czk.push(result.net_cost_czk);
                    result.evaluations.push(eval);
                }
            }
        }

        Ok(())
    }

    /// Calculate new SOC based on strategy evaluation
    ///
    /// Trust the strategy's calculated energy flows directly instead of recalculating.
    /// The strategy already has access to all constraints (battery capacity, charge rates,
    /// current SOC) via EvaluationContext and calculates appropriate values.
    fn calculate_new_soc(
        &self,
        current_soc: f32,
        eval: &fluxion_core::strategy::BlockEvaluation,
        config: &SimulationConfig,
    ) -> f32 {
        let capacity = config.battery_capacity_kwh;
        let efficiency = config.battery_efficiency;

        // Use the strategy's calculated energy flows directly
        let net_battery_change =
            eval.energy_flows.battery_charge_kwh - eval.energy_flows.battery_discharge_kwh;

        // Apply efficiency loss only to charging (discharge is already accounted for)
        let effective_change = if net_battery_change > 0.0 {
            net_battery_change * efficiency
        } else {
            net_battery_change // Discharge doesn't have efficiency loss
        };

        // Convert kWh to percentage
        let soc_change_percent = (effective_change / capacity) * 100.0;

        // Clamp to valid range
        (current_soc + soc_change_percent).clamp(config.min_soc, config.max_soc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::price_scenarios::PriceScenario;
    use crate::synthetic_data::{ConsumptionProfile, SolarProfile, SyntheticDayConfig};

    #[test]
    fn test_create_simulation() {
        let engine = SimulationEngine::new();

        let day_config = SyntheticDayConfig {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            consumption: ConsumptionProfile::default(),
            solar: SolarProfile::None,
            price_scenario: PriceScenario::UsualDay,
            initial_soc: 50.0,
            battery_capacity_kwh: 10.0,
            hdo_periods: None,
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        };

        let sim_config = SimulationConfig::default();

        let state = engine.create_simulation(day_config, sim_config).unwrap();

        assert_eq!(state.current_block, 0);
        assert!(!state.strategy_results.is_empty());
        assert!(state.strategy_results.contains_key("winter_adaptive_v4"));
        assert!(state.strategy_results.contains_key("no_battery"));
        assert!(state.strategy_results.contains_key("naive"));
    }

    #[test]
    fn test_step_simulation() {
        let engine = SimulationEngine::new();

        let day_config = SyntheticDayConfig::default();
        let sim_config = SimulationConfig::default();

        let mut state = engine.create_simulation(day_config, sim_config).unwrap();

        // Step 4 blocks (1 hour)
        engine.step(&mut state, 4).unwrap();

        assert_eq!(state.current_block, 4);

        // Check that results were recorded
        for result in state.strategy_results.values() {
            assert_eq!(result.soc_history.len(), 5); // Initial + 4 blocks
            assert_eq!(result.evaluations.len(), 4);
        }
    }

    #[test]
    fn test_run_to_completion() {
        let engine = SimulationEngine::new();

        let day_config = SyntheticDayConfig::default();
        let sim_config = SimulationConfig::default();

        let mut state = engine.create_simulation(day_config, sim_config).unwrap();

        engine.run_to_completion(&mut state).unwrap();

        assert_eq!(state.current_block, 96);
        assert!(state.is_complete());

        // All strategies should have full day of evaluations
        for result in state.strategy_results.values() {
            assert_eq!(result.evaluations.len(), 96);
        }
    }

    #[test]
    fn test_v4_beats_no_battery() {
        let engine = SimulationEngine::new();

        let day_config = SyntheticDayConfig::default();
        let sim_config = SimulationConfig::default();

        let mut state = engine.create_simulation(day_config, sim_config).unwrap();
        engine.run_to_completion(&mut state).unwrap();

        let v4_cost = state
            .strategy_results
            .get("winter_adaptive_v4")
            .unwrap()
            .net_cost_czk;
        let no_battery_cost = state
            .strategy_results
            .get("no_battery")
            .unwrap()
            .net_cost_czk;

        assert!(
            v4_cost < no_battery_cost,
            "V4 ({:.2} CZK) should beat no-battery ({:.2} CZK)",
            v4_cost,
            no_battery_cost
        );
    }

    #[test]
    fn test_load_override_affects_costs() {
        let engine = SimulationEngine::new();

        let day_config = SyntheticDayConfig::default();
        let sim_config = SimulationConfig::default();

        let mut state = engine.create_simulation(day_config, sim_config).unwrap();
        engine.run_to_completion(&mut state).unwrap();

        let original_cost = state
            .strategy_results
            .get("no_battery")
            .unwrap()
            .net_cost_czk;

        // Reset and apply high load override
        engine.reset_to_block(&mut state, 0).unwrap();
        engine
            .apply_load_override(&mut state, vec![(28, 2.0), (29, 2.0), (30, 2.0), (31, 2.0)])
            .unwrap();
        engine.run_to_completion(&mut state).unwrap();

        let new_cost = state
            .strategy_results
            .get("no_battery")
            .unwrap()
            .net_cost_czk;

        assert!(
            (new_cost - original_cost).abs() > 0.1,
            "Load override should affect costs: original {:.2}, new {:.2}",
            original_cost,
            new_cost
        );
    }
}
