// Copyright (c) 2025 SOLARE S.R.O.

//! # Winter Adaptive C20 Strategy - Maximally Configurable Adaptive Budget Allocation
//!
//! **Status:** Simulator-only - never exposed in production FluxION app
//!
//! ## Overview
//!
//! C20 ("Configurable V20") exposes every decision point from V20 as a config parameter,
//! enabling the Ralph Loop / simulation workflow to iterate on configs without recompilation.
//! With defaults matching V20 exactly, C20 is a strict superset.
//!
//! ## Config Parameters (45 total)
//!
//! All V20 config params (19) + all V20 hardcoded algorithm constants + all resolve_params
//! adjustment values. This allows sweep scripts to find optimal adjustment values per day type.
//!
//! ### V20 Base (19 params)
//! 12 V10 base params + 7 DayMetrics thresholds
//!
//! ### Algorithm Internals (C10-style, 12 params)
//! - daylight_start_hour, daylight_end_hour (V10 hardcodes 7..18)
//! - bootstrap_block_count (V10 hardcodes 6)
//! - export_enabled toggle
//! - gap_bridging, consecutive_charge_groups, hold_charge, short_gap_removal toggles
//! - gap_bridging_max_gap_blocks, short_gap_min_size_blocks
//! - max_charge_blocks_per_day, max_discharge_blocks_per_day
//!
//! ### Resolve Params Adjustment Values (14 params)
//! - volatile_min_savings, volatile_bootstrap_count, volatile_min_export_spread
//! - expensive_min_savings
//! - high_solar_daylight_start, high_solar_daylight_end, high_solar_confidence
//! - low_solar_daylight_start, low_solar_daylight_end, low_solar_confidence
//! - tomorrow_expensive_max_discharge_blocks
//! - tomorrow_cheap_charge_reduction_factor
//! - resolve_params_enabled (toggle to disable all adaptive adjustments → behaves like C10)

use chrono::Timelike;
use serde::{Deserialize, Serialize};

use fluxion_core::day_profiling::{compute_day_metrics, estimate_daily_consumption};
use fluxion_core::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::day_profile::DayMetrics;
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive C20 strategy (45 parameters)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WinterAdaptiveC20Config {
    // === Core ===
    pub enabled: bool,
    pub priority: u8,

    // === Battery ===
    pub target_battery_soc: f32,
    pub min_discharge_soc: f32,
    pub battery_round_trip_efficiency: f32,

    // === Price-Based Charging ===
    pub negative_price_handling_enabled: bool,
    pub opportunistic_charge_threshold_czk: f32,
    pub min_savings_threshold_czk: f32,

    // === Export ===
    pub export_enabled: bool,
    pub min_export_spread_czk: f32,
    pub min_soc_after_export: f32,

    // === Solar ===
    pub solar_threshold_kwh: f32,
    pub solar_confidence_factor: f32,

    // === Algorithm Internals (V10/V20 hardcodes these) ===
    pub daylight_start_hour: u8,
    pub daylight_end_hour: u8,
    pub bootstrap_block_count: usize,
    pub max_charge_blocks_per_day: Option<usize>,
    pub max_discharge_blocks_per_day: Option<usize>,

    // === Post-Processing Toggles ===
    pub consecutive_charge_groups_enabled: bool,
    pub gap_bridging_enabled: bool,
    pub gap_bridging_max_gap_blocks: usize,
    pub short_gap_removal_enabled: bool,
    pub short_gap_min_size_blocks: usize,
    pub hold_charge_enabled: bool,

    // === DayMetrics Thresholds (when to trigger adjustments) ===
    pub volatile_cv_threshold: f32,
    pub expensive_level_threshold: f32,
    pub high_solar_ratio_threshold: f32,
    pub low_solar_ratio_threshold: f32,
    pub tomorrow_expensive_ratio: f32,
    pub tomorrow_cheap_ratio: f32,
    pub negative_price_fraction_threshold: f32,

    // === Resolve Params: master toggle ===
    /// When false, skip all DayMetrics-based adjustments (behaves like C10)
    pub resolve_params_enabled: bool,

    // === Resolve Params: Volatile day adjustment values ===
    pub volatile_min_savings: f32,
    pub volatile_bootstrap_count: usize,
    pub volatile_min_export_spread: f32,

    // === Resolve Params: Expensive day adjustment values ===
    pub expensive_min_savings: f32,

    // === Resolve Params: High solar adjustment values ===
    pub high_solar_daylight_start: u8,
    pub high_solar_daylight_end: u8,
    pub high_solar_confidence: f32,

    // === Resolve Params: Low solar adjustment values ===
    pub low_solar_daylight_start: u8,
    pub low_solar_daylight_end: u8,
    pub low_solar_confidence: f32,

    // === Resolve Params: Tomorrow expensive adjustment ===
    pub tomorrow_expensive_max_discharge_blocks: usize,

    // === Resolve Params: Tomorrow cheap adjustment ===
    pub tomorrow_cheap_charge_reduction_factor: f32,
}

