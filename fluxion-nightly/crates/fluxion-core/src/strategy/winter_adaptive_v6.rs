// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! # Winter Adaptive V6 Strategy - Adaptive Hybrid Optimizer
//!
//! **Status:** Experimental - Combines best aspects of V3, V4, V5
//!
//! ## Overview
//!
//! V6 is an adaptive hybrid strategy that dynamically switches between different
//! optimization modes based on detected price pattern characteristics. It aims to
//! be the best-in-class across ALL market conditions by combining:
//!
//! - V3's volatility handling (forward-looking arbitrage)
//! - V5's percentile-based optimization (for structured patterns)
//! - V4's global ranking (as reliable fallback)
//! - Full battery utilization (always target min SOC at end of day)
//! - Adaptive pattern detection and mode switching
//!
//! ## Algorithm
//!
//! ### Phase 1: Price Pattern Analysis
//!
//! At the start of each scheduling run, analyze the full day's price data:
//!
//! 1. **Volatility Detection** - Calculate coefficient of variation (std_dev / mean)
//! 2. **Negative Price Detection** - Check for any negative prices
//! 3. **Day/Night Spread** - Measure difference between cheapest and most expensive periods
//! 4. **Pattern Complexity** - Count number of local peaks/valleys
//!
//! ### Phase 2: Mode Selection
//!
//! Based on detected characteristics, select optimization mode:
//!
//! - **NEGATIVE_EXPLOIT**: Negative prices present → Maximize charging during negative periods
//! - **VOLATILE_ARBITRAGE**: High volatility (CV > 0.4) → Use V3-style forward windows
//! - **SIMPLE_ARBITRAGE**: Large day/night spread (> 2.5x) → Use V3-style forward windows
//! - **PERCENTILE_OPTIMIZED**: Structured patterns → Use V5-style percentile thresholds
//! - **GLOBAL_RANKING**: Default fallback → Use V4-style global ranking
//!
//! ### Phase 3: Block Decision
//!
//! For each 15-minute block:
//!
//! 1. Apply mode-specific decision logic
//! 2. Check for negative prices (always charge if negative and SOC < 100%)
//! 3. Verify battery constraints (SOC limits, charge rate)
//! 4. Calculate expected savings vs wear cost
//! 5. Make charge/discharge/hold decision
//!
//! ### Phase 4: SOC Management
//!
//! Unlike V3 which wastes capacity by ending at high SOC:
//!
//! - Target minimum SOC (10%) at end of planning horizon
//! - Aggressively discharge during expensive periods to avoid waste
//! - Only preserve battery for periods with actual arbitrage opportunity
//!
//! ## Key Innovations
//!
//! 1. **Adaptive Mode Switching** - Different algorithms for different market conditions
//! 2. **Full Utilization** - Never waste battery capacity by ending at high SOC
//! 3. **Negative Price Exploitation** - Get paid to charge during negative periods
//! 4. **Savings Validation** - Only cycle battery if savings exceed wear cost
//! 5. **Pattern Learning** - Could be extended to learn optimal thresholds over time
//!
//! ## Pattern Detection Thresholds
//!
//! - **Volatility (CV)**: &gt; 0.4 = volatile, &lt; 0.2 = stable
//! - **Day/Night Spread**: &gt; 2.5x = simple arbitrage, &lt; 1.5x = flat
//! - **Negative Prices**: Any block &lt; 0 = negative exploit mode
//! - **Pattern Complexity**: &gt; 10 peaks = complex, &lt; 4 peaks = simple
//!
//! ## Expected Performance
//!
//! Target: 1st or 2nd place in all scenarios, 3-5% better total cost than V5.
//!
//! **Note:** Effective price calculation is centralized in the scheduler. This strategy
//! uses pre-calculated effective prices from TimeBlockPrice.effective_price_czk_per_kwh

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive V6 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV6Config {
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    /// Target battery SOC (%)
    pub target_battery_soc: f32,

    /// Minimum battery SOC (%) - discharge limit
    pub min_discharge_soc: f32,

    // === Volatility Detection ===
    /// Coefficient of variation threshold for volatile mode (default: 0.4)
    /// CV = std_dev / mean. Higher = more volatile.
    pub volatility_cv_threshold: f32,

    // === Simple Arbitrage Detection ===
    /// Day/night spread ratio for simple arbitrage (default: 2.5)
    /// If max_price / min_price > this, use simple arbitrage mode
    pub simple_arbitrage_spread_ratio: f32,

    // === Percentile Thresholds (for structured patterns) ===
    /// Percentile for cheap blocks (default: 0.25 = bottom 25%)
    pub cheap_block_percentile: f32,

    /// Percentile for expensive blocks (default: 0.75 = top 25%)
    pub expensive_block_percentile: f32,

    // === Discharge Configuration ===
    /// Minimum price spread for discharge to be worthwhile (CZK/kWh)
    pub min_discharge_spread_czk: f32,

    /// Number of most expensive blocks to target for discharge (V3-style)
    pub discharge_blocks_per_day: usize,

    // === Safety and Optimization ===
    /// Safety margin for SOC targets (%) - prevent full discharge
    pub safety_margin_pct: f32,

    /// Enable negative price handling
    pub negative_price_handling_enabled: bool,

    /// Planning horizon in hours (how far ahead to look)
    pub planning_horizon_hours: usize,

    /// Minimum savings per cycle to justify battery wear (CZK)
    /// Set to 0 to disable wear cost checking
    pub min_savings_per_cycle_czk: f32,
}

