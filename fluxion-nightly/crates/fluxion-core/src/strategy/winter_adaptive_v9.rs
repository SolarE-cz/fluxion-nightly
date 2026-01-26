// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! # Winter Adaptive V9 Strategy - Solar-Aware Morning Peak Optimizer
//!
//! **Status:** Production - Maximum solar utilization with morning peak coverage
//!
//! ## Overview
//!
//! V9 is designed to maximize solar utilization while ensuring morning peak coverage.
//! It combines the best of V7's arbitrage detection with intelligent solar-aware charging.
//!
//! ## Key Design Principles
//!
//! 1. **Solar-First Approach**
//!    - When solar forecast is high, minimize grid charging
//!    - Let solar charge battery naturally during daylight hours
//!    - Only grid-charge to cover the morning peak before solar kicks in
//!
//! 2. **Morning Peak Coverage**
//!    - Morning peak typically 6:00-9:00 AM (configurable)
//!    - Calculate minimum SOC needed to cover morning consumption
//!    - Target ~20% SOC by end of morning peak (leaves headroom for solar)
//!
//! 3. **Arbitrage When Profitable**
//!    - If price spread >= 3 CZK, schedule additional charge blocks
//!    - Discharge during expensive peaks to grid or home
//!    - Works alongside solar-aware logic
//!
//! 4. **Seasonal Adaptation**
//!    - High solar days: Minimal grid charging, rely on solar
//!    - Low solar days: Fall back to V7-style arbitrage
//!    - Hybrid detection based on solar forecast threshold
//!
//! ## Algorithm
//!
//! ### Phase 1: Solar Assessment
//!
//! - Check solar_forecast_remaining_today_kwh against threshold (default: 5 kWh)
//! - High solar: Use morning-peak-only charging strategy
//! - Low solar: Use full arbitrage strategy (like V7)
//!
//! ### Phase 2: Morning Peak Analysis (High Solar Mode)
//!
//! - Identify morning peak blocks (6:00-9:00 AM)
//! - Calculate consumption during morning peak
//! - Determine minimum overnight charge needed to cover peak
//! - Target: End morning peak with ~20% SOC
//!
//! ### Phase 3: Arbitrage Overlay
//!
//! - Find cheapest and most expensive blocks
//! - If spread >= min_arbitrage_spread_czk, add arbitrage opportunities
//! - Schedule additional charge blocks for arbitrage discharge
//!
//! ### Phase 4: Real-Time Execution
//!
//! - ForceCharge in scheduled charge windows
//! - ForceDischarge or SelfUse in discharge windows
//! - SelfUse otherwise (let solar/battery handle naturally)

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive V9 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV9Config {
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    /// Target battery SOC (%) - maximum charge level
    pub target_battery_soc: f32,

    /// Hardware minimum battery SOC (%) - never discharge below this
    pub min_discharge_soc: f32,

    // === Morning Peak Configuration ===
    /// Morning peak start hour (0-23, local time approximation)
    /// Default: 6 (6:00 AM)
    pub morning_peak_start_hour: u8,

    /// Morning peak end hour (0-23, local time approximation)
    /// Default: 9 (9:00 AM)
    pub morning_peak_end_hour: u8,

    /// Target SOC (%) at end of morning peak
    /// This leaves room for solar charging during the day
    /// Default: 20%
    pub target_soc_after_morning_peak: f32,

    /// Average household consumption (kWh) per 15-minute block during morning peak
    /// Default: 0.5 kWh (higher than average due to morning activities)
    pub morning_peak_consumption_per_block_kwh: f32,

    // === Solar Threshold ===
    /// Minimum solar forecast (kWh remaining today) to trigger solar-first mode
    /// Below this, fall back to full arbitrage mode (like V7)
    /// Default: 5.0 kWh
    pub solar_threshold_kwh: f32,

    /// Factor to apply to solar forecast for conservative planning
    /// E.g., 0.7 means we only count on 70% of forecast
    /// Default: 0.7
    pub solar_confidence_factor: f32,

    // === Arbitrage Configuration ===
    /// Minimum price spread (CZK) for arbitrage to be profitable
    /// Only add arbitrage cycles if spread >= this value
    /// Default: 3.0 CZK
    pub min_arbitrage_spread_czk: f32,

    /// Percentile for cheap blocks (0.0-1.0) used for arbitrage charging
    /// Default: 0.25 (bottom 25%)
    pub cheap_block_percentile: f32,

    /// Number of top expensive blocks to target for arbitrage discharge
    /// Default: 8 (2 hours of discharge)
    pub top_discharge_blocks_count: usize,

    // === Export Policy ===
    /// Minimum price spread (CZK) to allow grid export instead of home use
    /// Default: 5.0 CZK
    pub min_export_spread_czk: f32,

    /// Minimum SOC (%) after discharge to allow grid export
    /// Default: 50%
    pub min_soc_after_export: f32,

    // === Safety ===
    /// Round-trip battery efficiency (charge × discharge efficiency)
    /// Default: 0.90 (90%)
    pub battery_round_trip_efficiency: f32,

    /// Enable negative price handling (always charge if getting paid)
    pub negative_price_handling_enabled: bool,

    /// Minimum overnight charge blocks regardless of solar forecast
    /// Safety margin in case solar doesn't perform as expected
    /// Default: 4 (1 hour minimum)
    pub min_overnight_charge_blocks: usize,

    /// Price threshold (CZK/kWh) below which we always charge
    /// Very cheap power is worth grabbing regardless of solar
    /// Default: 1.5 CZK/kWh
    pub opportunistic_charge_threshold_czk: f32,
}

