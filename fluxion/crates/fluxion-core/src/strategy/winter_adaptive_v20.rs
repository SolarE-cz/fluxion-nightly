// Copyright (c) 2025 SOLARE S.R.O.

//! # Winter Adaptive V20 Strategy - Adaptive Budget Allocation
//!
//! **Status:** Experimental - V10 algorithm + DayMetrics-driven parameter resolution
//!
//! ## Overview
//!
//! V20 extends V10's unified budget allocation with adaptive parameter resolution
//! based on measurable day characteristics (DayMetrics). Instead of fixed parameters,
//! V20 adjusts its behavior based on price volatility, solar availability, price level,
//! and tomorrow's outlook.
//!
//! ## Algorithm Flow
//!
//! ```text
//! evaluate() → compute_day_metrics() → resolve_params(metrics) → generate_plan(params) → dispatch
//! ```
//!
//! ## Key Design Principles
//!
//! 1. **DayMetrics-Driven Parameters**
//!    - Volatile days: lower savings threshold, more bootstrap blocks, lower export spread
//!    - Expensive days: higher savings threshold (only discharge for big savings)
//!    - High solar: wider daylight window, higher solar confidence
//!    - Low solar: tighter daylight window, lower solar confidence
//!    - Tomorrow expensive: limit discharge blocks (save battery for tomorrow)
//!    - Tomorrow cheap: reduce charge blocks (charge cheaper tomorrow)
//!
//! 2. **Composable Adjustments**
//!    - Multiple conditions can apply simultaneously (volatile + high solar + negative prices)
//!    - Each adjustment is independent and stacks
//!
//! 3. **Same Core Algorithm as V10**
//!    - Budget-based allocation, savings threshold, solar excess, post-processing
//!    - Internal constants (daylight window, bootstrap count) become resolved parameters
//!
//! ## Config: 19 Parameters
//!
//! 12 from V10 (base behavior) + 7 new (metric thresholds for parameter switching)

use chrono::Timelike;
use serde::{Deserialize, Serialize};

use crate::day_profiling::{compute_day_metrics, estimate_daily_consumption};
use crate::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::day_profile::DayMetrics;
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive V20 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV20Config {
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    // === Base V10 parameters ===
    /// Target battery SOC (%) - maximum charge level
    pub target_battery_soc: f32,

    /// Hardware minimum battery SOC (%) - never discharge below this
    pub min_discharge_soc: f32,

    /// Round-trip battery efficiency (charge x discharge efficiency)
    pub battery_round_trip_efficiency: f32,

    /// Enable negative price handling (always charge if getting paid)
    pub negative_price_handling_enabled: bool,

    /// Price threshold (CZK/kWh) below which we always charge
    pub opportunistic_charge_threshold_czk: f32,

    /// Minimum price spread (CZK) to allow grid export instead of home use
    pub min_export_spread_czk: f32,

    /// Minimum SOC (%) after discharge to allow grid export
    pub min_soc_after_export: f32,

    /// Minimum solar forecast (kWh remaining today) to consider solar excess
    pub solar_threshold_kwh: f32,

    /// Factor to apply to solar forecast for conservative planning
    pub solar_confidence_factor: f32,

    /// Minimum savings per kWh (block_price - avg_charge_price) to justify battery use
    pub min_savings_threshold_czk: f32,

    // === DayMetrics thresholds (7 new params) ===
    /// CV threshold above which day is considered volatile (default: 0.35)
    pub volatile_cv_threshold: f32,

    /// Price level threshold above which day is considered expensive (default: 0.5)
    pub expensive_level_threshold: f32,

    /// Solar ratio above which solar is considered high (default: 1.1)
    pub high_solar_ratio_threshold: f32,

    /// Solar ratio below which solar is considered low (default: 0.9)
    pub low_solar_ratio_threshold: f32,

    /// Tomorrow price ratio above which tomorrow is expensive (default: 1.3)
    pub tomorrow_expensive_ratio: f32,

    /// Tomorrow price ratio below which tomorrow is cheap (default: 0.7)
    pub tomorrow_cheap_ratio: f32,

    /// Negative price fraction above which significant negative pricing applies (default: 0.0)
    pub negative_price_fraction_threshold: f32,
}

impl Default for WinterAdaptiveV20Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 100,
            // Base V10 defaults
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            // DayMetrics thresholds
            volatile_cv_threshold: 0.35,
            expensive_level_threshold: 0.3,
            high_solar_ratio_threshold: 1.1,
            low_solar_ratio_threshold: 0.9,
            tomorrow_expensive_ratio: 1.3,
            tomorrow_cheap_ratio: 0.7,
            negative_price_fraction_threshold: 0.0,
        }
    }
}

/// Parameters resolved from base config + DayMetrics adjustments.
/// V10 hardcodes daylight_window and bootstrap_count — V20 makes them adjustable.
#[derive(Debug, Clone)]
struct ResolvedParams {
    // From V10 config (possibly adjusted by metrics)
    target_battery_soc: f32,
    min_discharge_soc: f32,
    battery_round_trip_efficiency: f32,
    negative_price_handling_enabled: bool,
    opportunistic_charge_threshold_czk: f32,
    min_export_spread_czk: f32,
    min_soc_after_export: f32,
    solar_confidence_factor: f32,
    min_savings_threshold_czk: f32,
    // V20 resolved internals (V10 hardcodes these)
    daylight_start_hour: u32,
    daylight_end_hour: u32,
    bootstrap_count: usize,
    /// Cap on battery-powered blocks (None = unlimited like V10)
    max_discharge_blocks: Option<usize>,
    /// Multiplier on charge_blocks_needed (1.0 = normal, <1.0 = reduce charging)
    charge_reduction_factor: f32,
}