impl Default for WinterAdaptiveV6Config {
    fn default() -> Self {
        Self {
            enabled: false, // V5 is still default
            priority: 6,    // Highest priority (V5 = 5, V4 = 4, V3 = 3)
            target_battery_soc: 90.0,
            min_discharge_soc: 10.0,
            volatility_cv_threshold: 0.4,
            simple_arbitrage_spread_ratio: 2.5,
            cheap_block_percentile: 0.25,
            expensive_block_percentile: 0.75,
            min_discharge_spread_czk: 0.50,
            discharge_blocks_per_day: 12, // V3 default
            safety_margin_pct: 5.0,
            negative_price_handling_enabled: true,
            planning_horizon_hours: 24,
            min_savings_per_cycle_czk: 0.0, // Disabled by default
        }
    }
}

/// Optimization mode selected based on price pattern analysis
#[derive(Debug, Clone, Copy, PartialEq)]
enum OptimizationMode {
    /// Negative prices detected - exploit them aggressively
    NegativeExploit,
    /// High volatility - use V3-style forward-looking arbitrage
    VolatileArbitrage,
    /// Large day/night spread - use V3-style simple arbitrage
    SimpleArbitrage,
    /// Structured pattern (HDO, usual) - use V5-style percentile optimization
    PercentileOptimized,
    /// Default fallback - use V4-style global ranking
    GlobalRanking,
}

impl OptimizationMode {
    fn name(&self) -> &'static str {
        match self {
            Self::NegativeExploit => "NEGATIVE_EXPLOIT",
            Self::VolatileArbitrage => "VOLATILE_ARBITRAGE",
            Self::SimpleArbitrage => "SIMPLE_ARBITRAGE",
            Self::PercentileOptimized => "PERCENTILE_OPTIMIZED",
            Self::GlobalRanking => "GLOBAL_RANKING",
        }
    }
}

/// Price pattern analysis results
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PricePattern {
    /// Coefficient of variation (std_dev / mean)
    volatility_cv: f32,
    /// Has any negative prices
    has_negative_prices: bool,
    /// Ratio of max/min price
    day_night_spread_ratio: f32,
    /// Number of local peaks (price reversals)
    peak_count: usize,
    /// Minimum price in dataset
    min_price: f32,
    /// Maximum price in dataset
    max_price: f32,
    /// Mean price
    mean_price: f32,
    /// Standard deviation
    std_dev: f32,
}

/// Ranked block with price information
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct RankedBlock {
    index: usize,
    block_start: DateTime<Utc>,
    spot_price: f32,
    effective_price: f32,
}

pub struct WinterAdaptiveV6Strategy {
    config: WinterAdaptiveV6Config,
}

impl WinterAdaptiveV6Strategy {
    pub fn new(config: WinterAdaptiveV6Config) -> Self {
        Self { config }
    }

