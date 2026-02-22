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

//! Winter Adaptive Strategy V4 - Global Price Optimization
//!
//! A pure price-optimized battery strategy that maximizes savings by:
//! - Charging during the **globally cheapest** blocks (not just "cheapest from now")
//! - Discharging during the **most expensive** blocks
//! - No "urgency charging" - SOC level doesn't affect WHEN to charge, only HOW MUCH
//!
//! ## Key Differences from V3
//!
//! | Aspect | V3 (Broken) | V4 (Fixed) |
//! |--------|-------------|------------|
//! | Block selection | Forward-looking only | Global across all blocks |
//! | SOC urgency | Charges early if SOC low | Pure price optimization |
//! | Discharge | SOC >= 50% required | No SOC requirement |
//!
//! ## Algorithm
//!
//! 1. Receive pre-calculated effective prices (spot + HDO grid fee) for ALL blocks from scheduler
//! 2. Sort ALL blocks by effective price (cheapest first)
//! 3. Select N cheapest blocks globally for charging
//! 4. Sort ALL blocks by effective price (most expensive first)
//! 5. Select M most expensive blocks for discharge
//! 6. Current block decision: charge if in charge set, discharge if in discharge set, else hold
//!
//! **Note:** Effective price calculation is now centralized in the scheduler. This strategy
//! uses pre-calculated effective prices from TimeBlockPrice.effective_price_czk_per_kwh
//!
//! ## Example
//!
//! Given blocks with prices: [3.73, 3.55, 3.30, 2.96, 2.31, 2.59, 2.73]
//! - V3 would charge at 3.73, 3.55, 3.30 (first "cheap enough" blocks)
//! - V4 charges at 2.31, 2.59, 2.73 (globally cheapest blocks)

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::{DateTime, Utc};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for Winter Adaptive V4 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV4Config {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Strategy priority (higher = more important)
    pub priority: u8,

    /// Target battery SOC for charging (default: 100%)
    pub target_battery_soc: f32,

    /// Number of top expensive blocks per day for discharge (default: 4)
    pub discharge_blocks_per_day: usize,

    /// Minimum price spread for discharge to be worthwhile (CZK)
    /// Discharge only if: discharge_price - avg_charge_price > spread
    pub min_discharge_spread_czk: f32,

    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,

    /// Planning horizon in hours (default: 36)
    /// How far ahead to look when selecting optimal blocks
    pub planning_horizon_hours: usize,
}

impl Default for WinterAdaptiveV4Config {
    fn default() -> Self {
        Self {
            enabled: true, // V4 is the recommended strategy
            priority: 90,  // Highest priority among winter strategies
            target_battery_soc: 100.0,
            discharge_blocks_per_day: 4,
            min_discharge_spread_czk: 0.50,
            negative_price_handling_enabled: true,
            planning_horizon_hours: 36,
        }
    }
}

// ============================================================================
// Block with effective price for sorting
// ============================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields kept for debugging
struct RankedBlock {
    index: usize,
    block_start: DateTime<Utc>,
    spot_price: f32,
    effective_price: f32,
    is_low_tariff: bool,
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug)]
pub struct WinterAdaptiveV4Strategy {
    config: WinterAdaptiveV4Config,
}

impl WinterAdaptiveV4Strategy {
    pub fn new(config: WinterAdaptiveV4Config) -> Self {
        Self { config }
    }

    /// Rank all blocks by effective price (pre-calculated in scheduler)
    fn rank_all_blocks(&self, all_blocks: &[TimeBlockPrice]) -> Vec<RankedBlock> {
        all_blocks
            .iter()
            .enumerate()
            .map(|(idx, block)| {
                RankedBlock {
                    index: idx,
                    block_start: block.block_start,
                    spot_price: block.price_czk_per_kwh,
                    effective_price: block.effective_price_czk_per_kwh,
                    is_low_tariff: false, // Not needed anymore since we use pre-calculated prices
                }
            })
            .collect()
    }

