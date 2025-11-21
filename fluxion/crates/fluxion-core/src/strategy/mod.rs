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

mod day_ahead_planning;
mod morning_precharge;
mod optimizer;
mod price_arbitrage;
mod seasonal_mode;
mod seasonal_optimizer;
mod self_use;
mod solar_first;
mod time_aware_charge;
mod winter_peak_discharge;

// New unified strategies
mod enhanced_self_use;
mod unified_smart_charge;
mod smart_discharge;

pub use day_ahead_planning::DayAheadChargePlanningStrategy;
pub use morning_precharge::MorningPreChargeStrategy;
pub use optimizer::EconomicOptimizer;
pub use price_arbitrage::PriceArbitrageStrategy;
pub use seasonal_mode::SeasonalMode;
pub use seasonal_optimizer::{AdaptiveSeasonalOptimizer, SeasonalStrategiesConfig};
pub use self_use::SelfUseStrategy;
pub use solar_first::SolarFirstStrategy;
pub use time_aware_charge::TimeAwareChargeStrategy;

// Export new unified strategies
pub use enhanced_self_use::EnhancedSelfUseStrategy;
pub use unified_smart_charge::{UnifiedSmartChargeConfig, UnifiedSmartChargeStrategy};
pub use smart_discharge::{DischargeSeasonConfig, SmartDischargeStrategy};

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::resources::ControlConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Detailed energy flow information for a time block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnergyFlows {
    /// Energy purchased from grid (kWh)
    pub grid_import_kwh: f32,

    /// Energy exported to grid (kWh)
    pub grid_export_kwh: f32,

    /// Energy charged to battery (kWh)
    pub battery_charge_kwh: f32,

    /// Energy discharged from battery (kWh)
    pub battery_discharge_kwh: f32,

    /// Solar energy generated (kWh)
    pub solar_generation_kwh: f32,

    /// Household consumption (kWh)
    pub household_consumption_kwh: f32,
}

impl Default for EnergyFlows {
    fn default() -> Self {
        Self {
            grid_import_kwh: 0.0,
            grid_export_kwh: 0.0,
            battery_charge_kwh: 0.0,
            battery_discharge_kwh: 0.0,
            solar_generation_kwh: 0.0,
            household_consumption_kwh: 0.0,
        }
    }
}

/// Assumptions and parameters used in strategy evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assumptions {
    /// Assumed solar generation for this block (kWh)
    pub solar_forecast_kwh: f32,

    /// Assumed household consumption for this block (kWh)
    pub consumption_forecast_kwh: f32,

    /// Current battery state of charge (%)
    pub current_battery_soc: f32,

    /// Battery round-trip efficiency (0.0 to 1.0)
    pub battery_efficiency: f32,

    /// Battery wear cost per kWh cycled (CZK/kWh)
    pub battery_wear_cost_czk_per_kwh: f32,

    /// Grid import price (CZK/kWh)
    pub grid_import_price_czk_per_kwh: f32,

    /// Grid export price (CZK/kWh)
    pub grid_export_price_czk_per_kwh: f32,
}

impl Default for Assumptions {
    fn default() -> Self {
        Self {
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25, // ~1 kWh per hour typical
            current_battery_soc: 50.0,
            battery_efficiency: 0.95,
            battery_wear_cost_czk_per_kwh: 0.125,
            grid_import_price_czk_per_kwh: 0.50,
            grid_export_price_czk_per_kwh: 0.40,
        }
    }
}

/// Debug information about strategy evaluation for a block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEvaluation {
    /// Name of the strategy
    pub strategy_name: String,

    /// Operation mode this strategy recommends
    pub mode: InverterOperationMode,

    /// Net profit score from this strategy (CZK)
    pub net_profit_czk: f32,

    /// Detailed reasoning for this strategy's decision
    pub reason: String,
}

/// Debug information captured during block scheduling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDebugInfo {
    /// All strategies that were evaluated for this block
    pub evaluated_strategies: Vec<StrategyEvaluation>,

    /// Explanation of why the winning strategy was chosen
    pub winning_reason: String,

    /// Key conditions that were checked
    pub conditions: Vec<String>,
}

/// Complete economic evaluation of a time block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEvaluation {
    /// Time block start
    pub block_start: DateTime<Utc>,

    /// Duration in minutes (typically 15)
    pub duration_minutes: u32,

    /// Recommended operation mode
    pub mode: InverterOperationMode,

    /// Expected revenue from this block (CZK)
    pub revenue_czk: f32,

    /// Expected costs for this block (CZK)
    pub cost_czk: f32,

    /// Net profit (revenue - cost) (CZK)
    pub net_profit_czk: f32,

    /// Detailed energy flows
    pub energy_flows: EnergyFlows,

    /// Assumptions used in evaluation
    pub assumptions: Assumptions,

    /// Human-readable reason for this decision
    pub reason: String,

    /// Name of the strategy that generated this evaluation
    pub strategy_name: String,

    /// Debug information (only populated when log_level = debug)
    pub debug_info: Option<BlockDebugInfo>,
}

impl BlockEvaluation {
    /// Create a new block evaluation with basic info
    pub fn new(
        block_start: DateTime<Utc>,
        duration_minutes: u32,
        mode: InverterOperationMode,
        strategy_name: String,
    ) -> Self {
        Self {
            block_start,
            duration_minutes,
            mode,
            revenue_czk: 0.0,
            cost_czk: 0.0,
            net_profit_czk: 0.0,
            energy_flows: EnergyFlows::default(),
            assumptions: Assumptions::default(),
            reason: String::new(),
            strategy_name,
            debug_info: None,
        }
    }

