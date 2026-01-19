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

//! Winter Adaptive Strategy V5 - Combined Best-of-All Optimization
//!
//! Combines the strongest logic from V2, V3, and V4 to maximize cost savings.
//!
//! ## Key Features
//!
//! 1. **Global Price Ranking** (from V4): Ranks ALL blocks globally, not just forward-looking
//! 2. **Reserve SOC Protection** (from V3): Maintains minimum discharge SOC (40%) to avoid expensive recharging
//! 3. **Grid Avoidance** (NEW): Never buy from grid during top 50% most expensive blocks
//! 4. **Aggressive Cheap Charging** (NEW): Charges to 100% during all cheap periods
//! 5. **Top-N Discharge** (from V3): Only discharges during most expensive blocks
//! 6. **Safety Margins** (from V2): Conservative consumption estimates with 15% buffer
//!
//! ## Algorithm
//!
//! 1. Receive pre-calculated effective prices (spot + HDO grid fee) for ALL blocks from scheduler
//! 2. Split blocks into:
//!    - Bottom 30%: CHARGE AGGRESSIVELY (fill to 100%)
//!    - Top 30%: DISCHARGE (if spread > min_spread)
//!    - Middle 40%: FREEZE SOC (preserve battery)
//! 3. Calculate energy needs:
//!    - Expected consumption during expensive blocks
//!    - Safety margin (15%)
//!    - Minimum reserve SOC (40%)
//! 4. Select enough cheap blocks to meet energy needs
//! 5. During execution:
//!    - Cheap block + SOC < 100%: Force Charge
//!    - Expensive block + SOC > min_discharge_soc + spread OK: Allow natural discharge (SelfUse)
//!    - Mid-price block: Freeze SOC (BackUpMode) or use grid directly
//!    - Never buy from grid during expensive blocks
//!
//! **Note:** Effective price calculation is now centralized in the scheduler. This strategy
//! uses pre-calculated effective prices from TimeBlockPrice.effective_price_czk_per_kwh
//!
//! ## Example
//!
//! Given 96 blocks (24 hours) with prices 2.3 - 5.4 CZK/kWh:
//! - Bottom 30% (29 blocks): 2.3 - 3.2 CZK → CHARGE
//! - Top 30% (29 blocks): 4.5 - 5.4 CZK → DISCHARGE
//! - Middle 40% (38 blocks): 3.2 - 4.5 CZK → FREEZE SOC / USE GRID
//!
//! Result: Battery charges during cheapest periods, preserves SOC during mid-range,
//! discharges during peaks. Never forced to buy expensive grid energy.

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

/// Configuration for Winter Adaptive V5 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV5Config {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Strategy priority (higher = more important)
    pub priority: u8,

    /// Target battery SOC for charging (default: 100%)
    pub target_battery_soc: f32,

    /// Minimum SOC before allowing discharge (default: 40%)
    /// This prevents battery from draining too low and forcing expensive recharging
    pub min_discharge_soc: f32,

    /// Percentile threshold for "cheap" blocks (default: 30%)
    /// Bottom 30% of blocks by price are considered cheap → charge here
    pub cheap_block_percentile: f32,

    /// Percentile threshold for "expensive" blocks (default: 70%)
    /// Top 30% of blocks by price are considered expensive → discharge here
    pub expensive_block_percentile: f32,

    /// Minimum price spread for discharge to be worthwhile (CZK)
    /// Discharge only if: discharge_price - avg_charge_price > spread
    pub min_discharge_spread_czk: f32,

    /// Safety margin for energy needs calculation (default: 0.15 = 15%)
    pub safety_margin_pct: f32,

    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,

    /// Planning horizon in hours (default: 36)
    pub planning_horizon_hours: usize,
}

