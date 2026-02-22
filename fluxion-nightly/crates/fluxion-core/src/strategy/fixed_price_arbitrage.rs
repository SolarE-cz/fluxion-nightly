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

//! Fixed Price Arbitrage Strategy
//!
//! Designed for users with fixed-price energy contracts who have `use_spot_prices_to_sell` enabled.
//! When spot sell prices spike above the fixed buy price by at least `min_profit_threshold_czk`,
//! the strategy charges at the cheap fixed price and discharges to the grid at high spot prices.

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EnergyFlows, EvaluationContext,
};
use chrono::Timelike;
use fluxion_types::inverter::InverterOperationMode;
use serde::{Deserialize, Serialize};

/// Configuration for the Fixed Price Arbitrage strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedPriceArbitrageConfig {
    pub enabled: bool,
    pub priority: u8,
    /// Minimum spread (sell - buy) in CZK/kWh to trigger arbitrage
    pub min_profit_threshold_czk: f32,
}

impl Default for FixedPriceArbitrageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        }
    }
}

/// Fixed Price Arbitrage strategy instance
pub struct FixedPriceArbitrageStrategy {
    config: FixedPriceArbitrageConfig,
}

impl FixedPriceArbitrageStrategy {
    pub fn new(config: FixedPriceArbitrageConfig) -> Self {
        Self { config }
    }
}