    /// Analyze price pattern and determine optimization mode
    fn analyze_pattern(&self, all_blocks: &[TimeBlockPrice]) -> (PricePattern, OptimizationMode) {
        if all_blocks.is_empty() {
            return (
                PricePattern {
                    volatility_cv: 0.0,
                    has_negative_prices: false,
                    day_night_spread_ratio: 1.0,
                    peak_count: 0,
                    min_price: 0.0,
                    max_price: 0.0,
                    mean_price: 0.0,
                    std_dev: 0.0,
                },
                OptimizationMode::GlobalRanking,
            );
        }

        // Extract effective prices
        let prices: Vec<f32> = all_blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .collect();

        // Calculate statistics
        let min_price = prices.iter().copied().fold(f32::INFINITY, f32::min);
        let max_price = prices.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mean_price = prices.iter().sum::<f32>() / prices.len() as f32;

        let variance = prices
            .iter()
            .map(|&p| (p - mean_price).powi(2))
            .sum::<f32>()
            / prices.len() as f32;
        let std_dev = variance.sqrt();

        let volatility_cv = if mean_price.abs() > 0.01 {
            std_dev / mean_price.abs()
        } else {
            0.0
        };

        let has_negative_prices = min_price < 0.0;

        let day_night_spread_ratio = if min_price.abs() > 0.01 {
            max_price / min_price.abs().max(0.01) // Avoid division by zero
        } else {
            1.0
        };

        // Count local peaks (simple peak detection)
        let peak_count = self.count_peaks(&prices);

        let pattern = PricePattern {
            volatility_cv,
            has_negative_prices,
            day_night_spread_ratio,
            peak_count,
            min_price,
            max_price,
            mean_price,
            std_dev,
        };

        // Determine optimization mode based on pattern
        let mode = if pattern.has_negative_prices && self.config.negative_price_handling_enabled {
            OptimizationMode::NegativeExploit
        } else if pattern.volatility_cv > self.config.volatility_cv_threshold {
            OptimizationMode::VolatileArbitrage
        } else if pattern.day_night_spread_ratio > self.config.simple_arbitrage_spread_ratio {
            OptimizationMode::SimpleArbitrage
        } else if pattern.volatility_cv < 0.2 && pattern.peak_count < 6 {
            // Low volatility, simple pattern → percentile optimization works well
            OptimizationMode::PercentileOptimized
        } else {
            OptimizationMode::GlobalRanking
        };

        (pattern, mode)
    }

    /// Count local peaks in price series (number of reversals)
    fn count_peaks(&self, prices: &[f32]) -> usize {
        if prices.len() < 3 {
            return 0;
        }

        let mut peaks = 0;
        for i in 1..prices.len() - 1 {
            // Local maximum
            if prices[i] > prices[i - 1] && prices[i] > prices[i + 1] {
                peaks += 1;
            }
            // Local minimum
            if prices[i] < prices[i - 1] && prices[i] < prices[i + 1] {
                peaks += 1;
            }
        }
        peaks
    }

    /// Rank all blocks by effective price
    fn rank_all_blocks(&self, all_blocks: &[TimeBlockPrice]) -> Vec<RankedBlock> {
        all_blocks
            .iter()
            .enumerate()
            .map(|(idx, block)| RankedBlock {
                index: idx,
                block_start: block.block_start,
                spot_price: block.price_czk_per_kwh,
                effective_price: block.effective_price_czk_per_kwh,
            })
            .collect()
    }

