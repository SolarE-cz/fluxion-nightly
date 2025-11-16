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

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::Timelike;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct WinterPeakDischargeStrategy {
    enabled: bool,
    pub min_spread_czk: f32,          // 3.0
    pub min_soc_to_start: f32,        // 70%
    pub min_soc_target: f32,          // 50%
    pub min_hours_to_solar: u32,      // 4
    pub solar_window_start_hour: u32, // 10
    pub solar_window_end_hour: u32,   // 14
    /// Cache of selected discharge block timestamps (global awareness)
    selected_discharge_blocks:
        std::sync::Arc<std::sync::RwLock<HashSet<chrono::DateTime<chrono::Utc>>>>,
}

impl WinterPeakDischargeStrategy {
    #[must_use]
    pub fn new(
        enabled: bool,
        min_spread_czk: f32,
        min_soc_to_start: f32,
        min_soc_target: f32,
        min_hours_to_solar: u32,
        solar_window_start_hour: u32,
        solar_window_end_hour: u32,
    ) -> Self {
        Self {
            enabled,
            min_spread_czk,
            min_soc_to_start,
            min_soc_target,
            min_hours_to_solar,
            solar_window_start_hour,
            solar_window_end_hour,
            selected_discharge_blocks: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
        }
    }

    /// Pre-select the highest-priced blocks for discharge (global planning)
    /// This should be called before the main optimization loop
    pub fn plan_discharge_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        initial_soc: f32,
        control_config: &crate::resources::ControlConfig,
    ) {
        if !self.enabled {
            return;
        }

        // Clear previous selection
        if let Ok(mut selected) = self.selected_discharge_blocks.write() {
            selected.clear();
        } else {
            return;
        }

        // Find cheapest price for spread calculation
        let cheapest = all_blocks
            .iter()
            .map(|b| b.price_czk_per_kwh)
            .fold(f32::INFINITY, |acc, p| acc.min(p));

        if !cheapest.is_finite() {
            return;
        }

        // Calculate safe discharge target
        let reserve_kwh = control_config.average_household_load_kw * 4.0;
        let reserve_pct =
            (reserve_kwh / control_config.battery_capacity_kwh * 100.0).clamp(0.0, 100.0);
        let safe_target_soc = self.min_soc_target.max(reserve_pct);

        // Check if we have enough SOC to discharge
        if initial_soc <= safe_target_soc || initial_soc < self.min_soc_to_start {
            return;
        }

        // Calculate total energy available for discharge
        let total_discharge_kwh =
            control_config.battery_capacity_kwh * (initial_soc - safe_target_soc) / 100.0;
        let discharge_rate_per_block = control_config.max_battery_charge_rate_kw * 0.25; // 15 min block
        let blocks_needed = (total_discharge_kwh / discharge_rate_per_block).ceil() as usize;

        // Collect eligible blocks (passing safety checks)
        let mut eligible_blocks: Vec<(chrono::DateTime<chrono::Utc>, f32)> = all_blocks
            .iter()
            .filter(|block| {
                let hour = block.block_start.hour();
                let spread = block.price_czk_per_kwh - cheapest;

                // Safety: avoid solar window
                let in_solar_window = hour >= self.solar_window_start_hour.saturating_sub(2)
                    && hour < self.solar_window_end_hour;

                // Safety: enough time to solar
                let hours_to_solar = if hour < self.solar_window_start_hour {
                    self.solar_window_start_hour - hour
                } else {
                    24 - hour + self.solar_window_start_hour
                };

                let safe_timing = hours_to_solar >= self.min_hours_to_solar;

                // Economic: sufficient spread
                let profitable = spread >= self.min_spread_czk;

                !in_solar_window && safe_timing && profitable
            })
            .map(|block| (block.block_start, block.price_czk_per_kwh))
            .collect();

        // Sort by price (descending) - we want the HIGHEST priced blocks
        eligible_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Select top N highest-priced blocks
        let selected_blocks: HashSet<chrono::DateTime<chrono::Utc>> = eligible_blocks
            .into_iter()
            .take(blocks_needed)
            .map(|(timestamp, _)| timestamp)
            .collect();

        tracing::info!(
            "Winter-Peak-Discharge: Selected {} highest-priced blocks for discharge (need {:.2} kWh)",
            selected_blocks.len(),
            total_discharge_kwh
        );

        // Store selection
        if let Ok(mut selected) = self.selected_discharge_blocks.write() {
            *selected = selected_blocks;
        }
    }
}

impl Default for WinterPeakDischargeStrategy {
    fn default() -> Self {
        Self::new(true, 3.0, 70.0, 50.0, 4, 10, 14)
    }
}

impl EconomicStrategy for WinterPeakDischargeStrategy {
    fn name(&self) -> &str {
        "Winter-Peak-Discharge"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let mut eval = BlockEvaluation::new(
            context.price_block.block_start,
            context.price_block.duration_minutes,
            InverterOperationMode::ForceDischarge,
            self.name().to_string(),
        );

        // Fill assumptions
        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        // GLOBAL BLOCK SELECTION: Check if this block was pre-selected for discharge
        let is_selected = if let Ok(selected) = self.selected_discharge_blocks.read() {
            selected.contains(&context.price_block.block_start)
        } else {
            false
        };

        if !is_selected {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = "Not in selected discharge blocks (globally optimized)".to_string();
            return eval;
        }

        // Safety: SOC high enough to start
        if context.current_battery_soc < self.min_soc_to_start {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "SOC {soc}% below start threshold {min}%",
                soc = context.current_battery_soc,
                min = self.min_soc_to_start
            );
            return eval;
        }