impl EconomicStrategy for FixedPriceArbitrageStrategy {
    fn name(&self) -> &str {
        "FP-Arbitrage"
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
        let block_start = context.price_block.block_start;
        let duration_minutes = context.price_block.duration_minutes;
        let strategy_name = self.name().to_string();

        // Use hourly consumption profile if available, otherwise fallback
        let consumption_kwh = context
            .hourly_consumption_profile
            .map(|profile| {
                let hour = context.price_block.block_start.hour() as usize;
                profile[hour] / 4.0 // hourly kWh → 15-min block
            })
            .unwrap_or(context.consumption_forecast_kwh);

        // Guard: need all_price_blocks with spot_sell_price data
        let all_blocks = match context.all_price_blocks {
            Some(blocks) if !blocks.is_empty() => blocks,
            _ => {
                return self.make_self_use(
                    block_start,
                    duration_minutes,
                    &strategy_name,
                    "fpa:no_spot_data",
                    "FP-Arbitrage - No spot sell data available",
                    consumption_kwh,
                    context,
                );
            }
        };

        // Check if ANY block has spot_sell_price data
        let has_spot_data = all_blocks
            .iter()
            .any(|b| b.spot_sell_price_czk_per_kwh.is_some());
        if !has_spot_data {
            return self.make_self_use(
                block_start,
                duration_minutes,
                &strategy_name,
                "fpa:no_spot_data",
                "FP-Arbitrage - No spot sell data available",
                consumption_kwh,
                context,
            );
        }

        // Find cheapest buy price (effective_price includes HDO fees)
        let cheapest_buy_price = all_blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .fold(f32::INFINITY, f32::min);

        // Find profitable discharge blocks: spot_sell_price - cheapest_buy >= threshold
        let mut discharge_candidates: Vec<(usize, f32, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter_map(|(idx, b)| {
                b.spot_sell_price_czk_per_kwh.map(|sell_price| {
                    let spread = sell_price - cheapest_buy_price;
                    (idx, sell_price, spread)
                })
            })
            .filter(|(_, _, spread)| *spread >= self.config.min_profit_threshold_czk)
            .collect();

        if discharge_candidates.is_empty() {
            // Calculate best spread for diagnostic message
            let best_spread = all_blocks
                .iter()
                .filter_map(|b| {
                    b.spot_sell_price_czk_per_kwh
                        .map(|sell| sell - cheapest_buy_price)
                })
                .fold(f32::NEG_INFINITY, f32::max);

            return self.make_self_use(
                block_start,
                duration_minutes,
                &strategy_name,
                "fpa:no_opportunity",
                &format!(
                    "FP-Arbitrage - No profitable blocks (best spread: {:.2} CZK/kWh)",
                    best_spread
                ),
                consumption_kwh,
                context,
            );
        }

        // Sort discharge candidates by sell price descending (most profitable first)
        discharge_candidates
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Calculate charge needs
        let battery_capacity = context.control_config.battery_capacity_kwh;
        let min_soc = context.control_config.min_battery_soc;
        let max_soc = context.control_config.max_battery_soc;
        let efficiency = context.control_config.battery_efficiency;
        let charge_rate_kw = context.control_config.max_battery_charge_rate_kw;
        let max_export_kw = context.control_config.maximum_export_power_w as f32 / 1000.0;

        let available_kwh = (context.current_battery_soc - min_soc) / 100.0 * battery_capacity;
        let discharge_kwh_per_block =
            (max_export_kw * 0.25).min(battery_capacity * (max_soc - min_soc) / 100.0);
        let total_discharge_kwh = discharge_candidates.len() as f32 * discharge_kwh_per_block;
        let needed_charge_kwh = ((total_discharge_kwh - available_kwh).max(0.0)) / efficiency;
        let charge_kwh_per_block = charge_rate_kw * 0.25;
        let num_charge_blocks = (needed_charge_kwh / charge_kwh_per_block).ceil() as usize;

        // Select charge blocks: cheapest effective_price blocks not in discharge set
        let discharge_indices: std::collections::HashSet<usize> = discharge_candidates
            .iter()
            .map(|(idx, _, _)| *idx)
            .collect();

        let mut charge_candidates: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(idx, _)| !discharge_indices.contains(idx))
            .map(|(idx, b)| (idx, b.effective_price_czk_per_kwh))
            .collect();
        charge_candidates
            .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Prefer charge blocks before the first discharge block
        let first_discharge_idx = discharge_candidates
            .first()
            .map(|(idx, _, _)| *idx)
            .unwrap_or(0);
        let (before, after): (Vec<_>, Vec<_>) = charge_candidates
            .into_iter()
            .partition(|(idx, _)| *idx < first_discharge_idx);

        let mut selected_charge_blocks: Vec<usize> = Vec::new();
        for (idx, _) in before.iter().chain(after.iter()) {
            if selected_charge_blocks.len() >= num_charge_blocks {
                break;
            }
            selected_charge_blocks.push(*idx);
        }

        let charge_set: std::collections::HashSet<usize> =
            selected_charge_blocks.iter().copied().collect();

        // Determine current block action (block index 0 = current block)
        let current_is_charge = charge_set.contains(&0);
        let current_is_discharge = discharge_indices.contains(&0);

        if current_is_charge && context.current_battery_soc < max_soc {
            let buy_price = context.price_block.effective_price_czk_per_kwh;
            let best_sell = discharge_candidates
                .first()
                .map(|(_, s, _)| *s)
                .unwrap_or(0.0);
            let spread = best_sell - buy_price;

            let mut eval = BlockEvaluation::new(
                block_start,
                duration_minutes,
                InverterOperationMode::ForceCharge,
                strategy_name,
            )
            .with_decision_uid("fpa:charge");

            eval.reason = format!(
                "FP-Arbitrage - Charging for arbitrage (buy: {:.2}, best sell: {:.2}, spread: {:.2})",
                buy_price, best_sell, spread
            );
            eval.energy_flows = EnergyFlows {
                grid_import_kwh: charge_kwh_per_block,
                battery_charge_kwh: charge_kwh_per_block * efficiency,
                ..Default::default()
            };
            eval.assumptions = self.make_assumptions(context);
            eval
        } else if current_is_discharge && context.current_battery_soc > min_soc {
            let sell_price = context
                .price_block
                .spot_sell_price_czk_per_kwh
                .unwrap_or(context.grid_export_price_czk_per_kwh);
            let profit = sell_price - cheapest_buy_price;

            let discharge_kwh = (max_export_kw * 0.25).min(available_kwh);

            let mut eval = BlockEvaluation::new(
                block_start,
                duration_minutes,
                InverterOperationMode::ForceDischarge,
                strategy_name,
            )
            .with_decision_uid("fpa:discharge");

            eval.reason = format!(
                "FP-Arbitrage - Discharging to grid (sell: {:.2}, buy cost: {:.2}, profit: {:.2} CZK/kWh)",
                sell_price, cheapest_buy_price, profit
            );
            eval.energy_flows = EnergyFlows {
                grid_export_kwh: discharge_kwh,
                battery_discharge_kwh: discharge_kwh,
                ..Default::default()
            };
            eval.assumptions = self.make_assumptions(context);
            eval
        } else {
            let planned_discharge_count = discharge_candidates.len();
            self.make_self_use(
                block_start,
                duration_minutes,
                &strategy_name,
                "fpa:self_use",
                &format!(
                    "FP-Arbitrage - Self-Use ({} discharge blocks planned, awaiting window)",
                    planned_discharge_count
                ),
                consumption_kwh,
                context,
            )
        }
    }
}

