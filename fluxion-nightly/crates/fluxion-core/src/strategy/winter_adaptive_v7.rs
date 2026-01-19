// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! # Winter Adaptive V7 Strategy - Unconstrained Multi-Cycle Arbitrage Optimizer
//!
//! **Status:** Production - Maximum cost savings through unrestricted arbitrage
//!
//! ## Overview
//!
//! V7 is designed with one goal: **maximum cost savings**. It removes all artificial
//! limitations found in previous strategies and uses pure economic decision-making.
//!
//! ## Key Design Principles
//!
//! 1. **No Artificial Limitations**
//!    - No "top N discharge blocks" limit
//!    - No "only charge below median" constraint
//!    - No fixed SOC thresholds for discharge
//!    - No block locking that prevents rapid arbitrage
//!
//! 2. **Multi-Cycle Arbitrage**
//!    - Detects ALL profitable valley-peak pairs in the planning horizon
//!    - Executes 2-3+ cycles per day when profitable
//!    - Each cycle must have 3+ CZK spread (total charge cost vs discharge value)
//!
//! 3. **Provable Economics Only**
//!    - Uses real costs: spot price + grid fees + buy/sell fees
//!    - No estimated battery degradation costs
//!    - Minimum 3 CZK spread required for any cycle
//!
//! 4. **Home-First Export Policy**
//!    - Battery discharge prioritizes home consumption
//!    - Grid export only when:
//!      a) Price spread is significantly profitable (>5 CZK)
//!      b) Predicted SOC after discharge remains >50%
//!
//! 5. **Hybrid Approach with Fallback**
//!    - Primary: Valley-peak arbitrage detection for volatile days
//!    - Fallback: Percentile-based scheduling for stable days
//!    - Always ensures minimum charging happens
//!
//! ## Algorithm
//!
//! ### Phase 1: Price Pattern Analysis
//!
//! Analyze the entire price timeline:
//! - Calculate coefficient of variation (std_dev / mean)
//! - If CV > 0.15: Use valley-peak arbitrage detection
//! - If CV <= 0.15: Use percentile-based scheduling (like V5)
//!
//! ### Phase 2: Opportunity Detection
//!
//! For volatile days (valley-peak):
//! - Valley: consecutive blocks where price < mean - threshold
//! - Peak: consecutive blocks where price > mean + threshold
//! - Keep pairs with profit >= 3 CZK per cycle
//!
//! For stable days (percentile):
//! - Charge in bottom 25% cheapest blocks
//! - Discharge in top 25% most expensive blocks
//!
//! ### Phase 3: Real-Time Execution
//!
//! For current block:
//! - If in scheduled charge window → ForceCharge
//! - If in scheduled discharge window → discharge to home (or grid if profitable)
//! - Otherwise → SelfUse (cover home load from battery if possible)

use serde::{Deserialize, Serialize};

use crate::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive V7 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV7Config {
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    /// Target battery SOC (%) - charge up to this level
    pub target_battery_soc: f32,

    /// Hardware minimum battery SOC (%) - never discharge below this
    pub min_discharge_soc: f32,

    // === Arbitrage Detection ===
    /// Minimum price spread (CZK) for a charge-discharge cycle to be profitable
    /// This is the total cost difference: discharge_value - charge_cost >= this value
    /// Default: 3.0 CZK (based on typical grid fees and round-trip efficiency)
    pub min_cycle_profit_czk: f32,

    /// Valley detection threshold - blocks below (mean - this × std_dev) are valleys
    /// Default: 0.3 (lower = more sensitive detection)
    pub valley_threshold_std_dev: f32,

    /// Peak detection threshold - blocks above (mean + this × std_dev) are peaks
    /// Default: 0.3 (lower = more sensitive detection)
    pub peak_threshold_std_dev: f32,

    /// Coefficient of variation threshold to switch between valley-peak and percentile modes
    /// If CV > this: use valley-peak detection (volatile)
    /// If CV <= this: use percentile-based (stable)
    /// Default: 0.15 (15% relative variation)
    pub volatility_threshold_cv: f32,

    /// Percentile for cheap blocks (0.0-1.0) used in stable days
    /// Default: 0.25 (bottom 25%)
    pub cheap_block_percentile: f32,

    /// Percentile for expensive blocks (0.0-1.0) used in stable days
    /// Default: 0.75 (top 25%)
    pub expensive_block_percentile: f32,

    // === Export Policy ===
    /// Minimum price spread (CZK) to allow grid export instead of home use
    /// Default: 5.0 CZK (must be significantly more profitable than home use)
    pub min_export_spread_czk: f32,

    /// Minimum predicted SOC (%) after discharge to allow grid export
    /// Default: 50% (ensures battery reserve for home consumption)
    pub min_soc_after_export: f32,

    // === Consumption Prediction ===
    /// Average household consumption (kWh) per 15-minute block
    /// Used to predict SOC changes and validate export decisions
    /// Default: 0.25 kWh (1 kW average load)
    pub avg_consumption_per_block_kwh: f32,

    // === Safety ===
    /// Enable negative price handling (charge when getting paid)
    pub negative_price_handling_enabled: bool,

    /// Round-trip battery efficiency (charge × discharge efficiency)
    /// Default: 0.90 (90% round-trip efficiency)
    pub battery_round_trip_efficiency: f32,

    /// Minimum discharge spread (CZK) for percentile mode
    /// Default: 0.50 CZK
    pub min_discharge_spread_czk: f32,
}