        let current_hour = context.price_block.block_start.hour();

        // Safety: avoid discharging close to or inside solar window (1-2h before and during window)
        if current_hour >= self.solar_window_start_hour.saturating_sub(2)
            && current_hour < self.solar_window_end_hour
        {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "Near solar window ({start}-{end}h), skipping discharge",
                start = self.solar_window_start_hour,
                end = self.solar_window_end_hour
            );
            return eval;
        }

        // Safety: ensure enough time for solar recharge or strong forecast
        let hours_to_solar = if current_hour < self.solar_window_start_hour {
            self.solar_window_start_hour - current_hour
        } else {
            24 - current_hour + self.solar_window_start_hour
        };

        if hours_to_solar < self.min_hours_to_solar && context.solar_forecast_kwh < 3.0 {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "Only {hours_to_solar}h to solar and low forecast {forecast} kWh",
                hours_to_solar = hours_to_solar,
                forecast = context.solar_forecast_kwh
            );
            return eval;
        }

        // Economic: need all prices to compute cheapest
        let Some(all_blocks) = context.all_price_blocks else {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = "Missing all_price_blocks for spread calculation".to_string();
            return eval;
        };

        let cheapest = all_blocks
            .iter()
            .map(|b| b.price_czk_per_kwh)
            .fold(f32::INFINITY, |acc, p| acc.min(p));

        if !cheapest.is_finite() {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = "Invalid cheapest price".to_string();
            return eval;
        }

        let current_price = context.price_block.price_czk_per_kwh;
        let spread = current_price - cheapest;
        if spread < self.min_spread_czk {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "Spread {spread:.2} CZK < min {min:.2} CZK",
                spread = spread,
                min = self.min_spread_czk
            );
            return eval;
        }

        // Compute safe discharge target: keep at least min_soc_target and 4h reserve
        let reserve_kwh = context.control_config.average_household_load_kw * 4.0; // 4 hours reserve
        let reserve_pct =
            (reserve_kwh / context.control_config.battery_capacity_kwh * 100.0).clamp(0.0, 100.0);
        let safe_target_soc = self.min_soc_target.max(reserve_pct);

        if context.current_battery_soc <= safe_target_soc {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!(
                "Current SOC {soc}% <= safe target {target}%",
                soc = context.current_battery_soc,
                target = safe_target_soc
            );
            return eval;
        }

        // Available energy to discharge down to safe target
        let available_kwh = context.control_config.battery_capacity_kwh
            * (context.current_battery_soc - safe_target_soc)
            / 100.0;

        // Calculate per-block discharge based on actual discharge rate (assume same as charge rate)
        let max_discharge_this_block = context.control_config.max_battery_charge_rate_kw * 0.25;
        let discharge_kwh = available_kwh.min(max_discharge_this_block);

        // Economics
        let efficiency = context.control_config.battery_efficiency;
        let wear_cost = economics::battery_degradation_cost(
            discharge_kwh,
            context.control_config.battery_wear_cost_czk_per_kwh,
        );
        let historical_charge_cost = economics::grid_import_cost(discharge_kwh, cheapest);
        let revenue = economics::grid_export_revenue(
            discharge_kwh * efficiency,
            context.grid_export_price_czk_per_kwh,
        );

        eval.cost_czk = wear_cost + historical_charge_cost;
        eval.revenue_czk = revenue;
        eval.calculate_net_profit();

        if eval.net_profit_czk <= 0.0 {
            eval.net_profit_czk = f32::NEG_INFINITY;
            eval.reason = format!("Not profitable after costs at price {current_price:.2} CZK/kWh");
            return eval;
        }

        eval.reason = format!(
            "Winter peak discharge: price {current_price:.2} CZK/kWh (spread {spread:.2}), target ≥ {safe_target_soc:.0}% ({hours_to_solar}h to solar)"
        );
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
    fn test_low_spread_skips() {
        let s = WinterPeakDischargeStrategy::default();
        let config = cfg();
        let now = Utc::now();
        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 4.0,
        };
        let all = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
            },
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 3.5,
            },
        ];
        let ctx = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 100.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 4.0,
            all_price_blocks: Some(&all),
        };
        let eval = s.evaluate(&ctx);
        assert_eq!(eval.net_profit_czk, f32::NEG_INFINITY);
    }

    #[test]
    fn test_low_soc_skips() {
        let s = WinterPeakDischargeStrategy::default();
        let config = cfg();
        let now = Utc::now();
        let price_block = TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 9.5,
        };
        let all = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 2.4,
        }];
        let ctx = EvaluationContext {
            price_block: &price_block,
            control_config: &config,
            current_battery_soc: 60.0, // below 70
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 9.5,
            all_price_blocks: Some(&all),
        };
        let eval = s.evaluate(&ctx);
        assert_eq!(eval.net_profit_czk, f32::NEG_INFINITY);
    }
}