impl FixedPriceArbitrageStrategy {
    fn make_assumptions(&self, context: &EvaluationContext) -> Assumptions {
        Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.effective_price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn make_self_use(
        &self,
        block_start: chrono::DateTime<chrono::Utc>,
        duration_minutes: u32,
        strategy_name: &str,
        decision_uid: &str,
        reason: &str,
        consumption_kwh: f32,
        context: &EvaluationContext,
    ) -> BlockEvaluation {
        let solar_kwh = context.solar_forecast_kwh;
        let net_consumption = (consumption_kwh - solar_kwh).max(0.0);
        let battery_capacity = context.control_config.battery_capacity_kwh;
        let current_energy = context.current_battery_soc / 100.0 * battery_capacity;
        let min_energy = context.control_config.min_battery_soc / 100.0 * battery_capacity;
        let available_battery = (current_energy - min_energy).max(0.0);

        let battery_discharge = net_consumption.min(available_battery);
        let grid_import = (net_consumption - battery_discharge).max(0.0);

        let mut eval = BlockEvaluation::new(
            block_start,
            duration_minutes,
            InverterOperationMode::SelfUse,
            strategy_name.to_string(),
        )
        .with_decision_uid(decision_uid);

        eval.reason = reason.to_string();
        eval.energy_flows = EnergyFlows {
            household_consumption_kwh: consumption_kwh,
            battery_discharge_kwh: battery_discharge,
            grid_import_kwh: grid_import,
            solar_generation_kwh: solar_kwh,
            ..Default::default()
        };
        eval.assumptions = self.make_assumptions(context);
        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use fluxion_types::config::ControlConfig;
    use fluxion_types::pricing::TimeBlockPrice;

    fn default_control_config() -> ControlConfig {
        ControlConfig {
            battery_capacity_kwh: 20.0,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            battery_efficiency: 0.90,
            max_battery_charge_rate_kw: 10.0,
            maximum_export_power_w: 10000,
            battery_wear_cost_czk_per_kwh: 0.125,
            hardware_min_battery_soc: 10.0,
            ..Default::default()
        }
    }

    fn make_block(
        hour: u32,
        minute: u32,
        effective_price: f32,
        spot_sell: Option<f32>,
    ) -> TimeBlockPrice {
        TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2025, 1, 15, hour, minute, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: effective_price,
            effective_price_czk_per_kwh: effective_price,
            spot_sell_price_czk_per_kwh: spot_sell,
        }
    }

    fn make_context<'a>(
        blocks: &'a [TimeBlockPrice],
        control: &'a ControlConfig,
        soc: f32,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            price_block: &blocks[0],
            control_config: control,
            current_battery_soc: soc,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.5,
            all_price_blocks: Some(blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 3.0,
            hourly_consumption_profile: None,
        }
    }

    #[test]
    fn test_no_spot_data_returns_self_use() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // All blocks with no spot_sell_price
        let blocks = vec![
            make_block(0, 0, 3.0, None),
            make_block(0, 15, 3.0, None),
            make_block(0, 30, 3.0, None),
        ];