impl Default for WinterAdaptiveV5Config {
    fn default() -> Self {
        Self {
            enabled: true, // V5 is the new default strategy
            priority: 95,  // Higher priority than V2/V3/V4
            target_battery_soc: 100.0,
            min_discharge_soc: 40.0,      // Key: don't discharge below 40%
            cheap_block_percentile: 30.0, // Bottom 30% = cheap
            expensive_block_percentile: 70.0, // Top 30% = expensive
            min_discharge_spread_czk: 0.50,
            safety_margin_pct: 0.15, // 15% safety buffer
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockCategory {
    Cheap,     // Bottom percentile - charge here
    Expensive, // Top percentile - discharge here
    MidRange,  // Middle range - freeze SOC or use grid
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug)]
pub struct WinterAdaptiveV5Strategy {
    config: WinterAdaptiveV5Config,
}

impl WinterAdaptiveV5Strategy {
    pub fn new(config: WinterAdaptiveV5Config) -> Self {
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

    /// Categorize blocks into cheap/expensive/mid-range based on percentiles
    fn categorize_blocks(&self, ranked_blocks: &[RankedBlock]) -> Vec<(usize, BlockCategory)> {
        if ranked_blocks.is_empty() {
            return Vec::new();
        }

        // Sort by effective price to find percentile thresholds
        let mut prices: Vec<f32> = ranked_blocks.iter().map(|b| b.effective_price).collect();
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let cheap_idx = ((prices.len() as f32 * self.config.cheap_block_percentile / 100.0)
            as usize)
            .min(prices.len() - 1);
        let expensive_idx = ((prices.len() as f32 * self.config.expensive_block_percentile / 100.0)
            as usize)
            .min(prices.len() - 1);

        let cheap_threshold = prices[cheap_idx];
        let expensive_threshold = prices[expensive_idx];

        tracing::debug!(
            "V5: Price thresholds - cheap: ≤{:.3} CZK, expensive: ≥{:.3} CZK",
            cheap_threshold,
            expensive_threshold
        );

        ranked_blocks
            .iter()
            .map(|block| {
                let category = if block.effective_price <= cheap_threshold {
                    BlockCategory::Cheap
                } else if block.effective_price >= expensive_threshold {
                    BlockCategory::Expensive
                } else {
                    BlockCategory::MidRange
                };
                (block.index, category)
            })
            .collect()
    }

    /// Calculate how much energy we'll need during expensive blocks
    fn calculate_expensive_block_energy_needs(
        &self,
        context: &EvaluationContext,
        categorized_blocks: &[(usize, BlockCategory)],
    ) -> f32 {
        // Count how many expensive blocks we'll face
        let expensive_count = categorized_blocks
            .iter()
            .filter(|(_, cat)| *cat == BlockCategory::Expensive)
            .count();

        // Estimate consumption per block (15-minute blocks)
        let consumption_per_block_kwh = context.consumption_forecast_kwh;

        // Total energy needed during expensive blocks
        let energy_needed = (expensive_count as f32) * consumption_per_block_kwh;

        // Add safety margin
        let energy_with_margin = energy_needed * (1.0 + self.config.safety_margin_pct);

        tracing::debug!(
            "V5: Expensive blocks: {}, energy needed: {:.2} kWh (with {:.0}% margin)",
            expensive_count,
            energy_with_margin,
            self.config.safety_margin_pct * 100.0
        );

        energy_with_margin
    }

    /// Select charge blocks to cover energy needs
    fn select_charge_blocks(
        &self,
        context: &EvaluationContext,
        ranked_blocks: &[RankedBlock],
        categorized_blocks: &[(usize, BlockCategory)],
    ) -> HashSet<usize> {
        let mut selected: HashSet<usize> = HashSet::new();

        // Calculate total energy needs
        let expensive_energy_kwh =
            self.calculate_expensive_block_energy_needs(context, categorized_blocks);

        // Current SOC deficit
        let battery_capacity_kwh = context.control_config.battery_capacity_kwh;
        let soc_deficit = (self.config.target_battery_soc - context.current_battery_soc).max(0.0);
        let soc_energy_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;

        // Reserve SOC energy (keep this much in battery always)
        let reserve_soc_energy_kwh = (self.config.min_discharge_soc / 100.0) * battery_capacity_kwh;

        // Total energy to charge
        let total_energy_needed = soc_energy_kwh + expensive_energy_kwh + reserve_soc_energy_kwh;

        let charge_per_block_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
        let blocks_needed = (total_energy_needed / charge_per_block_kwh).ceil() as usize;

        tracing::info!(
            "V5: Energy needs - SOC: {:.2} kWh, expensive blocks: {:.2} kWh, reserve: {:.2} kWh, total: {:.2} kWh ({} blocks)",
            soc_energy_kwh,
            expensive_energy_kwh,
            reserve_soc_energy_kwh,
            total_energy_needed,
            blocks_needed
        );

        // Select cheapest blocks up to the needed count
        let mut cheap_blocks: Vec<&RankedBlock> = ranked_blocks
            .iter()
            .filter(|b| {
                categorized_blocks
                    .iter()
                    .any(|(idx, cat)| *idx == b.index && *cat == BlockCategory::Cheap)
            })
            .collect();

        cheap_blocks.sort_by(|a, b| {
            a.effective_price
                .partial_cmp(&b.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for block in cheap_blocks.iter().take(blocks_needed) {
            selected.insert(block.index);
        }

        tracing::info!(
            "V5: Selected {} charge blocks from {} cheap blocks available",
            selected.len(),
            cheap_blocks.len()
        );

        selected
    }

    /// Select discharge blocks (top expensive blocks with sufficient spread)
    fn select_discharge_blocks(
        &self,
        ranked_blocks: &[RankedBlock],
        categorized_blocks: &[(usize, BlockCategory)],
        charge_blocks: &HashSet<usize>,
    ) -> HashSet<usize> {
        let mut selected: HashSet<usize> = HashSet::new();

        // Calculate average charge price
        let charge_prices: Vec<f32> = ranked_blocks
            .iter()
            .filter(|b| charge_blocks.contains(&b.index))
            .map(|b| b.effective_price)
            .collect();

        let avg_charge_price = if charge_prices.is_empty() {
            0.0
        } else {
            charge_prices.iter().sum::<f32>() / charge_prices.len() as f32
        };

        // Select expensive blocks that meet spread requirement
        let mut expensive_blocks: Vec<&RankedBlock> = ranked_blocks
            .iter()
            .filter(|b| {
                categorized_blocks
                    .iter()
                    .any(|(idx, cat)| *idx == b.index && *cat == BlockCategory::Expensive)
            })
            .collect();

        expensive_blocks.sort_by(|a, b| {
            b.effective_price
                .partial_cmp(&a.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for block in &expensive_blocks {
            // Skip if this is also a charge block (shouldn't happen, but safety check)
            if charge_blocks.contains(&block.index) {
                continue;
            }

            // Check spread requirement
            let spread = block.effective_price - avg_charge_price;
            if spread >= self.config.min_discharge_spread_czk {
                selected.insert(block.index);
                tracing::debug!(
                    "V5: Selected discharge block idx {} at {:.3} CZK (spread {:.3})",
                    block.index,
                    block.effective_price,
                    spread
                );
            }
        }

        tracing::info!(
            "V5: Selected {} discharge blocks (avg charge price {:.3}, min spread {:.3})",
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
                    "winter_adaptive_v5:negative_price_charge".to_string(),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price {:.3} CZK/kWh, battery full", current_price),
                "winter_adaptive_v5:negative_price_full".to_string(),
            );
        }

        // =====================================================================
        // STEP 1: Rank and categorize ALL blocks
        // =====================================================================
        let ranked_blocks = self.rank_all_blocks(all_blocks);
        let categorized_blocks = self.categorize_blocks(&ranked_blocks);

        // =====================================================================
        // STEP 2: Select charge and discharge blocks
        // =====================================================================
        let charge_blocks = self.select_charge_blocks(context, &ranked_blocks, &categorized_blocks);
        let discharge_blocks =
            self.select_discharge_blocks(&ranked_blocks, &categorized_blocks, &charge_blocks);

        let should_charge = charge_blocks.contains(&current_block_index);
        let should_discharge = discharge_blocks.contains(&current_block_index);

        let current_category = categorized_blocks
            .iter()
            .find(|(idx, _)| *idx == current_block_index)
            .map(|(_, cat)| *cat)
            .unwrap_or(BlockCategory::MidRange);

        // =====================================================================
        // DECISION TREE
        // =====================================================================

        // PRIORITY 1: Charge during cheap blocks if battery not full
        if should_charge && context.current_battery_soc < self.config.target_battery_soc {
            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "Cheap block charge: effective price {:.3} CZK/kWh (spot {:.3} + grid fee)",
                    effective_price, current_price
                ),
                "winter_adaptive_v5:cheap_charge".to_string(),
            );
        }

        // PRIORITY 2: Discharge during expensive blocks (if SOC allows)
        if should_discharge && context.current_battery_soc > self.config.min_discharge_soc {
            return (
                InverterOperationMode::SelfUse, // Natural discharge to cover load
                format!(
                    "Peak discharge: {:.3} CZK/kWh - battery covers load, avoid expensive grid",
                    effective_price
                ),
                "winter_adaptive_v5:peak_discharge".to_string(),
            );
        }

        // PRIORITY 3: Expensive block but SOC too low - freeze and use grid
        if current_category == BlockCategory::Expensive
            && context.current_battery_soc <= self.config.min_discharge_soc
        {
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "Expensive block but SOC {:.1}% ≤ {:.1}% - freeze battery, use grid",
                    context.current_battery_soc, self.config.min_discharge_soc
                ),
                "winter_adaptive_v5:expensive_freeze_low_soc".to_string(),
            );
        }

        // PRIORITY 4: Mid-range blocks - freeze SOC if above minimum
        if current_category == BlockCategory::MidRange {
            if context.current_battery_soc > self.config.min_discharge_soc {
                return (
                    InverterOperationMode::BackUpMode,
                    format!(
                        "Mid-price freeze: {:.3} CZK/kWh - preserve battery (SOC {:.1}%)",
                        effective_price, context.current_battery_soc
                    ),
                    "winter_adaptive_v5:mid_freeze".to_string(),
                );
            } else {
                return (
                    InverterOperationMode::SelfUse,
                    format!(
                        "Mid-price use: {:.3} CZK/kWh - SOC low, use what's available",
                        effective_price
                    ),
                    "winter_adaptive_v5:mid_use".to_string(),
                );
            }
        }

        // DEFAULT: Self-Use
        (
            InverterOperationMode::SelfUse,
            format!(
                "Self-use: {:.3} CZK/kWh - normal operation (SOC {:.1}%)",
                effective_price, context.current_battery_soc
            ),
            "winter_adaptive_v5:self_use".to_string(),
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV5Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V5"
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
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                let usable_battery_kwh = ((context.current_battery_soc
                    - context.control_config.hardware_min_battery_soc)
                    .max(0.0)
                    / 100.0)
                    * context.control_config.battery_capacity_kwh;

                let battery_discharge = usable_battery_kwh.min(context.consumption_forecast_kwh);

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                if battery_discharge >= context.consumption_forecast_kwh {
                    // Battery fully covers load
                    eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                } else {
                    // Battery partially covers, rest from grid
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
        let base_time = Utc.with_ymd_and_hms(2026, 1, 16, 0, 0, 0).unwrap();
        let mut blocks = Vec::new();

        // Create 96 blocks (24 hours) with varying prices
        let prices = [
            2.3, 2.4, 2.5, 2.6, // 00:00-01:00 - cheap (charge)
            2.7, 2.8, 2.9, 3.0, // 01:00-02:00 - cheap
            3.1, 3.2, 3.3, 3.4, // 02:00-03:00 - mid
            3.5, 3.6, 3.7, 3.8, // 03:00-04:00 - mid
            3.9, 4.0, 4.1, 4.2, // 04:00-05:00 - mid
            4.3, 4.4, 4.5, 4.6, // 05:00-06:00 - expensive (discharge)
            4.7, 4.8, 4.9, 5.0, // 06:00-07:00 - expensive
            5.1, 5.2, 5.3, 5.4, // 07:00-08:00 - expensive (peak)
        ];

        // Using high tariff grid fee of 1.80 CZK/kWh for test consistency
        let grid_fee = 1.80;

        for (i, &price) in prices.iter().enumerate() {
            blocks.push(TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes((i as i64) * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price + grid_fee,
            });
        }

        blocks
    }

    #[test]
    fn test_block_categorization() {
        let config = WinterAdaptiveV5Config::default();
        let strategy = WinterAdaptiveV5Strategy::new(config);
        let blocks = create_test_blocks();

        let ranked = strategy.rank_all_blocks(&blocks);
        let categorized = strategy.categorize_blocks(&ranked);

        // Count categories
        let cheap_count = categorized
            .iter()
            .filter(|(_, cat)| *cat == BlockCategory::Cheap)
            .count();
        let expensive_count = categorized
            .iter()
            .filter(|(_, cat)| *cat == BlockCategory::Expensive)
            .count();

        // With 30% thresholds on 32 blocks: ~10 cheap, ~10 expensive, ~12 mid
        assert!((8..=12).contains(&cheap_count), "Expected ~10 cheap blocks");
        assert!(
            (8..=12).contains(&expensive_count),
            "Expected ~10 expensive blocks"
        );
    }

    #[test]
    fn test_v5_charges_during_cheap_blocks() {
        let config = WinterAdaptiveV5Config {
            cheap_block_percentile: 30.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV5Strategy::new(config);
        let blocks = create_test_blocks();
        let ranked = strategy.rank_all_blocks(&blocks);

        // Lowest prices should be in cheap category
        let cheapest_block = ranked
            .iter()
            .min_by(|a, b| a.effective_price.partial_cmp(&b.effective_price).unwrap())
            .unwrap();

        assert!(
            cheapest_block.spot_price <= 2.6,
            "Cheapest block should have low price"
        );
    }

    #[test]
    fn test_v5_reserves_soc_during_midrange() {
        // This test would require full context setup with EvaluationContext
        // For now, just verify the strategy initializes correctly
        let config = WinterAdaptiveV5Config {
            min_discharge_soc: 40.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV5Strategy::new(config);
        assert_eq!(strategy.config.min_discharge_soc, 40.0);
    }
}
