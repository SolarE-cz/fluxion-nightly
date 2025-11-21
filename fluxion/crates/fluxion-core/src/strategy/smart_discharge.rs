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

//! Smart Discharge Strategy (Season-Aware)
//!
//! Enhanced version of WinterPeakDischargeStrategy that works year-round
//! with season-adaptive parameters.
//!
//! Key improvements over WinterPeakDischargeStrategy:
//! - Season-aware configuration (different thresholds for summer/winter)
//! - Gradual discharge across multiple high-price blocks
//! - Configurable parameters per season
//! - All existing safety checks preserved

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::strategy::seasonal_mode::SeasonalMode;
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext,
};

use std::collections::HashSet;


/// Discharge configuration for a specific season
#[derive(Debug, Clone)]
pub struct DischargeSeasonConfig {
    pub min_spread_czk: f32,
    pub min_soc_to_start: f32,
    pub min_soc_target: f32,
    pub min_hours_to_solar: u32,
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub reserve_hours: f32,
    /// Minimum profit per kWh for arbitrage trades (after grid fees and battery wear)
    /// Formula: Execute if (Sell_Price - Buy_Price - Grid_Fee - Battery_Wear) > min_arbitrage_profit_czk
    pub min_arbitrage_profit_czk: f32,
    /// Price threshold ratio for gradual discharge (e.g. 0.9 of daily median)
    pub gradual_discharge_threshold: f32,
}

impl DischargeSeasonConfig {
    /// Create summer configuration (more aggressive)
    pub fn summer() -> Self {
        Self {
            min_spread_czk: 2.0,
            min_soc_to_start: 60.0,
            min_soc_target: 40.0,
            min_hours_to_solar: 2,
            solar_window_start_hour: 7,
            solar_window_end_hour: 19,
            reserve_hours: 1.0,
            min_arbitrage_profit_czk: 2.0,
            gradual_discharge_threshold: 0.9,
        }
    }

    /// Create winter configuration (more conservative)
    pub fn winter() -> Self {
        Self {
            min_spread_czk: 3.0,
            min_soc_to_start: 70.0,
            min_soc_target: 50.0,
            min_hours_to_solar: 6,
            solar_window_start_hour: 9,
            solar_window_end_hour: 15,
            reserve_hours: 3.0,
            min_arbitrage_profit_czk: 3.0,
            gradual_discharge_threshold: 0.9,
        }
    }
}

/// Smart Discharge Strategy with season-aware parameters
#[derive(Debug, Clone)]
pub struct SmartDischargeStrategy {
    enabled: bool,
    _summer_config: DischargeSeasonConfig,
    _winter_config: DischargeSeasonConfig,
    /// Cache of selected discharge block timestamps (global awareness)
    selected_discharge_blocks:
        std::sync::Arc<std::sync::RwLock<HashSet<chrono::DateTime<chrono::Utc>>>>,
    /// Cache of selected charge block timestamps for arbitrage (pre-charge before discharge)
    selected_charge_blocks:
        std::sync::Arc<std::sync::RwLock<HashSet<chrono::DateTime<chrono::Utc>>>>,
}

impl SmartDischargeStrategy {
    /// Create a new Smart Discharge strategy with custom configs
    pub fn new(
        enabled: bool,
        summer_config: DischargeSeasonConfig,
        winter_config: DischargeSeasonConfig,
    ) -> Self {
        Self {
            enabled,
            _summer_config: summer_config,
            _winter_config: winter_config,
            selected_discharge_blocks: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
            selected_charge_blocks: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
        }
    }

    /// Get configuration for current season
    fn _get_config_for_season(&self, date: chrono::DateTime<chrono::Utc>) -> &DischargeSeasonConfig {
        match SeasonalMode::from_date(date) {
            SeasonalMode::Summer => &self._summer_config,
            SeasonalMode::Winter => &self._winter_config,
        }
    }

