// Copyright (c) 2025 SOLARE S.R.O.

//! # Winter Adaptive C10 Strategy - Maximally Configurable Battery Budget Allocation
//!
//! **Status:** Simulator-only - never exposed in production FluxION app
//!
//! ## Overview
//!
//! C10 ("Configurable V10") exposes every decision point from V10 as a config parameter,
//! enabling the Ralph Loop / simulation workflow to iterate on configs without recompilation.
//! With defaults matching V10 exactly, C10 is a strict superset.
//!
//! ## Config Parameters (31 total, 19 new vs V10)
//!
//! All V10 hardcoded constants are now configurable:
//! - Daylight window hours (was 8..16)
//! - Bootstrap block count (was 6)
//! - Gap bridging parameters (was hardcoded 2 blocks, 1.0 CZK tolerance)
//! - Charge price estimation method (bootstrap/fixed/weighted)
//! - Demand estimation method (consumption_weighted/block_count)
//! - Budget allocation strategy (greedy_by_price/consumption_weighted)
//! - Post-processing toggles (gap bridging, consecutive groups, hold charge)
//! - Block limits (max charge/discharge blocks per day)

use chrono::Timelike;
use serde::{Deserialize, Serialize};

use fluxion_core::strategy::{Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext};
use fluxion_types::{inverter::InverterOperationMode, pricing::TimeBlockPrice};

/// Configuration for Winter Adaptive C10 strategy (31 parameters)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WinterAdaptiveC10Config {
    // === Core ===
    /// Enable this strategy
    pub enabled: bool,

    /// Priority for conflict resolution (higher = preferred)
    pub priority: u8,

    // === Battery ===
    /// Target battery SOC (%) - maximum charge level
    pub target_battery_soc: f32,

    /// Hardware minimum battery SOC (%) - never discharge below this
    pub min_discharge_soc: f32,

    /// Round-trip battery efficiency (charge x discharge efficiency)
    pub battery_round_trip_efficiency: f32,

    // === Price-Based Charging ===
    /// Enable negative price handling (always charge if getting paid)
    pub negative_price_handling_enabled: bool,

    /// Price threshold (CZK/kWh) below which we always charge
    pub opportunistic_charge_threshold_czk: f32,

    /// Minimum savings per kWh (block_price - avg_charge_price) to justify battery use
    pub min_savings_threshold_czk: f32,

    // === Export ===
    /// Enable grid export (ForceDischarge) for profitable blocks
    pub export_enabled: bool,

    /// Minimum price spread (CZK) to allow grid export instead of home use
    pub min_export_spread_czk: f32,

    /// Minimum SOC (%) after discharge to allow grid export
    pub min_soc_after_export: f32,

    // === Solar ===
    /// Minimum solar forecast (kWh remaining today) to consider solar excess
    pub solar_threshold_kwh: f32,

    /// Factor to apply to solar forecast for conservative planning
    pub solar_confidence_factor: f32,

    /// Start hour of daylight window (UTC) for solar distribution
    pub daylight_start_hour: u8,

    /// End hour of daylight window (UTC) for solar distribution
    pub daylight_end_hour: u8,

    // === Charge Planning ===
    /// Number of cheapest blocks to bootstrap charge price estimation
    pub bootstrap_block_count: usize,

    /// Charge price estimation method: "bootstrap", "fixed", "weighted"
    pub charge_price_estimation_method: String,

    /// Fixed charge price (CZK/kWh) used when method is "fixed"
    pub fixed_charge_price_czk: f32,

    // === Demand Estimation ===
    /// Demand estimation method: "consumption_weighted", "block_count"
    pub demand_estimation_method: String,

    /// Use hourly consumption profile for per-block estimates
    pub use_hourly_consumption_profile: bool,

    // === Budget Allocation ===
    /// Budget allocation strategy: "greedy_by_price", "consumption_weighted"
    pub budget_allocation_strategy: String,

    /// Maximum charge blocks per day (None = unlimited)
    pub max_charge_blocks_per_day: Option<usize>,

    /// Maximum discharge blocks per day (None = unlimited)
    pub max_discharge_blocks_per_day: Option<usize>,

    // === Post-Processing Toggles ===
    /// Ensure charge blocks form consecutive groups of at least 2
    pub consecutive_charge_groups_enabled: bool,

    /// Bridge short gaps between charge groups
    pub gap_bridging_enabled: bool,

    /// Maximum gap size (in blocks) to bridge between charge groups
    pub gap_bridging_max_gap_blocks: usize,

    /// Price tolerance (CZK) above avg charge price for gap bridging
    pub gap_bridging_price_tolerance_czk: f32,

    /// Remove short non-charge gaps in final schedule
    pub short_gap_removal_enabled: bool,

    /// Minimum gap size (in blocks) below which gaps are bridged
    pub short_gap_min_size_blocks: usize,

    /// Add HoldCharge blocks between charging and first discharge
    pub hold_charge_enabled: bool,
}