/// Scheduled action for a specific block in the V20 plan
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScheduledAction {
    Charge { reason: ChargeReason },
    BatteryPowered,
    GridPowered,
    Export,
    SolarExcess,
    HoldCharge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChargeReason {
    Opportunistic,
    Arbitrage,
    NegativePrice,
}

/// Planning result for the day
#[derive(Debug)]
struct DayPlan {
    schedule: Vec<ScheduledAction>,
    avg_charge_price: f32,
    battery_powered_blocks: usize,
    charge_blocks: usize,
    export_blocks: usize,
}

pub struct WinterAdaptiveV20Strategy {
    config: WinterAdaptiveV20Config,
}

impl WinterAdaptiveV20Strategy {
    pub fn new(config: WinterAdaptiveV20Config) -> Self {
        Self { config }
    }

    /// Resolve effective parameters from base config + DayMetrics.
    /// Adjustments are composable — multiple can apply simultaneously.
    fn resolve_params(&self, metrics: &DayMetrics) -> ResolvedParams {
        let mut params = ResolvedParams {
            target_battery_soc: self.config.target_battery_soc,
            min_discharge_soc: self.config.min_discharge_soc,
            battery_round_trip_efficiency: self.config.battery_round_trip_efficiency,
            negative_price_handling_enabled: self.config.negative_price_handling_enabled,
            opportunistic_charge_threshold_czk: self.config.opportunistic_charge_threshold_czk,
            min_export_spread_czk: self.config.min_export_spread_czk,
            min_soc_after_export: self.config.min_soc_after_export,
            solar_confidence_factor: self.config.solar_confidence_factor,
            min_savings_threshold_czk: self.config.min_savings_threshold_czk,
            // V10 defaults for internal algorithm constants
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        // --- Volatile: more aggressive arbitrage ---
        if metrics.price_cv > self.config.volatile_cv_threshold {
            params.min_savings_threshold_czk = params.min_savings_threshold_czk.min(0.2);
            params.bootstrap_count = 12;
            params.min_export_spread_czk = params.min_export_spread_czk.min(3.0);
        }

        // --- Expensive: only discharge for big savings ---
        if metrics.price_level_vs_charge_cost > self.config.expensive_level_threshold {
            params.min_savings_threshold_czk = params.min_savings_threshold_czk.max(1.0);
        }

        // --- High solar: wider daylight window, trust solar more, limit discharge ---
        if metrics.solar_ratio > self.config.high_solar_ratio_threshold {
            params.daylight_start_hour = 6;
            params.daylight_end_hour = 19;
            params.solar_confidence_factor = params.solar_confidence_factor.max(0.9);
            // C20 sweep: limiting discharge to 12 blocks on high-solar days preserves
            // solar-charged battery for peak export hours (+41 CZK improvement)
            params.max_discharge_blocks = Some(
                params
                    .max_discharge_blocks
                    .map_or(12, |existing| existing.min(12)),
            );
        }

        // --- Low solar: tighter daylight window, trust solar less ---
        if metrics.solar_ratio < self.config.low_solar_ratio_threshold {
            params.daylight_start_hour = 10;
            params.daylight_end_hour = 14;
            params.solar_confidence_factor = params.solar_confidence_factor.min(0.5);
        }

        // --- Tomorrow expensive: save battery for tomorrow ---
        if metrics
            .tomorrow_price_ratio
            .is_some_and(|r| r > self.config.tomorrow_expensive_ratio)
        {
            // Limit discharge to ~4 hours (16 blocks) to preserve battery
            params.max_discharge_blocks = Some(16);
        }

        // --- Tomorrow cheap: reduce charging, charge cheaper tomorrow ---
        if metrics
            .tomorrow_price_ratio
            .is_some_and(|r| r < self.config.tomorrow_cheap_ratio)
        {
            params.charge_reduction_factor = 0.5;
        }

        params
    }

    /// Estimate net consumption for a block (consumption - solar)
    fn estimate_block_consumption(
        block: &TimeBlockPrice,
        solar_per_block_kwh: f32,
        hourly_profile: Option<&[f32; 24]>,
        fallback_consumption: f32,
        params: &ResolvedParams,
    ) -> f32 {
        let consumption_kwh = hourly_profile
            .map(|profile| {
                let hour = block.block_start.hour() as usize;
                profile[hour] / 4.0 // hourly kWh → 15-min block
            })
            .unwrap_or(fallback_consumption);

        let is_daylight = {
            let h = block.block_start.hour();
            (params.daylight_start_hour..params.daylight_end_hour).contains(&h)
        };
        let solar = if is_daylight {
            solar_per_block_kwh
        } else {
            0.0
        };

        (consumption_kwh - solar).max(0.0)
    }

    /// Ensure all selected charge blocks form consecutive groups of at least 2.
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

    /// Remove short SelfUse/GridPowered gaps in the final schedule.
    fn remove_short_gaps_in_schedule(
        schedule: &mut [ScheduledAction],
        blocks: &[TimeBlockPrice],
        min_gap: usize,
    ) {
        if schedule.is_empty() || min_gap == 0 {
            return;
        }

        let is_non_charge = |a: &ScheduledAction| {
            matches!(
                a,
                ScheduledAction::GridPowered | ScheduledAction::BatteryPowered
            )
        };

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
        let price_tolerance = 2.0;

        let mut i = 0;
        while i < schedule.len() {
            if is_non_charge(&schedule[i]) {
                let gap_start = i;
                while i < schedule.len() && is_non_charge(&schedule[i]) {
                    i += 1;
                }
                let gap_end = i;
                let gap_len = gap_end - gap_start;

                if gap_len > 0 && gap_len < min_gap {
                    let before_is_charge = gap_start > 0
                        && matches!(schedule[gap_start - 1], ScheduledAction::Charge { .. });
                    let after_is_charge = gap_end < schedule.len()
                        && matches!(schedule[gap_end], ScheduledAction::Charge { .. });

                    let should_bridge = (gap_start == 0 || before_is_charge) && after_is_charge;

                    if should_bridge {
                        let all_prices_ok = (gap_start..gap_end).all(|j| {
                            blocks.get(j).is_some_and(|b| {
                                b.effective_price_czk_per_kwh <= avg_charge_price + price_tolerance
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

    /// Add HoldCharge blocks between charging and the first battery-powered block.
    fn add_hold_charge_blocks(schedule: &mut [ScheduledAction]) {
        let last_charge_idx = schedule
            .iter()
            .enumerate()
            .rev()
            .find(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .map(|(i, _)| i);

        let Some(last_charge) = last_charge_idx else {
            return;
        };

        let first_discharge_idx = schedule
            .iter()
            .enumerate()
            .skip(last_charge + 1)
            .find(|(_, a)| matches!(a, ScheduledAction::BatteryPowered | ScheduledAction::Export))
            .map(|(i, _)| i);

        let end = first_discharge_idx.unwrap_or(schedule.len());

        for action in schedule.iter_mut().take(end).skip(last_charge + 1) {
            if matches!(action, ScheduledAction::GridPowered) {
                *action = ScheduledAction::HoldCharge;
            }
        }
    }

    /// Generate the day plan using resolved parameters.
    /// Core algorithm is V10's charge-first budget allocation with adjustable internals.
    #[allow(clippy::too_many_arguments)]
    fn generate_plan(
        &self,
        params: &ResolvedParams,
        blocks: &[TimeBlockPrice],
        current_soc: f32,
        battery_capacity_kwh: f32,
        max_charge_rate_kw: f32,
        solar_remaining_kwh: f32,
        hourly_profile: Option<&[f32; 24]>,
        fallback_consumption: f32,
        battery_avg_charge_price: f32,
    ) -> DayPlan {
        let n = blocks.len();
        let mut schedule = vec![ScheduledAction::GridPowered; n];
        let mut charge_indices: Vec<usize> = Vec::new();

        // === Phase 1: Negative prices → always Charge ===
        if params.negative_price_handling_enabled {
            for (i, block) in blocks.iter().enumerate() {
                if block.effective_price_czk_per_kwh < 0.0 {
                    schedule[i] = ScheduledAction::Charge {
                        reason: ChargeReason::NegativePrice,
                    };
                    charge_indices.push(i);
                }
            }
        }

        // === Phase 2: Estimate net consumption per block ===
        let effective_solar = solar_remaining_kwh * params.solar_confidence_factor;
        let solar_per_block = if n > 0 && effective_solar > 0.0 {
            let daylight_blocks = blocks
                .iter()
                .filter(|b| {
                    let h = b.block_start.hour();
                    (params.daylight_start_hour..params.daylight_end_hour).contains(&h)
                })
                .count();
            if daylight_blocks > 0 {
                effective_solar / daylight_blocks as f32
            } else {
                0.0
            }
        } else {
            0.0
        };

        let net_consumption: Vec<f32> = blocks
            .iter()
            .map(|b| {
                Self::estimate_block_consumption(
                    b,
                    solar_per_block,
                    hourly_profile,
                    fallback_consumption,
                    params,
                )
            })
            .collect();

        // === Pre-compute energy parameters ===
        let energy_per_charge_block = max_charge_rate_kw * 0.25;
        let existing_energy = (current_soc - params.min_discharge_soc).max(0.0) / 100.0
            * battery_capacity_kwh
            * params.battery_round_trip_efficiency;

        // === Phase 3: Estimate battery demand (demand-driven charging) ===
        let mut available_blocks: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(schedule[*i], ScheduledAction::GridPowered))
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        available_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Use resolved bootstrap_count instead of V10's hardcoded 6
        let bootstrap_count = params.bootstrap_count.min(available_blocks.len());
        let cheapest_grid_price = if bootstrap_count > 0 {
            available_blocks[..bootstrap_count]
                .iter()
                .map(|(_, p)| p)
                .sum::<f32>()
                / bootstrap_count as f32
        } else {
            battery_avg_charge_price
        };

        let estimated_charge_price = if existing_energy > 0.0 {
            let total_est = existing_energy + bootstrap_count as f32 * energy_per_charge_block;
            if total_est > 0.0 {
                (existing_energy * battery_avg_charge_price
                    + bootstrap_count as f32 * energy_per_charge_block * cheapest_grid_price)
                    / total_est
            } else {
                battery_avg_charge_price
            }
        } else {
            cheapest_grid_price
        };

        let mut demand_kwh: f32 = 0.0;
        let mut demand_blocks: usize = 0;
        for &(idx, price) in available_blocks.iter().rev() {
            let savings = price - estimated_charge_price;
            if savings < params.min_savings_threshold_czk {
                break;
            }
            let consumption = net_consumption[idx];
            if consumption > 0.0 {
                demand_kwh += consumption;
                demand_blocks += 1;
            }
        }

        // Apply max_discharge_blocks cap from resolve_params
        if let Some(max_blocks) = params.max_discharge_blocks
            && demand_blocks > max_blocks
        {
            // Recalculate demand with capped block count
            demand_kwh = 0.0;
            let mut counted = 0;
            for &(idx, price) in available_blocks.iter().rev() {
                if counted >= max_blocks {
                    break;
                }
                let savings = price - estimated_charge_price;
                if savings < params.min_savings_threshold_czk {
                    break;
                }
                let consumption = net_consumption[idx];
                if consumption > 0.0 {
                    demand_kwh += consumption;
                    counted += 1;
                }
            }
        }

        // === Phase 4: Calculate charge blocks needed (demand-driven) ===
        let energy_to_charge = (demand_kwh - existing_energy).max(0.0);

        let max_chargeable =
            (params.target_battery_soc - current_soc).max(0.0) / 100.0 * battery_capacity_kwh;
        let capped_energy = energy_to_charge.min(max_chargeable);

        // Apply charge_reduction_factor from resolve_params
        let adjusted_energy = capped_energy * params.charge_reduction_factor;

        let charge_blocks_needed = if energy_per_charge_block > 0.0 {
            (adjusted_energy / energy_per_charge_block).ceil() as usize
        } else {
            0
        };

        // === Phase 5: Select cheapest blocks for charging ===
        for (i, block) in blocks.iter().enumerate() {
            if matches!(schedule[i], ScheduledAction::GridPowered)
                && block.effective_price_czk_per_kwh < params.opportunistic_charge_threshold_czk
            {
                schedule[i] = ScheduledAction::Charge {
                    reason: ChargeReason::Opportunistic,
                };
                charge_indices.push(i);
            }
        }

        let remaining_needed = charge_blocks_needed.saturating_sub(charge_indices.len());
        let mut added_charge = 0;
        for &(idx, _price) in &available_blocks {
            if added_charge >= remaining_needed {
                break;
            }
            if !matches!(schedule[idx], ScheduledAction::Charge { .. }) {
                schedule[idx] = ScheduledAction::Charge {
                    reason: ChargeReason::Arbitrage,
                };
                charge_indices.push(idx);
                added_charge += 1;
            }
        }

        // === Phase 6: Calculate actual battery budget ===
        let charged_energy = charge_indices.len() as f32 * energy_per_charge_block;
        let budget = (existing_energy + charged_energy * params.battery_round_trip_efficiency).min(
            (params.target_battery_soc - params.min_discharge_soc) / 100.0
                * battery_capacity_kwh
                * params.battery_round_trip_efficiency,
        );

        let avg_charge_price = {
            let existing_cost = existing_energy * battery_avg_charge_price;
            let grid_charge_cost: f32 = charge_indices
                .iter()
                .filter_map(|&i| blocks.get(i))
                .map(|b| b.effective_price_czk_per_kwh * energy_per_charge_block)
                .sum();
            let total_energy = existing_energy + charged_energy;
            if total_energy > 0.0 {
                (existing_cost + grid_charge_cost) / total_energy
            } else {
                estimated_charge_price
            }
        };

        // === Phase 7: Rank remaining blocks by effective_price descending ===
        let mut rankable: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(schedule[*i], ScheduledAction::GridPowered))
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        rankable.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // === Phase 8: Allocate battery to most expensive blocks first ===
        let mut remaining_budget = budget;
        let mut battery_powered_count = 0;

        for &(idx, price) in &rankable {
            // Apply max_discharge_blocks cap
            if let Some(max_blocks) = params.max_discharge_blocks
                && battery_powered_count >= max_blocks
            {
                break;
            }

            let savings = price - avg_charge_price;
            if savings < params.min_savings_threshold_czk {
                break;
            }

            let consumption = net_consumption[idx];
            if consumption <= 0.0 {
                continue;
            }

            if remaining_budget >= consumption {
                schedule[idx] = ScheduledAction::BatteryPowered;
                remaining_budget -= consumption;
                battery_powered_count += 1;
            }
        }

        // === Phase 9: Export upgrades ===
        let mut export_count = 0;
        for (i, action) in schedule.iter_mut().enumerate() {
            if matches!(action, ScheduledAction::BatteryPowered) {
                let spread = blocks[i].effective_price_czk_per_kwh - avg_charge_price;
                if spread >= params.min_export_spread_czk {
                    *action = ScheduledAction::Export;
                    export_count += 1;
                }
            }
        }

        // === Phase 10: Solar excess blocks → SelfUse ===
        for (i, block) in blocks.iter().enumerate() {
            if matches!(schedule[i], ScheduledAction::GridPowered) {
                let is_daylight = {
                    let h = block.block_start.hour();
                    (params.daylight_start_hour..params.daylight_end_hour).contains(&h)
                };
                if is_daylight && solar_per_block > 0.0 && net_consumption[i] <= 0.0 {
                    schedule[i] = ScheduledAction::SolarExcess;
                }
            }
        }

        // === Phase 11: Post-processing ===
        Self::ensure_consecutive_charge_groups(&mut charge_indices, blocks);
        for &idx in &charge_indices {
            if matches!(schedule[idx], ScheduledAction::GridPowered) {
                schedule[idx] = ScheduledAction::Charge {
                    reason: ChargeReason::Arbitrage,
                };
            }
        }

        Self::bridge_short_charge_gaps(&mut charge_indices, blocks, 2);
        for &idx in &charge_indices {
            if matches!(schedule[idx], ScheduledAction::GridPowered) {
                schedule[idx] = ScheduledAction::Charge {
                    reason: ChargeReason::Arbitrage,
                };
            }
        }

        Self::remove_short_gaps_in_schedule(&mut schedule, blocks, 2);
        Self::add_hold_charge_blocks(&mut schedule);

        // Final stats
        let final_charge_prices: Vec<f32> = schedule
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .filter_map(|(i, _)| blocks.get(i).map(|b| b.effective_price_czk_per_kwh))
            .collect();

        let final_avg_charge = if final_charge_prices.is_empty() {
            avg_charge_price
        } else {
            final_charge_prices.iter().sum::<f32>() / final_charge_prices.len() as f32
        };

        let final_charge_count = schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge { .. }))
            .count();

        DayPlan {
            schedule,
            avg_charge_price: final_avg_charge,
            battery_powered_blocks: battery_powered_count,
            charge_blocks: final_charge_count,
            export_blocks: export_count,
        }
    }

    /// Generate summary string for logging
    fn generate_summary(plan: &DayPlan, metrics: &DayMetrics) -> String {
        format!(
            "ADAPTIVE: {} chg/{} bat/{} exp, avg {:.2} CZK | solar={:.2} cv={:.2} level={:.2}",
            plan.charge_blocks,
            plan.battery_powered_blocks,
            plan.export_blocks,
            plan.avg_charge_price,
            metrics.solar_ratio,
            metrics.price_cv,
            metrics.price_level_vs_charge_cost,
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV20Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V20"
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

        // === V20 addition: Compute DayMetrics ===
        let daily_consumption = estimate_daily_consumption(
            context.hourly_consumption_profile,
            context.control_config.average_household_load_kw,
        );

        let metrics = compute_day_metrics(
            all_blocks,
            context.solar_forecast_remaining_today_kwh,
            context.solar_forecast_tomorrow_kwh,
            context.battery_avg_charge_price_czk_per_kwh,
            daily_consumption,
        );

        // === V20 addition: Resolve parameters from metrics ===
        let params = self.resolve_params(&metrics);

        // Generate the day plan with resolved parameters
        let plan = self.generate_plan(
            &params,
            all_blocks,
            context.current_battery_soc,
            context.control_config.battery_capacity_kwh,
            context.control_config.max_battery_charge_rate_kw,
            context.solar_forecast_remaining_today_kwh,
            context.hourly_consumption_profile,
            context.consumption_forecast_kwh,
            context.battery_avg_charge_price_czk_per_kwh,
        );

        let summary = Self::generate_summary(&plan, &metrics);

        // Find current block index
        let block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let current_action = plan
            .schedule
            .get(block_index)
            .copied()
            .unwrap_or(ScheduledAction::GridPowered);

        let effective_price = context.price_block.effective_price_czk_per_kwh;

        match current_action {
            ScheduledAction::Charge { reason } => {
                if context.current_battery_soc < params.target_battery_soc {
                    eval.mode = InverterOperationMode::ForceCharge;

                    let reason_str = match reason {
                        ChargeReason::Opportunistic => "OPPORTUNISTIC",
                        ChargeReason::Arbitrage => "ARBITRAGE",
                        ChargeReason::NegativePrice => "NEGATIVE PRICE",
                    };

                    eval.reason = format!(
                        "{} CHARGE: {:.3} CZK/kWh [{}]",
                        reason_str, effective_price, summary
                    );
                    eval.decision_uid = Some(format!(
                        "winter_adaptive_v20:charge:{}",
                        reason_str.to_lowercase().replace(' ', "_")
                    ));

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
                    eval.mode = InverterOperationMode::NoChargeNoDischarge;
                    eval.reason = format!(
                        "HOLD CHARGE: Battery at target ({:.1}%), grid powers house [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v20:hold_charge".to_string());

                    let net_consumption =
                        context.consumption_forecast_kwh - context.solar_forecast_kwh;
                    if net_consumption > 0.0 {
                        eval.energy_flows.grid_import_kwh = net_consumption;
                        eval.cost_czk = net_consumption * effective_price;
                    } else {
                        let excess = -net_consumption;
                        eval.energy_flows.grid_export_kwh = excess;
                        eval.revenue_czk = excess * context.grid_export_price_czk_per_kwh;
                    }
                }
            }

            ScheduledAction::BatteryPowered => {
                eval.mode = InverterOperationMode::SelfUse;
                eval.reason = format!(
                    "BATTERY POWERED: {:.3} CZK/kWh saved [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_v20:battery_powered".to_string());

                let consumption_kwh = context
                    .hourly_consumption_profile
                    .map(|profile| {
                        let hour = context.price_block.block_start.hour() as usize;
                        profile[hour] / 4.0
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
                    let max_charge_rate = context.control_config.max_battery_charge_rate_kw * 0.25;
                    let charge_amount = excess.min(available_charge_capacity).min(max_charge_rate);

                    eval.energy_flows.battery_charge_kwh = charge_amount;

                    let export_amount = excess - charge_amount;
                    if export_amount > 0.0 {
                        eval.energy_flows.grid_export_kwh = export_amount;
                        eval.revenue_czk = export_amount * context.grid_export_price_czk_per_kwh;
                    }
                }
            }

            ScheduledAction::GridPowered | ScheduledAction::HoldCharge => {
                eval.mode = InverterOperationMode::NoChargeNoDischarge;

                let label = if matches!(current_action, ScheduledAction::HoldCharge) {
                    "HOLD AT TARGET"
                } else {
                    "GRID POWERED"
                };

                eval.reason = format!(
                    "{}: {:.3} CZK/kWh, preserving battery [{}]",
                    label, effective_price, summary
                );
                eval.decision_uid = Some(format!(
                    "winter_adaptive_v20:{}",
                    label.to_lowercase().replace(' ', "_")
                ));

                let net_consumption = context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if net_consumption > 0.0 {
                    eval.energy_flows.grid_import_kwh = net_consumption;
                    eval.cost_czk = net_consumption * effective_price;
                } else {
                    let excess = -net_consumption;
                    eval.energy_flows.grid_export_kwh = excess;
                    eval.revenue_czk = excess * context.grid_export_price_czk_per_kwh;
                }
            }

            ScheduledAction::Export => {
                if context.current_battery_soc > params.min_soc_after_export {
                    eval.mode = InverterOperationMode::ForceDischarge;
                    eval.reason = format!(
                        "EXPORT TO GRID: {:.3} CZK/kWh [{}]",
                        effective_price, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v20:export".to_string());

                    let discharge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                    eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                    eval.energy_flows.grid_export_kwh = discharge_kwh;
                    eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: Low SOC ({:.1}%) for export [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_v20:self_use_low_soc".to_string());
                }
            }

            ScheduledAction::SolarExcess => {
                eval.mode = InverterOperationMode::SelfUse;
                eval.reason = format!(
                    "SOLAR EXCESS: {:.3} CZK/kWh, natural charging [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_v20:solar_excess".to_string());

                let net_consumption = context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if net_consumption <= 0.0 {
                    let excess = -net_consumption;
                    let battery_capacity = context.control_config.battery_capacity_kwh;
                    let available_charge_capacity = (battery_capacity
                        * (context.control_config.max_battery_soc / 100.0)
                        - battery_capacity * (context.current_battery_soc / 100.0))
                        .max(0.0);
                    let max_charge_rate = context.control_config.max_battery_charge_rate_kw * 0.25;
                    let charge_amount = excess.min(available_charge_capacity).min(max_charge_rate);

                    eval.energy_flows.battery_charge_kwh = charge_amount;

                    let export_amount = excess - charge_amount;
                    if export_amount > 0.0 {
                        eval.energy_flows.grid_export_kwh = export_amount;
                        eval.revenue_czk = export_amount * context.grid_export_price_czk_per_kwh;
                    }
                }
            }
        }

        // Handle unscheduled negative prices
        if params.negative_price_handling_enabled
            && effective_price < 0.0
            && !matches!(current_action, ScheduledAction::Charge { .. })
        {
            if context.current_battery_soc < params.target_battery_soc {
                eval.mode = InverterOperationMode::ForceCharge;
                eval.reason = format!(
                    "NEGATIVE PRICE: {:.3} CZK/kWh (getting paid!) [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_v20:negative_price".to_string());

                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk = charge_kwh * effective_price;
            } else {
                eval.mode = InverterOperationMode::NoChargeNoDischarge;
                eval.reason = format!(
                    "NEGATIVE PRICE HOLD: {:.3} CZK/kWh, battery full ({:.1}%) [{}]",
                    effective_price, context.current_battery_soc, summary
                );
                eval.decision_uid = Some("winter_adaptive_v20:negative_price_hold".to_string());
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

    fn create_test_blocks() -> Vec<TimeBlockPrice> {
        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let grid_fee = 1.80;

        let mut blocks = Vec::new();
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
                    spot_sell_price_czk_per_kwh: None,
                });
            }
        }

        blocks
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
    fn test_strategy_basics() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        assert_eq!(strategy.name(), "Winter-Adaptive-V20");
        assert!(!strategy.is_enabled());

        let config_enabled = WinterAdaptiveV20Config {
            enabled: true,
            ..Default::default()
        };
        let strategy_enabled = WinterAdaptiveV20Strategy::new(config_enabled);
        assert!(strategy_enabled.is_enabled());
    }

    #[test]
    fn test_resolve_params_default_no_adjustments() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        // Neutral metrics — no adjustments should fire
        let metrics = DayMetrics {
            solar_ratio: 1.0, // medium (not high, not low)
            price_cv: 0.2,    // below volatile threshold (0.35)
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.0, // neutral
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0), // neutral
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert_eq!(params.daylight_start_hour, 7);
        assert_eq!(params.daylight_end_hour, 18);
        assert_eq!(params.bootstrap_count, 6);
        assert!(params.max_discharge_blocks.is_none());
        assert!((params.charge_reduction_factor - 1.0).abs() < f32::EPSILON);
        assert!((params.min_savings_threshold_czk - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_params_volatile_day() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 1.0,
            price_cv: 0.5, // above volatile threshold (0.35)
            price_spread_ratio: 2.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0),
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert!((params.min_savings_threshold_czk - 0.2).abs() < f32::EPSILON);
        assert_eq!(params.bootstrap_count, 12);
        assert!((params.min_export_spread_czk - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_params_expensive_day() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 1.0,
            price_cv: 0.2,
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.8, // above expensive threshold (0.5)
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0),
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert!((params.min_savings_threshold_czk - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_params_high_solar() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 1.5, // above high threshold (1.1)
            price_cv: 0.2,
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0),
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert_eq!(params.daylight_start_hour, 6);
        assert_eq!(params.daylight_end_hour, 19);
        assert!((params.solar_confidence_factor - 0.9).abs() < f32::EPSILON);
        assert_eq!(params.max_discharge_blocks, Some(12));
    }

    #[test]
    fn test_resolve_params_low_solar() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 0.3, // below low threshold (0.9)
            price_cv: 0.2,
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0),
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert_eq!(params.daylight_start_hour, 10);
        assert_eq!(params.daylight_end_hour, 14);
        assert!((params.solar_confidence_factor - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_params_tomorrow_expensive() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 1.0,
            price_cv: 0.2,
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.5), // above expensive ratio (1.3)
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert_eq!(params.max_discharge_blocks, Some(16));
    }

    #[test]
    fn test_resolve_params_tomorrow_cheap() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let metrics = DayMetrics {
            solar_ratio: 1.0,
            price_cv: 0.2,
            price_spread_ratio: 1.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(0.5), // below cheap ratio (0.7)
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        assert!((params.charge_reduction_factor - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_params_composable_volatile_high_solar() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);

        // Both volatile AND high solar should apply simultaneously
        let metrics = DayMetrics {
            solar_ratio: 1.5, // high solar
            price_cv: 0.5,    // volatile
            price_spread_ratio: 2.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: Some(1.0),
            tomorrow_solar_ratio: 1.0,
            price_stats: Default::default(),
        };

        let params = strategy.resolve_params(&metrics);
        // Volatile adjustments
        assert!((params.min_savings_threshold_czk - 0.2).abs() < f32::EPSILON);
        assert_eq!(params.bootstrap_count, 12);
        // High solar adjustments
        assert_eq!(params.daylight_start_hour, 6);
        assert_eq!(params.daylight_end_hour, 19);
        assert!((params.solar_confidence_factor - 0.9).abs() < f32::EPSILON);
        assert_eq!(params.max_discharge_blocks, Some(12));
    }

    #[test]
    fn test_generate_plan_basic() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);
        let blocks = create_test_blocks();

        // Use default resolved params (no metrics adjustments)
        let params = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan = strategy.generate_plan(&params, &blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        // Should have some charge blocks and some battery-powered blocks
        assert!(
            plan.charge_blocks > 0 || plan.battery_powered_blocks > 0,
            "Plan should have either charge or battery-powered blocks"
        );
        assert_eq!(plan.schedule.len(), 96, "Should have 96 blocks (24h * 4)");
    }

    #[test]
    fn test_budget_allocates_expensive_first() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);
        let blocks = create_test_blocks();

        let params = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan = strategy.generate_plan(&params, &blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let battery_prices: Vec<f32> = plan
            .schedule
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, ScheduledAction::BatteryPowered | ScheduledAction::Export))
            .filter_map(|(i, _)| blocks.get(i).map(|b| b.effective_price_czk_per_kwh))
            .collect();

        let grid_prices: Vec<f32> = plan
            .schedule
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, ScheduledAction::GridPowered))
            .filter_map(|(i, _)| blocks.get(i).map(|b| b.effective_price_czk_per_kwh))
            .collect();

        if !battery_prices.is_empty() && !grid_prices.is_empty() {
            let min_battery = battery_prices.iter().cloned().fold(f32::INFINITY, f32::min);
            let max_grid = grid_prices
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max);

            assert!(
                min_battery >= max_grid - 1.0,
                "Cheapest battery block ({:.2}) should be >= most expensive grid block ({:.2}) - 1.0",
                min_battery,
                max_grid,
            );
        }
    }

    #[test]
    fn test_max_discharge_blocks_cap() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);
        let blocks = create_test_blocks();

        // Unlimited discharge blocks
        let params_unlimited = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan_unlimited = strategy.generate_plan(
            &params_unlimited,
            &blocks,
            80.0,
            10.0,
            3.0,
            0.0,
            None,
            0.25,
            3.0,
        );

        // Capped at 4 discharge blocks
        let params_capped = ResolvedParams {
            max_discharge_blocks: Some(4),
            ..params_unlimited.clone()
        };

        let plan_capped = strategy.generate_plan(
            &params_capped,
            &blocks,
            80.0,
            10.0,
            3.0,
            0.0,
            None,
            0.25,
            3.0,
        );

        // Capped should have fewer or equal battery-powered blocks
        assert!(
            plan_capped.battery_powered_blocks <= 4,
            "Capped plan should have at most 4 battery-powered blocks, got {}",
            plan_capped.battery_powered_blocks,
        );

        if plan_unlimited.battery_powered_blocks > 4 {
            assert!(
                plan_capped.battery_powered_blocks < plan_unlimited.battery_powered_blocks,
                "Capped ({}) should be less than unlimited ({})",
                plan_capped.battery_powered_blocks,
                plan_unlimited.battery_powered_blocks,
            );
        }
    }

    #[test]
    fn test_charge_reduction_factor() {
        let config = WinterAdaptiveV20Config::default();
        let strategy = WinterAdaptiveV20Strategy::new(config);
        let blocks = create_test_blocks();

        let params_normal = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 0.0, // disable opportunistic
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan_normal = strategy.generate_plan(
            &params_normal,
            &blocks,
            15.0,
            10.0,
            3.0,
            0.0,
            None,
            0.25,
            1.0,
        );

        let params_reduced = ResolvedParams {
            charge_reduction_factor: 0.5,
            ..params_normal.clone()
        };

        let plan_reduced = strategy.generate_plan(
            &params_reduced,
            &blocks,
            15.0,
            10.0,
            3.0,
            0.0,
            None,
            0.25,
            1.0,
        );

        // Reduced charging should have fewer or equal charge blocks
        assert!(
            plan_reduced.charge_blocks <= plan_normal.charge_blocks,
            "Reduced ({}) should be <= normal ({})",
            plan_reduced.charge_blocks,
            plan_normal.charge_blocks,
        );
    }

    #[test]
    fn test_evaluate_with_day_metrics() {
        let config = WinterAdaptiveV20Config {
            enabled: true,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV20Strategy::new(config);
        let blocks = create_test_blocks();
        let control_config = create_test_control_config();

        // Pick a morning peak block
        let block_index = 7 * 4; // hour 7
        let context = EvaluationContext {
            price_block: &blocks[block_index],
            all_price_blocks: Some(&blocks),
            control_config: &control_config,
            current_battery_soc: 80.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.5,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 3.0,
            hourly_consumption_profile: None,
        };

        let eval = strategy.evaluate(&context);

        // Should produce a valid evaluation with V20 decision UID
        assert!(
            eval.decision_uid
                .as_ref()
                .is_some_and(|uid| uid.starts_with("winter_adaptive_v20:")),
            "Decision UID should start with winter_adaptive_v20:, got {:?}",
            eval.decision_uid,
        );
        // The reason should include ADAPTIVE summary
        assert!(
            eval.reason.contains("ADAPTIVE") || eval.reason.contains("NEGATIVE PRICE"),
            "Reason should contain ADAPTIVE or NEGATIVE PRICE summary, got: {}",
            eval.reason,
        );
    }

    #[test]
    fn test_negative_price_always_charges() {
        let config = WinterAdaptiveV20Config {
            negative_price_handling_enabled: true,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let effective_price = if hour == 3 { -0.5 } else { 3.0 };
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: effective_price - 1.0,
                    effective_price_czk_per_kwh: effective_price,
                    spot_sell_price_czk_per_kwh: None,
                });
            }
        }

        let params = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan = strategy.generate_plan(&params, &blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        for i in (3 * 4)..(4 * 4) {
            assert!(
                matches!(
                    plan.schedule[i],
                    ScheduledAction::Charge {
                        reason: ChargeReason::NegativePrice
                    }
                ),
                "Block {} (hour 3) should be Charge{{NegativePrice}}, got: {:?}",
                i,
                plan.schedule[i],
            );
        }
    }

    #[test]
    fn test_flat_prices_no_cycling() {
        let config = WinterAdaptiveV20Config {
            min_savings_threshold_czk: 0.5,
            opportunistic_charge_threshold_czk: 0.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveV20Strategy::new(config);

        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let blocks: Vec<TimeBlockPrice> = (0..96)
            .map(|i| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
                effective_price_czk_per_kwh: 3.0,
                spot_sell_price_czk_per_kwh: None,
            })
            .collect();

        let params = ResolvedParams {
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 0.0,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_count: 6,
            max_discharge_blocks: None,
            charge_reduction_factor: 1.0,
        };

        let plan = strategy.generate_plan(&params, &blocks, 80.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let battery_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::BatteryPowered | ScheduledAction::Export))
            .count();

        assert_eq!(battery_count, 0, "Flat prices should not allocate battery");
    }
}