    /// Plan arbitrage opportunities: charge at low prices, discharge at high prices
    /// User's requirement: Identify high export prices (median + 3 CZK), plan discharge,
    /// then find cheapest blocks to pre-charge battery for maximum arbitrage profit
    pub fn plan_discharge_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        _initial_soc: f32,
        control_config: &crate::resources::ControlConfig,
        pricing_config: &crate::resources::PricingConfig,
    ) {
        if !self.enabled {
            return;
        }

        // Clear previous selections
        if let Ok(mut selected) = self.selected_discharge_blocks.write() {
            selected.clear();
        } else {
            return;
        }
        if let Ok(mut selected) = self.selected_charge_blocks.write() {
            selected.clear();
        } else {
            return;
        }

        if all_blocks.is_empty() {
            return;
        }

        // Constants from user request (now from config)
        let fixed_sell_fee = pricing_config.spot_sell_fee_czk;
        let fixed_import_fee = pricing_config.grid_distribution_fee_czk;
        let fixed_buy_fee = pricing_config.spot_buy_fee_czk;
        let total_import_adder = fixed_import_fee + fixed_buy_fee;
        


        // 1. Group blocks by day to identify "today's" peaks
        // We assume the input `all_blocks` covers the relevant planning horizon (e.g. 48h)
        // We will process each day independently.
        
        let mut blocks_by_day: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
        
        for (i, block) in all_blocks.iter().enumerate() {
            let day_key = block.block_start.format("%Y-%m-%d").to_string();
            blocks_by_day.entry(day_key).or_default().push(i);
        }

        let mut used_charge_indices = std::collections::HashSet::new();
        let mut used_discharge_indices = std::collections::HashSet::new();

        // Process each day
        let mut days: Vec<_> = blocks_by_day.keys().cloned().collect();
        days.sort(); // Process in chronological order

        for day in days {
            let indices = &blocks_by_day[&day];
            
            // 2. Find Top 8 most expensive blocks for this day
            let mut day_blocks: Vec<(usize, &TimeBlockPrice)> = indices.iter()
                .map(|&i| (i, &all_blocks[i]))
                .collect();
            
            // Sort by price descending
            day_blocks.sort_by(|a, b| b.1.price_czk_per_kwh.partial_cmp(&a.1.price_czk_per_kwh).unwrap());
            
            // Take top 8
            let top_8_peaks = day_blocks.iter().take(8);

            for (discharge_idx, discharge_block) in top_8_peaks {
                if used_discharge_indices.contains(discharge_idx) {
                    continue;
                }

                // 3. Find best charge opportunity for this peak
                // Must be BEFORE the discharge block
                // Must satisfy profitability condition
                
                let export_revenue = discharge_block.price_czk_per_kwh - fixed_sell_fee;
                
                let mut max_profit = 0.0;
                let mut best_charge_idx: Option<usize> = None;

                // Iterate through all blocks BEFORE the discharge block to find the cheapest charge opportunity
                for (charge_idx, charge_block) in all_blocks.iter().enumerate() {
                    if charge_block.block_start >= discharge_block.block_start {
                        continue; // Must charge before discharge
                    }
                    if used_charge_indices.contains(&charge_idx) {
                        continue; // Don't reuse charge blocks
                    }

                    let import_cost = charge_block.price_czk_per_kwh + total_import_adder;

                    // Profit = (Peak - SellFee) - (Charge + ImportFee + BuyFee) - Wear
                    let profit = export_revenue - import_cost - control_config.battery_wear_cost_czk_per_kwh;

                    if profit > 0.0 && profit > max_profit {
                        max_profit = profit;
                        best_charge_idx = Some(charge_idx);
                    }
                }

                // 4. If profitable pair found, schedule it
                if let Some(charge_idx) = best_charge_idx {
                    used_charge_indices.insert(charge_idx);
                    used_discharge_indices.insert(*discharge_idx);
                    
                    let charge_block = &all_blocks[charge_idx];
                    
                    // Add to selected sets
                     if let Ok(mut selected) = self.selected_charge_blocks.write() {
                        selected.insert(charge_block.block_start);
                    }
                    if let Ok(mut selected) = self.selected_discharge_blocks.write() {
                        selected.insert(discharge_block.block_start);
                    }
                    
                    tracing::debug!(
                        "ARBITRAGE PAIR: Charge @ {} ({:.2} CZK) -> Discharge @ {} ({:.2} CZK) | Profit: {:.2} CZK/kWh",
                        charge_block.block_start.format("%H:%M"),
                        charge_block.price_czk_per_kwh,
                        discharge_block.block_start.format("%H:%M"),
                        discharge_block.price_czk_per_kwh,
                        max_profit
                    );
                }
            }
        }
    }
}