impl Default for WinterAdaptiveV9Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 9,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            // Morning peak config
            morning_peak_start_hour: 6,
            morning_peak_end_hour: 9,
            target_soc_after_morning_peak: 20.0,
            morning_peak_consumption_per_block_kwh: 0.5,
            // Solar threshold
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            // Arbitrage config
            min_arbitrage_spread_czk: 3.0,
            cheap_block_percentile: 0.25,
            top_discharge_blocks_count: 8,
            // Export policy
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            // Safety
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            min_overnight_charge_blocks: 4,
            opportunistic_charge_threshold_czk: 1.5,
        }
    }
}

/// Operating mode determined by solar forecast
#[derive(Debug, Clone, Copy, PartialEq)]
enum OperatingMode {
    /// High solar - minimal grid charging, cover morning peak only
    SolarFirst,
    /// Low solar - full arbitrage mode like V7
    Arbitrage,
    /// Negative prices detected - charge as much as possible
    NegativePrice,
}

/// Scheduled action for a specific block
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScheduledAction {
    /// Charge during this block (overnight or arbitrage)
    Charge { reason: ChargeReason },
    /// Discharge during this block (arbitrage peak)
    Discharge,
    /// No scheduled action - use self-use mode
    SelfUse,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChargeReason {
    /// Overnight charge to cover morning peak
    MorningPeakCoverage,
    /// Opportunistic charge (very cheap price)
    Opportunistic,
    /// Arbitrage charge (for later profitable discharge)
    Arbitrage,
    /// Negative price - getting paid to charge
    NegativePrice,
}

/// Planning result for the day
#[derive(Debug)]
struct DayPlan {
    /// Scheduled actions for each block
    schedule: Vec<ScheduledAction>,
    /// Operating mode used
    mode: OperatingMode,
    /// Average charge price (for scheduled charge blocks)
    #[allow(dead_code)]
    avg_charge_price: f32,
    /// Average discharge price (for scheduled discharge blocks)
    #[allow(dead_code)]
    avg_discharge_price: f32,
    /// Expected profit per kWh from arbitrage
    arbitrage_profit_per_kwh: f32,
    /// Number of blocks allocated for morning peak coverage
    morning_peak_charge_blocks: usize,
    /// Number of blocks allocated for arbitrage
    arbitrage_charge_blocks: usize,
}

pub struct WinterAdaptiveV9Strategy {
    config: WinterAdaptiveV9Config,
}

impl WinterAdaptiveV9Strategy {
    pub fn new(config: WinterAdaptiveV9Config) -> Self {
        Self { config }
    }

    /// Determine operating mode based on solar forecast and price analysis
    fn determine_mode(&self, blocks: &[TimeBlockPrice], solar_remaining_kwh: f32) -> OperatingMode {
        // Check for negative prices first
        let has_negative = blocks.iter().any(|b| b.effective_price_czk_per_kwh < 0.0);
        if has_negative && self.config.negative_price_handling_enabled {
            return OperatingMode::NegativePrice;
        }

        // Apply confidence factor to solar forecast
        let effective_solar = solar_remaining_kwh * self.config.solar_confidence_factor;

        // Use solar-first mode if enough solar is expected
        if effective_solar >= self.config.solar_threshold_kwh {
            OperatingMode::SolarFirst
        } else {
            OperatingMode::Arbitrage
        }
    }

    /// Check if a block is during morning peak hours
    fn is_morning_peak(&self, block_time: DateTime<Utc>) -> bool {
        let hour = block_time.hour() as u8;
        hour >= self.config.morning_peak_start_hour && hour < self.config.morning_peak_end_hour
    }

    /// Check if a block is overnight (before morning peak)
    fn is_overnight(&self, block_time: DateTime<Utc>) -> bool {
        let hour = block_time.hour() as u8;
        hour < self.config.morning_peak_start_hour
    }

    /// Calculate how many overnight charge blocks are needed for morning peak coverage
    fn calculate_morning_peak_charge_need(
        &self,
        blocks: &[TimeBlockPrice],
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
    ) -> usize {
        // Count morning peak blocks
        let morning_peak_blocks = blocks
            .iter()
            .filter(|b| self.is_morning_peak(b.block_start))
            .count();

        // Estimate consumption during morning peak
        let peak_consumption_kwh =
            morning_peak_blocks as f32 * self.config.morning_peak_consumption_per_block_kwh;

        // Calculate SOC needed to cover morning peak and end at target SOC
        // target_soc_after = current_soc - consumption_soc_delta + charge_soc_delta
        // We want: current_soc + charge_delta - consumption_delta = target_soc_after
        // So: charge_delta = target_soc_after - current_soc + consumption_delta

        let consumption_soc_delta = (peak_consumption_kwh / battery_capacity_kwh) * 100.0;
        let target_soc = self.config.target_soc_after_morning_peak;

        // How much SOC we need to add
        let soc_needed = (target_soc - current_soc + consumption_soc_delta).max(0.0);

        if soc_needed <= 0.0 {
            return 0;
        }

        // Convert to kWh
        let kwh_needed = (soc_needed / 100.0) * battery_capacity_kwh;

        // Convert to blocks
        let charge_per_block =
            max_charge_rate_kw * 0.25 * self.config.battery_round_trip_efficiency;
        let blocks_needed = (kwh_needed / charge_per_block).ceil() as usize;

        // Ensure minimum and don't exceed overnight hours
        let overnight_blocks = blocks
            .iter()
            .filter(|b| self.is_overnight(b.block_start))
            .count();

        blocks_needed
            .max(self.config.min_overnight_charge_blocks)
            .min(overnight_blocks)
    }

    /// Find the cheapest N overnight blocks for morning peak charging
    fn find_cheapest_overnight_blocks(
        &self,
        blocks: &[TimeBlockPrice],
        count: usize,
    ) -> Vec<usize> {
        let mut overnight: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| self.is_overnight(b.block_start))
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        overnight.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        overnight.into_iter().take(count).map(|(i, _)| i).collect()
    }

    /// Find opportunistic charge blocks (very cheap prices)
    fn find_opportunistic_blocks(&self, blocks: &[TimeBlockPrice]) -> Vec<usize> {
        blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.effective_price_czk_per_kwh < self.config.opportunistic_charge_threshold_czk
                    || b.effective_price_czk_per_kwh < 0.0
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Find arbitrage opportunities - cheap blocks to charge, expensive to discharge
    fn find_arbitrage_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
    ) -> (Vec<usize>, Vec<usize>, f32) {
        if blocks.is_empty() {
            return (Vec::new(), Vec::new(), 0.0);
        }

        // Find cheapest blocks
        let n = blocks.len();
        let cheap_count = ((n as f32) * self.config.cheap_block_percentile).ceil() as usize;

        let mut ranked: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        // Sort by price ascending
        ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let charge_blocks: Vec<usize> = ranked.iter().take(cheap_count).map(|(i, _)| *i).collect();

        // Find most expensive blocks
        let discharge_count = self.config.top_discharge_blocks_count.min(n);
        let discharge_blocks: Vec<usize> = ranked
            .iter()
            .rev()
            .take(discharge_count)
            .map(|(i, _)| *i)
            .collect();

        // Calculate profit
        let avg_charge = if charge_blocks.is_empty() {
            0.0
        } else {
            charge_blocks
                .iter()
                .filter_map(|&i| blocks.get(i))
                .map(|b| b.effective_price_czk_per_kwh)
                .sum::<f32>()
                / charge_blocks.len() as f32
        };

        let avg_discharge = if discharge_blocks.is_empty() {
            0.0
        } else {
            discharge_blocks
                .iter()
                .filter_map(|&i| blocks.get(i))
                .map(|b| b.effective_price_czk_per_kwh)
                .sum::<f32>()
                / discharge_blocks.len() as f32
        };

        let profit = (avg_discharge - avg_charge) * self.config.battery_round_trip_efficiency;

        (charge_blocks, discharge_blocks, profit)
    }

    /// Generate the day plan based on mode and opportunities
    fn generate_plan(
        &self,
        blocks: &[TimeBlockPrice],
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
        solar_remaining_kwh: f32,
    ) -> DayPlan {
        let mode = self.determine_mode(blocks, solar_remaining_kwh);
        let mut schedule = vec![ScheduledAction::SelfUse; blocks.len()];
        let mut morning_peak_charge_blocks = 0;
        let mut arbitrage_charge_blocks = 0;
        let avg_charge_price: f32;
        let mut avg_discharge_price = 0.0;
        let mut arbitrage_profit = 0.0;

        match mode {
            OperatingMode::NegativePrice => {
                // Charge on all negative price blocks + cheapest positive blocks
                let negative_blocks: Vec<usize> = blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.effective_price_czk_per_kwh < 0.0)
                    .map(|(i, _)| i)
                    .collect();

                for &idx in &negative_blocks {
                    schedule[idx] = ScheduledAction::Charge {
                        reason: ChargeReason::NegativePrice,
                    };
                }

                // Also find arbitrage opportunities for non-negative blocks
                let (arb_charge, arb_discharge, profit) = self.find_arbitrage_opportunities(blocks);

                if profit >= self.config.min_arbitrage_spread_czk {
                    for &idx in &arb_charge {
                        if !negative_blocks.contains(&idx) {
                            schedule[idx] = ScheduledAction::Charge {
                                reason: ChargeReason::Arbitrage,
                            };
                            arbitrage_charge_blocks += 1;
                        }
                    }
                    for &idx in &arb_discharge {
                        schedule[idx] = ScheduledAction::Discharge;
                    }
                    arbitrage_profit = profit;
                }

                avg_charge_price = negative_blocks
                    .iter()
                    .filter_map(|&i| blocks.get(i))
                    .map(|b| b.effective_price_czk_per_kwh)
                    .sum::<f32>()
                    / negative_blocks.len().max(1) as f32;
            }

            OperatingMode::SolarFirst => {
                // Step 1: Calculate overnight charge needed for morning peak
                let morning_blocks_needed = self.calculate_morning_peak_charge_need(
                    blocks,
                    current_soc,
                    battery_capacity_kwh,
                    max_charge_rate_kw,
                );

                // Step 2: Find cheapest overnight blocks
                let overnight_charge =
                    self.find_cheapest_overnight_blocks(blocks, morning_blocks_needed);
                morning_peak_charge_blocks = overnight_charge.len();

                for &idx in &overnight_charge {
                    schedule[idx] = ScheduledAction::Charge {
                        reason: ChargeReason::MorningPeakCoverage,
                    };
                }

                avg_charge_price = if overnight_charge.is_empty() {
                    0.0
                } else {
                    overnight_charge
                        .iter()
                        .filter_map(|&i| blocks.get(i))
                        .map(|b| b.effective_price_czk_per_kwh)
                        .sum::<f32>()
                        / overnight_charge.len() as f32
                };

                // Step 3: Add opportunistic charging (very cheap prices)
                let opportunistic = self.find_opportunistic_blocks(blocks);
                for &idx in &opportunistic {
                    if matches!(schedule[idx], ScheduledAction::SelfUse) {
                        schedule[idx] = ScheduledAction::Charge {
                            reason: ChargeReason::Opportunistic,
                        };
                    }
                }

                // Step 4: Check if arbitrage is still profitable
                let (arb_charge, arb_discharge, profit) = self.find_arbitrage_opportunities(blocks);

                if profit >= self.config.min_arbitrage_spread_czk {
                    // Add arbitrage charge blocks (only if not already scheduled)
                    for &idx in &arb_charge {
                        if matches!(schedule[idx], ScheduledAction::SelfUse) {
                            schedule[idx] = ScheduledAction::Charge {
                                reason: ChargeReason::Arbitrage,
                            };
                            arbitrage_charge_blocks += 1;
                        }
                    }

                    // Add discharge blocks
                    for &idx in &arb_discharge {
                        // Don't discharge during morning peak (we need that energy)
                        if !self.is_morning_peak(blocks[idx].block_start) {
                            schedule[idx] = ScheduledAction::Discharge;
                        }
                    }

                    arbitrage_profit = profit;
                    avg_discharge_price = arb_discharge
                        .iter()
                        .filter_map(|&i| blocks.get(i))
                        .map(|b| b.effective_price_czk_per_kwh)
                        .sum::<f32>()
                        / arb_discharge.len().max(1) as f32;
                }
            }

            OperatingMode::Arbitrage => {
                // Full arbitrage mode like V7
                let (arb_charge, arb_discharge, profit) = self.find_arbitrage_opportunities(blocks);

                if profit >= self.config.min_arbitrage_spread_czk {
                    for &idx in &arb_charge {
                        schedule[idx] = ScheduledAction::Charge {
                            reason: ChargeReason::Arbitrage,
                        };
                        arbitrage_charge_blocks += 1;
                    }

                    for &idx in &arb_discharge {
                        schedule[idx] = ScheduledAction::Discharge;
                    }

                    arbitrage_profit = profit;
                    avg_charge_price = arb_charge
                        .iter()
                        .filter_map(|&i| blocks.get(i))
                        .map(|b| b.effective_price_czk_per_kwh)
                        .sum::<f32>()
                        / arb_charge.len().max(1) as f32;

                    avg_discharge_price = arb_discharge
                        .iter()
                        .filter_map(|&i| blocks.get(i))
                        .map(|b| b.effective_price_czk_per_kwh)
                        .sum::<f32>()
                        / arb_discharge.len().max(1) as f32;
                } else {
                    // Even if not profitable, charge in cheapest blocks
                    for &idx in &arb_charge {
                        schedule[idx] = ScheduledAction::Charge {
                            reason: ChargeReason::Arbitrage,
                        };
                        arbitrage_charge_blocks += 1;
                    }
                    avg_charge_price = arb_charge
                        .iter()
                        .filter_map(|&i| blocks.get(i))
                        .map(|b| b.effective_price_czk_per_kwh)
                        .sum::<f32>()
                        / arb_charge.len().max(1) as f32;
                }

                // Also add opportunistic charging
                let opportunistic = self.find_opportunistic_blocks(blocks);
                for &idx in &opportunistic {
                    if matches!(schedule[idx], ScheduledAction::SelfUse) {
                        schedule[idx] = ScheduledAction::Charge {
                            reason: ChargeReason::Opportunistic,
                        };
                    }
                }
            }
        }

        DayPlan {
            schedule,
            mode,
            avg_charge_price,
            avg_discharge_price,
            arbitrage_profit_per_kwh: arbitrage_profit,
            morning_peak_charge_blocks,
            arbitrage_charge_blocks,
        }
    }

    /// Decide whether to export to grid or use for home
    fn should_export_to_grid(
        &self,
        current_soc: f32,
        discharge_price: f32,
        min_price: f32,
        battery_capacity_kwh: f32,
    ) -> bool {
        let spread = discharge_price - min_price;

        if spread < self.config.min_export_spread_czk {
            return false;
        }

        // Predict SOC after discharge
        let discharge_kwh = battery_capacity_kwh * 0.1;
        let predicted_soc_after = current_soc - (discharge_kwh / battery_capacity_kwh * 100.0);

        predicted_soc_after >= self.config.min_soc_after_export
    }

    /// Get minimum price in horizon
    fn min_price(&self, blocks: &[TimeBlockPrice]) -> f32 {
        blocks
            .iter()
            .map(|b| b.effective_price_czk_per_kwh)
            .fold(f32::INFINITY, f32::min)
    }

    /// Generate summary string for logging
    fn generate_summary(&self, plan: &DayPlan) -> String {
        let mode_str = match plan.mode {
            OperatingMode::SolarFirst => "SOLAR-FIRST",
            OperatingMode::Arbitrage => "ARBITRAGE",
            OperatingMode::NegativePrice => "NEGATIVE",
        };

        if plan.arbitrage_profit_per_kwh > 0.0 {
            format!(
                "{}: {} peak/{} arb blocks, {:.1} CZK/kWh profit",
                mode_str,
                plan.morning_peak_charge_blocks,
                plan.arbitrage_charge_blocks,
                plan.arbitrage_profit_per_kwh
            )
        } else {
            format!(
                "{}: {} peak/{} arb blocks",
                mode_str, plan.morning_peak_charge_blocks, plan.arbitrage_charge_blocks
            )
        }
    }
}