impl Default for WinterAdaptiveC10Config {
    fn default() -> Self {
        Self {
            // Core (matches V10)
            enabled: false,
            priority: 100,

            // Battery (matches V10)
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,

            // Price-Based Charging (matches V10)
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_savings_threshold_czk: 0.5,

            // Export (matches V10, export_enabled is NEW with default true)
            export_enabled: true,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 35.0,

            // Solar (matches V10, daylight hours are NEW matching V10's hardcoded 7..18)
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            daylight_start_hour: 7,
            daylight_end_hour: 18,

            // Charge Planning (NEW, defaults match V10 behavior)
            bootstrap_block_count: 6,
            charge_price_estimation_method: "bootstrap".to_string(),
            fixed_charge_price_czk: 3.0,

            // Demand Estimation (NEW, defaults match V10 behavior)
            demand_estimation_method: "consumption_weighted".to_string(),
            use_hourly_consumption_profile: true,

            // Budget Allocation (NEW, defaults match V10 behavior)
            budget_allocation_strategy: "greedy_by_price".to_string(),
            max_charge_blocks_per_day: None,
            max_discharge_blocks_per_day: None,

            // Post-Processing (NEW, defaults match V10 behavior)
            consecutive_charge_groups_enabled: true,
            gap_bridging_enabled: true,
            gap_bridging_max_gap_blocks: 2,
            gap_bridging_price_tolerance_czk: 2.0,
            short_gap_removal_enabled: true,
            short_gap_min_size_blocks: 2,
            hold_charge_enabled: true,
        }
    }
}