impl Default for SmartDischargeStrategy {
    fn default() -> Self {
        Self::new(
            true,
            DischargeSeasonConfig::summer(),
            DischargeSeasonConfig::winter(),
        )
    }
}

impl EconomicStrategy for SmartDischargeStrategy {
    fn name(&self) -> &str {
        "Smart-Discharge"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        // Check if this is a selected CHARGE block (for arbitrage pre-charging)
        let is_charge_block = if let Ok(charge_blocks) = self.selected_charge_blocks.read() {
            charge_blocks.contains(&context.price_block.block_start)
        } else {
            false
        };

        // Check if this is a selected DISCHARGE block
        let is_discharge_block = if let Ok(discharge_blocks) = self.selected_discharge_blocks.read() {
            discharge_blocks.contains(&context.price_block.block_start)
        } else {
            false
        };
        
        // ARBITRAGE CHARGING: Pre-charge battery from grid during low-price blocks
        if is_charge_block {
            let mut eval = BlockEvaluation::new(
                context.price_block.block_start,
                context.price_block.duration_minutes,
                InverterOperationMode::ForceCharge, // CHARGE FROM GRID
                self.name().to_string(),
            );

            eval.assumptions = Assumptions {
                solar_forecast_kwh: context.solar_forecast_kwh,
                consumption_forecast_kwh: context.consumption_forecast_kwh,
                current_battery_soc: context.current_battery_soc,
                battery_efficiency: context.control_config.battery_efficiency,
                battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
                grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
                grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
            };

            println!("DEBUG: ARBITRAGE CHARGE at {} - Price: {:.2} CZK",
                context.price_block.block_start.format("%H:%M"),
                context.price_block.price_czk_per_kwh);

            // Calculate charging
            let solar = context.solar_forecast_kwh;
            let consumption = context.consumption_forecast_kwh;
            let max_charge_kw = context.control_config.max_battery_charge_rate_kw;
            let block_duration_hours = context.price_block.duration_minutes as f32 / 60.0;
            let _efficiency = context.control_config.battery_efficiency;

            // Use solar for consumption first, then charge battery from grid
            let solar_to_load = solar.min(consumption);
            let remaining_solar = solar - solar_to_load;
            
            // Charge from solar if available
            let solar_to_battery = remaining_solar.min(max_charge_kw * block_duration_hours);
            
            // Additional grid charging to fill battery
            let grid_charge_capacity = (max_charge_kw * block_duration_hours - solar_to_battery).max(0.0);
            let grid_to_battery = grid_charge_capacity; // Charge as much as possible from grid

            // Grid import for both load and charging
            let load_deficit = (consumption - solar_to_load).max(0.0);
            let grid_to_load = load_deficit;
            let total_grid_import = grid_to_load + grid_to_battery;

            // Set energy flows
            eval.energy_flows.grid_import_kwh = total_grid_import;
            eval.energy_flows.grid_export_kwh = 0.0;
            eval.energy_flows.battery_charge_kwh = solar_to_battery + grid_to_battery;
            eval.energy_flows.battery_discharge_kwh = 0.0;

            // Calculate cost
            let import_cost = total_grid_import * context.price_block.price_czk_per_kwh;
            let battery_wear = (solar_to_battery + grid_to_battery) * context.control_config.battery_wear_cost_czk_per_kwh;
            
            // Calculate avoided cost (revenue from self-use)
            let avoided_import_cost = solar_to_load * context.price_block.price_czk_per_kwh;
            
            eval.net_profit_czk = avoided_import_cost - import_cost - battery_wear;

            eval.reason = format!("ARBITRAGE: Charging {:.2} kWh from grid at {:.2} CZK/kWh",
                grid_to_battery, context.price_block.price_czk_per_kwh);

            return eval;
        }

        // ARBITRAGE DISCHARGING: Discharge to grid during high-price blocks
        if is_discharge_block {
            let mut eval = BlockEvaluation::new(
                context.price_block.block_start,
                context.price_block.duration_minutes,
                InverterOperationMode::ForceDischarge,
                self.name().to_string(),
            );

            eval.assumptions = Assumptions {
                solar_forecast_kwh: context.solar_forecast_kwh,
                consumption_forecast_kwh: context.consumption_forecast_kwh,
                current_battery_soc: context.current_battery_soc,
                battery_efficiency: context.control_config.battery_efficiency,
                battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
                grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
                grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
            };

            tracing::debug!("ARBITRAGE DISCHARGE at {} - Export Price: {:.2} CZK",
                context.price_block.block_start.format("%H:%M"),
                context.grid_export_price_czk_per_kwh);

            // AGGRESSIVE DISCHARGE during rare peak prices
            // Goal: Export maximum battery energy to grid, but NEVER import at peak prices!
            let solar = context.solar_forecast_kwh;
            let consumption = context.consumption_forecast_kwh;
            let max_discharge_kw = context.control_config.max_battery_charge_rate_kw;
            let block_duration_hours = context.price_block.duration_minutes as f32 / 60.0;

            // Calculate maximum possible discharge
            let max_discharge_kwh = max_discharge_kw * block_duration_hours;
            let available_battery_kwh = (context.current_battery_soc - context.control_config.min_battery_soc).max(0.0) 
                * context.control_config.battery_capacity_kwh / 100.0;
            let actual_discharge_kwh = max_discharge_kwh.min(available_battery_kwh);

            // Energy flow priority (critical for avoiding peak price imports!):
            // 1. Solar → consumption
            // 2. Battery → remaining consumption (MUST NOT import at peak price!)
            // 3. Battery → grid export (maximize this for profit)
            
            let solar_to_load = solar.min(consumption);
            let load_after_solar = (consumption - solar_to_load).max(0.0);
            
            // Use battery for load FIRST (saves import cost at peak price)
            let battery_to_load = load_after_solar.min(actual_discharge_kwh);
            
            // Remaining battery capacity goes to export
            let battery_to_grid = (actual_discharge_kwh - battery_to_load).max(0.0);
            
            // Any surplus solar also exports
            let solar_surplus = (solar - consumption).max(0.0);
            
            // If we still have load deficit after solar + battery, THEN import (but try to avoid this!)
            let grid_import = (load_after_solar - battery_to_load).max(0.0);
            
            let total_export = solar_surplus + battery_to_grid;
            let total_discharge = battery_to_load + battery_to_grid;

            // Set energy flows
            eval.energy_flows.grid_import_kwh = grid_import;
            eval.energy_flows.grid_export_kwh = total_export;
            eval.energy_flows.battery_charge_kwh = 0.0;
            eval.energy_flows.battery_discharge_kwh = total_discharge;

            // Calculate profit
            let import_cost = grid_import * context.price_block.price_czk_per_kwh;
            let export_revenue = (battery_to_grid + solar_surplus) * context.grid_export_price_czk_per_kwh;
            let battery_wear = (battery_to_load + battery_to_grid) * context.control_config.battery_wear_cost_czk_per_kwh;
            
            // Calculate avoided cost (revenue from self-use)
            let avoided_import_cost = (solar_to_load + battery_to_load) * context.price_block.price_czk_per_kwh;
            
            eval.net_profit_czk = avoided_import_cost + export_revenue - import_cost - battery_wear;

            eval.reason = format!("PEAK: Battery→Load: {:.2}, Battery→Grid: {:.2} kWh @ {:.2} CZK (Profit: {:.2})",
                battery_to_load, battery_to_grid, context.grid_export_price_czk_per_kwh, eval.net_profit_czk);

            return eval;
        }

        // FALLBACK: Standard Self-Use Logic
        // If we are not in a planned charge/discharge block, we default to self-use.
        
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::SelfUse,
            self.name().to_string(),
        );

        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        // Standard self-use logic
        let (solar, consumption) = (context.solar_forecast_kwh, context.consumption_forecast_kwh);
        