impl Default for WinterAdaptiveV7Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 7,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            min_cycle_profit_czk: 3.0,
            valley_threshold_std_dev: 0.3, // Lowered from 0.5 for better detection
            peak_threshold_std_dev: 0.3,   // Lowered from 0.5 for better detection
            volatility_threshold_cv: 0.15, // Switch modes at 15% CV
            cheap_block_percentile: 0.25,  // Bottom 25% for charging
            expensive_block_percentile: 0.75, // Top 25% for discharging
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            avg_consumption_per_block_kwh: 0.25,
            negative_price_handling_enabled: true,
            battery_round_trip_efficiency: 0.90,
            min_discharge_spread_czk: 0.50,
        }
    }
}

/// A detected arbitrage opportunity (valley-peak pair or percentile-based)
#[derive(Debug, Clone)]
struct ArbitrageOpportunity {
    /// Indices of blocks in the charge window
    charge_blocks: Vec<usize>,
    /// Indices of blocks in the discharge window
    discharge_blocks: Vec<usize>,
    /// Expected profit per kWh cycled (after efficiency loss)
    profit_per_kwh: f32,
    /// Total expected profit for full battery cycle
    #[allow(dead_code)]
    total_profit: f32,
    /// Whether this is from valley-peak detection or percentile fallback
    #[allow(dead_code)]
    is_valley_peak: bool,
}

/// Scheduled action for a specific block
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScheduledAction {
    /// Charge during this block (part of arbitrage opportunity N)
    Charge { opportunity_id: usize },
    /// Discharge during this block (part of arbitrage opportunity N)
    Discharge { opportunity_id: usize },
    /// No scheduled action - use self-use mode
    None,
}

/// Detection mode used
#[derive(Debug, Clone, Copy, PartialEq)]
enum DetectionMode {
    /// Valley-peak detection for volatile days
    ValleyPeak,
    /// Percentile-based for stable days
    Percentile,
    /// Negative price exploitation
    NegativePrice,
}

pub struct WinterAdaptiveV7Strategy {
    config: WinterAdaptiveV7Config,
}

impl WinterAdaptiveV7Strategy {
    pub fn new(config: WinterAdaptiveV7Config) -> Self {
        Self { config }
    }

    /// Calculate price statistics for the planning horizon
    fn calculate_price_stats(&self, blocks: &[TimeBlockPrice]) -> (f32, f32, f32, f32, f32) {
        if blocks.is_empty() {
            return (0.0, 0.0, 0.0, 0.0, 0.0);
        }

        let prices: Vec<f32> = blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .collect();

        let min_price = prices.iter().copied().fold(f32::INFINITY, f32::min);
        let max_price = prices.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mean_price = prices.iter().sum::<f32>() / prices.len() as f32;

        let variance = prices
            .iter()
            .map(|&p| (p - mean_price).powi(2))
            .sum::<f32>()
            / prices.len() as f32;
        let std_dev = variance.sqrt();

        // Coefficient of variation (relative volatility)
        let cv = if mean_price > 0.0 {
            std_dev / mean_price
        } else {
            0.0
        };

        (min_price, max_price, mean_price, std_dev, cv)
    }