impl EconomicStrategy for WinterAdaptiveV9Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V9"
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
            battery_wear_cost_czk_per_kwh: 0.0,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available".to_string();
            return eval;
        };

        // Generate the day plan
        let plan = self.generate_plan(
            all_blocks,
            context.current_battery_soc,
            context.control_config.battery_capacity_kwh,
            context.control_config.max_battery_charge_rate_kw,
            context.solar_forecast_remaining_today_kwh,
        );

        let summary = self.generate_summary(&plan);

        // Find current block index
        let block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let current_action = plan
            .schedule
            .get(block_index)
            .copied()
            .unwrap_or(ScheduledAction::SelfUse);

        let effective_price = context.price_block.effective_price_czk_per_kwh;
        let min_price = self.min_price(all_blocks);

        match current_action {
            ScheduledAction::Charge { reason } => {
                if context.current_battery_soc < self.config.target_battery_soc {
                    eval.mode = InverterOperationMode::ForceCharge;

                    let reason_str = match reason {
                        ChargeReason::MorningPeakCoverage => "MORNING PEAK",
                        ChargeReason::Opportunistic => "OPPORTUNISTIC",
                        ChargeReason::Arbitrage => "ARBITRAGE",
                        ChargeReason::NegativePrice => "NEGATIVE PRICE",
                    };

                    eval.reason = format!(
                        "{} CHARGE: {:.3} CZK/kWh [{}]",
                        reason_str, effective_price, summary
                    );
                    eval.decision_uid = Some(format!(
                        "winter_adaptive_v9:charge:{}",
                        reason_str.to_lowercase().replace(' ', "_")
                    ));

                    // Calculate energy flows
                    let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                    let available_excess =
                        context.solar_forecast_kwh + (-context.consumption_forecast_kwh).max(0.0);
                    let grid_charge_needed = (charge_kwh - available_excess).max(0.0);

                    eval.energy_flows.battery_charge_kwh = charge_kwh;
                    eval.energy_flows.grid_import_kwh = grid_charge_needed;
                    eval.cost_czk = grid_charge_needed * effective_price;

                    let excess_after_charge = (available_excess - charge_kwh).max(0.0);
                    if excess_after_charge > 0.0 {
                        eval.energy_flows.grid_export_kwh = excess_after_charge;
                        eval.revenue_czk =
                            excess_after_charge * context.grid_export_price_czk_per_kwh;
                    }
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: Battery at target ({:.1}%) [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v9:self_use".to_string());
                }
            }

            ScheduledAction::Discharge => {
                if context.current_battery_soc > self.config.min_discharge_soc {
                    let should_export = self.should_export_to_grid(
                        context.current_battery_soc,
                        effective_price,
                        min_price,
                        context.control_config.battery_capacity_kwh,
                    );

                    if should_export {
                        eval.mode = InverterOperationMode::ForceDischarge;
                        eval.reason = format!(
                            "DISCHARGE→GRID: {:.3} CZK/kWh (profit: {:.2}) [{}]",
                            effective_price, plan.arbitrage_profit_per_kwh, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v9:discharge_grid".to_string());

                        let discharge_kwh =
                            context.control_config.max_battery_charge_rate_kw * 0.25;
                        eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                        eval.energy_flows.grid_export_kwh = discharge_kwh;
                        eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
                    } else {
                        eval.mode = InverterOperationMode::SelfUse;
                        eval.reason = format!(
                            "DISCHARGE→HOME: {:.3} CZK/kWh [{}]",
                            effective_price, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v9:discharge_home".to_string());

                        let net_consumption =
                            context.consumption_forecast_kwh - context.solar_forecast_kwh;

                        if net_consumption > 0.0 {
                            let usable_battery_kwh = ((context.current_battery_soc
                                - self.config.min_discharge_soc)
                                .max(0.0)
                                / 100.0)
                                * context.control_config.battery_capacity_kwh;

                            let battery_discharge = usable_battery_kwh.min(net_consumption);
                            eval.energy_flows.battery_discharge_kwh = battery_discharge;

                            if battery_discharge >= net_consumption {
                                eval.revenue_czk = net_consumption * effective_price;
                            } else {
                                eval.revenue_czk = battery_discharge * effective_price;
                                let grid_needed = net_consumption - battery_discharge;
                                eval.cost_czk = grid_needed * effective_price;
                                eval.energy_flows.grid_import_kwh = grid_needed;
                            }
                        } else {
                            let excess = -net_consumption;
                            let battery_capacity = context.control_config.battery_capacity_kwh;
                            let available_charge_capacity = (battery_capacity
                                * (context.control_config.max_battery_soc / 100.0)
                                - battery_capacity * (context.current_battery_soc / 100.0))
                                .max(0.0);
                            let max_charge_rate =
                                context.control_config.max_battery_charge_rate_kw * 0.25;
                            let charge_amount =
                                excess.min(available_charge_capacity).min(max_charge_rate);

                            eval.energy_flows.battery_charge_kwh = charge_amount;

                            let export_amount = excess - charge_amount;
                            if export_amount > 0.0 {
                                eval.energy_flows.grid_export_kwh = export_amount;
                                eval.revenue_czk =
                                    export_amount * context.grid_export_price_czk_per_kwh;
                            }
                        }
                    }
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: Low SOC ({:.1}%) [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v9:self_use".to_string());
                }
            }

            ScheduledAction::SelfUse => {
                // Check for unscheduled negative prices
                if self.config.negative_price_handling_enabled && effective_price < 0.0 {
                    if context.current_battery_soc < self.config.target_battery_soc {
                        eval.mode = InverterOperationMode::ForceCharge;
                        eval.reason = format!(
                            "NEGATIVE PRICE: {:.3} CZK/kWh (getting paid!) [{}]",
                            effective_price, summary
                        );
                        eval.decision_uid = Some("winter_adaptive_v9:negative_price".to_string());

                        let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                        let available_excess = context.solar_forecast_kwh
                            + (-context.consumption_forecast_kwh).max(0.0);
                        let grid_charge_needed = (charge_kwh - available_excess).max(0.0);

                        eval.energy_flows.battery_charge_kwh = charge_kwh;
                        eval.energy_flows.grid_import_kwh = grid_charge_needed;
                        eval.cost_czk = grid_charge_needed * effective_price;

                        let excess_after_charge = (available_excess - charge_kwh).max(0.0);
                        if excess_after_charge > 0.0 {
                            eval.energy_flows.grid_export_kwh = excess_after_charge;
                            eval.revenue_czk =
                                excess_after_charge * context.grid_export_price_czk_per_kwh;
                        }
                    }
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!("SELF-USE: {:.3} CZK/kWh [{}]", effective_price, summary);
                    eval.decision_uid = Some("winter_adaptive_v9:self_use".to_string());

                    let net_consumption =
                        context.consumption_forecast_kwh - context.solar_forecast_kwh;

                    if net_consumption > 0.0 {
                        let usable_battery_kwh = ((context.current_battery_soc
                            - context.control_config.hardware_min_battery_soc)
                            .max(0.0)
                            / 100.0)
                            * context.control_config.battery_capacity_kwh;

                        let battery_discharge = usable_battery_kwh.min(net_consumption);
                        eval.energy_flows.battery_discharge_kwh = battery_discharge;

                        if battery_discharge >= net_consumption {
                            eval.revenue_czk = net_consumption * effective_price;
                        } else {
                            eval.revenue_czk = battery_discharge * effective_price;
                            let grid_needed = net_consumption - battery_discharge;
                            eval.cost_czk = grid_needed * effective_price;
                            eval.energy_flows.grid_import_kwh = grid_needed;
                        }
                    } else {
                        let excess = -net_consumption;
                        let battery_capacity = context.control_config.battery_capacity_kwh;
                        let available_charge_capacity = (battery_capacity
                            * (context.control_config.max_battery_soc / 100.0)
                            - battery_capacity * (context.current_battery_soc / 100.0))
                            .max(0.0);
                        let max_charge_rate =
                            context.control_config.max_battery_charge_rate_kw * 0.25;
                        let charge_amount =
                            excess.min(available_charge_capacity).min(max_charge_rate);

                        eval.energy_flows.battery_charge_kwh = charge_amount;

                        let export_amount = excess - charge_amount;
                        if export_amount > 0.0 {
                            eval.energy_flows.grid_export_kwh = export_amount;
                            eval.revenue_czk =
                                export_amount * context.grid_export_price_czk_per_kwh;
                        }
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
    use chrono::TimeZone;

    fn create_test_blocks_with_prices() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        let mut blocks = Vec::new();

        // Full 24-hour day with 15-minute blocks
        for hour in 0..24 {
            for quarter in 0..4 {
                let price = match hour {
                    0..=5 => 1.5,   // Overnight cheap
                    6..=8 => 5.0,   // Morning peak
                    9..=11 => 2.5,  // Mid-morning
                    12..=14 => 0.5, // Solar dip (cheap)
                    15..=17 => 4.5, // Afternoon expensive
                    18..=20 => 6.0, // Evening peak
                    _ => 2.0,       // Night
                };

                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: price,
                    effective_price_czk_per_kwh: price + grid_fee,
                });
            }
        }

        blocks
    }

    #[test]
    fn test_mode_detection_high_solar() {
        let config = WinterAdaptiveV9Config::default();
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        let mode = strategy.determine_mode(&blocks, 10.0); // High solar
        assert_eq!(
            mode,
            OperatingMode::SolarFirst,
            "High solar should use SolarFirst mode"
        );
    }

    #[test]
    fn test_mode_detection_low_solar() {
        let config = WinterAdaptiveV9Config::default();
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        let mode = strategy.determine_mode(&blocks, 2.0); // Low solar
        assert_eq!(
            mode,
            OperatingMode::Arbitrage,
            "Low solar should use Arbitrage mode"
        );
    }

    #[test]
    fn test_morning_peak_detection() {
        let config = WinterAdaptiveV9Config {
            morning_peak_start_hour: 6,
            morning_peak_end_hour: 9,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();

        assert!(
            !strategy.is_morning_peak(base_time + chrono::Duration::hours(5)),
            "5 AM should not be morning peak"
        );
        assert!(
            strategy.is_morning_peak(base_time + chrono::Duration::hours(6)),
            "6 AM should be morning peak"
        );
        assert!(
            strategy.is_morning_peak(base_time + chrono::Duration::hours(8)),
            "8 AM should be morning peak"
        );
        assert!(
            !strategy.is_morning_peak(base_time + chrono::Duration::hours(9)),
            "9 AM should not be morning peak"
        );
    }

    #[test]
    fn test_overnight_detection() {
        let config = WinterAdaptiveV9Config {
            morning_peak_start_hour: 6,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();

        assert!(strategy.is_overnight(base_time), "0 AM should be overnight");
        assert!(
            strategy.is_overnight(base_time + chrono::Duration::hours(3)),
            "3 AM should be overnight"
        );
        assert!(
            strategy.is_overnight(base_time + chrono::Duration::hours(5)),
            "5 AM should be overnight"
        );
        assert!(
            !strategy.is_overnight(base_time + chrono::Duration::hours(6)),
            "6 AM should not be overnight"
        );
    }

    #[test]
    fn test_solar_first_plan_minimal_charging() {
        let config = WinterAdaptiveV9Config {
            target_soc_after_morning_peak: 20.0,
            solar_threshold_kwh: 5.0,
            min_overnight_charge_blocks: 4,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        // High solar, battery already at 50%
        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 15.0);

        assert_eq!(plan.mode, OperatingMode::SolarFirst);

        // Should have minimal overnight charging (just for morning peak)
        let charge_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge { .. }))
            .count();

        println!(
            "Solar-first mode charge blocks: {} (peak: {}, arb: {})",
            charge_count, plan.morning_peak_charge_blocks, plan.arbitrage_charge_blocks
        );

        // Should have at least minimum blocks
        assert!(
            charge_count >= 4,
            "Should have at least min_overnight_charge_blocks"
        );
    }

    #[test]
    fn test_arbitrage_mode_full_charging() {
        let config = WinterAdaptiveV9Config::default();
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        // Low solar - arbitrage mode
        let plan = strategy.generate_plan(&blocks, 30.0, 10.0, 3.0, 2.0);

        assert_eq!(plan.mode, OperatingMode::Arbitrage);

        let charge_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge { .. }))
            .count();

        let discharge_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Discharge))
            .count();

        println!(
            "Arbitrage mode: {} charge, {} discharge blocks, {:.2} CZK/kWh profit",
            charge_count, discharge_count, plan.arbitrage_profit_per_kwh
        );

        // Should have more charge blocks in arbitrage mode
        assert!(charge_count > 8, "Arbitrage should have many charge blocks");
    }

    #[test]
    fn test_morning_peak_charge_calculation() {
        let config = WinterAdaptiveV9Config {
            target_soc_after_morning_peak: 20.0,
            morning_peak_consumption_per_block_kwh: 0.5,
            morning_peak_start_hour: 6,
            morning_peak_end_hour: 9,
            min_overnight_charge_blocks: 2,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        // 12 blocks in morning peak (3 hours * 4 blocks/hour)
        // Consumption: 12 * 0.5 = 6 kWh
        // With 10 kWh battery, that's 60% SOC consumed
        // Target: 20% at end
        // So we need: current_soc + charge - 60% = 20%
        // If current is 30%: 30 + charge - 60 = 20 -> charge = 50%

        let blocks_needed = strategy.calculate_morning_peak_charge_need(&blocks, 30.0, 10.0, 3.0);

        println!("Morning peak charge blocks needed: {}", blocks_needed);

        // Should need multiple blocks to charge 50%
        assert!(
            blocks_needed >= 2,
            "Should need at least a few charge blocks"
        );
    }

    #[test]
    fn test_arbitrage_with_profit_threshold() {
        let config = WinterAdaptiveV9Config {
            min_arbitrage_spread_czk: 3.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        let (charge, discharge, profit) = strategy.find_arbitrage_opportunities(&blocks);

        println!(
            "Arbitrage: {} charge, {} discharge blocks, {:.2} CZK/kWh profit",
            charge.len(),
            discharge.len(),
            profit
        );

        // Our test blocks have good spread (overnight 1.5 vs evening 6.0)
        // With grid fee 1.8: cheap=3.3, expensive=7.8, spread=4.5
        assert!(
            profit >= 3.0,
            "Test blocks should have profitable arbitrage"
        );
        assert!(!discharge.is_empty(), "Should have discharge blocks");
    }

    #[test]
    fn test_opportunistic_charging() {
        let config = WinterAdaptiveV9Config {
            opportunistic_charge_threshold_czk: 3.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();

        let opportunistic = strategy.find_opportunistic_blocks(&blocks);

        println!("Opportunistic charge blocks: {}", opportunistic.len());

        // Should find blocks during solar dip (12-14) with price 0.5 + 1.8 = 2.3 < 3.0
        assert!(
            !opportunistic.is_empty(),
            "Should find opportunistic blocks"
        );
    }
}