    /// Select the N globally cheapest blocks for charging
    /// Respects min_consecutive_force_blocks by selecting consecutive groups
    fn select_charge_blocks(
        &self,
        ranked_blocks: &[RankedBlock],
        blocks_needed: usize,
        min_consecutive: usize,
    ) -> HashSet<usize> {
        if ranked_blocks.is_empty() || blocks_needed == 0 {
            return HashSet::new();
        }

        // Sort by effective price (cheapest first)
        let mut sorted: Vec<&RankedBlock> = ranked_blocks.iter().collect();
        sorted.sort_by(|a, b| {
            a.effective_price
                .partial_cmp(&b.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Build consecutive groups ("super blocks") and rank them
        // A super block is a window of min_consecutive blocks
        let mut super_blocks: Vec<(f32, Vec<usize>)> = Vec::new();

        // We need to find consecutive windows in the ORIGINAL block order
        // So sort by index to find consecutive sequences
        let mut by_index: Vec<&RankedBlock> = ranked_blocks.iter().collect();
        by_index.sort_by_key(|b| b.index);

        // Only search for super-blocks if we have enough blocks
        if by_index.len() >= min_consecutive {
            for i in 0..=(by_index.len() - min_consecutive) {
                let window: Vec<&RankedBlock> = by_index[i..i + min_consecutive].to_vec();

                // Check if indices are consecutive
                let indices: Vec<usize> = window.iter().map(|b| b.index).collect();
                let is_consecutive = indices.windows(2).all(|w| w[1] == w[0] + 1);

                if is_consecutive {
                    let avg_price: f32 = window.iter().map(|b| b.effective_price).sum::<f32>()
                        / min_consecutive as f32;
                    super_blocks.push((avg_price, indices));
                }
            }
        }

        // Sort super blocks by average price (cheapest first)
        super_blocks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Select cheapest super blocks until we have enough
        let mut selected: HashSet<usize> = HashSet::new();

        for (avg_price, indices) in &super_blocks {
            if selected.len() >= blocks_needed {
                break;
            }

            // Check if any block in this window overlaps with already selected
            let overlaps = indices.iter().any(|idx| selected.contains(idx));

            if !overlaps {
                for &idx in indices {
                    if selected.len() < blocks_needed {
                        selected.insert(idx);
                    }
                }

                tracing::debug!(
                    "V4: Selected charge super-block avg {:.3} CZK, indices {:?}",
                    avg_price,
                    indices
                );
            }
        }

        // If we still need more blocks (can happen if blocks_needed > available super-blocks)
        // Fall back to selecting individual cheapest blocks
        if selected.len() < blocks_needed {
            for block in &sorted {
                if selected.len() >= blocks_needed {
                    break;
                }
                if !selected.contains(&block.index) {
                    selected.insert(block.index);
                    tracing::debug!(
                        "V4: Added individual charge block idx {} at {:.3} CZK",
                        block.index,
                        block.effective_price
                    );
                }
            }
        }

        tracing::info!(
            "V4: Selected {} charge blocks (needed {})",
            selected.len(),
            blocks_needed
        );

        selected
    }

    /// Select the N globally most expensive blocks for discharge
    fn select_discharge_blocks(
        &self,
        ranked_blocks: &[RankedBlock],
        charge_block_indices: &HashSet<usize>,
    ) -> HashSet<usize> {
        if ranked_blocks.is_empty() {
            return HashSet::new();
        }

        // Calculate average charge price for spread comparison
        let charge_prices: Vec<f32> = ranked_blocks
            .iter()
            .filter(|b| charge_block_indices.contains(&b.index))
            .map(|b| b.effective_price)
            .collect();

        let avg_charge_price = if charge_prices.is_empty() {
            0.0
        } else {
            charge_prices.iter().sum::<f32>() / charge_prices.len() as f32
        };

        // Sort by effective price (most expensive first)
        let mut sorted: Vec<&RankedBlock> = ranked_blocks.iter().collect();
        sorted.sort_by(|a, b| {
            b.effective_price
                .partial_cmp(&a.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Select top N expensive blocks that meet the spread requirement
        let mut selected: HashSet<usize> = HashSet::new();

        for block in sorted {
            if selected.len() >= self.config.discharge_blocks_per_day {
                break;
            }

            // Skip if this is also a charge block
            if charge_block_indices.contains(&block.index) {
                continue;
            }

            // Check spread requirement
            let spread = block.effective_price - avg_charge_price;
            if spread >= self.config.min_discharge_spread_czk {
                selected.insert(block.index);
                tracing::debug!(
                    "V4: Selected discharge block idx {} at {:.3} CZK (spread {:.3})",
                    block.index,
                    block.effective_price,
                    spread
                );
            }
        }

        tracing::info!(
            "V4: Selected {} discharge blocks (avg charge price {:.3}, min spread {:.3})",
            selected.len(),
            avg_charge_price,
            self.config.min_discharge_spread_czk
        );

        selected
    }

    /// Main decision logic
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
    ) -> (InverterOperationMode, String, String) {
        let current_price = context.price_block.price_czk_per_kwh;
        let effective_price = context.price_block.effective_price_czk_per_kwh;

        // =====================================================================
        // PRIORITY 1: Negative prices - always charge (free/paid energy!)
        // =====================================================================
        if self.config.negative_price_handling_enabled && current_price < 0.0 {
            if context.current_battery_soc < 100.0 {
                return (
                    InverterOperationMode::ForceCharge,
                    format!("Negative price: {:.3} CZK/kWh - FREE ENERGY", current_price),
                    "winter_adaptive_v4:negative_price_charge".to_string(),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price {:.3} CZK/kWh, battery full", current_price),
                "winter_adaptive_v4:negative_price_full".to_string(),
            );
        }

        // =====================================================================
        // STEP 1: Rank ALL blocks by effective price (GLOBAL view)
        // =====================================================================
        let ranked_blocks = self.rank_all_blocks(all_blocks);

        // =====================================================================
        // STEP 2: Calculate how many charge blocks we need
        // =====================================================================
        let battery_capacity_kwh = context.control_config.battery_capacity_kwh;
        let charge_per_block_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
        let target_soc = self.config.target_battery_soc;
        let soc_deficit = (target_soc - context.current_battery_soc).max(0.0);
        let energy_needed_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;
        let blocks_for_soc = (energy_needed_kwh / charge_per_block_kwh).ceil() as usize;

        // Use configured minimum or calculated need, whichever is larger
        let config_charge_blocks = context.control_config.force_charge_hours * 4;
        let blocks_needed = config_charge_blocks.max(blocks_for_soc);

        tracing::debug!(
            "V4: SOC {:.1}% -> {:.1}% target, energy needed {:.2} kWh, blocks needed {}",
            context.current_battery_soc,
            target_soc,
            energy_needed_kwh,
            blocks_needed
        );

        // =====================================================================
        // STEP 3: Select GLOBALLY cheapest blocks for charging
        // =====================================================================
        let min_consecutive = context.control_config.min_consecutive_force_blocks;
        let charge_blocks =
            self.select_charge_blocks(&ranked_blocks, blocks_needed, min_consecutive);

        // =====================================================================
        // STEP 4: Select GLOBALLY most expensive blocks for discharge
        // =====================================================================
        let discharge_blocks = self.select_discharge_blocks(&ranked_blocks, &charge_blocks);

        // =====================================================================
        // STEP 5: Decide based on whether current block is in charge/discharge set
        // =====================================================================
        let should_charge = charge_blocks.contains(&current_block_index);
        let should_discharge = discharge_blocks.contains(&current_block_index);

        // Log the schedule for debugging
        if tracing::enabled!(tracing::Level::DEBUG) {
            let charge_prices: Vec<f32> = ranked_blocks
                .iter()
                .filter(|b| charge_blocks.contains(&b.index))
                .map(|b| b.effective_price)
                .collect();
            let discharge_prices: Vec<f32> = ranked_blocks
                .iter()
                .filter(|b| discharge_blocks.contains(&b.index))
                .map(|b| b.effective_price)
                .collect();

            tracing::debug!(
                "V4: Current block {} - charge: {}, discharge: {}, eff price {:.3}",
                current_block_index,
                should_charge,
                should_discharge,
                effective_price
            );
            tracing::debug!(
                "V4: Charge block prices: {:?}, Discharge block prices: {:?}",
                charge_prices,
                discharge_prices
            );
        }

        // =====================================================================
        // Calculate average charge price for spread comparisons
        // =====================================================================
        let charge_prices: Vec<f32> = ranked_blocks
            .iter()
            .filter(|b| charge_blocks.contains(&b.index))
            .map(|b| b.effective_price)
            .collect();
        let avg_charge_price = if charge_prices.is_empty() {
            effective_price // Fallback to current price
        } else {
            charge_prices.iter().sum::<f32>() / charge_prices.len() as f32
        };

        // =====================================================================
        // DECISION: Charge > Discharge > Freeze SOC > Self-Use
        // =====================================================================

        // PRIORITY 1: Charge if in optimal charge window AND battery not full
        if should_charge && context.current_battery_soc < target_soc {
            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "Global-optimal charge: effective price {:.3} CZK/kWh (spot {:.3} + grid fee)",
                    effective_price, current_price
                ),
                "winter_adaptive_v4:optimal_charge".to_string(),
            );
        }

        // PRIORITY 2: Allow discharge during peak price windows
        if should_discharge {
            let spread = effective_price - avg_charge_price;
            return (
                InverterOperationMode::SelfUse, // SelfUse allows natural discharge
                format!(
                    "Peak discharge: {:.3} CZK/kWh (spread +{:.3} vs charge avg {:.3})",
                    effective_price, spread, avg_charge_price
                ),
                "winter_adaptive_v4:peak_discharge".to_string(),
            );
        }

        // PRIORITY 3: Freeze SOC (BackUpMode) when battery is near-full AND price is cheap
        // This prevents unnecessary battery cycling - use grid directly instead
        // BackUpMode = Manual Mode + Stop Charge and Discharge on Solax
        let battery_near_full = context.current_battery_soc >= (target_soc - 5.0);
        let price_is_cheap = effective_price <= avg_charge_price * 1.1; // Within 10% of charge price

        if battery_near_full && price_is_cheap {
            return (
                InverterOperationMode::NoChargeNoDischarge,
                format!(
                    "Freeze SOC: battery {:.1}% near full, cheap price {:.3} CZK/kWh - use grid directly",
                    context.current_battery_soc, effective_price
                ),
                "winter_adaptive_v4:freeze_soc".to_string(),
            );
        }

        // PRIORITY 4: Self-Use during medium prices (battery covers load naturally)
        // The battery will discharge to cover household load but won't force-discharge
        (
            InverterOperationMode::SelfUse,
            format!(
                "Self-use: {:.3} CZK/kWh - battery covers load, saves for peaks",
                effective_price
            ),
            "winter_adaptive_v4:self_use".to_string(),
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV4Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V4"
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&self, context: &EvaluationContext) -> BlockEvaluation {
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

        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available".to_string();
            return eval;
        };

        let current_block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let (mode, reason, decision_uid) =
            self.decide_mode(context, all_blocks, current_block_index);

        eval.mode = mode;
        eval.reason = reason;
        eval.decision_uid = Some(decision_uid);

        // Calculate energy flows based on mode (using pre-calculated effective price)
        let effective_price = context.price_block.effective_price_czk_per_kwh;

        match mode {
            InverterOperationMode::ForceCharge => {
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk = economics::grid_import_cost(charge_kwh, effective_price);
            }
            InverterOperationMode::ForceDischarge => {
                let discharge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                eval.energy_flows.grid_export_kwh = discharge_kwh;
                eval.revenue_czk = economics::grid_export_revenue(
                    discharge_kwh,
                    context.grid_export_price_czk_per_kwh,
                );
            }
            InverterOperationMode::SelfUse
            | InverterOperationMode::BackUpMode
            | InverterOperationMode::NoChargeNoDischarge => {
                let usable_battery_kwh = ((context.current_battery_soc
                    - context.control_config.hardware_min_battery_soc)
                    .max(0.0)
                    / 100.0)
                    * context.control_config.battery_capacity_kwh;

                // Calculate how much battery will discharge to cover load
                let battery_discharge = usable_battery_kwh.min(context.consumption_forecast_kwh);

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                if battery_discharge >= context.consumption_forecast_kwh {
                    // Battery fully covers load
                    eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                } else {
                    // Battery partially covers load, rest from grid
                    eval.revenue_czk = battery_discharge * effective_price;
                    eval.cost_czk =
                        (context.consumption_forecast_kwh - battery_discharge) * effective_price;
                    eval.energy_flows.grid_import_kwh =
                        context.consumption_forecast_kwh - battery_discharge;
                }
            }
        }

        eval.calculate_net_profit();
        eval
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn create_test_blocks() -> Vec<TimeBlockPrice> {
        // Simulate the problematic scenario from the export:
        // Earlier blocks (19:00-20:45): expensive 3.73, 3.55, 3.30, etc.
        // Later blocks (22:00-23:45): cheap 2.59, 2.31, etc.
        let base_time = Utc.with_ymd_and_hms(2026, 1, 16, 18, 0, 0).unwrap();

        // Using high tariff grid fee of 1.80 CZK/kWh for test consistency
        let grid_fee = 1.80;

        vec![
            // 19:00 - expensive
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(60),
                duration_minutes: 15,
                price_czk_per_kwh: 3.73,
                effective_price_czk_per_kwh: 3.73 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 19:15 - expensive
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(75),
                duration_minutes: 15,
                price_czk_per_kwh: 3.55,
                effective_price_czk_per_kwh: 3.55 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 19:30 - expensive
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(90),
                duration_minutes: 15,
                price_czk_per_kwh: 3.30,
                effective_price_czk_per_kwh: 3.30 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 19:45 - medium
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(105),
                duration_minutes: 15,
                price_czk_per_kwh: 2.96,
                effective_price_czk_per_kwh: 2.96 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 20:00 - medium
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(120),
                duration_minutes: 15,
                price_czk_per_kwh: 3.17,
                effective_price_czk_per_kwh: 3.17 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 22:00 - cheap
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(240),
                duration_minutes: 15,
                price_czk_per_kwh: 2.59,
                effective_price_czk_per_kwh: 2.59 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 22:15 - cheapest!
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(255),
                duration_minutes: 15,
                price_czk_per_kwh: 2.31,
                effective_price_czk_per_kwh: 2.31 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
            // 22:30 - cheap
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(270),
                duration_minutes: 15,
                price_czk_per_kwh: 2.45,
                effective_price_czk_per_kwh: 2.45 + grid_fee,
                spot_sell_price_czk_per_kwh: None,
            },
        ]
    }

    #[test]
    fn test_global_block_ranking() {
        let config = WinterAdaptiveV4Config {
            enabled: true,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV4Strategy::new(config);
        let blocks = create_test_blocks();

        let ranked = strategy.rank_all_blocks(&blocks);

        // All blocks should be ranked
        assert_eq!(ranked.len(), blocks.len());

        // Find the cheapest block - should be the 2.31 one (index 6)
        let cheapest = ranked
            .iter()
            .min_by(|a, b| a.effective_price.partial_cmp(&b.effective_price).unwrap());

        assert!(cheapest.is_some());
        let cheapest = cheapest.unwrap();
        assert_eq!(cheapest.spot_price, 2.31);
        assert_eq!(cheapest.index, 6);
    }

    #[test]
    fn test_charge_block_selection_picks_cheapest() {
        let config = WinterAdaptiveV4Config {
            enabled: true,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV4Strategy::new(config);
        let blocks = create_test_blocks();
        let ranked = strategy.rank_all_blocks(&blocks);

        // Select 3 blocks with min_consecutive = 1 (no grouping requirement)
        let selected = strategy.select_charge_blocks(&ranked, 3, 1);

        assert_eq!(selected.len(), 3);

        // The selected blocks should be the cheapest ones (indices 5, 6, 7)
        // which have prices 2.59, 2.31, 2.45
        assert!(
            selected.contains(&5) || selected.contains(&6) || selected.contains(&7),
            "Should select from cheap blocks, got {:?}",
            selected
        );

        // Should NOT contain the expensive blocks (0, 1, 2 with prices 3.73, 3.55, 3.30)
        assert!(
            !selected.contains(&0),
            "Should not select expensive block 0 (3.73 CZK)"
        );
        assert!(
            !selected.contains(&1),
            "Should not select expensive block 1 (3.55 CZK)"
        );
    }

    #[test]
    fn test_discharge_block_selection_picks_expensive() {
        let config = WinterAdaptiveV4Config {
            enabled: true,
            discharge_blocks_per_day: 2,
            min_discharge_spread_czk: 0.50,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV4Strategy::new(config);
        let blocks = create_test_blocks();
        let ranked = strategy.rank_all_blocks(&blocks);

        // First select charge blocks
        let charge_blocks = strategy.select_charge_blocks(&ranked, 3, 1);

        // Then select discharge blocks
        let discharge_blocks = strategy.select_discharge_blocks(&ranked, &charge_blocks);

        // Discharge blocks should be the expensive ones (0, 1 with 3.73, 3.55)
        // and should NOT overlap with charge blocks
        for idx in &discharge_blocks {
            assert!(
                !charge_blocks.contains(idx),
                "Discharge block {} should not be in charge set",
                idx
            );
        }

        // Should contain expensive blocks
        let has_expensive = discharge_blocks.contains(&0) || discharge_blocks.contains(&1);
        assert!(
            has_expensive,
            "Should select expensive blocks for discharge, got {:?}",
            discharge_blocks
        );
    }

    #[test]
    fn test_v4_selects_later_cheap_blocks_not_early_expensive() {
        // This test verifies the fix for the V3 bug where it charged at 3.73 CZK
        // instead of waiting for 2.31 CZK blocks

        let config = WinterAdaptiveV4Config {
            enabled: true,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV4Strategy::new(config);
        let blocks = create_test_blocks();
        let ranked = strategy.rank_all_blocks(&blocks);

        // Need 4 blocks
        let selected = strategy.select_charge_blocks(&ranked, 4, 1);

        // Calculate average price of selected blocks
        let avg_selected_price: f32 = ranked
            .iter()
            .filter(|b| selected.contains(&b.index))
            .map(|b| b.effective_price)
            .sum::<f32>()
            / selected.len() as f32;

        // V3 would select early blocks with avg ~5.0 CZK (3.73+1.80, 3.55+1.80, etc.)
        // V4 should select cheap blocks with avg ~4.1 CZK (2.31+1.80, 2.45+1.80, 2.59+1.80, 2.96+1.80)
        assert!(
            avg_selected_price < 4.8,
            "V4 should select cheaper blocks, avg {:.3} CZK is too high",
            avg_selected_price
        );

        println!(
            "V4 selected blocks: {:?} with avg price {:.3} CZK",
            selected, avg_selected_price
        );
    }
}