    /// Determine which detection mode to use based on price volatility
    fn determine_detection_mode(&self, blocks: &[TimeBlockPrice]) -> DetectionMode {
        // Check for negative prices first
        let has_negative = blocks.iter().any(|b| b.effective_price_czk_per_kwh < 0.0);
        if has_negative && self.config.negative_price_handling_enabled {
            return DetectionMode::NegativePrice;
        }

        let (_, _, _, _, cv) = self.calculate_price_stats(blocks);

        if cv > self.config.volatility_threshold_cv {
            DetectionMode::ValleyPeak
        } else {
            DetectionMode::Percentile
        }
    }

    /// Find opportunities using valley-peak detection (for volatile days)
    fn find_valley_peak_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
        battery_capacity_kwh: f32,
    ) -> Vec<ArbitrageOpportunity> {
        if blocks.len() < 4 {
            return Vec::new();
        }

        let (_, _, mean_price, std_dev, _) = self.calculate_price_stats(blocks);

        // Dynamic thresholds based on price distribution
        let valley_threshold = mean_price - (self.config.valley_threshold_std_dev * std_dev);
        let peak_threshold = mean_price + (self.config.peak_threshold_std_dev * std_dev);

        // Find all valleys and peaks
        let valleys = self.find_price_regions(blocks, |p| p < valley_threshold);
        let peaks = self.find_price_regions(blocks, |p| p > peak_threshold);

        let mut opportunities = Vec::new();

        // For each valley, find subsequent peaks that form profitable arbitrage
        for valley in &valleys {
            let valley_avg = self.region_average_price(blocks, valley);

            for peak in &peaks {
                // Peak must start after valley ends (temporal feasibility)
                if peak.first().copied().unwrap_or(0) <= valley.last().copied().unwrap_or(0) {
                    continue;
                }

                let peak_avg = self.region_average_price(blocks, peak);

                // Calculate profit considering round-trip efficiency
                let gross_spread = peak_avg - valley_avg;
                let net_profit_per_kwh = gross_spread * self.config.battery_round_trip_efficiency;

                // Check minimum profit threshold (3 CZK per kWh cycled)
                if net_profit_per_kwh >= self.config.min_cycle_profit_czk {
                    let total_profit = net_profit_per_kwh * battery_capacity_kwh * 0.8;

                    opportunities.push(ArbitrageOpportunity {
                        charge_blocks: valley.clone(),
                        discharge_blocks: peak.clone(),
                        profit_per_kwh: net_profit_per_kwh,
                        total_profit,
                        is_valley_peak: true,
                    });
                }
            }
        }

        // Sort by profit (best first)
        opportunities.sort_by(|a, b| {
            b.profit_per_kwh
                .partial_cmp(&a.profit_per_kwh)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Remove overlapping opportunities
        self.remove_overlapping_opportunities(opportunities)
    }

    /// Find opportunities using percentile-based scheduling (for stable days)
    fn find_percentile_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
        battery_capacity_kwh: f32,
    ) -> Vec<ArbitrageOpportunity> {
        if blocks.len() < 4 {
            return Vec::new();
        }

        // Rank all blocks by price
        let mut ranked: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();
        ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let n = ranked.len();
        let cheap_count = ((n as f32) * self.config.cheap_block_percentile).ceil() as usize;
        let expensive_start =
            ((n as f32) * self.config.expensive_block_percentile).floor() as usize;

        // Get cheapest blocks for charging
        let charge_blocks: Vec<usize> = ranked.iter().take(cheap_count).map(|(i, _)| *i).collect();

        // Get most expensive blocks for discharging
        let discharge_blocks: Vec<usize> = ranked
            .iter()
            .skip(expensive_start)
            .map(|(i, _)| *i)
            .collect();

        if charge_blocks.is_empty() || discharge_blocks.is_empty() {
            return Vec::new();
        }

        // Calculate average prices
        let charge_avg = charge_blocks
            .iter()
            .filter_map(|&i| blocks.get(i))
            .map(|b| b.effective_price_czk_per_kwh)
            .sum::<f32>()
            / charge_blocks.len() as f32;

        let discharge_avg = discharge_blocks
            .iter()
            .filter_map(|&i| blocks.get(i))
            .map(|b| b.effective_price_czk_per_kwh)
            .sum::<f32>()
            / discharge_blocks.len() as f32;

        let spread = discharge_avg - charge_avg;

        // Only create opportunity if spread is profitable
        if spread >= self.config.min_discharge_spread_czk {
            let profit_per_kwh = spread * self.config.battery_round_trip_efficiency;
            let total_profit = profit_per_kwh * battery_capacity_kwh * 0.8;

            vec![ArbitrageOpportunity {
                charge_blocks,
                discharge_blocks,
                profit_per_kwh,
                total_profit,
                is_valley_peak: false,
            }]
        } else {
            // Even if not profitable for arbitrage, still charge in cheapest blocks
            vec![ArbitrageOpportunity {
                charge_blocks,
                discharge_blocks: Vec::new(), // No discharge scheduled
                profit_per_kwh: 0.0,
                total_profit: 0.0,
                is_valley_peak: false,
            }]
        }
    }

    /// Find opportunities for negative price scenarios
    fn find_negative_price_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
        battery_capacity_kwh: f32,
    ) -> Vec<ArbitrageOpportunity> {
        // Find all negative price blocks
        let negative_blocks: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.effective_price_czk_per_kwh < 0.0)
            .map(|(i, _)| i)
            .collect();

        if negative_blocks.is_empty() {
            return Vec::new();
        }

        // Find cheapest positive blocks for additional charging
        let mut positive_ranked: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.effective_price_czk_per_kwh >= 0.0)
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();
        positive_ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let cheap_positive: Vec<usize> = positive_ranked
            .iter()
            .take(8) // Top 8 cheapest positive blocks
            .map(|(i, _)| *i)
            .collect();

        // Combine negative + cheap positive for charging
        let mut charge_blocks = negative_blocks;
        charge_blocks.extend(cheap_positive);
        charge_blocks.sort();
        charge_blocks.dedup();

        // Find most expensive blocks for discharge (all blocks above median)
        let (_, _, mean_price, _, _) = self.calculate_price_stats(blocks);
        let discharge_blocks: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.effective_price_czk_per_kwh > mean_price)
            .map(|(i, _)| i)
            .collect();

        let charge_avg = charge_blocks
            .iter()
            .filter_map(|&i| blocks.get(i))
            .map(|b| b.effective_price_czk_per_kwh)
            .sum::<f32>()
            / charge_blocks.len().max(1) as f32;

        let discharge_avg = if discharge_blocks.is_empty() {
            mean_price
        } else {
            discharge_blocks
                .iter()
                .filter_map(|&i| blocks.get(i))
                .map(|b| b.effective_price_czk_per_kwh)
                .sum::<f32>()
                / discharge_blocks.len() as f32
        };

        let profit_per_kwh =
            (discharge_avg - charge_avg) * self.config.battery_round_trip_efficiency;
        let total_profit = profit_per_kwh * battery_capacity_kwh * 0.8;

        vec![ArbitrageOpportunity {
            charge_blocks,
            discharge_blocks,
            profit_per_kwh,
            total_profit,
            is_valley_peak: false,
        }]
    }

    /// Find all arbitrage opportunities based on detected mode
    fn find_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
        battery_capacity_kwh: f32,
    ) -> (Vec<ArbitrageOpportunity>, DetectionMode) {
        let mode = self.determine_detection_mode(blocks);

        let opportunities = match mode {
            DetectionMode::NegativePrice => {
                self.find_negative_price_opportunities(blocks, battery_capacity_kwh)
            }
            DetectionMode::ValleyPeak => {
                let mut opps = self.find_valley_peak_opportunities(blocks, battery_capacity_kwh);

                // If valley-peak found nothing, fall back to percentile
                if opps.is_empty() || opps.iter().all(|o| o.charge_blocks.is_empty()) {
                    opps = self.find_percentile_opportunities(blocks, battery_capacity_kwh);
                }
                opps
            }
            DetectionMode::Percentile => {
                self.find_percentile_opportunities(blocks, battery_capacity_kwh)
            }
        };

        (opportunities, mode)
    }

    /// Find contiguous regions where price satisfies the predicate
    fn find_price_regions<F>(&self, blocks: &[TimeBlockPrice], predicate: F) -> Vec<Vec<usize>>
    where
        F: Fn(f32) -> bool,
    {
        let mut regions = Vec::new();
        let mut current_region = Vec::new();

        for (i, block) in blocks.iter().enumerate() {
            if predicate(block.effective_price_czk_per_kwh) {
                current_region.push(i);
            } else if !current_region.is_empty() {
                regions.push(std::mem::take(&mut current_region));
            }
        }

        if !current_region.is_empty() {
            regions.push(current_region);
        }

        // Filter out very short regions (at least 2 blocks = 30 minutes)
        regions.into_iter().filter(|r| r.len() >= 2).collect()
    }

    /// Calculate average effective price for a region
    fn region_average_price(&self, blocks: &[TimeBlockPrice], indices: &[usize]) -> f32 {
        if indices.is_empty() {
            return 0.0;
        }

        let sum: f32 = indices
            .iter()
            .filter_map(|&i| blocks.get(i))
            .map(|b| b.effective_price_czk_per_kwh)
            .sum();

        sum / indices.len() as f32
    }

    /// Remove overlapping opportunities, keeping highest profit ones
    fn remove_overlapping_opportunities(
        &self,
        mut opportunities: Vec<ArbitrageOpportunity>,
    ) -> Vec<ArbitrageOpportunity> {
        let mut used_blocks: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut result = Vec::new();

        for opp in opportunities.drain(..) {
            // Check if any blocks are already used
            let charge_overlap = opp.charge_blocks.iter().any(|b| used_blocks.contains(b));
            let discharge_overlap = opp.discharge_blocks.iter().any(|b| used_blocks.contains(b));

            if !charge_overlap && !discharge_overlap {
                // Mark blocks as used
                for &b in &opp.charge_blocks {
                    used_blocks.insert(b);
                }
                for &b in &opp.discharge_blocks {
                    used_blocks.insert(b);
                }
                result.push(opp);
            }
        }

        result
    }

    /// Generate the optimal schedule for all blocks
    fn generate_schedule(
        &self,
        blocks: &[TimeBlockPrice],
        opportunities: &[ArbitrageOpportunity],
    ) -> Vec<ScheduledAction> {
        let mut schedule = vec![ScheduledAction::None; blocks.len()];

        for (opp_id, opp) in opportunities.iter().enumerate() {
            for &block_idx in &opp.charge_blocks {
                if block_idx < schedule.len() {
                    schedule[block_idx] = ScheduledAction::Charge {
                        opportunity_id: opp_id,
                    };
                }
            }
            for &block_idx in &opp.discharge_blocks {
                if block_idx < schedule.len() {
                    schedule[block_idx] = ScheduledAction::Discharge {
                        opportunity_id: opp_id,
                    };
                }
            }
        }

        schedule
    }

    /// Decide whether discharge should go to grid or home
    fn should_export_to_grid(
        &self,
        current_soc: f32,
        discharge_price: f32,
        min_price_in_horizon: f32,
        predicted_consumption_kwh: f32,
        battery_capacity_kwh: f32,
    ) -> bool {
        // Calculate spread
        let spread = discharge_price - min_price_in_horizon;

        // Check if spread is large enough
        if spread < self.config.min_export_spread_czk {
            return false;
        }

        // Predict SOC after discharge (assume we discharge one block's worth)
        let discharge_kwh = battery_capacity_kwh * 0.1; // ~10% per block at max rate
        let predicted_soc_after = current_soc
            - (discharge_kwh / battery_capacity_kwh * 100.0)
            - (predicted_consumption_kwh / battery_capacity_kwh * 100.0);

        // Only export if predicted SOC stays above threshold
        predicted_soc_after >= self.config.min_soc_after_export
    }

    /// Calculate summary statistics for logging
    fn calculate_opportunity_summary(
        &self,
        opportunities: &[ArbitrageOpportunity],
        mode: DetectionMode,
    ) -> String {
        let mode_str = match mode {
            DetectionMode::ValleyPeak => "VALLEY-PEAK",
            DetectionMode::Percentile => "PERCENTILE",
            DetectionMode::NegativePrice => "NEGATIVE",
        };

        if opportunities.is_empty() {
            return format!("{}: No opportunities", mode_str);
        }

        let total_charge_blocks: usize = opportunities.iter().map(|o| o.charge_blocks.len()).sum();
        let total_discharge_blocks: usize =
            opportunities.iter().map(|o| o.discharge_blocks.len()).sum();
        let avg_profit = opportunities.iter().map(|o| o.profit_per_kwh).sum::<f32>()
            / opportunities.len() as f32;

        format!(
            "{}: {} opps, {:.1} CZK/kWh avg, {} charge/{} discharge blocks",
            mode_str,
            opportunities.len(),
            avg_profit,
            total_charge_blocks,
            total_discharge_blocks
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV7Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V7"
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
            battery_wear_cost_czk_per_kwh: 0.0, // V7 doesn't use wear cost (provable data only)
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available".to_string();
            return eval;
        };

        // Phase 1: Find all opportunities
        let (opportunities, mode) =
            self.find_opportunities(all_blocks, context.control_config.battery_capacity_kwh);

        let opportunity_summary = self.calculate_opportunity_summary(&opportunities, mode);

        // Phase 2: Generate optimal schedule
        let schedule = self.generate_schedule(all_blocks, &opportunities);

        // Find current block index
        let block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let current_action = schedule
            .get(block_index)
            .copied()
            .unwrap_or(ScheduledAction::None);
        let effective_price = context.price_block.effective_price_czk_per_kwh;
        let (min_price, _, _, _, _) = self.calculate_price_stats(all_blocks);

        // Phase 3: Execute based on schedule
        match current_action {
            ScheduledAction::Charge { opportunity_id } => {
                // Check if we should charge
                if context.current_battery_soc < self.config.target_battery_soc {
                    let opp = opportunities.get(opportunity_id);
                    let profit_info = opp
                        .map(|o| format!("{:.2} CZK/kWh", o.profit_per_kwh))
                        .unwrap_or_default();

                    eval.mode = InverterOperationMode::ForceCharge;
                    eval.reason = format!(
                        "CHARGE: {:.3} CZK/kWh ({}) [{}]",
                        effective_price, profit_info, opportunity_summary
                    );
                    eval.decision_uid =
                        Some(format!("winter_adaptive_v7:charge:opp{}", opportunity_id));

                    // Calculate energy flows
                    let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                    eval.energy_flows.battery_charge_kwh = charge_kwh;
                    eval.energy_flows.grid_import_kwh = charge_kwh;
                    eval.cost_czk = charge_kwh * effective_price;
                }
            }

            ScheduledAction::Discharge { opportunity_id } => {
                // Check if we should discharge
                if context.current_battery_soc > self.config.min_discharge_soc {
                    let opp = opportunities.get(opportunity_id);
                    let profit_info = opp
                        .map(|o| format!("{:.2} CZK/kWh", o.profit_per_kwh))
                        .unwrap_or_default();

                    // Determine if we should export to grid or use for home
                    let should_export = self.should_export_to_grid(
                        context.current_battery_soc,
                        effective_price,
                        min_price,
                        context.consumption_forecast_kwh,
                        context.control_config.battery_capacity_kwh,
                    );

                    if should_export {
                        // Export to grid - aggressive discharge
                        eval.mode = InverterOperationMode::ForceDischarge;
                        eval.reason = format!(
                            "DISCHARGE→GRID: {:.3} CZK/kWh ({}) [{}]",
                            effective_price, profit_info, opportunity_summary
                        );
                        eval.decision_uid = Some(format!(
                            "winter_adaptive_v7:discharge_grid:opp{}",
                            opportunity_id
                        ));

                        let discharge_kwh =
                            context.control_config.max_battery_charge_rate_kw * 0.25;
                        eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                        eval.energy_flows.grid_export_kwh = discharge_kwh;
                        eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
                    } else {
                        // Home use - let battery cover consumption, avoid grid import
                        eval.mode = InverterOperationMode::SelfUse;
                        eval.reason = format!(
                            "DISCHARGE→HOME: {:.3} CZK/kWh ({}) [{}]",
                            effective_price, profit_info, opportunity_summary
                        );
                        eval.decision_uid = Some(format!(
                            "winter_adaptive_v7:discharge_home:opp{}",
                            opportunity_id
                        ));

                        // Battery covers home consumption
                        let usable_battery_kwh = ((context.current_battery_soc
                            - self.config.min_discharge_soc)
                            .max(0.0)
                            / 100.0)
                            * context.control_config.battery_capacity_kwh;

                        let battery_discharge =
                            usable_battery_kwh.min(context.consumption_forecast_kwh);
                        eval.energy_flows.battery_discharge_kwh = battery_discharge;

                        if battery_discharge >= context.consumption_forecast_kwh {
                            // Battery fully covers load - revenue is avoided grid cost
                            eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                        } else {
                            // Partial coverage
                            eval.revenue_czk = battery_discharge * effective_price;
                            let grid_needed = context.consumption_forecast_kwh - battery_discharge;
                            eval.cost_czk = grid_needed * effective_price;
                            eval.energy_flows.grid_import_kwh = grid_needed;
                        }
                    }
                }
            }

            ScheduledAction::None => {
                // PRIORITY: Check for negative prices (always charge if getting paid)
                if self.config.negative_price_handling_enabled && effective_price < 0.0 {
                    if context.current_battery_soc < self.config.target_battery_soc {
                        eval.mode = InverterOperationMode::ForceCharge;
                        eval.reason = format!(
                            "NEGATIVE PRICE: {:.3} CZK/kWh (getting paid!) [{}]",
                            effective_price, opportunity_summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v7:negative_price".to_string());

                        let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                        eval.energy_flows.battery_charge_kwh = charge_kwh;
                        eval.energy_flows.grid_import_kwh = charge_kwh;
                        eval.cost_czk = charge_kwh * effective_price; // Negative = revenue!
                    }
                } else {
                    // Self-use mode - battery covers consumption if available
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: {:.3} CZK/kWh [{}]",
                        effective_price, opportunity_summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v7:self_use".to_string());

                    // Calculate self-use energy flows
                    let usable_battery_kwh = ((context.current_battery_soc
                        - context.control_config.hardware_min_battery_soc)
                        .max(0.0)
                        / 100.0)
                        * context.control_config.battery_capacity_kwh;

                    let battery_discharge =
                        usable_battery_kwh.min(context.consumption_forecast_kwh);
                    eval.energy_flows.battery_discharge_kwh = battery_discharge;

                    if battery_discharge >= context.consumption_forecast_kwh {
                        eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                    } else {
                        eval.revenue_czk = battery_discharge * effective_price;
                        let grid_needed = context.consumption_forecast_kwh - battery_discharge;
                        eval.cost_czk = grid_needed * effective_price;
                        eval.energy_flows.grid_import_kwh = grid_needed;
                    }
                }
            }
        }

        eval.calculate_net_profit();
        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn create_volatile_test_blocks() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 18, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        // Simulate a volatile day with multiple arbitrage opportunities
        vec![
            // Valley 1: 00:00-01:00 (cheap overnight)
            TimeBlockPrice {
                block_start: base_time,
                duration_minutes: 15,
                price_czk_per_kwh: 1.00,
                effective_price_czk_per_kwh: 1.00 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 1.10,
                effective_price_czk_per_kwh: 1.10 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 1.05,
                effective_price_czk_per_kwh: 1.05 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(45),
                duration_minutes: 15,
                price_czk_per_kwh: 1.15,
                effective_price_czk_per_kwh: 1.15 + grid_fee,
            },
            // Peak 1: 07:00-08:00 (morning peak)
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(7),
                duration_minutes: 15,
                price_czk_per_kwh: 5.50,
                effective_price_czk_per_kwh: 5.50 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(7) + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.80,
                effective_price_czk_per_kwh: 5.80 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(7) + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 5.60,
                effective_price_czk_per_kwh: 5.60 + grid_fee,
            },
            // Valley 2: 12:00-13:00 (midday solar dip)
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(12),
                duration_minutes: 15,
                price_czk_per_kwh: 0.80,
                effective_price_czk_per_kwh: 0.80 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time
                    + chrono::Duration::hours(12)
                    + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 0.50,
                effective_price_czk_per_kwh: 0.50 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time
                    + chrono::Duration::hours(12)
                    + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 0.60,
                effective_price_czk_per_kwh: 0.60 + grid_fee,
            },
            // Peak 2: 18:00-19:00 (evening peak)
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(18),
                duration_minutes: 15,
                price_czk_per_kwh: 6.50,
                effective_price_czk_per_kwh: 6.50 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time
                    + chrono::Duration::hours(18)
                    + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 7.00,
                effective_price_czk_per_kwh: 7.00 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time
                    + chrono::Duration::hours(18)
                    + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 6.80,
                effective_price_czk_per_kwh: 6.80 + grid_fee,
            },
        ]
    }

    fn create_stable_test_blocks() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 18, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        // Simulate a stable day with small price variations
        (0..24)
            .map(|hour| {
                let price = 3.0 + (hour as f32 - 12.0).abs() * 0.1; // Slight variation
                TimeBlockPrice {
                    block_start: base_time + chrono::Duration::hours(hour),
                    duration_minutes: 15,
                    price_czk_per_kwh: price,
                    effective_price_czk_per_kwh: price + grid_fee,
                }
            })
            .collect()
    }

    #[test]
    fn test_mode_detection_volatile() {
        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);
        let blocks = create_volatile_test_blocks();

        let mode = strategy.determine_detection_mode(&blocks);
        assert_eq!(
            mode,
            DetectionMode::ValleyPeak,
            "Volatile prices should use valley-peak"
        );
    }

    #[test]
    fn test_mode_detection_stable() {
        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);
        let blocks = create_stable_test_blocks();

        let mode = strategy.determine_detection_mode(&blocks);
        assert_eq!(
            mode,
            DetectionMode::Percentile,
            "Stable prices should use percentile"
        );
    }

    #[test]
    fn test_finds_opportunities_volatile() {
        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);
        let blocks = create_volatile_test_blocks();

        let (opportunities, mode) = strategy.find_opportunities(&blocks, 10.0);

        assert_eq!(mode, DetectionMode::ValleyPeak);
        assert!(
            !opportunities.is_empty(),
            "Should find arbitrage opportunities in volatile prices"
        );
    }

    #[test]
    fn test_finds_opportunities_stable() {
        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);
        let blocks = create_stable_test_blocks();

        let (opportunities, mode) = strategy.find_opportunities(&blocks, 10.0);

        assert_eq!(mode, DetectionMode::Percentile);
        assert!(
            !opportunities.is_empty(),
            "Should find percentile-based opportunities even in stable prices"
        );
        assert!(
            !opportunities[0].charge_blocks.is_empty(),
            "Should have charge blocks scheduled"
        );
    }

    #[test]
    fn test_schedule_generation() {
        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);
        let blocks = create_volatile_test_blocks();

        let (opportunities, _) = strategy.find_opportunities(&blocks, 10.0);
        let schedule = strategy.generate_schedule(&blocks, &opportunities);

        assert_eq!(
            schedule.len(),
            blocks.len(),
            "Schedule should have same length as blocks"
        );

        // Count scheduled actions
        let charge_count = schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge { .. }))
            .count();

        assert!(charge_count > 0, "Should have some charge blocks scheduled");
    }

    #[test]
    fn test_negative_price_detection() {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 18, 12, 0, 0).unwrap();
        let blocks = vec![
            TimeBlockPrice {
                block_start: base_time,
                duration_minutes: 15,
                price_czk_per_kwh: -1.00,
                effective_price_czk_per_kwh: -1.00 + 1.80,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: -2.00,
                effective_price_czk_per_kwh: -2.00 + 1.80,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(6),
                duration_minutes: 15,
                price_czk_per_kwh: 5.00,
                effective_price_czk_per_kwh: 5.00 + 1.80,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(6) + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.50,
                effective_price_czk_per_kwh: 5.50 + 1.80,
            },
        ];

        let config = WinterAdaptiveV7Config::default();
        let strategy = WinterAdaptiveV7Strategy::new(config);

        let mode = strategy.determine_detection_mode(&blocks);
        assert_eq!(
            mode,
            DetectionMode::NegativePrice,
            "Should detect negative prices"
        );
    }

    #[test]
    fn test_export_decision_respects_soc_threshold() {
        let config = WinterAdaptiveV7Config {
            min_soc_after_export: 50.0,
            min_export_spread_czk: 5.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV7Strategy::new(config);

        // High SOC, good spread - should export
        let should_export_high_soc = strategy.should_export_to_grid(
            80.0, // current SOC
            8.0,  // discharge price
            2.0,  // min price (spread = 6 CZK > 5 CZK threshold)
            0.25, // consumption
            10.0, // battery capacity
        );
        assert!(
            should_export_high_soc,
            "Should export with high SOC and good spread"
        );

        // Low SOC - should NOT export (would drop below 50%)
        let should_export_low_soc = strategy.should_export_to_grid(
            55.0, // current SOC (close to threshold)
            8.0,  // discharge price
            2.0,  // min price
            0.25, // consumption
            10.0, // battery capacity
        );
        assert!(!should_export_low_soc, "Should NOT export with low SOC");

        // Small spread - should NOT export
        let should_export_small_spread = strategy.should_export_to_grid(
            80.0, // current SOC
            5.0,  // discharge price
            3.0,  // min price (spread = 2 CZK < 5 CZK threshold)
            0.25, // consumption
            10.0, // battery capacity
        );
        assert!(
            !should_export_small_spread,
            "Should NOT export with small spread"
        );
    }
}