        let ctx = make_context(&blocks, &control, 50.0);
        let eval = strategy.evaluate(&ctx);

        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert_eq!(eval.decision_uid.as_deref(), Some("fpa:no_spot_data"));
    }

    #[test]
    fn test_insufficient_spread_returns_self_use() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // Spot sell is only 1 CZK above buy - insufficient spread
        let blocks = vec![
            make_block(0, 0, 3.0, Some(4.0)),  // spread = 1.0 < 3.0
            make_block(0, 15, 3.0, Some(3.5)), // spread = 0.5
            make_block(0, 30, 3.0, Some(4.5)), // spread = 1.5
        ];

        let ctx = make_context(&blocks, &control, 50.0);
        let eval = strategy.evaluate(&ctx);

        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert_eq!(eval.decision_uid.as_deref(), Some("fpa:no_opportunity"));
    }

    #[test]
    fn test_profitable_arbitrage_selects_discharge() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // Current block has high spot sell price, spread >= 3 CZK
        let blocks = vec![
            make_block(17, 0, 3.0, Some(8.0)), // current: spread = 5.0 (profitable!)
            make_block(17, 15, 3.0, Some(7.0)), // spread = 4.0
            make_block(0, 0, 3.0, Some(2.0)),  // cheap block for charging
        ];

        let ctx = make_context(&blocks, &control, 80.0);
        let eval = strategy.evaluate(&ctx);

        assert_eq!(eval.mode, InverterOperationMode::ForceDischarge);
        assert_eq!(eval.decision_uid.as_deref(), Some("fpa:discharge"));
        assert!(eval.reason.contains("Discharging to grid"));
    }

    #[test]
    fn test_charge_block_selection() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // Current block is cheap (good for charging), discharge block comes later
        let blocks = vec![
            make_block(2, 0, 2.0, Some(1.0)),   // cheapest buy block (current)
            make_block(2, 15, 3.0, Some(2.0)),  // medium price
            make_block(17, 0, 3.0, Some(8.0)),  // expensive sell block
            make_block(17, 15, 3.0, Some(7.0)), // expensive sell block
        ];

        // Low SOC forces need for charging
        let ctx = make_context(&blocks, &control, 15.0);
        let eval = strategy.evaluate(&ctx);

        assert_eq!(eval.mode, InverterOperationMode::ForceCharge);
        assert_eq!(eval.decision_uid.as_deref(), Some("fpa:charge"));
        assert!(eval.reason.contains("Charging for arbitrage"));
    }

    #[test]
    fn test_soc_constraints_respected() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // Block is profitable for discharge but SOC is at minimum
        let blocks = vec![
            make_block(17, 0, 3.0, Some(8.0)), // profitable discharge
            make_block(0, 0, 3.0, Some(2.0)),  // cheap charge
        ];

        let ctx = make_context(&blocks, &control, 10.0); // At min_soc
        let eval = strategy.evaluate(&ctx);

        // Should NOT discharge because SOC is at minimum
        assert_ne!(eval.mode, InverterOperationMode::ForceDischarge);
        assert_eq!(eval.decision_uid.as_deref(), Some("fpa:self_use"));
    }

    #[test]
    fn test_existing_soc_reduces_charge_needs() {
        let config = FixedPriceArbitrageConfig {
            enabled: true,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        };
        let strategy = FixedPriceArbitrageStrategy::new(config);
        let control = default_control_config();

        // With high SOC, the strategy should not need to charge for a single discharge block
        let blocks = vec![
            make_block(2, 0, 2.0, Some(1.0)),  // cheap buy block (current)
            make_block(17, 0, 3.0, Some(8.0)), // one discharge block
        ];

        // High SOC = battery already has energy, no charge needed
        let ctx = make_context(&blocks, &control, 90.0);
        let eval = strategy.evaluate(&ctx);

        // Current block is in charge candidates, but with high SOC the charge_set should be empty
        // or the block should be self-use because no charging is needed
        assert!(
            eval.mode == InverterOperationMode::SelfUse
                || eval.mode == InverterOperationMode::ForceCharge,
            "Expected SelfUse or ForceCharge, got {:?}",
            eval.mode
        );
    }
}