    /// Calculate net profit from revenue and cost
    pub fn calculate_net_profit(&mut self) {
        self.net_profit_czk = self.revenue_czk - self.cost_czk;
    }
}

/// Context information for strategy evaluation
#[derive(Debug, Clone)]
pub struct EvaluationContext<'a> {
    /// Price information for this block
    pub price_block: &'a TimeBlockPrice,

    /// Control configuration (battery parameters, constraints)
    pub control_config: &'a ControlConfig,

    /// Current battery state of charge (%)
    pub current_battery_soc: f32,

    /// Forecasted solar generation for this block (kWh)
    pub solar_forecast_kwh: f32,

    /// Forecasted household consumption for this block (kWh)
    pub consumption_forecast_kwh: f32,

    /// Price for selling to grid (CZK/kWh) - can differ from buying
    pub grid_export_price_czk_per_kwh: f32,

    /// All price blocks for today/tomorrow (for global price analysis)
    pub all_price_blocks: Option<&'a [TimeBlockPrice]>,
}

/// Trait for economic battery operation strategies
///
/// Each strategy evaluates the profitability of operating the battery
/// in various modes for a given time block, considering:
/// - Energy prices (import/export)
/// - Battery degradation costs
/// - Round-trip efficiency losses
/// - Solar generation forecasts
/// - Consumption patterns
/// - Opportunity costs
pub trait EconomicStrategy: Send + Sync {
    /// Get the name of this strategy
    fn name(&self) -> &str;

    /// Evaluate this strategy for a given time block
    ///
    /// Returns a `BlockEvaluation` with the recommended mode and
    /// detailed economic analysis
    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation;

    /// Check if this strategy is enabled
    fn is_enabled(&self) -> bool {
        true // By default, strategies are enabled
    }
}

/// Helper functions for economic calculations
pub mod economics {
    /// Calculate battery degradation cost for a given energy throughput
    ///
    /// # Arguments
    /// * `energy_kwh` - Energy cycled through battery (kWh)
    /// * `wear_cost_per_kwh` - Cost per kWh cycled (CZK/kWh)
    ///
    /// # Returns
    /// Degradation cost in CZK
    pub fn battery_degradation_cost(energy_kwh: f32, wear_cost_per_kwh: f32) -> f32 {
        energy_kwh * wear_cost_per_kwh
    }

    /// Calculate efficiency loss for battery operation
    ///
    /// # Arguments
    /// * `energy_kwh` - Energy input to battery (kWh)
    /// * `efficiency` - Round-trip efficiency (0.0 to 1.0)
    ///
    /// # Returns
    /// Energy lost due to inefficiency (kWh)
    pub fn efficiency_loss(energy_kwh: f32, efficiency: f32) -> f32 {
        energy_kwh * (1.0 - efficiency)
    }

    /// Calculate cost of grid import
    pub fn grid_import_cost(energy_kwh: f32, price_per_kwh: f32) -> f32 {
        energy_kwh * price_per_kwh
    }

    /// Calculate revenue from grid export
    pub fn grid_export_revenue(energy_kwh: f32, price_per_kwh: f32) -> f32 {
        energy_kwh * price_per_kwh
    }

    /// Calculate opportunity cost of not exporting solar energy
    ///
    /// When we store solar energy instead of exporting it immediately,
    /// we forgo the immediate export revenue but can potentially earn
    /// more by exporting later at a higher price (minus losses)
    pub fn solar_opportunity_cost(
        solar_kwh: f32,
        current_export_price: f32,
        efficiency: f32,
    ) -> f32 {
        // Immediate revenue we're giving up
        let immediate_revenue = solar_kwh * current_export_price;

        // After round-trip losses, we'd have this much to export later
        let future_exportable = solar_kwh * efficiency;

        // Opportunity cost is the guaranteed revenue minus what we might earn
        // (This is a simplified model; full model would consider future price forecasts)
        immediate_revenue - (future_exportable * current_export_price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_energy_flows_default() {
        let flows = EnergyFlows::default();
        assert_eq!(flows.grid_import_kwh, 0.0);
        assert_eq!(flows.battery_charge_kwh, 0.0);
    }

    #[test]
    fn test_assumptions_default() {
        let assumptions = Assumptions::default();
        assert_eq!(assumptions.battery_efficiency, 0.95);
        assert_eq!(assumptions.current_battery_soc, 50.0);
    }

    #[test]
    fn test_block_evaluation_calculate_profit() {
        let mut eval = BlockEvaluation::new(
            Utc::now(),
            15,
            InverterOperationMode::SelfUse,
            "test".to_string(),
        );

        eval.revenue_czk = 10.0;
        eval.cost_czk = 6.0;
        eval.calculate_net_profit();

        assert_eq!(eval.net_profit_czk, 4.0);
    }

    #[test]
    fn test_battery_degradation_cost() {
        let cost = economics::battery_degradation_cost(10.0, 0.125);
        assert_eq!(cost, 1.25);
    }

    #[test]
    fn test_efficiency_loss() {
        let loss = economics::efficiency_loss(10.0, 0.95);
        assert!((loss - 0.5).abs() < 0.001); // Use floating point tolerance
    }

    #[test]
    fn test_grid_import_cost() {
        let cost = economics::grid_import_cost(5.0, 0.50);
        assert_eq!(cost, 2.5);
    }

    #[test]
    fn test_grid_export_revenue() {
        let revenue = economics::grid_export_revenue(5.0, 0.40);
        assert_eq!(revenue, 2.0);
    }
}