impl Default for WinterAdaptiveC20Config {
    fn default() -> Self {
        Self {
            // Core (matches V20)
            enabled: false,
            priority: 100,

            // Battery (matches V20)
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,

            // Price-Based Charging (matches V20)
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_savings_threshold_czk: 0.5,

            // Export (matches V20, export_enabled NEW with default true)
            export_enabled: true,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 35.0,

            // Solar (matches V20)
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,

            // Algorithm Internals (V20 defaults before resolve_params)
            daylight_start_hour: 7,
            daylight_end_hour: 18,
            bootstrap_block_count: 6,
            max_charge_blocks_per_day: None,
            max_discharge_blocks_per_day: None,

            // Post-Processing (matches V20)
            consecutive_charge_groups_enabled: true,
            gap_bridging_enabled: true,
            gap_bridging_max_gap_blocks: 2,
            short_gap_removal_enabled: true,
            short_gap_min_size_blocks: 2,
            hold_charge_enabled: true,

            // DayMetrics Thresholds (matches V20)
            volatile_cv_threshold: 0.35,
            expensive_level_threshold: 0.5,
            high_solar_ratio_threshold: 1.1,
            low_solar_ratio_threshold: 0.9,
            tomorrow_expensive_ratio: 1.3,
            tomorrow_cheap_ratio: 0.7,
            negative_price_fraction_threshold: 0.0,

            // Resolve Params: enabled by default (matches V20 behavior)
            resolve_params_enabled: true,

            // Resolve Params: Volatile adjustment values (V20 hardcodes these)
            volatile_min_savings: 0.2,
            volatile_bootstrap_count: 12,
            volatile_min_export_spread: 3.0,

            // Resolve Params: Expensive adjustment (V20 hardcodes 1.0)
            expensive_min_savings: 1.0,

            // Resolve Params: High solar (V20 hardcodes 6, 19, 0.9)
            high_solar_daylight_start: 6,
            high_solar_daylight_end: 19,
            high_solar_confidence: 0.9,

            // Resolve Params: Low solar (V20 hardcodes 10, 14, 0.5)
            low_solar_daylight_start: 10,
            low_solar_daylight_end: 14,
            low_solar_confidence: 0.5,

            // Resolve Params: Tomorrow expensive (V20 hardcodes 16)
            tomorrow_expensive_max_discharge_blocks: 16,

            // Resolve Params: Tomorrow cheap (V20 hardcodes 0.5)
            tomorrow_cheap_charge_reduction_factor: 0.5,
        }
    }
}

/// Parameters resolved from base config + DayMetrics adjustments.
#[derive(Debug, Clone)]
struct ResolvedParams {
    target_battery_soc: f32,
    min_discharge_soc: f32,
    battery_round_trip_efficiency: f32,
    negative_price_handling_enabled: bool,
    opportunistic_charge_threshold_czk: f32,
    min_export_spread_czk: f32,
    min_soc_after_export: f32,
    solar_confidence_factor: f32,
    min_savings_threshold_czk: f32,
    daylight_start_hour: u32,
    daylight_end_hour: u32,
    bootstrap_count: usize,
    max_discharge_blocks: Option<usize>,
    charge_reduction_factor: f32,
    export_enabled: bool,
}

/// Scheduled action for a specific block in the C20 plan
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

pub struct WinterAdaptiveC20Strategy {
    config: WinterAdaptiveC20Config,
}

impl WinterAdaptiveC20Strategy {
    pub fn new(config: WinterAdaptiveC20Config) -> Self {
        Self { config }
    }