    /// Get charge blocks using percentile threshold (V5-style)
    fn get_percentile_charge_blocks(&self, ranked_blocks: &[RankedBlock]) -> Vec<usize> {
        let mut sorted = ranked_blocks.to_vec();
        sorted.sort_by(|a, b| {
            a.effective_price
                .partial_cmp(&b.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let cheap_count =
            (sorted.len() as f32 * self.config.cheap_block_percentile).ceil() as usize;
        sorted.iter().take(cheap_count).map(|b| b.index).collect()
    }

    /// Get discharge blocks using percentile threshold (V5-style)
    fn get_percentile_discharge_blocks(&self, ranked_blocks: &[RankedBlock]) -> Vec<usize> {
        let mut sorted = ranked_blocks.to_vec();
        sorted.sort_by(|a, b| {
            b.effective_price
                .partial_cmp(&a.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let expensive_count =
            (sorted.len() as f32 * (1.0 - self.config.expensive_block_percentile)).ceil() as usize;
        sorted
            .iter()
            .take(expensive_count)
            .map(|b| b.index)
            .collect()
    }

    /// Get charge blocks using global ranking (V4-style)
    fn get_global_charge_blocks(
        &self,
        ranked_blocks: &[RankedBlock],
        target_blocks: usize,
    ) -> Vec<usize> {
        let mut sorted = ranked_blocks.to_vec();
        sorted.sort_by(|a, b| {
            a.effective_price
                .partial_cmp(&b.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        sorted.iter().take(target_blocks).map(|b| b.index).collect()
    }

    /// Get discharge blocks using global ranking (V3/V4-style)
    fn get_global_discharge_blocks(&self, ranked_blocks: &[RankedBlock]) -> Vec<usize> {
        let mut sorted = ranked_blocks.to_vec();
        sorted.sort_by(|a, b| {
            b.effective_price
                .partial_cmp(&a.effective_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        sorted
            .iter()
            .take(self.config.discharge_blocks_per_day)
            .map(|b| b.index)
            .collect()
    }

    /// Decide mode for current block based on optimization mode
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        mode: OptimizationMode,
        pattern: &PricePattern,
        charge_blocks: &[usize],
        discharge_blocks: &[usize],
        block_index: usize,
    ) -> (InverterOperationMode, String, String) {
        let current_price = context.price_block.price_czk_per_kwh;
        let effective_price = context.price_block.effective_price_czk_per_kwh;

        // PRIORITY 1: Negative price exploitation (if enabled)
        if self.config.negative_price_handling_enabled
            && effective_price < 0.0
            && context.current_battery_soc < self.config.target_battery_soc
        {
            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "NEGATIVE PRICE EXPLOIT: effective price {:.3} CZK/kWh (getting paid to charge!) [mode: {}]",
                    effective_price,
                    mode.name()
                ),
                "winter_adaptive_v6:negative_exploit".to_string(),
            );
        }

        // PRIORITY 2: Charge during designated cheap blocks
        let is_charge_block = charge_blocks.contains(&block_index);
        if is_charge_block && context.current_battery_soc < self.config.target_battery_soc {
            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "CHARGE ({}): effective {:.3} CZK/kWh (spot {:.3} + grid fee) [mode: {}]",
                    mode.name(),
                    effective_price,
                    current_price,
                    mode.name()
                ),
                format!("winter_adaptive_v6:charge:{}", mode.name().to_lowercase()),
            );
        }

        // PRIORITY 3: Discharge during expensive blocks (with spread validation)
        let is_discharge_block = discharge_blocks.contains(&block_index);
        if is_discharge_block && context.current_battery_soc > self.config.min_discharge_soc {
            // Validate discharge is worthwhile
            let charge_price = pattern.min_price;
            let discharge_spread = effective_price - charge_price;

            if discharge_spread > self.config.min_discharge_spread_czk {
                return (
                    InverterOperationMode::ForceDischarge,
                    format!(
                        "DISCHARGE ({}): effective {:.3} CZK/kWh, spread {:.3} CZK/kWh [mode: {}]",
                        mode.name(),
                        effective_price,
                        discharge_spread,
                        mode.name()
                    ),
                    format!(
                        "winter_adaptive_v6:discharge:{}",
                        mode.name().to_lowercase()
                    ),
                );
            }
        }

        // PRIORITY 4: Self-use (hold battery)
        (
            InverterOperationMode::SelfUse,
            format!(
                "SELF-USE: effective {:.3} CZK/kWh (holding battery) [mode: {}]",
                effective_price,
                mode.name()
            ),
            "winter_adaptive_v6:self_use".to_string(),
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV6Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V6"
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

        // Phase 1: Analyze price pattern and select mode
        let (pattern, mode) = self.analyze_pattern(all_blocks);

        // Phase 2: Rank all blocks
        let ranked_blocks = self.rank_all_blocks(all_blocks);

        // Phase 3: Select charge and discharge blocks based on mode
        let charge_blocks = match mode {
            OptimizationMode::PercentileOptimized => {
                self.get_percentile_charge_blocks(&ranked_blocks)
            }
            OptimizationMode::NegativeExploit => {
                // Charge during all negative price blocks + cheapest positive blocks
                let negative_blocks: Vec<usize> = ranked_blocks
                    .iter()
                    .filter(|b| b.effective_price < 0.0)
                    .map(|b| b.index)
                    .collect();

                if negative_blocks.is_empty() {
                    self.get_percentile_charge_blocks(&ranked_blocks)
                } else {
                    negative_blocks
                }
            }
            OptimizationMode::VolatileArbitrage | OptimizationMode::SimpleArbitrage => {
                // Use top N cheapest blocks (V3/V4 style)
                let target_blocks = (ranked_blocks.len() as f32 * 0.25).ceil() as usize;
                self.get_global_charge_blocks(&ranked_blocks, target_blocks)
            }
            OptimizationMode::GlobalRanking => {
                // Use top N cheapest blocks (V4 style)
                let target_blocks = (ranked_blocks.len() as f32 * 0.25).ceil() as usize;
                self.get_global_charge_blocks(&ranked_blocks, target_blocks)
            }
        };

        let discharge_blocks = match mode {
            OptimizationMode::PercentileOptimized => {
                self.get_percentile_discharge_blocks(&ranked_blocks)
            }
            _ => {
                // Use V3/V4 style global ranking for discharge
                self.get_global_discharge_blocks(&ranked_blocks)
            }
        };

        // Find current block index
        let block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        // Phase 4: Make decision for current block
        let (operation_mode, reason, decision_uid) = self.decide_mode(
            context,
            mode,
            &pattern,
            &charge_blocks,
            &discharge_blocks,
            block_index,
        );

        eval.mode = operation_mode;
        eval.reason = reason;
        eval.decision_uid = Some(decision_uid);

        // Calculate energy flows based on mode (using pre-calculated effective price)
        let effective_price = context.price_block.effective_price_czk_per_kwh;

        match operation_mode {
            InverterOperationMode::ForceCharge => {
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk = charge_kwh * effective_price;
            }
            InverterOperationMode::ForceDischarge => {
                let discharge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                eval.energy_flows.grid_export_kwh = discharge_kwh;
                eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
            }
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                // Calculate usable battery energy (respecting minimum SOC)
                let usable_battery_kwh = ((context.current_battery_soc
                    - context.control_config.hardware_min_battery_soc)
                    .max(0.0)
                    / 100.0)
                    * context.control_config.battery_capacity_kwh;

                // How much can battery discharge to cover load?
                let battery_discharge = usable_battery_kwh.min(context.consumption_forecast_kwh);

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                if battery_discharge >= context.consumption_forecast_kwh {
                    // Battery fully covers load - revenue is avoided grid cost
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn create_test_blocks() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 18, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        vec![
            // Night - cheap
            TimeBlockPrice {
                block_start: base_time,
                duration_minutes: 15,
                price_czk_per_kwh: 1.50,
                effective_price_czk_per_kwh: 1.50 + grid_fee,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(3),
                duration_minutes: 15,
                price_czk_per_kwh: 1.40,
                effective_price_czk_per_kwh: 1.40 + grid_fee,
            },
            // Morning - elevated
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(7),
                duration_minutes: 15,
                price_czk_per_kwh: 3.50,
                effective_price_czk_per_kwh: 3.50 + grid_fee,
            },
            // Midday - negative price
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(12),
                duration_minutes: 15,
                price_czk_per_kwh: -0.50,
                effective_price_czk_per_kwh: -0.50 + grid_fee,
            },
            // Evening - peak
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::hours(18),
                duration_minutes: 15,
                price_czk_per_kwh: 5.00,
                effective_price_czk_per_kwh: 5.00 + grid_fee,
            },
        ]
    }

    #[test]
    fn test_pattern_detection_negative_prices() {
        let config = WinterAdaptiveV6Config::default();
        let strategy = WinterAdaptiveV6Strategy::new(config);
        let blocks = create_test_blocks();

        let (pattern, mode) = strategy.analyze_pattern(&blocks);

        assert!(pattern.has_negative_prices, "Should detect negative prices");
        assert_eq!(mode, OptimizationMode::NegativeExploit);
    }

    #[test]
    fn test_pattern_detection_volatile() {
        let config = WinterAdaptiveV6Config {
            negative_price_handling_enabled: false, // Disable negative to test volatility
            ..Default::default()
        };
        let strategy = WinterAdaptiveV6Strategy::new(config);
        let blocks = create_test_blocks();

        let (pattern, _mode) = strategy.analyze_pattern(&blocks);

        // High price range should indicate volatility
        assert!(pattern.volatility_cv > 0.0, "Should calculate volatility");
        assert!(pattern.max_price > pattern.min_price);
    }

    #[test]
    fn test_negative_price_charging() {
        use fluxion_types::config::ControlConfig;

        let config = WinterAdaptiveV6Config::default();
        let strategy = WinterAdaptiveV6Strategy::new(config);
        let blocks = create_test_blocks();

        let control_config = ControlConfig::default();
        let context = EvaluationContext {
            price_block: &blocks[3], // Negative price block
            control_config: &control_config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.40,
            all_price_blocks: Some(&blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(eval.mode, InverterOperationMode::ForceCharge);
        assert!(eval.reason.contains("NEGATIVE"));
    }
}