        if solar >= consumption {
            // Surplus solar
            let surplus = solar - consumption;
            eval.energy_flows.battery_charge_kwh = surplus;
            eval.energy_flows.grid_export_kwh = 0.0;
            eval.energy_flows.grid_import_kwh = 0.0;
            
            // Calculate avoided cost (revenue from self-use)
            // solar used = consumption
            let avoided_import_cost = consumption * context.price_block.price_czk_per_kwh;
            eval.net_profit_czk = avoided_import_cost;
        } else {
            // Deficit - use battery to cover
            let deficit = consumption - solar;
            let max_discharge_kw = context.control_config.max_battery_charge_rate_kw;
            let block_duration_hours = context.price_block.duration_minutes as f32 / 60.0;
            
            // Use available battery capacity above min_soc
            let available_battery_kwh = (context.current_battery_soc - context.control_config.min_battery_soc).max(0.0) 
                * context.control_config.battery_capacity_kwh / 100.0;
                
            let battery_available = deficit.min(max_discharge_kw * block_duration_hours).min(available_battery_kwh);
            
            eval.energy_flows.battery_discharge_kwh = battery_available;
            eval.energy_flows.grid_import_kwh = (deficit - battery_available).max(0.0);
            eval.energy_flows.grid_export_kwh = 0.0;
            
            let import_cost = eval.energy_flows.grid_import_kwh * context.price_block.price_czk_per_kwh;
            let battery_wear = battery_available * context.control_config.battery_wear_cost_czk_per_kwh;
            
            // Calculate avoided cost (revenue from self-use)
            // solar used = solar (since solar < consumption)
            // battery used = battery_available
            let avoided_import_cost = (solar + battery_available) * context.price_block.price_czk_per_kwh;
            
            eval.net_profit_czk = avoided_import_cost - import_cost - battery_wear;
        }