    /// Resolve effective parameters from base config + DayMetrics.
    /// All adjustment values are now configurable instead of hardcoded.
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
            daylight_start_hour: self.config.daylight_start_hour as u32,
            daylight_end_hour: self.config.daylight_end_hour as u32,
            bootstrap_count: self.config.bootstrap_block_count,
            max_discharge_blocks: self.config.max_discharge_blocks_per_day,
            charge_reduction_factor: 1.0,
            export_enabled: self.config.export_enabled,
        };

        if !self.config.resolve_params_enabled {
            return params;
        }

        // --- Volatile: configurable adjustment values ---
        if metrics.price_cv > self.config.volatile_cv_threshold {
            params.min_savings_threshold_czk = params
                .min_savings_threshold_czk
                .min(self.config.volatile_min_savings);
            params.bootstrap_count = self.config.volatile_bootstrap_count;
            params.min_export_spread_czk = params
                .min_export_spread_czk
                .min(self.config.volatile_min_export_spread);
        }

        // --- Expensive: configurable min_savings floor ---
        if metrics.price_level_vs_charge_cost > self.config.expensive_level_threshold {
            params.min_savings_threshold_czk = params
                .min_savings_threshold_czk
                .max(self.config.expensive_min_savings);
        }

        // --- High solar: configurable daylight window + confidence ---
        if metrics.solar_ratio > self.config.high_solar_ratio_threshold {
            params.daylight_start_hour = self.config.high_solar_daylight_start as u32;
            params.daylight_end_hour = self.config.high_solar_daylight_end as u32;
            params.solar_confidence_factor = params
                .solar_confidence_factor
                .max(self.config.high_solar_confidence);
        }

        // --- Low solar: configurable daylight window + confidence ---
        if metrics.solar_ratio < self.config.low_solar_ratio_threshold {
            params.daylight_start_hour = self.config.low_solar_daylight_start as u32;
            params.daylight_end_hour = self.config.low_solar_daylight_end as u32;
            params.solar_confidence_factor = params
                .solar_confidence_factor
                .min(self.config.low_solar_confidence);
        }

        // --- Tomorrow expensive: configurable max discharge blocks ---
        if metrics
            .tomorrow_price_ratio
            .is_some_and(|r| r > self.config.tomorrow_expensive_ratio)
        {
            params.max_discharge_blocks = Some(self.config.tomorrow_expensive_max_discharge_blocks);
        }

        // --- Tomorrow cheap: configurable charge reduction factor ---
        if metrics
            .tomorrow_price_ratio
            .is_some_and(|r| r < self.config.tomorrow_cheap_ratio)
        {
            params.charge_reduction_factor = self.config.tomorrow_cheap_charge_reduction_factor;
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
                profile[hour] / 4.0
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

        // Apply max_discharge_blocks cap
        if let Some(max_blocks) = params.max_discharge_blocks
            && demand_blocks > max_blocks
        {
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

        let adjusted_energy = capped_energy * params.charge_reduction_factor;

        let mut charge_blocks_needed = if energy_per_charge_block > 0.0 {
            (adjusted_energy / energy_per_charge_block).ceil() as usize
        } else {
            0
        };

        // Apply max_charge_blocks_per_day cap
        if let Some(max_charge) = self.config.max_charge_blocks_per_day {
            charge_blocks_needed = charge_blocks_needed.min(max_charge);
        }

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
        if params.export_enabled {
            for (i, action) in schedule.iter_mut().enumerate() {
                if matches!(action, ScheduledAction::BatteryPowered) {
                    let spread = blocks[i].effective_price_czk_per_kwh - avg_charge_price;
                    if spread >= params.min_export_spread_czk {
                        *action = ScheduledAction::Export;
                        export_count += 1;
                    }
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

        // === Phase 11: Post-processing (all togglable) ===
        if self.config.consecutive_charge_groups_enabled {
            Self::ensure_consecutive_charge_groups(&mut charge_indices, blocks);
            for &idx in &charge_indices {
                if matches!(schedule[idx], ScheduledAction::GridPowered) {
                    schedule[idx] = ScheduledAction::Charge {
                        reason: ChargeReason::Arbitrage,
                    };
                }
            }
        }

        if self.config.gap_bridging_enabled {
            Self::bridge_short_charge_gaps(
                &mut charge_indices,
                blocks,
                self.config.gap_bridging_max_gap_blocks,
            );
            for &idx in &charge_indices {
                if matches!(schedule[idx], ScheduledAction::GridPowered) {
                    schedule[idx] = ScheduledAction::Charge {
                        reason: ChargeReason::Arbitrage,
                    };
                }
            }
        }

        if self.config.short_gap_removal_enabled {
            Self::remove_short_gaps_in_schedule(
                &mut schedule,
                blocks,
                self.config.short_gap_min_size_blocks,
            );
        }

        if self.config.hold_charge_enabled {
            Self::add_hold_charge_blocks(&mut schedule);
        }

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

    fn generate_summary(plan: &DayPlan, metrics: &DayMetrics) -> String {
        format!(
            "C20-ADAPTIVE: {} chg/{} bat/{} exp, avg {:.2} CZK | solar={:.2} cv={:.2} level={:.2}",
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

impl EconomicStrategy for WinterAdaptiveC20Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-C20"
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

        // Compute DayMetrics
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

        // Resolve parameters (configurable adjustments)
        let params = self.resolve_params(&metrics);

        // Generate the day plan
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
                        "winter_adaptive_c20:charge:{}",
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
                    eval.decision_uid = Some("winter_adaptive_c20:hold_charge".to_string());

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
                eval.decision_uid = Some("winter_adaptive_c20:battery_powered".to_string());

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
                    "winter_adaptive_c20:{}",
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
                    eval.decision_uid = Some("winter_adaptive_c20:export".to_string());

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
                    eval.decision_uid = Some("winter_adaptive_c20:self_use_low_soc".to_string());
                }
            }

            ScheduledAction::SolarExcess => {
                eval.mode = InverterOperationMode::SelfUse;
                eval.reason = format!(
                    "SOLAR EXCESS: {:.3} CZK/kWh, natural charging [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_c20:solar_excess".to_string());

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
                eval.decision_uid = Some("winter_adaptive_c20:negative_price".to_string());

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
                eval.decision_uid = Some("winter_adaptive_c20:negative_price_hold".to_string());
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
                    0..=5 => 1.5,
                    6..=8 => 5.0,
                    9..=11 => 2.5,
                    12..=14 => 0.5,
                    15..=17 => 4.5,
                    18..=20 => 6.0,
                    _ => 2.0,
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

    #[test]
    fn test_c20_defaults_match_v20() {
        let config = WinterAdaptiveC20Config::default();
        let strategy = WinterAdaptiveC20Strategy::new(config);

        assert_eq!(strategy.name(), "Winter-Adaptive-C20");
        assert!(!strategy.is_enabled());
    }

    #[test]
    fn test_resolve_params_disabled_acts_like_base() {
        let config = WinterAdaptiveC20Config {
            resolve_params_enabled: false,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC20Strategy::new(config);

        // Even with extreme metrics, params should be unchanged
        let metrics = DayMetrics {
            solar_ratio: 2.0,
            price_cv: 1.0,
            price_level_vs_charge_cost: 2.0,
            negative_price_fraction: 0.5,
            tomorrow_price_ratio: Some(2.0),
            ..Default::default()
        };

        let params = strategy.resolve_params(&metrics);
        assert_eq!(params.daylight_start_hour, 7);
        assert_eq!(params.daylight_end_hour, 18);
        assert_eq!(params.bootstrap_count, 6);
        assert!(params.max_discharge_blocks.is_none());
        assert!((params.charge_reduction_factor - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_custom_volatile_adjustments() {
        let config = WinterAdaptiveC20Config {
            volatile_min_savings: 0.1,
            volatile_bootstrap_count: 20,
            volatile_min_export_spread: 2.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC20Strategy::new(config);

        let metrics = DayMetrics {
            price_cv: 0.5, // above default threshold 0.35
            ..Default::default()
        };

        let params = strategy.resolve_params(&metrics);
        assert!((params.min_savings_threshold_czk - 0.1).abs() < 0.001);
        assert_eq!(params.bootstrap_count, 20);
        assert!((params.min_export_spread_czk - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_custom_solar_adjustments() {
        let config = WinterAdaptiveC20Config {
            high_solar_daylight_start: 5,
            high_solar_daylight_end: 20,
            high_solar_confidence: 0.95,
            low_solar_daylight_start: 11,
            low_solar_daylight_end: 13,
            low_solar_confidence: 0.3,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC20Strategy::new(config);

        // High solar
        let metrics_high = DayMetrics {
            solar_ratio: 1.5,
            ..Default::default()
        };
        let params = strategy.resolve_params(&metrics_high);
        assert_eq!(params.daylight_start_hour, 5);
        assert_eq!(params.daylight_end_hour, 20);
        assert!((params.solar_confidence_factor - 0.95).abs() < 0.001);

        // Low solar
        let metrics_low = DayMetrics {
            solar_ratio: 0.5,
            ..Default::default()
        };
        let params = strategy.resolve_params(&metrics_low);
        assert_eq!(params.daylight_start_hour, 11);
        assert_eq!(params.daylight_end_hour, 13);
        assert!((params.solar_confidence_factor - 0.3).abs() < 0.001);
    }

    #[test]
    fn test_generate_plan_basic() {
        let config = WinterAdaptiveC20Config::default();
        let strategy = WinterAdaptiveC20Strategy::new(config);
        let blocks = create_test_blocks();

        let metrics = DayMetrics::default();
        let params = strategy.resolve_params(&metrics);

        let plan = strategy.generate_plan(&params, &blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        assert_eq!(plan.schedule.len(), 96);
        assert!(plan.charge_blocks > 0);
    }

    #[test]
    fn test_export_disabled() {
        let config = WinterAdaptiveC20Config {
            export_enabled: false,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC20Strategy::new(config);
        let blocks = create_test_blocks();

        let metrics = DayMetrics::default();
        let params = strategy.resolve_params(&metrics);

        let plan = strategy.generate_plan(&params, &blocks, 80.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let export_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Export))
            .count();
        assert_eq!(
            export_count, 0,
            "With export disabled, no blocks should be Export"
        );
    }
}