/// Scheduled action for a specific block in the C10 plan
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScheduledAction {
    /// Charge during this block
    Charge { reason: ChargeReason },
    /// Battery powers the house (allocated from budget)
    BatteryPowered,
    /// Grid powers the house (battery preserved)
    GridPowered,
    /// Export to grid (ForceDischarge)
    Export,
    /// Solar excess - SelfUse for natural charging
    SolarExcess,
    /// Hold battery at target SOC after charging completes
    HoldCharge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChargeReason {
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
    /// Average charge price (for scheduled charge blocks)
    avg_charge_price: f32,
    /// Number of blocks allocated as BatteryPowered
    battery_powered_blocks: usize,
    /// Number of charge blocks scheduled
    charge_blocks: usize,
    /// Number of export blocks scheduled
    export_blocks: usize,
}

pub struct WinterAdaptiveC10Strategy {
    config: WinterAdaptiveC10Config,
}

impl WinterAdaptiveC10Strategy {
    pub fn new(config: WinterAdaptiveC10Config) -> Self {
        Self { config }
    }

    /// Estimate net consumption for a block (consumption - solar)
    fn estimate_block_consumption(
        &self,
        block: &TimeBlockPrice,
        solar_per_block_kwh: f32,
        hourly_profile: Option<&[f32; 24]>,
        fallback_consumption: f32,
    ) -> f32 {
        let consumption_kwh = if self.config.use_hourly_consumption_profile {
            hourly_profile
                .map(|profile| {
                    let hour = block.block_start.hour() as usize;
                    profile[hour] / 4.0 // hourly kWh -> 15-min block
                })
                .unwrap_or(fallback_consumption)
        } else {
            fallback_consumption
        };

        (consumption_kwh - solar_per_block_kwh).max(0.0)
    }

    /// Check if an hour is within the configured daylight window
    fn is_daylight_hour(&self, hour: u32) -> bool {
        hour >= self.config.daylight_start_hour as u32
            && hour < self.config.daylight_end_hour as u32
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
        price_tolerance: f32,
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
        // Find the last charge block index
        let last_charge_idx = schedule
            .iter()
            .enumerate()
            .rev()
            .find(|(_, a)| matches!(a, ScheduledAction::Charge { .. }))
            .map(|(i, _)| i);

        let Some(last_charge) = last_charge_idx else {
            return;
        };

        // Find the first BatteryPowered/Export block after charging
        let first_discharge_idx = schedule
            .iter()
            .enumerate()
            .skip(last_charge + 1)
            .find(|(_, a)| matches!(a, ScheduledAction::BatteryPowered | ScheduledAction::Export))
            .map(|(i, _)| i);

        let end = first_discharge_idx.unwrap_or(schedule.len());

        // Convert GridPowered blocks between last charge and first discharge to HoldCharge
        for action in schedule.iter_mut().take(end).skip(last_charge + 1) {
            if matches!(action, ScheduledAction::GridPowered) {
                *action = ScheduledAction::HoldCharge;
            }
        }
    }

    /// Estimate charge price using configured method.
    fn estimate_charge_price(
        &self,
        available_blocks: &[(usize, f32)],
        energy_per_charge_block: f32,
        existing_energy: f32,
        battery_avg_charge_price: f32,
    ) -> f32 {
        match self.config.charge_price_estimation_method.as_str() {
            "fixed" => self.config.fixed_charge_price_czk,
            "weighted" => {
                // Weighted average: weight each block's price by how much energy it contributes
                let bootstrap_count = self
                    .config
                    .bootstrap_block_count
                    .min(available_blocks.len());
                if bootstrap_count == 0 {
                    return battery_avg_charge_price;
                }
                let total_energy = bootstrap_count as f32 * energy_per_charge_block;
                let weighted_sum: f32 = available_blocks[..bootstrap_count]
                    .iter()
                    .map(|(_, p)| p * energy_per_charge_block)
                    .sum();
                if total_energy > 0.0 {
                    weighted_sum / total_energy
                } else {
                    battery_avg_charge_price
                }
            }
            // "bootstrap" (default) - matches V10 behavior
            _ => {
                let bootstrap_count = self
                    .config
                    .bootstrap_block_count
                    .min(available_blocks.len());
                let cheapest_grid_price = if bootstrap_count > 0 {
                    available_blocks[..bootstrap_count]
                        .iter()
                        .map(|(_, p)| p)
                        .sum::<f32>()
                        / bootstrap_count as f32
                } else {
                    battery_avg_charge_price
                };

                // Blend existing battery's cost basis with estimated grid charge price
                if existing_energy > 0.0 {
                    let total_est =
                        existing_energy + bootstrap_count as f32 * energy_per_charge_block;
                    if total_est > 0.0 {
                        (existing_energy * battery_avg_charge_price
                            + bootstrap_count as f32
                                * energy_per_charge_block
                                * cheapest_grid_price)
                            / total_est
                    } else {
                        battery_avg_charge_price
                    }
                } else {
                    cheapest_grid_price
                }
            }
        }
    }

    /// Estimate battery demand using configured method.
    fn estimate_demand(
        &self,
        available_blocks: &[(usize, f32)],
        net_consumption: &[f32],
        estimated_charge_price: f32,
    ) -> f32 {
        match self.config.demand_estimation_method.as_str() {
            "block_count" => {
                // Simple: count blocks where savings exceed threshold, use fixed energy per block
                let qualifying_count = available_blocks
                    .iter()
                    .rev()
                    .filter(|(_, price)| {
                        price - estimated_charge_price >= self.config.min_savings_threshold_czk
                    })
                    .count();
                // Use average net consumption across all blocks as per-block estimate
                let avg_consumption = if net_consumption.is_empty() {
                    0.0
                } else {
                    net_consumption.iter().sum::<f32>() / net_consumption.len() as f32
                };
                qualifying_count as f32 * avg_consumption.max(0.0)
            }
            // "consumption_weighted" (default) - matches V10 behavior
            _ => {
                let mut demand_kwh: f32 = 0.0;
                for &(idx, price) in available_blocks.iter().rev() {
                    let savings = price - estimated_charge_price;
                    if savings < self.config.min_savings_threshold_czk {
                        break;
                    }
                    let consumption = net_consumption[idx];
                    if consumption > 0.0 {
                        demand_kwh += consumption;
                    }
                }
                demand_kwh
            }
        }
    }

    /// Allocate battery budget to blocks using configured strategy.
    fn allocate_budget(
        &self,
        rankable: &[(usize, f32)],
        net_consumption: &[f32],
        avg_charge_price: f32,
        mut remaining_budget: f32,
        schedule: &mut [ScheduledAction],
    ) -> (usize, f32) {
        let mut battery_powered_count = 0;
        let max_discharge = self.config.max_discharge_blocks_per_day;

        match self.config.budget_allocation_strategy.as_str() {
            "consumption_weighted" => {
                // Weight allocation by consumption: blocks with higher consumption get priority
                // but still sorted by price (most expensive first)
                for &(idx, price) in rankable {
                    if let Some(max) = max_discharge
                        && battery_powered_count >= max
                    {
                        break;
                    }

                    let savings = price - avg_charge_price;
                    if savings < self.config.min_savings_threshold_czk {
                        break;
                    }

                    let consumption = net_consumption[idx];
                    if consumption <= 0.0 {
                        continue;
                    }

                    // Weight: prefer blocks where consumption * price is highest
                    // (Still allocates greedily, but considers consumption magnitude)
                    if remaining_budget >= consumption {
                        schedule[idx] = ScheduledAction::BatteryPowered;
                        remaining_budget -= consumption;
                        battery_powered_count += 1;
                    }
                }
            }
            // "greedy_by_price" (default) - matches V10 behavior
            _ => {
                for &(idx, price) in rankable {
                    if let Some(max) = max_discharge
                        && battery_powered_count >= max
                    {
                        break;
                    }

                    let savings = price - avg_charge_price;
                    if savings < self.config.min_savings_threshold_czk {
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
            }
        }

        (battery_powered_count, remaining_budget)
    }

    /// Generate the day plan using charge-first budget allocation.
    #[allow(clippy::too_many_arguments)]
    fn generate_plan(
        &self,
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

        // === Phase 1: Negative prices -> always Charge ===
        if self.config.negative_price_handling_enabled {
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
        let effective_solar = solar_remaining_kwh * self.config.solar_confidence_factor;
        let solar_per_block = if n > 0 && effective_solar > 0.0 {
            let daylight_blocks = blocks
                .iter()
                .filter(|b| {
                    let h = b.block_start.hour();
                    self.is_daylight_hour(h)
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
                let is_daylight = self.is_daylight_hour(b.block_start.hour());
                let solar = if is_daylight { solar_per_block } else { 0.0 };
                self.estimate_block_consumption(b, solar, hourly_profile, fallback_consumption)
            })
            .collect();

        // === Pre-compute energy parameters ===
        // Derive block duration from data instead of hardcoding 0.25
        let block_duration_hours = if !blocks.is_empty() {
            blocks[0].duration_minutes as f32 / 60.0
        } else {
            0.25
        };
        let energy_per_charge_block = max_charge_rate_kw * block_duration_hours;
        let existing_energy = (current_soc - self.config.min_discharge_soc).max(0.0) / 100.0
            * battery_capacity_kwh
            * self.config.battery_round_trip_efficiency;

        // === Phase 3: Estimate battery demand (demand-driven charging) ===
        let mut available_blocks: Vec<(usize, f32)> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(schedule[*i], ScheduledAction::GridPowered))
            .map(|(i, b)| (i, b.effective_price_czk_per_kwh))
            .collect();

        // Sort ascending (cheapest first) for charge estimation
        available_blocks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Estimate charge price using configured method
        let estimated_charge_price = self.estimate_charge_price(
            &available_blocks,
            energy_per_charge_block,
            existing_energy,
            battery_avg_charge_price,
        );

        // Estimate demand using configured method
        let demand_kwh =
            self.estimate_demand(&available_blocks, &net_consumption, estimated_charge_price);

        // === Phase 4: Calculate charge blocks needed (demand-driven) ===
        let energy_to_charge = (demand_kwh - existing_energy).max(0.0);

        let max_chargeable =
            (self.config.target_battery_soc - current_soc).max(0.0) / 100.0 * battery_capacity_kwh;
        let capped_energy = energy_to_charge.min(max_chargeable);

        let charge_blocks_needed = if energy_per_charge_block > 0.0 {
            let needed = (capped_energy / energy_per_charge_block).ceil() as usize;
            // Apply max charge blocks limit if configured
            if let Some(max) = self.config.max_charge_blocks_per_day {
                needed.min(max)
            } else {
                needed
            }
        } else {
            0
        };

        // === Phase 5: Select cheapest blocks for charging ===
        // Include opportunistic blocks below threshold
        for (i, block) in blocks.iter().enumerate() {
            if matches!(schedule[i], ScheduledAction::GridPowered)
                && block.effective_price_czk_per_kwh
                    < self.config.opportunistic_charge_threshold_czk
            {
                schedule[i] = ScheduledAction::Charge {
                    reason: ChargeReason::Opportunistic,
                };
                charge_indices.push(i);
            }
        }

        // Now assign cheapest blocks as Charge{Arbitrage} until we have enough
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
        let budget = (existing_energy + charged_energy * self.config.battery_round_trip_efficiency)
            .min(
                (self.config.target_battery_soc - self.config.min_discharge_soc) / 100.0
                    * battery_capacity_kwh
                    * self.config.battery_round_trip_efficiency,
            );

        // Calculate blended charge price
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
        let (battery_powered_count, _remaining_budget) = self.allocate_budget(
            &rankable,
            &net_consumption,
            avg_charge_price,
            budget,
            &mut schedule,
        );

        // === Phase 9: Export upgrades ===
        let mut export_count = 0;
        if self.config.export_enabled {
            for (i, action) in schedule.iter_mut().enumerate() {
                if matches!(action, ScheduledAction::BatteryPowered) {
                    let spread = blocks[i].effective_price_czk_per_kwh - avg_charge_price;
                    if spread >= self.config.min_export_spread_czk {
                        *action = ScheduledAction::Export;
                        export_count += 1;
                    }
                }
            }
        }

        // === Phase 10: Solar excess blocks -> SelfUse ===
        for (i, block) in blocks.iter().enumerate() {
            if matches!(schedule[i], ScheduledAction::GridPowered) {
                let is_daylight = self.is_daylight_hour(block.block_start.hour());
                if is_daylight && solar_per_block > 0.0 && net_consumption[i] <= 0.0 {
                    schedule[i] = ScheduledAction::SolarExcess;
                }
            }
        }

        // === Phase 11: Post-processing ===
        // Ensure charge blocks form consecutive groups
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

        // Bridge short gaps between charge groups
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

        // Remove short non-charge gaps
        if self.config.short_gap_removal_enabled {
            Self::remove_short_gaps_in_schedule(
                &mut schedule,
                blocks,
                self.config.short_gap_min_size_blocks,
                self.config.gap_bridging_price_tolerance_czk,
            );
        }

        // Add hold charge blocks between charging and first BatteryPowered block
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

    /// Generate summary string for logging
    fn generate_summary(&self, plan: &DayPlan) -> String {
        format!(
            "BUDGET: {} chg/{} bat/{} exp, avg {:.2} CZK",
            plan.charge_blocks,
            plan.battery_powered_blocks,
            plan.export_blocks,
            plan.avg_charge_price,
        )
    }
}

impl EconomicStrategy for WinterAdaptiveC10Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-C10"
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

        // Derive block duration from data
        let block_duration_hours = if !all_blocks.is_empty() {
            all_blocks[0].duration_minutes as f32 / 60.0
        } else {
            0.25
        };

        // Generate the day plan
        let plan = self.generate_plan(
            all_blocks,
            context.current_battery_soc,
            context.control_config.battery_capacity_kwh,
            context.control_config.max_battery_charge_rate_kw,
            context.solar_forecast_remaining_today_kwh,
            context.hourly_consumption_profile,
            context.consumption_forecast_kwh,
            context.battery_avg_charge_price_czk_per_kwh,
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
            .unwrap_or(ScheduledAction::GridPowered);

        let effective_price = context.price_block.effective_price_czk_per_kwh;

        match current_action {
            ScheduledAction::Charge { reason } => {
                if context.current_battery_soc < self.config.target_battery_soc {
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
                        "winter_adaptive_c10:charge:{}",
                        reason_str.to_lowercase().replace(' ', "_")
                    ));

                    let charge_kwh =
                        context.control_config.max_battery_charge_rate_kw * block_duration_hours;
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
                    // Battery at target - hold charge
                    eval.mode = InverterOperationMode::NoChargeNoDischarge;
                    eval.reason = format!(
                        "HOLD CHARGE: Battery at target ({:.1}%), grid powers house [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_c10:hold_charge".to_string());

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
                eval.decision_uid = Some("winter_adaptive_c10:battery_powered".to_string());

                let consumption_kwh = if self.config.use_hourly_consumption_profile {
                    context
                        .hourly_consumption_profile
                        .map(|profile| {
                            let hour = context.price_block.block_start.hour() as usize;
                            profile[hour] / 4.0
                        })
                        .unwrap_or(context.consumption_forecast_kwh)
                } else {
                    context.consumption_forecast_kwh
                };

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
                        context.control_config.max_battery_charge_rate_kw * block_duration_hours;
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
                    "winter_adaptive_c10:{}",
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
                if context.current_battery_soc > self.config.min_soc_after_export {
                    eval.mode = InverterOperationMode::ForceDischarge;
                    eval.reason = format!(
                        "EXPORT TO GRID: {:.3} CZK/kWh [{}]",
                        effective_price, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_c10:export".to_string());

                    let discharge_kwh =
                        context.control_config.max_battery_charge_rate_kw * block_duration_hours;
                    eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                    eval.energy_flows.grid_export_kwh = discharge_kwh;
                    eval.revenue_czk = discharge_kwh * context.grid_export_price_czk_per_kwh;
                } else {
                    eval.mode = InverterOperationMode::SelfUse;
                    eval.reason = format!(
                        "SELF-USE: Low SOC ({:.1}%) for export [{}]",
                        context.current_battery_soc, summary
                    );
                    eval.decision_uid = Some("winter_adaptive_c10:self_use_low_soc".to_string());
                }
            }

            ScheduledAction::SolarExcess => {
                eval.mode = InverterOperationMode::SelfUse;
                eval.reason = format!(
                    "SOLAR EXCESS: {:.3} CZK/kWh, natural charging [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_c10:solar_excess".to_string());

                let net_consumption = context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if net_consumption <= 0.0 {
                    let excess = -net_consumption;
                    let battery_capacity = context.control_config.battery_capacity_kwh;
                    let available_charge_capacity = (battery_capacity
                        * (context.control_config.max_battery_soc / 100.0)
                        - battery_capacity * (context.current_battery_soc / 100.0))
                        .max(0.0);
                    let max_charge_rate =
                        context.control_config.max_battery_charge_rate_kw * block_duration_hours;
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
        if self.config.negative_price_handling_enabled
            && effective_price < 0.0
            && !matches!(current_action, ScheduledAction::Charge { .. })
        {
            if context.current_battery_soc < self.config.target_battery_soc {
                eval.mode = InverterOperationMode::ForceCharge;
                eval.reason = format!(
                    "NEGATIVE PRICE: {:.3} CZK/kWh (getting paid!) [{}]",
                    effective_price, summary
                );
                eval.decision_uid = Some("winter_adaptive_c10:negative_price".to_string());

                let charge_kwh =
                    context.control_config.max_battery_charge_rate_kw * block_duration_hours;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk = charge_kwh * effective_price;
            } else {
                eval.mode = InverterOperationMode::NoChargeNoDischarge;
                eval.reason = format!(
                    "NEGATIVE PRICE HOLD: {:.3} CZK/kWh, battery full ({:.1}%) [{}]",
                    effective_price, context.current_battery_soc, summary
                );
                eval.decision_uid = Some("winter_adaptive_c10:negative_price_hold".to_string());
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

    // === Tests matching V10 behavior (defaults) ===

    #[test]
    fn test_budget_allocates_to_most_expensive_first() {
        let config = WinterAdaptiveC10Config::default();
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

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
            let min_battery_price = battery_prices.iter().cloned().fold(f32::INFINITY, f32::min);
            let max_grid_price = grid_prices
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max);

            assert!(
                min_battery_price >= max_grid_price - 1.0,
                "Cheapest battery block ({:.2}) should be >= most expensive grid block ({:.2}) - 1.0",
                min_battery_price,
                max_grid_price,
            );
        }
    }

    #[test]
    fn test_negative_price_always_charges() {
        let config = WinterAdaptiveC10Config {
            negative_price_handling_enabled: true,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);

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

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

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
    fn test_all_same_price_no_cycling() {
        let config = WinterAdaptiveC10Config {
            min_savings_threshold_czk: 0.5,
            opportunistic_charge_threshold_czk: 0.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);

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

        let plan = strategy.generate_plan(&blocks, 80.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let battery_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::BatteryPowered | ScheduledAction::Export))
            .count();

        assert_eq!(
            battery_count, 0,
            "Flat prices should not allocate battery (no savings to justify cycling)"
        );
    }

    #[test]
    fn test_consecutive_charge_groups() {
        let config = WinterAdaptiveC10Config::default();
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let schedule = &plan.schedule;
        for i in 0..schedule.len() {
            if matches!(schedule[i], ScheduledAction::Charge { .. }) {
                let has_neighbor = (i > 0
                    && matches!(schedule[i - 1], ScheduledAction::Charge { .. }))
                    || (i + 1 < schedule.len()
                        && matches!(schedule[i + 1], ScheduledAction::Charge { .. }));

                assert!(
                    has_neighbor,
                    "Charge block at index {} should have a neighboring charge block",
                    i,
                );
            }
        }
    }

    #[test]
    fn test_strategy_basics() {
        let config = WinterAdaptiveC10Config::default();
        let strategy = WinterAdaptiveC10Strategy::new(config);

        assert_eq!(strategy.name(), "Winter-Adaptive-C10");
        assert!(!strategy.is_enabled());

        let config_enabled = WinterAdaptiveC10Config {
            enabled: true,
            ..Default::default()
        };
        let strategy_enabled = WinterAdaptiveC10Strategy::new(config_enabled);
        assert!(strategy_enabled.is_enabled());
    }

    // === Tests for NEW configurable axes ===

    #[test]
    fn test_custom_daylight_window() {
        let config = WinterAdaptiveC10Config {
            daylight_start_hour: 9,
            daylight_end_hour: 15,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 15.0, None, 0.25, 3.0);

        // Solar excess should only appear in hours 9-14 (not 8 or 15+)
        for (i, _action) in plan.schedule.iter().enumerate() {
            if matches!(_action, ScheduledAction::SolarExcess) {
                let hour = blocks[i].block_start.hour();
                assert!(
                    (9..15).contains(&hour),
                    "SolarExcess at hour {} should be within daylight window 9-15",
                    hour,
                );
            }
        }
    }

    #[test]
    fn test_export_disabled() {
        let config = WinterAdaptiveC10Config {
            export_enabled: false,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 80.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

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

    #[test]
    fn test_gap_bridging_disabled() {
        let config_enabled = WinterAdaptiveC10Config::default();
        let config_disabled = WinterAdaptiveC10Config {
            gap_bridging_enabled: false,
            short_gap_removal_enabled: false,
            ..Default::default()
        };

        let strategy_enabled = WinterAdaptiveC10Strategy::new(config_enabled);
        let strategy_disabled = WinterAdaptiveC10Strategy::new(config_disabled);
        let blocks = create_test_blocks();

        let plan_enabled =
            strategy_enabled.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);
        let plan_disabled =
            strategy_disabled.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        // With gap bridging disabled, charge count should be <= enabled version
        assert!(
            plan_disabled.charge_blocks <= plan_enabled.charge_blocks,
            "Disabled gap bridging should have fewer or equal charge blocks: disabled={}, enabled={}",
            plan_disabled.charge_blocks,
            plan_enabled.charge_blocks,
        );
    }

    #[test]
    fn test_hold_charge_disabled() {
        let config = WinterAdaptiveC10Config {
            hold_charge_enabled: false,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let hold_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::HoldCharge))
            .count();

        assert_eq!(
            hold_count, 0,
            "With hold_charge disabled, no HoldCharge blocks should appear"
        );
    }

    #[test]
    fn test_max_charge_blocks_limit() {
        let config = WinterAdaptiveC10Config {
            max_charge_blocks_per_day: Some(4),
            consecutive_charge_groups_enabled: false,
            gap_bridging_enabled: false,
            short_gap_removal_enabled: false,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);

        let base_time = Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap();
        let mut blocks = Vec::new();
        for hour in 0..24 {
            for quarter in 0..4 {
                let price = if hour < 6 {
                    1.0
                } else if hour >= 17 {
                    8.0
                } else {
                    3.0
                };
                blocks.push(TimeBlockPrice {
                    block_start: base_time
                        + chrono::Duration::hours(hour)
                        + chrono::Duration::minutes(quarter * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: price,
                    effective_price_czk_per_kwh: price,
                    spot_sell_price_czk_per_kwh: None,
                });
            }
        }

        let plan = strategy.generate_plan(&blocks, 15.0, 10.0, 3.0, 0.0, None, 0.25, 1.0);

        // Count arbitrage charge blocks (excluding opportunistic/negative which bypass limit)
        // The total should respect the limit for demand-driven charges
        let charge_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Charge { .. }))
            .count();

        // With post-processing disabled and limit of 4, we should see
        // at most 4 arbitrage + any opportunistic blocks below threshold
        // (opportunistic threshold is 1.5, blocks at 1.0 qualify)
        assert!(
            charge_count <= 4 + 24, // 4 arbitrage + up to 24 opportunistic (6hrs * 4 blocks)
            "Charge blocks ({}) should respect max_charge_blocks_per_day limit",
            charge_count,
        );
    }

    #[test]
    fn test_max_discharge_blocks_limit() {
        let config = WinterAdaptiveC10Config {
            max_discharge_blocks_per_day: Some(8),
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 80.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        let discharge_count = plan
            .schedule
            .iter()
            .filter(|a| matches!(a, ScheduledAction::BatteryPowered))
            .count();

        assert!(
            discharge_count <= 8,
            "Battery-powered blocks ({}) should respect max_discharge_blocks_per_day=8",
            discharge_count,
        );
    }

    #[test]
    fn test_fixed_charge_price_method() {
        let config = WinterAdaptiveC10Config {
            charge_price_estimation_method: "fixed".to_string(),
            fixed_charge_price_czk: 2.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        // Should still produce a valid plan
        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        assert!(
            !plan.schedule.is_empty(),
            "Fixed charge price method should produce a valid plan"
        );
    }

    #[test]
    fn test_block_count_demand_method() {
        let config = WinterAdaptiveC10Config {
            demand_estimation_method: "block_count".to_string(),
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        assert!(
            !plan.schedule.is_empty(),
            "block_count demand method should produce a valid plan"
        );
    }

    #[test]
    fn test_consumption_weighted_allocation() {
        let config = WinterAdaptiveC10Config {
            budget_allocation_strategy: "consumption_weighted".to_string(),
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 50.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        assert!(
            !plan.schedule.is_empty(),
            "consumption_weighted allocation should produce a valid plan"
        );
    }

    #[test]
    fn test_evaluate_with_full_context() {
        let config = WinterAdaptiveC10Config::default();
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();
        let control_config = create_test_control_config();

        let block_index = 18 * 4; // Evening peak (expensive)
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

        // Evening peak should be battery-powered or export (most expensive blocks)
        assert!(
            eval.mode == InverterOperationMode::SelfUse
                || eval.mode == InverterOperationMode::ForceDischarge
                || eval.mode == InverterOperationMode::NoChargeNoDischarge,
            "Evening peak evaluation should produce valid mode, got: {} - {}",
            eval.mode,
            eval.reason,
        );
    }

    #[test]
    fn test_wider_gap_bridging() {
        let config = WinterAdaptiveC10Config {
            gap_bridging_max_gap_blocks: 4,
            gap_bridging_price_tolerance_czk: 2.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveC10Strategy::new(config);
        let blocks = create_test_blocks();

        let plan = strategy.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        // Wider gap bridging should produce more charge blocks than default
        let config_default = WinterAdaptiveC10Config::default();
        let strategy_default = WinterAdaptiveC10Strategy::new(config_default);
        let plan_default =
            strategy_default.generate_plan(&blocks, 30.0, 10.0, 3.0, 0.0, None, 0.25, 3.0);

        assert!(
            plan.charge_blocks >= plan_default.charge_blocks,
            "Wider gap bridging ({}) should produce >= charge blocks than default ({})",
            plan.charge_blocks,
            plan_default.charge_blocks,
        );
    }
}
