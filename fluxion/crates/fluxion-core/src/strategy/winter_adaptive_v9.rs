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
    /// Hold battery at target SOC using BackUpMode after charging completes.
    /// Grid powers house while battery is preserved for upcoming expensive hours.
    HoldCharge,
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

    /// Calculate how many charge blocks are needed to reach target SOC from current SOC
    fn calculate_charge_blocks_needed(
        &self,
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
    ) -> usize {
        let soc_needed = (self.config.target_battery_soc - current_soc).max(0.0);
        if soc_needed <= 0.0 {
            return 0;
        }
        let kwh_needed = (soc_needed / 100.0) * battery_capacity_kwh;
        let charge_per_block =
            max_charge_rate_kw * 0.25 * self.config.battery_round_trip_efficiency;
        if charge_per_block <= 0.0 {
            return 0;
        }
        // Add +2 buffer to ensure we overshoot the target SOC.
        // Prefer charging too much over too little - real-world charge rates
        // taper at high SOC, household consumption reduces net charge per block,
        // and the scheduler's CFC post-processor may remove isolated blocks.
        // After reaching target, the HoldCharge logic preserves the SOC using
        // BackUpMode until expensive hours begin.
        (kwh_needed / charge_per_block).ceil() as usize + 2
    }

    /// Find arbitrage opportunities - cheap blocks to charge, expensive to discharge
    ///
    /// When `max_charge_blocks` is Some(n), only the cheapest n blocks are selected
    /// for charging instead of the full cheapest percentile. This ensures the strategy
    /// picks the absolute cheapest blocks when battery capacity is limited.
    fn find_arbitrage_opportunities(
        &self,
        blocks: &[TimeBlockPrice],
        max_charge_blocks: Option<usize>,
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

        // If max_charge_blocks is set, limit to the cheapest N blocks needed.
        // Otherwise use the full cheapest percentile as before.
        let effective_charge_count = match max_charge_blocks {
            Some(limit) => limit.min(cheap_count),
            None => cheap_count,
        };

        let mut charge_blocks: Vec<usize> = ranked
            .iter()
            .take(effective_charge_count)
            .map(|(i, _)| *i)
            .collect();

        // Ensure charge blocks form consecutive groups of at least 2 so they
        // survive the scheduler's remove_short_force_sequences post-processor.
        // Isolated blocks get their cheapest immediate neighbor added.
        Self::ensure_consecutive_charge_groups(&mut charge_blocks, blocks);

        // Bridge short gaps (< 2 blocks) between charge groups to form unified
        // continuous sequences. A 1-block SelfUse gap between two charge groups
        // is wasteful - the mode switch doesn't make sense for such a short window.
        Self::bridge_short_charge_gaps(&mut charge_blocks, blocks, 2);

        // Find most expensive blocks (excluding any charge blocks)
        let charge_set: std::collections::HashSet<usize> =
            charge_blocks.iter().copied().collect();
        let discharge_count = self.config.top_discharge_blocks_count.min(n);
        let discharge_blocks: Vec<usize> = ranked
            .iter()
            .rev()
            .filter(|(i, _)| !charge_set.contains(i))
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

    /// Ensure all selected charge blocks form consecutive groups of at least 2.
    ///
    /// The scheduler's `remove_short_force_sequences` post-processor removes
    /// ForceCharge blocks that appear in sequences shorter than
    /// `min_consecutive_force_blocks` (default 2). For any isolated charge block,
    /// this adds its cheapest immediate neighbor to form a valid pair.
    fn ensure_consecutive_charge_groups(
        charge_indices: &mut Vec<usize>,
        blocks: &[TimeBlockPrice],
    ) {
        use std::collections::HashSet;

        let total = blocks.len();
        let mut charge_set: HashSet<usize> = charge_indices.iter().copied().collect();
        let mut additions = Vec::new();

        for &idx in charge_indices.iter() {
            let has_neighbor = (idx > 0 && charge_set.contains(&(idx - 1)))
                || (idx + 1 < total && charge_set.contains(&(idx + 1)));
            if has_neighbor {
                continue;
            }

            // Isolated block - find cheapest immediate neighbor to pair with
            let prev = if idx > 0 && !charge_set.contains(&(idx - 1)) {
                Some((idx - 1, blocks[idx - 1].effective_price_czk_per_kwh))
            } else {
                None
            };
            let next = if idx + 1 < total && !charge_set.contains(&(idx + 1)) {
                Some((idx + 1, blocks[idx + 1].effective_price_czk_per_kwh))
            } else {
                None
            };

            let best = match (prev, next) {
                (Some((pi, pp)), Some((ni, np))) => {
                    if pp <= np {
                        Some(pi)
                    } else {
                        Some(ni)
                    }
                }
                (Some((pi, _)), None) => Some(pi),
                (None, Some((ni, _))) => Some(ni),
                (None, None) => None,
            };

            if let Some(neighbor) = best
                && !charge_set.contains(&neighbor)
            {
                additions.push(neighbor);
                charge_set.insert(neighbor);
            }
        }

        charge_indices.extend(additions);
    }

    /// Bridge short gaps between charge groups.
    ///
    /// When two charge groups are separated by fewer than `min_gap` SelfUse blocks,
    /// convert those gap blocks to Charge so the groups merge into one continuous
    /// sequence. This prevents the scheduler from switching modes back and forth
    /// for trivially short non-charge windows (e.g., 1 block of SelfUse between
    /// two ForceCharge groups makes no sense).
    fn bridge_short_charge_gaps(
        charge_indices: &mut Vec<usize>,
        blocks: &[TimeBlockPrice],
        min_gap: usize,
    ) {
        use std::collections::HashSet;
        let mut charge_set: HashSet<usize> = charge_indices.iter().copied().collect();
        if charge_set.is_empty() {
            return;
        }

        let mut sorted: Vec<usize> = charge_indices.clone();
        sorted.sort_unstable();
        sorted.dedup();

        let mut additions = Vec::new();
        for window in sorted.windows(2) {
            let (a, b) = (window[0], window[1]);
            let gap = b - a - 1;
            if gap > 0 && gap < min_gap && b < blocks.len() {
                for fill in (a + 1)..b {
                    if !charge_set.contains(&fill) {
                        additions.push(fill);
                        charge_set.insert(fill);
                    }
                }
            }
        }

        charge_indices.extend(additions);
    }

    /// Remove short SelfUse gaps in the final schedule.
    ///
    /// After the schedule is built, scan for SelfUse sequences between Charge
    /// groups (or at the start of the schedule before the first Charge group)
    /// that are shorter than `min_gap` blocks. Convert those SelfUse blocks to
    /// Charge, creating continuous charge sequences.
    ///
    /// This mirrors the scheduler's `remove_short_force_sequences` rule
    /// symmetrically: just as short ForceCharge sequences get removed by the
    /// scheduler, short non-charge gaps between charge groups should be filled
    /// by the strategy so the scheduler never sees them.
    ///
    /// The leading-gap case is critical: because the scheduler passes a sliding
    /// window (`all_price_blocks` starts at the current block), a block that was
    /// part of a charge group in the previous evaluation can become the first
    /// block in a new, smaller window. If the recalculated plan no longer
    /// selects it, a 1-block SelfUse gap appears at the window's leading edge.
    fn remove_short_selfuse_gaps_in_schedule(
        schedule: &mut [ScheduledAction],
        blocks: &[TimeBlockPrice],
        min_gap: usize,
    ) {
        if schedule.is_empty() || min_gap == 0 {
            return;
        }

        // Calculate average charge price for price sanity check
        let charge_prices: Vec<f32> = schedule
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .filter_map(|(i, _)| blocks.get(i).map(|b| b.effective_price_czk_per_kwh))
            .collect();

        if charge_prices.is_empty() {
            return;
        }

        let avg_charge_price = charge_prices.iter().sum::<f32>() / charge_prices.len() as f32;
        // Only bridge gaps where the price is within 1 CZK of the average charge price
        let price_tolerance = 1.0;

        let mut i = 0;
        while i < schedule.len() {
            if matches!(schedule[i], ScheduledAction::SelfUse) {
                let gap_start = i;
                while i < schedule.len() && matches!(schedule[i], ScheduledAction::SelfUse) {
                    i += 1;
                }
                let gap_end = i;
                let gap_len = gap_end - gap_start;

                if gap_len > 0 && gap_len < min_gap {
                    let before_is_charge = gap_start > 0
                        && matches!(schedule[gap_start - 1], ScheduledAction::Charge { .. });
                    let after_is_charge = gap_end < schedule.len()
                        && matches!(schedule[gap_end], ScheduledAction::Charge { .. });

                    // Bridge if:
                    // 1. Gap is between two charge groups (middle gap)
                    // 2. Gap is at the start before a charge group (leading gap from sliding window)
                    let should_bridge = (gap_start == 0 || before_is_charge) && after_is_charge;

                    if should_bridge {
                        // Verify prices are reasonable (not bridging into expensive blocks)
                        let all_prices_ok = (gap_start..gap_end).all(|j| {
                            blocks.get(j).is_some_and(|b| {
                                b.effective_price_czk_per_kwh
                                    <= avg_charge_price + price_tolerance
                            })
                        });

                        if all_prices_ok {
                            #[expect(clippy::needless_range_loop)]
                            for j in gap_start..gap_end {
                                schedule[j] = ScheduledAction::Charge {
                                    reason: ChargeReason::Arbitrage,
                                };
                            }
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    /// Add HoldCharge blocks after the overnight charge window.
    ///
    /// After charging completes, the battery should remain at target SOC using
    /// BackUpMode ("Stop Charge or Discharge") while grid prices are still low.
    /// The hold lasts until the first block where the grid fee jumps significantly
    /// (HDO low→high tariff transition) or until `morning_peak_start_hour`.
    fn add_hold_charge_blocks(
        &self,
        schedule: &mut [ScheduledAction],
        blocks: &[TimeBlockPrice],
    ) {
        // Find the last charge block in the overnight / early morning window
        let last_charge_idx = schedule
            .iter()
            .enumerate()
            .rev()
            .filter(|(i, _)| {
                blocks
                    .get(*i)
                    .is_some_and(|b| (b.block_start.hour() as u8) < self.config.morning_peak_start_hour)
            })
            .find(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .map(|(i, _)| i);

        let Some(last_charge) = last_charge_idx else {
            return;
        };

        // Grid fee at the last charge block (used to detect HDO transition)
        let charge_grid_fee = blocks[last_charge].effective_price_czk_per_kwh
            - blocks[last_charge].price_czk_per_kwh;

        for i in (last_charge + 1)..blocks.len() {
            let block = &blocks[i];
            let grid_fee = block.effective_price_czk_per_kwh - block.price_czk_per_kwh;

            // Stop at HDO high tariff transition (grid fee jumps by >0.5 CZK)
            if grid_fee > charge_grid_fee + 0.5 {
                break;
            }

            // Fallback: stop at morning peak start hour
            if block.block_start.hour() as u8 >= self.config.morning_peak_start_hour {
                break;
            }

            // Stop at discharge blocks
            if matches!(schedule[i], ScheduledAction::Discharge) {
                break;
            }

            // Convert SelfUse blocks to HoldCharge
            if matches!(schedule[i], ScheduledAction::SelfUse) {
                schedule[i] = ScheduledAction::HoldCharge;
            }
        }
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
                // Limit additional charge blocks to remaining battery capacity
                let blocks_needed = self.calculate_charge_blocks_needed(
                    current_soc,
                    battery_capacity_kwh,
                    max_charge_rate_kw,
                );
                let remaining_needed = blocks_needed.saturating_sub(negative_blocks.len());
                let (arb_charge, arb_discharge, profit) =
                    self.find_arbitrage_opportunities(blocks, Some(remaining_needed));

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
                // Limit additional charge to remaining battery capacity after morning peak
                let blocks_for_morning =
                    self.calculate_charge_blocks_needed(current_soc, battery_capacity_kwh, max_charge_rate_kw);
                let remaining_after_morning = blocks_for_morning.saturating_sub(morning_peak_charge_blocks);
                let (arb_charge, arb_discharge, profit) =
                    self.find_arbitrage_opportunities(blocks, Some(remaining_after_morning));

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
                // Full arbitrage mode - charge on the absolute cheapest blocks only
                // Calculate how many blocks are actually needed to fill the battery,
                // then select only the cheapest N from the candidate pool.
                let blocks_needed = self.calculate_charge_blocks_needed(
                    current_soc,
                    battery_capacity_kwh,
                    max_charge_rate_kw,
                );
                let (arb_charge, arb_discharge, profit) =
                    self.find_arbitrage_opportunities(blocks, Some(blocks_needed));

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

        // Apply min_consecutive_force_blocks rule symmetrically: remove short
        // SelfUse gaps between charge groups. This handles two cases:
        // 1. Middle gaps: Charge→SelfUse(1)→Charge patterns within one plan
        // 2. Leading gaps: The sliding window causes the first block to be SelfUse
        //    when it was Charge in the previous evaluation's plan
        // Using min_gap=2 to match the scheduler's min_consecutive_force_blocks default.
        Self::remove_short_selfuse_gaps_in_schedule(&mut schedule, blocks, 2);

        // After all charge/discharge scheduling, add HoldCharge blocks between
        // the last overnight charge block and the first expensive block (HDO transition
        // or morning peak). This preserves battery SOC at target using BackUpMode
        // while grid is still cheap, instead of draining battery via SelfUse.
        self.add_hold_charge_blocks(&mut schedule, blocks);

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
                    eval.mode = InverterOperationMode::BackUpMode;
                    eval.reason = format!(
                        "HOLD CHARGE: Battery at target ({:.1}%), grid powers house [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid =
                        Some("winter_adaptive_v9:hold_charge".to_string());

                    // In BackUpMode: grid powers house, battery doesn't discharge
                    let net_consumption =
                        context.consumption_forecast_kwh - context.solar_forecast_kwh;
                    if net_consumption > 0.0 {
                        eval.energy_flows.grid_import_kwh = net_consumption;
                        eval.cost_czk = net_consumption * effective_price;
                    } else {
                        let excess = -net_consumption;
                        eval.energy_flows.grid_export_kwh = excess;
                        eval.revenue_czk =
                            excess * context.grid_export_price_czk_per_kwh;
                    }
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

            ScheduledAction::HoldCharge => {
                // After charging is complete, hold battery at target SOC using BackUpMode.
                // Grid powers the house while electricity is still cheap (HDO low tariff).
                // Battery is preserved for the upcoming expensive hours.
                eval.mode = InverterOperationMode::BackUpMode;
                eval.reason = format!(
                    "HOLD AT TARGET: {:.3} CZK/kWh, preserving battery for expensive hours [{}]",
                    effective_price, summary
                );
                eval.decision_uid =
                    Some("winter_adaptive_v9:hold_for_peak".to_string());

                let net_consumption =
                    context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if net_consumption > 0.0 {
                    eval.energy_flows.grid_import_kwh = net_consumption;
                    eval.cost_czk = net_consumption * effective_price;
                } else {
                    let excess = -net_consumption;
                    eval.energy_flows.grid_export_kwh = excess;
                    eval.revenue_czk =
                        excess * context.grid_export_price_czk_per_kwh;
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
                    } else {
                        // Battery full during negative prices - use BackUpMode so house
                        // draws from grid (getting paid) while battery stays full
                        eval.mode = InverterOperationMode::BackUpMode;
                        eval.reason = format!(
                            "NEGATIVE PRICE HOLD: {:.3} CZK/kWh, battery full ({:.1}%) [{}]",
                            effective_price, context.current_battery_soc, summary
                        );
                        eval.decision_uid =
                            Some("winter_adaptive_v9:negative_price_hold".to_string());

                        let net_consumption =
                            context.consumption_forecast_kwh - context.solar_forecast_kwh;
                        if net_consumption > 0.0 {
                            eval.energy_flows.grid_import_kwh = net_consumption;
                            eval.cost_czk = net_consumption * effective_price;
                        } else {
                            let excess = -net_consumption;
                            eval.energy_flows.grid_export_kwh = excess;
                            eval.revenue_czk =
                                excess * context.grid_export_price_czk_per_kwh;
                        }
                    }
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!("SELF-USE: {:.3} CZK/kWh [{}]", effective_price, summary);
                    eval.decision_uid = Some("winter_adaptive_v9:self_use".to_string());

                    // Use hourly consumption profile for more accurate per-hour estimate.
                    // Falls back to flat forecast if hourly data unavailable.
                    let consumption_kwh = context
                        .hourly_consumption_profile
                        .map(|profile| {
                            let hour = context.price_block.block_start.hour() as usize;
                            profile[hour] / 4.0 // hourly kWh → 15-min block
                        })
                        .unwrap_or(context.consumption_forecast_kwh);

                    eval.energy_flows.household_consumption_kwh = consumption_kwh;
                    let net_consumption = consumption_kwh - context.solar_forecast_kwh;

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

        let (charge, discharge, profit) = strategy.find_arbitrage_opportunities(&blocks, None);

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

    #[test]
    fn test_arbitrage_selects_cheapest_blocks_not_earliest() {
        // Simulate the real-world scenario: moderate prices now (midnight),
        // cheaper prices later (3-5 AM), expensive peaks in evening.
        // The strategy should charge at 3-5 AM, not at midnight.
        let base_time = Utc.with_ymd_and_hms(2026, 1, 29, 23, 0, 0).unwrap();
        let grid_fee = 0.50;

        let mut blocks = Vec::new();
        let prices = [
            // 23:00-01:00 (8 blocks) - moderately cheap
            2.45, 2.50, 2.48, 2.52, 2.47, 2.53, 2.49, 2.51,
            // 01:00-03:00 (8 blocks) - medium prices
            2.90, 2.85, 2.95, 3.00, 2.80, 2.75, 2.70, 2.65,
            // 03:00-05:00 (8 blocks) - the CHEAPEST blocks
            2.29, 2.30, 2.32, 2.34, 2.31, 2.33, 2.35, 2.36,
            // 05:00-07:00 (8 blocks) - rising prices
            2.80, 3.00, 3.20, 3.40, 3.60, 3.80, 4.00, 4.20,
            // 07:00-09:00 (8 blocks) - morning peak
            4.50, 4.60, 4.70, 4.80, 4.90, 5.00, 4.80, 4.60,
            // 09:00-15:00 (24 blocks) - daytime
            3.50, 3.40, 3.30, 3.20, 3.10, 3.00, 2.90, 2.80,
            2.70, 2.60, 2.50, 2.40, 2.50, 2.60, 2.70, 2.80,
            2.90, 3.00, 3.10, 3.20, 3.30, 3.40, 3.50, 3.60,
            // 15:00-21:00 (24 blocks) - evening peak
            3.80, 4.00, 4.20, 4.40, 4.60, 4.80, 5.00, 5.20,
            4.80, 4.60, 4.40, 4.20, 4.00, 3.80, 3.60, 3.40,
            3.80, 4.00, 4.20, 4.40, 4.60, 4.50, 4.30, 4.10,
            // 21:00-23:00 (8 blocks) - late evening
            3.50, 3.30, 3.10, 2.90, 2.70, 2.60, 2.50, 2.40,
        ];

        for (i, &price) in prices.iter().enumerate() {
            blocks.push(TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price + grid_fee,
            });
        }

        let config = WinterAdaptiveV9Config {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            cheap_block_percentile: 0.25,
            min_arbitrage_spread_czk: 1.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        // Battery at 10% - needs ~9 blocks to charge to 95% (10 kWh battery, 3 kW charge rate)
        let plan = strategy.generate_plan(&blocks, 10.0, 10.0, 3.0, 0.0);

        assert_eq!(plan.mode, OperatingMode::Arbitrage);

        // Collect the charge blocks and their prices
        let charge_indices: Vec<usize> = plan
            .schedule
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .map(|(i, _)| i)
            .collect();

        let charge_prices: Vec<f32> = charge_indices
            .iter()
            .map(|&i| blocks[i].effective_price_czk_per_kwh)
            .collect();

        println!(
            "Charge blocks: {} blocks, prices: {:?}",
            charge_indices.len(),
            charge_prices
        );

        // The cheapest blocks are at indices 16-23 (3-5 AM, prices 2.29-2.36 + 0.50 fee)
        // The strategy should NOT be charging at midnight blocks (2.45-2.53 + fee)
        // when 3-5 AM blocks are cheaper (2.29-2.36 + fee)
        let max_charge_price = charge_prices
            .iter()
            .cloned()
            .fold(f32::NEG_INFINITY, f32::max);
        let cheapest_3am_price = 2.29 + grid_fee; // 2.79

        // The most expensive charge block should not be much above the 3-5 AM prices
        // (allowing some margin for the block count needed)
        println!(
            "Max charge price: {:.3}, cheapest available: {:.3}",
            max_charge_price, cheapest_3am_price
        );

        // Key assertion: ALL 3-5 AM cheap blocks (indices 16-23) must be selected
        // since they are the absolute cheapest. Midnight blocks may also be included
        // due to gap bridging (connecting charge groups into continuous sequences).
        let early_morning_count = charge_indices.iter().filter(|&&i| (16..24).contains(&i)).count();

        println!(
            "3-5 AM blocks selected: {}/8",
            early_morning_count,
        );

        // All 8 cheapest blocks (3-5 AM) must be included
        assert_eq!(
            early_morning_count, 8,
            "All 8 cheapest blocks (3-5 AM) should be selected, got {}",
            early_morning_count,
        );
    }

    #[test]
    fn test_charge_blocks_limited_to_battery_capacity() {
        let config = WinterAdaptiveV9Config {
            target_battery_soc: 95.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        // 10 kWh battery, 3 kW charge rate, 90% efficiency
        // Charge per block = 3.0 * 0.25 * 0.9 = 0.675 kWh
        // SOC needed = 95 - 10 = 85% = 8.5 kWh
        // Blocks needed = ceil(8.5 / 0.675) + 2 buffer = 15
        let blocks_needed = strategy.calculate_charge_blocks_needed(10.0, 10.0, 3.0);

        println!("Blocks needed from 10% to 95%: {}", blocks_needed);
        assert_eq!(blocks_needed, 15);

        // Already at target
        let blocks_needed_full = strategy.calculate_charge_blocks_needed(95.0, 10.0, 3.0);
        assert_eq!(blocks_needed_full, 0);

        // Halfway there (ceil(6.67) + 2 = 9)
        let blocks_needed_half = strategy.calculate_charge_blocks_needed(50.0, 10.0, 3.0);
        let expected = ((45.0_f32 / 100.0 * 10.0) / (3.0 * 0.25 * 0.90)).ceil() as usize + 2;
        assert_eq!(blocks_needed_half, expected);
    }

    fn create_test_control_config() -> fluxion_types::config::ControlConfig {
        fluxion_types::config::ControlConfig {
            battery_capacity_kwh: 10.0,
            max_battery_charge_rate_kw: 3.0,
            battery_efficiency: 0.95,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            hardware_min_battery_soc: 10.0,
            ..Default::default()
        }
    }

    #[test]
    fn test_charge_block_at_target_soc_returns_backup_mode() {
        // SolarFirst mode with high morning peak consumption ensures charge blocks
        // are scheduled even at 95% SOC (because morning peak drains a lot).
        // consumption_soc_delta = (12 blocks * 1.0 kWh / 10.0 capacity) * 100 = 120%
        // soc_needed = (20.0 - 95.0 + 120.0) = 45% > 0, so charge blocks are scheduled
        let config = WinterAdaptiveV9Config {
            solar_threshold_kwh: 5.0,
            min_overnight_charge_blocks: 4,
            morning_peak_consumption_per_block_kwh: 1.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();
        let control_config = create_test_control_config();

        // Block 0 is hour 0 (overnight, cheap) - will be scheduled as Charge in SolarFirst
        let context = EvaluationContext {
            price_block: &blocks[0],
            all_price_blocks: Some(&blocks),
            control_config: &control_config,
            current_battery_soc: 95.0, // At target
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.5,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 10.0,
            solar_forecast_remaining_today_kwh: 10.0, // High solar -> SolarFirst mode
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(
            eval.mode,
            InverterOperationMode::BackUpMode,
            "Charge block at target SOC should return BackUpMode, got: {} - {}",
            eval.mode,
            eval.reason
        );
        assert!(
            eval.reason.contains("HOLD CHARGE"),
            "Reason should mention HOLD CHARGE: {}",
            eval.reason
        );
    }

    #[test]
    fn test_charge_block_below_target_soc_returns_force_charge() {
        // SolarFirst mode with high solar, overnight block will be Charge
        let config = WinterAdaptiveV9Config {
            solar_threshold_kwh: 5.0,
            min_overnight_charge_blocks: 4,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let blocks = create_test_blocks_with_prices();
        let control_config = create_test_control_config();

        let context = EvaluationContext {
            price_block: &blocks[0],
            all_price_blocks: Some(&blocks),
            control_config: &control_config,
            current_battery_soc: 30.0, // Below target
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.5,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 10.0,
            solar_forecast_remaining_today_kwh: 10.0, // High solar -> SolarFirst mode
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let eval = strategy.evaluate(&context);

        assert_eq!(
            eval.mode,
            InverterOperationMode::ForceCharge,
            "Charge block below target SOC should return ForceCharge, got: {} - {}",
            eval.mode,
            eval.reason
        );
    }

    #[test]
    fn test_negative_price_at_target_soc_returns_backup_mode() {
        let config = WinterAdaptiveV9Config {
            negative_price_handling_enabled: true,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let control_config = create_test_control_config();

        // Create blocks where one block has negative effective price.
        // The planner detects negative prices and enters NegativePrice mode,
        // scheduling it as a Charge block. At SOC >= target, the Charge arm
        // returns BackUpMode (our fix).
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();

        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let effective_price = match hour {
                    0..=5 => 3.3,
                    6..=8 => 6.8,
                    9 => -0.5, // Single hour negative effective price
                    10..=14 => 2.3,
                    15..=17 => 6.3,
                    18..=20 => 7.8,
                    _ => 3.8,
                };

                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: effective_price - 1.8,
                    effective_price_czk_per_kwh: effective_price,
                });
            }
        }

        // Pick a negative-price block at hour 9
        let block_index = 9 * 4;

        let context = EvaluationContext {
            price_block: &blocks[block_index],
            all_price_blocks: Some(&blocks),
            control_config: &control_config,
            current_battery_soc: 95.0, // At target
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.5,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let eval = strategy.evaluate(&context);

        // Block is scheduled as Charge (NegativePrice) by the planner,
        // and at target SOC our fix returns BackUpMode
        assert_eq!(
            eval.mode,
            InverterOperationMode::BackUpMode,
            "Negative price block at target SOC should return BackUpMode, got: {} - {}",
            eval.mode,
            eval.reason
        );
        assert!(
            eval.reason.contains("HOLD CHARGE"),
            "Reason should mention HOLD CHARGE: {}",
            eval.reason
        );
    }

    #[test]
    fn test_hold_charge_after_overnight_charging() {
        // Create blocks with HDO low tariff (0.50 fee) overnight and
        // HDO high tariff (1.80 fee) from 6 AM onwards.
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let hdo_low_fee = 0.50;
        let hdo_high_fee = 1.80;

        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let spot_price = match hour {
                    0..=5 => 2.0,   // Overnight
                    6..=8 => 4.0,   // Morning peak
                    9..=14 => 2.5,  // Midday
                    15..=20 => 5.0, // Evening peak
                    _ => 2.5,       // Late evening
                };
                let grid_fee = if hour < 6 { hdo_low_fee } else { hdo_high_fee };

                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: spot_price,
                    effective_price_czk_per_kwh: spot_price + grid_fee,
                });
            }
        }

        let config = WinterAdaptiveV9Config {
            target_battery_soc: 100.0,
            morning_peak_start_hour: 6,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        // Battery at 50% - needs charging, arbitrage mode (low solar)
        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0);

        // Blocks between last charge and hour 6 should be HoldCharge
        let hold_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::HoldCharge))
            .count();

        println!("HoldCharge blocks after overnight charging: {}", hold_count);

        // There should be some HoldCharge blocks between last charge and 6 AM
        // (unless all overnight blocks are charging)
        let last_overnight_charge = plan
            .schedule
            .iter()
            .enumerate()
            .rev()
            .filter(|(i, _)| blocks[*i].block_start.hour() < 6)
            .find(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .map(|(i, _)| i);

        if let Some(last_idx) = last_overnight_charge {
            let hour_6_idx = 6 * 4; // Index for 06:00
            if last_idx + 1 < hour_6_idx {
                assert!(
                    hold_count > 0,
                    "Should have HoldCharge blocks between last charge (idx {}) and 6 AM (idx {})",
                    last_idx,
                    hour_6_idx
                );

                // Verify all hold blocks are before hour 6
                for (i, action) in plan.schedule.iter().enumerate() {
                    if matches!(action, ScheduledAction::HoldCharge) {
                        assert!(
                            blocks[i].block_start.hour() < 6,
                            "HoldCharge at hour {} should be before morning peak",
                            blocks[i].block_start.hour()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_hold_charge_stops_at_hdo_transition() {
        // HDO transition at hour 7 (not the default 6)
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let hdo_low_fee = 0.50;
        let hdo_high_fee = 1.80;

        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let spot_price = 2.0; // Flat spot price
                // HDO transition at hour 7
                let grid_fee = if hour < 7 { hdo_low_fee } else { hdo_high_fee };

                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: spot_price,
                    effective_price_czk_per_kwh: spot_price + grid_fee,
                });
            }
        }

        let config = WinterAdaptiveV9Config {
            target_battery_soc: 100.0,
            morning_peak_start_hour: 8, // Set peak later than HDO transition
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0);

        // No HoldCharge blocks should exist at hour 7+ (HDO high tariff)
        for (i, action) in plan.schedule.iter().enumerate() {
            if matches!(action, ScheduledAction::HoldCharge) {
                let hour = blocks[i].block_start.hour();
                assert!(
                    hour < 7,
                    "HoldCharge should stop at HDO transition (hour 7), found at hour {}",
                    hour
                );
            }
        }
    }

    #[test]
    fn test_bridge_short_charge_gaps() {
        // Create charge blocks with a 1-block gap: [5, 6, _, 8, 9]
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..20)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            })
            .collect();

        let mut charge_indices = vec![5, 6, 8, 9];
        WinterAdaptiveV9Strategy::bridge_short_charge_gaps(&mut charge_indices, &blocks, 2);

        // Gap of 1 block (index 7) should be filled
        assert!(
            charge_indices.contains(&7),
            "1-block gap at index 7 should be bridged, got: {:?}",
            charge_indices
        );
    }

    #[test]
    fn test_bridge_does_not_fill_large_gaps() {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..20)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            })
            .collect();

        // Gap of 3 blocks: [2, 3, _, _, _, 7, 8]
        let mut charge_indices = vec![2, 3, 7, 8];
        WinterAdaptiveV9Strategy::bridge_short_charge_gaps(&mut charge_indices, &blocks, 2);

        // Gap of 3 should NOT be filled
        assert!(
            !charge_indices.contains(&4),
            "3-block gap should not be bridged, got: {:?}",
            charge_indices
        );
        assert!(
            !charge_indices.contains(&5),
            "3-block gap should not be bridged, got: {:?}",
            charge_indices
        );
    }

    #[test]
    fn test_remove_short_selfuse_gaps_middle() {
        // Schedule: Charge, SelfUse(1), Charge → gap should be filled
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..5)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            })
            .collect();

        let mut schedule = vec![
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::SelfUse, // 1-block gap
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
        ];

        WinterAdaptiveV9Strategy::remove_short_selfuse_gaps_in_schedule(
            &mut schedule, &blocks, 2,
        );

        assert!(
            matches!(schedule[2], ScheduledAction::Charge { .. }),
            "1-block SelfUse gap between charge groups should be filled, got: {:?}",
            schedule[2]
        );
    }

    #[test]
    fn test_remove_short_selfuse_gaps_leading() {
        // Schedule: SelfUse(1), Charge, Charge → leading gap should be filled
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..4)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            })
            .collect();

        let mut schedule = vec![
            ScheduledAction::SelfUse, // Leading gap (1 block)
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::SelfUse,
        ];

        WinterAdaptiveV9Strategy::remove_short_selfuse_gaps_in_schedule(
            &mut schedule, &blocks, 2,
        );

        assert!(
            matches!(schedule[0], ScheduledAction::Charge { .. }),
            "1-block leading SelfUse before charge group should be filled, got: {:?}",
            schedule[0]
        );
        // Trailing SelfUse should NOT be changed (no charge group after it)
        assert!(
            matches!(schedule[3], ScheduledAction::SelfUse),
            "Trailing SelfUse should remain unchanged"
        );
    }

    #[test]
    fn test_remove_short_selfuse_gaps_respects_price() {
        // Schedule: Charge, SelfUse(1 expensive), Charge → should NOT bridge
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks = vec![
            TimeBlockPrice {
                block_start: base_time,
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 10.0,
                effective_price_czk_per_kwh: 10.5, // Very expensive gap
            },
            TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            },
        ];

        let mut schedule = vec![
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::SelfUse, // Expensive gap
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
        ];

        WinterAdaptiveV9Strategy::remove_short_selfuse_gaps_in_schedule(
            &mut schedule, &blocks, 2,
        );

        assert!(
            matches!(schedule[1], ScheduledAction::SelfUse),
            "Expensive gap should NOT be bridged, got: {:?}",
            schedule[1]
        );
    }

    #[test]
    fn test_remove_short_selfuse_gaps_keeps_2block_gaps() {
        // Schedule: Charge, SelfUse(2), Charge → 2-block gap should NOT be filled (min_gap=2)
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..6)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
                effective_price_czk_per_kwh: 2.5,
            })
            .collect();

        let mut schedule = vec![
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::SelfUse, // 2-block gap
            ScheduledAction::SelfUse,
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
            ScheduledAction::Charge { reason: ChargeReason::Arbitrage },
        ];

        WinterAdaptiveV9Strategy::remove_short_selfuse_gaps_in_schedule(
            &mut schedule, &blocks, 2,
        );

        assert!(
            matches!(schedule[2], ScheduledAction::SelfUse),
            "2-block gap should NOT be bridged with min_gap=2"
        );
        assert!(
            matches!(schedule[3], ScheduledAction::SelfUse),
            "2-block gap should NOT be bridged with min_gap=2"
        );
    }

    #[test]
    fn test_hold_charge_evaluate_returns_backup_mode() {
        // Test that evaluating a block scheduled as HoldCharge returns BackUpMode
        // Use blocks with HDO low fee overnight, high fee from 6 AM
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let hdo_low_fee = 0.50;
        let hdo_high_fee = 1.80;

        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let spot_price = match hour {
                    0..=5 => 2.0,
                    _ => 4.0,
                };
                let grid_fee = if hour < 6 { hdo_low_fee } else { hdo_high_fee };

                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: spot_price,
                    effective_price_czk_per_kwh: spot_price + grid_fee,
                });
            }
        }

        let config = WinterAdaptiveV9Config {
            target_battery_soc: 100.0,
            morning_peak_start_hour: 6,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV9Strategy::new(config);
        let control_config = create_test_control_config();

        // Generate plan first to find a HoldCharge block
        let plan = strategy.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0);

        // Find a HoldCharge block
        let hold_block_idx = plan
            .schedule
            .iter()
            .position(|a| matches!(a, ScheduledAction::HoldCharge));

        if let Some(idx) = hold_block_idx {
            let context = EvaluationContext {
                price_block: &blocks[idx],
                all_price_blocks: Some(&blocks),
                control_config: &control_config,
                current_battery_soc: 30.0,
                solar_forecast_kwh: 0.0,
                consumption_forecast_kwh: 0.5,
                grid_export_price_czk_per_kwh: 0.5,
                backup_discharge_min_soc: 10.0,
                grid_import_today_kwh: None,
                consumption_today_kwh: None,
                solar_forecast_total_today_kwh: 0.0,
                solar_forecast_remaining_today_kwh: 0.0,
                solar_forecast_tomorrow_kwh: 0.0,
                battery_avg_charge_price_czk_per_kwh: 0.0,
                hourly_consumption_profile: None,
            };

            let eval = strategy.evaluate(&context);

            assert_eq!(
                eval.mode,
                InverterOperationMode::BackUpMode,
                "HoldCharge block should return BackUpMode, got: {} - {}",
                eval.mode,
                eval.reason
            );
            assert!(
                eval.reason.contains("HOLD AT TARGET"),
                "Reason should mention HOLD AT TARGET: {}",
                eval.reason
            );
        }
    }
}