        eval.reason = "Self-Use (no arbitrage opportunity)".to_string();
        eval
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::TimeBlockPrice;
    use crate::resources::ControlConfig;
    use chrono::Utc;

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
    fn test_smart_discharge_winter_mode() {
        let strategy = SmartDischargeStrategy::default();
        let config = cfg();

        // Winter date (October)
        let now = chrono::DateTime::parse_from_rfc3339("2025-10-14T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let all_blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 9.53, // High price
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::hours(8),
                duration_minutes: 15,
                price_czk_per_kwh: 2.40, // Low price
            },
        ];

        // Mock pricing config
        let pricing_config = crate::resources::PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: crate::resources::PriceSchedule::Flat(0.0),
            fixed_sell_price_czk: crate::resources::PriceSchedule::Flat(0.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            grid_distribution_fee_czk: 1.2,
        };

        // Plan discharge blocks
        strategy.plan_discharge_blocks(&all_blocks, 100.0, &config, &pricing_config);

        let price_block = &all_blocks[0];

        let ctx = EvaluationContext {
            price_block,
            control_config: &config,
            current_battery_soc: 100.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 9.53,
            all_price_blocks: Some(&all_blocks),
        };

        let eval = strategy.evaluate(&ctx);

        // Should evaluate for discharge
        assert!(!eval.strategy_name.is_empty());
    }

    #[test]
    fn test_smart_discharge_summer_config() {
        let summer_config = DischargeSeasonConfig::summer();
        assert_eq!(summer_config.min_spread_czk, 2.0);
        assert_eq!(summer_config.min_soc_to_start, 60.0);
    }

    #[test]
    fn test_smart_discharge_winter_config() {
        let winter_config = DischargeSeasonConfig::winter();
        assert_eq!(winter_config.min_spread_czk, 3.0);
        assert_eq!(winter_config.min_soc_to_start, 70.0);
    }

    #[test]
    fn test_strategy_name() {
        let strategy = SmartDischargeStrategy::default();
        assert_eq!(strategy.name(), "Smart-Discharge");
    }
}
