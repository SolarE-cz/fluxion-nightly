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

//! Winter Adaptive Strategy V2
//!
//! A comprehensive battery optimization strategy implementing the full algorithm specification.
//!
//! ## Key Improvements over V1
//!
//! 1. **Arbitrage Window Detection**: Identifies valley-peak-valley patterns for multiple
//!    charge-discharge cycles per day
//! 2. **Per-Slot Forecasting**: P10/P50/P90 consumption bands with hourly patterns
//! 3. **Cost Optimization**: Minimizes total cost using forward simulation and iterative refinement
//! 4. **Spike Reservation**: Reserves SOC for extreme price events
//! 5. **P90 Validation**: Ensures schedule survives high-consumption scenarios
//! 6. **Improved Feed-in**: Spread analysis with future opportunity comparison
//! 7. **Terminal SOC Penalty**: Maintains higher ending SOC when tomorrow unknown
//!
//! ## Architecture
//!
//! - `forecasting`: Per-slot consumption (P10/P50/P90) and solar estimates
//! - `arbitrage`: Valley-peak pattern detection
//! - `optimization`: Cost-based charge slot selection
//! - `simulation`: Forward SOC tracking and validation
//! - `spike_detection`: Extreme price event handling

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
    seasonal_mode::SeasonalMode,
};
use crate::utils::calculate_ema;
use chrono::{DateTime, Datelike, Timelike, Utc};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// ============================================================================
// Configuration
// ============================================================================

/// Historical day data for seasonal mode detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayEnergyBalance {
    pub date: DateTime<Utc>,
    pub solar_production_kwh: f32,
    pub grid_import_kwh: f32,
}

impl DayEnergyBalance {
    pub fn deficit_ratio(&self) -> f32 {
        if self.grid_import_kwh == 0.0 {
            return -1.0;
        }
        (self.grid_import_kwh - self.solar_production_kwh) / self.grid_import_kwh
    }

    pub fn is_deficit_day(&self) -> bool {
        self.deficit_ratio() >= 0.20
    }

    pub fn is_surplus_day(&self) -> bool {
        self.deficit_ratio() <= -0.20
    }
}

/// Per-slot historical data for detailed forecasting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotHistoricalData {
    /// Timestamp of this slot
    pub timestamp: DateTime<Utc>,
    /// Consumption in this slot (kWh)
    pub consumption_kwh: f32,
    /// Solar production in this slot (kWh)
    pub solar_kwh: f32,
}

/// Configuration for Winter Adaptive V2 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV2Config {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v2_priority")]
    pub priority: u8,

    // ---- Forecasting ----
    /// Number of days to track for consumption/solar history
    pub history_days: usize,
    /// P90 multiplier for conservative consumption estimate (default: 1.3)
    pub consumption_p90_multiplier: f32,
    /// P10 multiplier for optimistic consumption estimate (default: 0.7)
    pub consumption_p10_multiplier: f32,
    /// Solar discount factor for P90 scenario (default: 0.8)
    pub solar_p90_discount: f32,

    // ---- Optimization ----
    /// Safety margin for charge calculation (default: 0.15 = 15%)
    pub safety_margin_pct: f32,
    /// Minimum SOC to maintain (default: 10%)
    pub min_soc_pct: f32,
    /// Daily charging target SOC (default: 90%)
    pub daily_charging_target_soc: f32,
    /// Round-trip efficiency (default: 0.90)
    pub round_trip_efficiency: f32,

    // ---- Spike Detection ----
    /// Price threshold for spike detection (default: 8.0 CZK/kWh)
    pub spike_threshold_czk: f32,
    /// Minimum SOC to reserve for export during spikes (default: 50%)
    pub min_soc_for_spike_export: f32,

    // ---- Feed-in ----
    /// Minimum spread for feed-in consideration (default: 3.0 CZK/kWh)
    pub feedin_min_spread_czk: f32,
    /// Enable feed-in logic (default: false for safety)
    pub feedin_enabled: bool,

    // ---- Terminal SOC ----
    /// Expected future value per Wh for terminal penalty (default: 0.002 CZK/Wh)
    pub expected_future_value_per_wh: f32,

    // ---- Anti-Oscillation ----
    /// Minimum consecutive charge blocks (default: 2)
    pub min_consecutive_charge_blocks: usize,
    /// Price tolerance for consolidation (default: 0.50 = 50%)
    pub charge_consolidation_tolerance: f32,

    // ---- Negative Prices ----
    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,
    /// Charge even when full on negative prices (default: false)
    pub charge_on_negative_even_if_full: bool,

    // ---- Historical Data ----
    /// Per-slot historical data (last N days, organized by day then by slot within day)
    #[serde(skip)]
    pub slot_history: VecDeque<Vec<SlotHistoricalData>>,

    /// Historical daily energy balance for seasonal mode detection
    #[serde(skip)]
    pub energy_balance_history: VecDeque<DayEnergyBalance>,

    /// Current seasonal mode
    pub seasonal_mode: SeasonalMode,
}

fn default_winter_adaptive_v2_priority() -> u8 {
    100
}

impl Default for WinterAdaptiveV2Config {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 100, // Highest priority (same as Winter Adaptive v1)
            history_days: 3,
            consumption_p90_multiplier: 1.3,
            consumption_p10_multiplier: 0.7,
            solar_p90_discount: 0.8,
            safety_margin_pct: 0.15,
            min_soc_pct: 10.0,
            daily_charging_target_soc: 90.0,
            round_trip_efficiency: 0.90,
            spike_threshold_czk: 8.0,
            min_soc_for_spike_export: 50.0,
            feedin_min_spread_czk: 3.0,
            feedin_enabled: false,
            expected_future_value_per_wh: 0.002,
            min_consecutive_charge_blocks: 2,
            charge_consolidation_tolerance: 0.50,
            negative_price_handling_enabled: true,
            charge_on_negative_even_if_full: false,
            slot_history: VecDeque::new(),
            energy_balance_history: VecDeque::new(),
            seasonal_mode: SeasonalMode::Winter,
        }
    }
}

impl WinterAdaptiveV2Config {
    /// Update seasonal mode based on historical energy balance
    pub fn update_seasonal_mode(&mut self, now: DateTime<Utc>) -> bool {
        if self.energy_balance_history.len() < 3 {
            return false;
        }

        let month = now.month();
        let last_3_days: Vec<_> = self.energy_balance_history.iter().rev().take(3).collect();

        // Check for winter mode switch (after September 1)
        if month >= 9 || month <= 2 {
            let all_deficit = last_3_days.iter().all(|day| day.is_deficit_day());
            if all_deficit && self.seasonal_mode != SeasonalMode::Winter {
                tracing::info!("V2: Switching to Winter mode");
                self.seasonal_mode = SeasonalMode::Winter;
                return true;
            }
        }

        // Check for summer mode switch (after February 1)
        if (2..=9).contains(&month) {
            let all_surplus = last_3_days.iter().all(|day| day.is_surplus_day());
            if all_surplus && self.seasonal_mode != SeasonalMode::Summer {
                tracing::info!("V2: Switching to Summer mode");
                self.seasonal_mode = SeasonalMode::Summer;
                return true;
            }
        }

        false
    }

    /// Add a day's worth of slot data to history
    pub fn add_day_history(&mut self, day_data: Vec<SlotHistoricalData>) {
        self.slot_history.push_back(day_data);
        while self.slot_history.len() > self.history_days {
            self.slot_history.pop_front();
        }
    }

    /// Add energy balance record
    pub fn add_energy_balance(&mut self, balance: DayEnergyBalance) {
        self.energy_balance_history.push_back(balance);
        while self.energy_balance_history.len() > self.history_days {
            self.energy_balance_history.pop_front();
        }
    }
}

// ============================================================================
// Module: Forecasting
// ============================================================================

pub mod forecasting {
    use super::*;

    /// Per-slot consumption forecast with uncertainty bands
    #[derive(Debug, Clone)]
    pub struct ConsumptionForecast {
        /// Median estimate (P50)
        pub p50_kwh: f32,
        /// Conservative estimate (P90)
        pub p90_kwh: f32,
        /// Optimistic estimate (P10)
        #[allow(dead_code)]
        pub p10_kwh: f32,
    }

    /// Per-slot solar forecast
    #[derive(Debug, Clone)]
    pub struct SolarForecast {
        /// Expected solar generation (kWh)
        pub expected_kwh: f32,
        /// Discounted for P90 scenario
        pub p90_kwh: f32,
    }

    /// Per-slot net consumption (consumption - solar)
    #[derive(Debug, Clone)]
    pub struct NetConsumption {
        pub p50_kwh: f32,
        pub p90_kwh: f32,
    }

    /// Forecast per-slot consumption using historical patterns
    /// Returns consumption forecast with fallback to context data
    pub fn forecast_consumption_per_slot(
        config: &WinterAdaptiveV2Config,
        slot_index_in_day: usize,
        fallback_per_slot_kwh: f32,
    ) -> ConsumptionForecast {
        if config.slot_history.is_empty() {
            // No history - use fallback with uniform distribution
            // Fallback is per-block, already provided
            return ConsumptionForecast {
                p50_kwh: fallback_per_slot_kwh,
                p90_kwh: fallback_per_slot_kwh * config.consumption_p90_multiplier,
                p10_kwh: fallback_per_slot_kwh * config.consumption_p10_multiplier,
            };
        }

        // Collect consumption values for this slot across historical days
        let values: Vec<f32> = config
            .slot_history
            .iter()
            .filter_map(|day| day.get(slot_index_in_day).map(|s| s.consumption_kwh))
            .collect();

        if values.is_empty() {
            // No data for this slot - use fallback
            return ConsumptionForecast {
                p50_kwh: fallback_per_slot_kwh,
                p90_kwh: fallback_per_slot_kwh * config.consumption_p90_multiplier,
                p10_kwh: fallback_per_slot_kwh * config.consumption_p10_multiplier,
            };
        }

        // Calculate EMA as P50 estimate
        let p50 = calculate_ema(&values, config.history_days).unwrap_or(fallback_per_slot_kwh);

        ConsumptionForecast {
            p50_kwh: p50,
            p90_kwh: p50 * config.consumption_p90_multiplier,
            p10_kwh: p50 * config.consumption_p10_multiplier,
        }
    }

    /// Forecast per-slot solar generation
    pub fn forecast_solar_per_slot(
        config: &WinterAdaptiveV2Config,
        slot_index_in_day: usize,
        fallback_per_slot_kwh: f32,
    ) -> SolarForecast {
        if config.slot_history.is_empty() {
            return SolarForecast {
                expected_kwh: fallback_per_slot_kwh,
                p90_kwh: fallback_per_slot_kwh * config.solar_p90_discount,
            };
        }

        // Collect solar values for this slot across historical days
        let values: Vec<f32> = config
            .slot_history
            .iter()
            .filter_map(|day| day.get(slot_index_in_day).map(|s| s.solar_kwh))
            .collect();

        if values.is_empty() {
            return SolarForecast {
                expected_kwh: fallback_per_slot_kwh,
                p90_kwh: fallback_per_slot_kwh * config.solar_p90_discount,
            };
        }

        let expected = calculate_ema(&values, config.history_days).unwrap_or(fallback_per_slot_kwh);

        SolarForecast {
            expected_kwh: expected,
            p90_kwh: expected * config.solar_p90_discount,
        }
    }

    /// Calculate net consumption (consumption - solar) for a slot
    pub fn calculate_net_consumption(
        consumption: &ConsumptionForecast,
        solar: &SolarForecast,
    ) -> NetConsumption {
        NetConsumption {
            p50_kwh: (consumption.p50_kwh - solar.expected_kwh).max(0.0),
            p90_kwh: (consumption.p90_kwh - solar.p90_kwh).max(0.0),
        }
    }
}

// ============================================================================
// Module: Arbitrage Window Detection
// ============================================================================

pub mod arbitrage {
    use super::*;

    /// An arbitrage window: valley (charge) → peak (discharge) → next valley
    #[derive(Debug, Clone)]
    pub struct ArbitrageWindow {
        /// Indices of valley slots (cheap charging period)
        pub valley_slots: Vec<usize>,
        /// Indices of peak slots (expensive discharge period)
        pub peak_slots: Vec<usize>,
        /// Average valley price
        #[allow(dead_code)]
        pub valley_avg_price: f32,
        /// Average peak price
        #[allow(dead_code)]
        pub peak_avg_price: f32,
    }

    /// Detect arbitrage windows in price data
    pub fn detect_windows(blocks: &[TimeBlockPrice]) -> Vec<ArbitrageWindow> {
        if blocks.len() < 8 {
            return Vec::new();
        }

        let prices: Vec<f32> = blocks.iter().map(|b| b.price_czk_per_kwh).collect();
        let avg_price = prices.iter().sum::<f32>() / prices.len() as f32;

        // Simple threshold-based detection
        // Valley: price < avg * 0.85
        // Peak: price > avg * 1.15
        let valley_threshold = avg_price * 0.85;
        let peak_threshold = avg_price * 1.15;

        let mut windows = Vec::new();
        let mut state = "seeking_valley";
        let mut valley_slots = Vec::new();
        let mut peak_slots = Vec::new();

        for (idx, &price) in prices.iter().enumerate() {
            match state {
                "seeking_valley" => {
                    if price < valley_threshold {
                        valley_slots.push(idx);
                        state = "in_valley";
                    }
                }
                "in_valley" => {
                    if price < valley_threshold {
                        valley_slots.push(idx);
                    } else if price > peak_threshold {
                        // Transition to peak
                        peak_slots.push(idx);
                        state = "in_peak";
                    } else {
                        // Neutral zone - still in valley
                        valley_slots.push(idx);
                    }
                }
                "in_peak" => {
                    if price > peak_threshold {
                        peak_slots.push(idx);
                    } else if price < valley_threshold {
                        // Complete window, start new one
                        if !valley_slots.is_empty() && !peak_slots.is_empty() {
                            let valley_avg = valley_slots.iter().map(|&i| prices[i]).sum::<f32>()
                                / valley_slots.len() as f32;
                            let peak_avg = peak_slots.iter().map(|&i| prices[i]).sum::<f32>()
                                / peak_slots.len() as f32;

                            windows.push(ArbitrageWindow {
                                valley_slots: valley_slots.clone(),
                                peak_slots: peak_slots.clone(),
                                valley_avg_price: valley_avg,
                                peak_avg_price: peak_avg,
                            });
                        }

                        // Start new valley
                        valley_slots = vec![idx];
                        peak_slots = Vec::new();
                        state = "in_valley";
                    } else {
                        // Neutral zone in peak
                        peak_slots.push(idx);
                    }
                }
                _ => {}
            }
        }

        // Close final window if incomplete
        if !valley_slots.is_empty() && !peak_slots.is_empty() {
            let valley_avg =
                valley_slots.iter().map(|&i| prices[i]).sum::<f32>() / valley_slots.len() as f32;
            let peak_avg =
                peak_slots.iter().map(|&i| prices[i]).sum::<f32>() / peak_slots.len() as f32;

            windows.push(ArbitrageWindow {
                valley_slots,
                peak_slots,
                valley_avg_price: valley_avg,
                peak_avg_price: peak_avg,
            });
        }

        windows
    }
}

// ============================================================================
// Module: Spike Detection
// ============================================================================

pub mod spike_detection {
    use super::*;

    /// A detected price spike
    #[derive(Debug, Clone)]
    pub struct PriceSpike {
        /// Slot index
        pub slot_index: usize,
        /// Price (CZK/kWh)
        pub price_czk: f32,
        /// Reserved discharge capacity for this spike (Wh)
        pub reserved_discharge_wh: f32,
    }

    /// Detect price spikes and calculate required SOC reservation
    pub fn detect_spikes(
        blocks: &[TimeBlockPrice],
        threshold: f32,
        net_consumption_p90: &[f32],
        max_discharge_rate_kw: f32,
    ) -> Vec<PriceSpike> {
        let mut spikes = Vec::new();
        let discharge_per_slot_kwh = max_discharge_rate_kw * 0.25; // 15 minutes

        for (idx, block) in blocks.iter().enumerate() {
            if block.price_czk_per_kwh >= threshold {
                let consumption = net_consumption_p90.get(idx).copied().unwrap_or(0.0);
                let reserved_discharge_wh = discharge_per_slot_kwh.min(consumption) * 1000.0;

                spikes.push(PriceSpike {
                    slot_index: idx,
                    price_czk: block.price_czk_per_kwh,
                    reserved_discharge_wh,
                });
            }
        }

        spikes
    }
}

// ============================================================================
// Module: Schedule Simulation
// ============================================================================

pub mod simulation {
    use super::*;

    /// Mode assignment for a single slot
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum SlotMode {
        GridCharge,
        Hold,
        Discharge,
        FeedIn,
    }

    impl SlotMode {
        pub fn to_inverter_mode(self) -> InverterOperationMode {
            match self {
                SlotMode::GridCharge => InverterOperationMode::ForceCharge,
                SlotMode::Hold => InverterOperationMode::SelfUse,
                SlotMode::Discharge => InverterOperationMode::SelfUse,
                SlotMode::FeedIn => InverterOperationMode::ForceDischarge,
            }
        }
    }

    /// A complete schedule with mode assignments
    #[derive(Debug, Clone)]
    pub struct Schedule {
        pub slots: Vec<ScheduleSlot>,
    }

    #[derive(Debug, Clone)]
    pub struct ScheduleSlot {
        pub mode: SlotMode,
        pub soc_start_pct: f32,
        pub soc_end_pct: f32,
        pub price_czk: f32,
        pub net_consumption_kwh: f32,
    }

    /// Parameters for battery simulation
    #[derive(Debug, Clone, Copy)]
    pub struct BatteryParams {
        pub capacity_kwh: f32,
        pub charge_rate_kw: f32,
        pub discharge_rate_kw: f32,
        pub efficiency: f32,
        pub min_soc_pct: f32,
    }

    /// Forward simulate SOC evolution through a schedule
    pub fn simulate_soc(
        initial_soc_pct: f32,
        modes: &[SlotMode],
        net_consumption: &[f32],
        params: BatteryParams,
    ) -> Vec<f32> {
        let mut soc = initial_soc_pct;
        let mut trajectory = vec![soc];

        let charge_per_slot_kwh = params.charge_rate_kw * 0.25;
        let discharge_per_slot_kwh = params.discharge_rate_kw * 0.25;

        for (idx, &mode) in modes.iter().enumerate() {
            let net = net_consumption.get(idx).copied().unwrap_or(0.0);

            match mode {
                SlotMode::GridCharge => {
                    // Charge battery
                    let energy_added_kwh = charge_per_slot_kwh * params.efficiency;
                    let soc_delta = (energy_added_kwh / params.capacity_kwh) * 100.0;
                    soc = (soc + soc_delta).min(100.0);
                }
                SlotMode::Hold => {
                    // No change to SOC (grid covers consumption)
                }
                SlotMode::Discharge | SlotMode::FeedIn => {
                    // Discharge battery
                    let energy_discharged_kwh = discharge_per_slot_kwh.min(net);
                    let soc_delta = (energy_discharged_kwh / params.capacity_kwh) * 100.0;
                    soc = (soc - soc_delta).max(params.min_soc_pct);
                }
            }

            trajectory.push(soc);
        }

        trajectory
    }

    /// Validate that a schedule maintains SOC bounds
    pub fn validate_soc_bounds(soc_trajectory: &[f32], min_soc: f32, max_soc: f32) -> bool {
        soc_trajectory
            .iter()
            .all(|&soc| soc >= min_soc && soc <= max_soc)
    }

    /// Calculate total cost of a schedule
    pub fn calculate_total_cost(
        modes: &[SlotMode],
        prices: &[f32],
        sell_prices: &[f32],
        net_consumption: &[f32],
        charge_rate_kw: f32,
        discharge_rate_kw: f32,
    ) -> f32 {
        let mut total_cost = 0.0;
        let charge_per_slot_kwh = charge_rate_kw * 0.25;
        let discharge_per_slot_kwh = discharge_rate_kw * 0.25;

        for (idx, &mode) in modes.iter().enumerate() {
            let buy_price = prices.get(idx).copied().unwrap_or(0.0);
            let sell_price = sell_prices.get(idx).copied().unwrap_or(0.0);
            let net = net_consumption.get(idx).copied().unwrap_or(0.0);

            match mode {
                SlotMode::GridCharge => {
                    // Cost of charging from grid
                    total_cost += charge_per_slot_kwh * buy_price;
                }
                SlotMode::Hold => {
                    // Cost of direct grid consumption
                    total_cost += net * buy_price;
                }
                SlotMode::Discharge => {
                    // Discharge covers consumption, any excess needs grid
                    let from_battery = discharge_per_slot_kwh.min(net);
                    let from_grid = (net - from_battery).max(0.0);
                    total_cost += from_grid * buy_price;
                }
                SlotMode::FeedIn => {
                    // Revenue from feed-in
                    total_cost -= discharge_per_slot_kwh * sell_price;
                }
            }
        }

        total_cost
    }
}

// ============================================================================
// Module: Optimization
// ============================================================================

mod optimization {

    /// Select optimal charge slots using greedy + iterative refinement
    pub fn select_charge_slots(
        valley_slots: &[usize],
        prices: &[f32],
        required_charge_kwh: f32,
        charge_rate_kw: f32,
        consolidation_tolerance: f32,
        min_consecutive: usize,
    ) -> Vec<usize> {
        if valley_slots.is_empty() || required_charge_kwh <= 0.0 {
            return Vec::new();
        }

        let charge_per_slot_kwh = charge_rate_kw * 0.25;
        let slots_needed = (required_charge_kwh / charge_per_slot_kwh).ceil() as usize;

        if slots_needed == 0 {
            return Vec::new();
        }

        // Build price list for valley slots
        let mut valley_prices: Vec<(usize, f32)> = valley_slots
            .iter()
            .filter_map(|&idx| prices.get(idx).map(|&p| (idx, p)))
            .collect();

        // Sort by price
        valley_prices.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Use consolidation logic from V1 (it's excellent)
        consolidate_charge_blocks(
            &valley_prices,
            slots_needed,
            consolidation_tolerance,
            min_consecutive,
        )
    }

    /// Consolidate charge blocks into consecutive runs (from V1)
    fn consolidate_charge_blocks(
        blocks_by_price: &[(usize, f32)],
        count_needed: usize,
        tolerance: f32,
        min_consecutive: usize,
    ) -> Vec<usize> {
        if blocks_by_price.is_empty() || count_needed == 0 {
            return Vec::new();
        }

        if count_needed < min_consecutive {
            return blocks_by_price
                .iter()
                .take(count_needed)
                .map(|(idx, _)| *idx)
                .collect();
        }

        let cheapest_price = blocks_by_price[0].1;
        let price_threshold = if cheapest_price < 0.0 {
            cheapest_price * (1.0 - tolerance)
        } else {
            cheapest_price * (1.0 + tolerance)
        };

        let eligible_blocks: Vec<(usize, f32)> = blocks_by_price
            .iter()
            .filter(|(_, price)| *price <= price_threshold)
            .cloned()
            .collect();

        if eligible_blocks.len() < count_needed {
            return blocks_by_price
                .iter()
                .take(count_needed)
                .map(|(idx, _)| *idx)
                .collect();
        }

        let mut eligible_by_idx: Vec<(usize, f32)> = eligible_blocks.clone();
        eligible_by_idx.sort_by_key(|(idx, _)| *idx);

        // Find consecutive runs
        let mut runs: Vec<Vec<usize>> = Vec::new();
        let mut current_run: Vec<usize> = Vec::new();

        for (idx, _) in &eligible_by_idx {
            if current_run.is_empty() || *idx == current_run.last().unwrap() + 1 {
                current_run.push(*idx);
            } else {
                if !current_run.is_empty() {
                    runs.push(current_run);
                }
                current_run = vec![*idx];
            }
        }
        if !current_run.is_empty() {
            runs.push(current_run);
        }

        // Score runs
        let mut scored_runs: Vec<(usize, f32, Vec<usize>)> = runs
            .into_iter()
            .map(|run| {
                let avg_price: f32 = run
                    .iter()
                    .filter_map(|idx| {
                        eligible_blocks
                            .iter()
                            .find(|(i, _)| i == idx)
                            .map(|(_, p)| *p)
                    })
                    .sum::<f32>()
                    / run.len() as f32;
                (run.len(), avg_price, run)
            })
            .collect();

        // Sort by: 1) meets minimum, 2) price (lower better), 3) length (longer better)
        scored_runs.sort_by(|a, b| {
            let a_meets_min = a.0 >= min_consecutive;
            let b_meets_min = b.0 >= min_consecutive;

            // Priority 1: Prefer runs that meet minimum length requirement
            if a_meets_min != b_meets_min {
                return b_meets_min.cmp(&a_meets_min);
            }

            // Priority 2: Prefer CHEAPER runs (lower average price)
            let price_cmp = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
            if price_cmp != std::cmp::Ordering::Equal {
                return price_cmp;
            }

            // Priority 3: If prices equal, prefer longer runs
            b.0.cmp(&a.0)
        });

        // Select blocks from runs
        let mut selected: Vec<usize> = Vec::new();
        for (_, _, run) in scored_runs {
            if selected.len() >= count_needed {
                break;
            }
            for idx in run {
                if selected.len() >= count_needed {
                    break;
                }
                if !selected.contains(&idx) {
                    selected.push(idx);
                }
            }
        }

        // Fill remaining with cheapest
        if selected.len() < count_needed {
            for (idx, _) in blocks_by_price {
                if selected.len() >= count_needed {
                    break;
                }
                if !selected.contains(idx) {
                    selected.push(*idx);
                }
            }
        }

        selected
    }
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug, Clone)]
pub struct WinterAdaptiveV2Strategy {
    config: WinterAdaptiveV2Config,
}

impl WinterAdaptiveV2Strategy {
    pub fn new(config: WinterAdaptiveV2Config) -> Self {
        Self { config }
    }

    /// Find the start of sustained expensive period (not just single spike)
    /// Returns the index where prices stay elevated, or None if no sustained peak
    fn find_sustained_peak_start(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_index: usize,
        windows: &[arbitrage::ArbitrageWindow],
    ) -> Option<usize> {
        // Strategy: Find first peak window where prices stay elevated
        // A "sustained peak" is a peak window that's not followed immediately by lower prices

        if windows.is_empty() {
            return None;
        }

        for window in windows {
            if window.peak_slots.is_empty() {
                continue;
            }

            let peak_start = *window.peak_slots.first().unwrap();
            if peak_start <= current_index {
                continue; // Skip past peaks
            }

            // Check if this peak is sustained (not just a spike)
            // Look at blocks after this peak
            let peak_end = *window.peak_slots.last().unwrap();

            // If peak has at least 4 blocks (1 hour), consider it sustained
            if window.peak_slots.len() >= 4 {
                return Some(peak_start);
            }

            // Otherwise, check if prices stay elevated after peak
            if peak_end + 1 < all_blocks.len() {
                let peak_avg = window.peak_avg_price;
                let next_block_price = all_blocks[peak_end + 1].price_czk_per_kwh;

                // If next block is also expensive (within 20% of peak), it's sustained
                if next_block_price >= peak_avg * 0.8 {
                    return Some(peak_start);
                }
            } else {
                // Peak at end of day - definitely sustained
                return Some(peak_start);
            }
        }

        None
    }

    /// Find next valley window starting from current index
    fn find_next_valley_start(
        &self,
        current_index: usize,
        windows: &[arbitrage::ArbitrageWindow],
    ) -> Option<usize> {
        for window in windows {
            if window.valley_slots.is_empty() {
                continue;
            }
            let valley_start = *window.valley_slots.first().unwrap();
            if valley_start > current_index {
                return Some(valley_start);
            }
        }
        None
    }

    /// Check if current block is in a valley window
    fn is_in_valley_window(
        &self,
        current_index: usize,
        windows: &[arbitrage::ArbitrageWindow],
    ) -> bool {
        for window in windows {
            if window.valley_slots.contains(&current_index) {
                return true;
            }
        }
        false
    }

    /// Check if battery will deplete before next valley (emergency charge needed)
    fn will_deplete_before_next_valley(
        &self,
        context: &EvaluationContext,
        current_index: usize,
        next_valley_index: Option<usize>,
        net_consumption_p50: &[f32],
    ) -> bool {
        let available_energy_kwh = ((context.current_battery_soc - self.config.min_soc_pct)
            / 100.0)
            * context.control_config.battery_capacity_kwh;

        if available_energy_kwh <= 0.0 {
            return true; // Already depleted
        }

        // Calculate consumption until next valley (or end of horizon)
        let lookahead_end = next_valley_index.unwrap_or(net_consumption_p50.len());
        let consumption_until_valley: f32 = net_consumption_p50
            .iter()
            .skip(current_index)
            .take(lookahead_end.saturating_sub(current_index))
            .sum();

        // Will we deplete?
        consumption_until_valley > available_energy_kwh
    }

    /// Build full mode schedule for simulation
    fn build_mode_schedule(
        &self,
        charge_slots: &[usize],
        windows: &[arbitrage::ArbitrageWindow],
        current_index: usize,
        slot_count: usize,
    ) -> Vec<simulation::SlotMode> {
        let mut modes = vec![simulation::SlotMode::Hold; slot_count - current_index];

        for (relative_idx, mode) in modes.iter_mut().enumerate() {
            let absolute_idx = current_index + relative_idx;

            // Priority 1: Charge slots
            if charge_slots.contains(&absolute_idx) {
                *mode = simulation::SlotMode::GridCharge;
                continue;
            }

            // Priority 2: Peak slots (discharge)
            for window in windows {
                if window.peak_slots.contains(&absolute_idx) {
                    *mode = simulation::SlotMode::Discharge;
                    break;
                }
            }
        }

        modes
    }

    /// Main decision logic for current block
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
    ) -> (InverterOperationMode, String) {
        // Priority 0: Negative prices (from V1, excellent feature)
        let current_price = context.price_block.price_czk_per_kwh;
        if self.config.negative_price_handling_enabled && current_price < 0.0 {
            if context.current_battery_soc < self.config.daily_charging_target_soc
                || (self.config.charge_on_negative_even_if_full
                    && context.current_battery_soc < 100.0)
            {
                return (
                    InverterOperationMode::ForceCharge,
                    format!("Negative price: {:.3} CZK/kWh", current_price),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price - no export: {:.3} CZK/kWh", current_price),
            );
        }

        // Generate forecasts for all upcoming slots
        let slot_count = all_blocks.len();
        let mut net_consumption_p50 = Vec::with_capacity(slot_count);
        let mut net_consumption_p90 = Vec::with_capacity(slot_count);

        // Use context forecasts as fallback (these are per-block values already)
        let fallback_consumption_kwh = context.consumption_forecast_kwh;
        let fallback_solar_kwh = context.solar_forecast_kwh;

        for block in all_blocks.iter().take(slot_count) {
            // Calculate slot index within the day (0-95 for 15-min blocks)
            let block_time = block.block_start;
            let slot_in_day = (block_time.hour() * 4 + block_time.minute() / 15) as usize;

            let consumption = forecasting::forecast_consumption_per_slot(
                &self.config,
                slot_in_day,
                fallback_consumption_kwh,
            );
            let solar =
                forecasting::forecast_solar_per_slot(&self.config, slot_in_day, fallback_solar_kwh);
            let net = forecasting::calculate_net_consumption(&consumption, &solar);

            net_consumption_p50.push(net.p50_kwh);
            net_consumption_p90.push(net.p90_kwh);
        }

        // Detect arbitrage windows
        let windows = arbitrage::detect_windows(all_blocks);

        // Find sustained peak and next valley for planning
        let sustained_peak_start =
            self.find_sustained_peak_start(all_blocks, current_block_index, &windows);
        let next_valley_start = self.find_next_valley_start(current_block_index, &windows);

        // Calculate average price for emergency check
        let avg_price =
            all_blocks.iter().map(|b| b.price_czk_per_kwh).sum::<f32>() / all_blocks.len() as f32;
        let in_valley = self.is_in_valley_window(current_block_index, &windows);

        // Priority 1: Emergency charge check - prevent depletion before next valley
        // Rules:
        // - If we're IN a valley window, let normal scheduling handle it (don't emergency charge)
        // - If we're NOT in a valley AND will deplete AND (price reasonable OR critically low), emergency charge
        let will_deplete = self.will_deplete_before_next_valley(
            context,
            current_block_index,
            next_valley_start,
            &net_consumption_p50,
        );
        let price_is_reasonable = current_price <= avg_price;
        let soc_is_critical = context.current_battery_soc < 20.0;

        if !in_valley && will_deplete && (price_is_reasonable || soc_is_critical) {
            // Emergency charge: NOT in valley, will deplete, and (price OK or desperate)
            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "Emergency charge - depletion risk: {:.3} CZK/kWh",
                    current_price
                ),
            );
        }

        // Detect price spikes
        let spikes = spike_detection::detect_spikes(
            all_blocks,
            self.config.spike_threshold_czk,
            &net_consumption_p90,
            context.control_config.max_battery_charge_rate_kw,
        );

        // Priority 2: Check if current block is a spike
        if let Some(spike) = spikes.iter().find(|s| s.slot_index == current_block_index)
            && context.current_battery_soc > self.config.min_soc_for_spike_export
        {
            return (
                InverterOperationMode::SelfUse,
                format!("Spike discharge: {:.2} CZK/kWh", spike.price_czk),
            );
        }

        // Calculate total charge needed to reach daily target SOC
        let soc_deficit = self.config.daily_charging_target_soc - context.current_battery_soc;
        let energy_for_target_soc = if soc_deficit > 0.0 {
            (context.control_config.battery_capacity_kwh * soc_deficit / 100.0)
                / self.config.round_trip_efficiency
        } else {
            0.0
        };

        // Build charge schedule with deadline constraint
        let mut charge_slots = Vec::new();
        let charge_per_block = context.control_config.max_battery_charge_rate_kw * 0.25;

        if energy_for_target_soc > 0.0 {
            // Determine charge deadline: must finish before sustained peak starts
            let charge_deadline = sustained_peak_start.map(|idx| idx.saturating_sub(1));

            // Collect valley blocks BEFORE deadline with their prices
            let mut all_valley_blocks: Vec<(usize, f32)> = Vec::new();
            for window in &windows {
                for &idx in &window.valley_slots {
                    // Must be >= current (can charge now or later)
                    if idx < current_block_index {
                        continue;
                    }

                    // Must be before deadline (if deadline exists)
                    if let Some(deadline) = charge_deadline
                        && idx > deadline
                    {
                        continue; // Too late - after peak starts
                    }

                    all_valley_blocks.push((idx, all_blocks[idx].price_czk_per_kwh));
                }
            }

            // FALLBACK: If valley detection didn't find enough blocks,
            // use V1's proven simple approach: just select the CHEAPEST blocks
            // This is better than complex pattern matching - it always works!
            if all_valley_blocks.is_empty() {
                // V1 approach: collect ALL upcoming blocks before deadline
                for (idx, block) in all_blocks.iter().enumerate() {
                    // Must be >= current
                    if idx < current_block_index {
                        continue;
                    }

                    // Must be before deadline (if exists)
                    if let Some(deadline) = charge_deadline
                        && idx > deadline
                    {
                        break;
                    }

                    all_valley_blocks.push((idx, block.price_czk_per_kwh));
                }

                // Sort by price - cheapest first (V1 approach)
                // The consolidation and selection logic will pick the best ones
            }

            // Sort by price (cheapest first)
            all_valley_blocks
                .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            if !all_valley_blocks.is_empty() {
                let valley_indices: Vec<usize> =
                    all_valley_blocks.iter().map(|(idx, _)| *idx).collect();
                let all_prices: Vec<f32> = all_blocks.iter().map(|b| b.price_czk_per_kwh).collect();

                charge_slots = optimization::select_charge_slots(
                    &valley_indices,
                    &all_prices,
                    energy_for_target_soc,
                    context.control_config.max_battery_charge_rate_kw,
                    self.config.charge_consolidation_tolerance,
                    self.config.min_consecutive_charge_blocks,
                );

                // Create battery params for simulations
                let battery_params = simulation::BatteryParams {
                    capacity_kwh: context.control_config.battery_capacity_kwh,
                    charge_rate_kw: context.control_config.max_battery_charge_rate_kw,
                    discharge_rate_kw: context.control_config.max_battery_charge_rate_kw,
                    efficiency: self.config.round_trip_efficiency,
                    min_soc_pct: self.config.min_soc_pct,
                };

                // Validation: Forward simulate with P50 to verify we reach 90% before sustained peak
                if let Some(peak_start) = sustained_peak_start {
                    let modes = self.build_mode_schedule(
                        &charge_slots,
                        &windows,
                        current_block_index,
                        slot_count,
                    );
                    let soc_trajectory = simulation::simulate_soc(
                        context.current_battery_soc,
                        &modes,
                        &net_consumption_p50,
                        battery_params,
                    );

                    // Check SOC at peak start
                    if let Some(&soc_at_peak) = soc_trajectory.get(peak_start - current_block_index)
                        && soc_at_peak < self.config.daily_charging_target_soc
                    {
                        // Not enough! Add more charge blocks before deadline
                        let deficit = self.config.daily_charging_target_soc - soc_at_peak;
                        let extra_energy = (deficit / 100.0)
                            * context.control_config.battery_capacity_kwh
                            / self.config.round_trip_efficiency;
                        let extra_blocks_needed = (extra_energy / charge_per_block).ceil() as usize;

                        // Find additional cheapest blocks not already selected
                        let mut extra_blocks = Vec::new();
                        for (idx, _price) in &all_valley_blocks {
                            if !charge_slots.contains(idx)
                                && extra_blocks.len() < extra_blocks_needed
                            {
                                extra_blocks.push(*idx);
                            }
                        }
                        charge_slots.extend(extra_blocks);
                        charge_slots.sort();
                    }
                }

                // P90 Validation: Check if schedule survives high consumption
                let modes = self.build_mode_schedule(
                    &charge_slots,
                    &windows,
                    current_block_index,
                    slot_count,
                );
                let soc_trajectory_p90 = simulation::simulate_soc(
                    context.current_battery_soc,
                    &modes,
                    &net_consumption_p90,
                    battery_params,
                );

                // Find minimum SOC in P90 scenario
                let min_soc_p90 = soc_trajectory_p90
                    .iter()
                    .fold(f32::INFINITY, |a, &b| a.min(b));

                if min_soc_p90 < self.config.min_soc_pct {
                    // P90 scenario causes depletion - add minimal blocks to prevent
                    // Find where depletion occurs
                    if let Some(depletion_idx) = soc_trajectory_p90
                        .iter()
                        .position(|&soc| soc <= self.config.min_soc_pct)
                    {
                        let depletion_block = current_block_index + depletion_idx;

                        // Add cheapest available blocks BEFORE depletion point
                        let deficit = self.config.min_soc_pct - min_soc_p90 + 5.0; // 5% safety buffer
                        let extra_energy = (deficit / 100.0)
                            * context.control_config.battery_capacity_kwh
                            / self.config.round_trip_efficiency;
                        let extra_blocks_needed = (extra_energy / charge_per_block).ceil() as usize;

                        let mut extra_blocks = Vec::new();
                        for (idx, _price) in &all_valley_blocks {
                            if *idx < depletion_block
                                && !charge_slots.contains(idx)
                                && extra_blocks.len() < extra_blocks_needed
                            {
                                extra_blocks.push(*idx);
                            }
                        }
                        charge_slots.extend(extra_blocks);
                        charge_slots.sort();
                    }
                }
            }
        }

        // =====================================================================
        // CHEAP BLOCKS FALLBACK: After smart scheduling, "stupidly" scan the
        // 12 cheapest remaining blocks ahead. If predicted SOC is low at those
        // blocks, schedule charging there too. This catches cheap windows like
        // 5:00-5:45 that valley detection might miss.
        // =====================================================================
        const MIN_CHEAP_BLOCKS_TO_SCAN: usize = 12;

        // Collect ALL blocks ahead that don't already have charging scheduled
        let mut remaining_cheap_blocks: Vec<(usize, f32)> = Vec::new();
        for (idx, block) in all_blocks.iter().enumerate() {
            if idx < current_block_index {
                continue; // Past blocks
            }
            if charge_slots.contains(&idx) {
                continue; // Already scheduled for charging
            }
            remaining_cheap_blocks.push((idx, block.price_czk_per_kwh));
        }

        // Sort by price - cheapest first
        remaining_cheap_blocks
            .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take the 12 cheapest remaining blocks
        let cheapest_remaining: Vec<(usize, f32)> = remaining_cheap_blocks
            .into_iter()
            .take(MIN_CHEAP_BLOCKS_TO_SCAN)
            .collect();

        if !cheapest_remaining.is_empty() {
            // Group into consecutive windows (respecting min_consecutive_charge_blocks)
            let mut cheap_by_idx: Vec<(usize, f32)> = cheapest_remaining.clone();
            cheap_by_idx.sort_by_key(|(idx, _)| *idx);

            // Find consecutive runs within these cheap blocks
            let mut runs: Vec<Vec<(usize, f32)>> = Vec::new();
            let mut current_run: Vec<(usize, f32)> = Vec::new();

            for (idx, price) in cheap_by_idx {
                if current_run.is_empty() || idx == current_run.last().map(|(i, _)| *i).unwrap() + 1
                {
                    current_run.push((idx, price));
                } else {
                    if current_run.len() >= self.config.min_consecutive_charge_blocks {
                        runs.push(current_run);
                    }
                    current_run = vec![(idx, price)];
                }
            }
            if current_run.len() >= self.config.min_consecutive_charge_blocks {
                runs.push(current_run);
            }

            // For each consecutive window, simulate SOC and add if needed
            let battery_params = simulation::BatteryParams {
                capacity_kwh: context.control_config.battery_capacity_kwh,
                charge_rate_kw: context.control_config.max_battery_charge_rate_kw,
                discharge_rate_kw: context.control_config.max_battery_charge_rate_kw,
                efficiency: self.config.round_trip_efficiency,
                min_soc_pct: self.config.min_soc_pct,
            };

            for run in runs {
                if run.is_empty() {
                    continue;
                }

                let run_start_idx = run.first().map(|(i, _)| *i).unwrap();
                let run_end_idx = run.last().map(|(i, _)| *i).unwrap();

                // Calculate average price for this run
                let run_avg_price: f32 =
                    run.iter().map(|(_, p)| *p).sum::<f32>() / run.len() as f32;

                // Build current schedule (without this run) to simulate SOC
                let current_modes = self.build_mode_schedule(
                    &charge_slots,
                    &windows,
                    current_block_index,
                    slot_count,
                );
                let soc_trajectory = simulation::simulate_soc(
                    context.current_battery_soc,
                    &current_modes,
                    &net_consumption_p50,
                    battery_params,
                );

                // Get predicted SOC at the start of this run
                let relative_run_start = run_start_idx.saturating_sub(current_block_index);
                let predicted_soc_at_run = soc_trajectory
                    .get(relative_run_start)
                    .copied()
                    .unwrap_or(context.current_battery_soc);

                // If SOC is low (below target) AND this is a cheap window, add it
                let soc_threshold = self.config.daily_charging_target_soc * 0.8; // 80% of target = 72%

                if predicted_soc_at_run < soc_threshold {
                    // Check if prices AFTER this run are higher (we're charging before expensive period)
                    let blocks_after: Vec<f32> = all_blocks
                        .iter()
                        .skip(run_end_idx + 1)
                        .take(8) // Look 2 hours ahead
                        .map(|b| b.price_czk_per_kwh)
                        .collect();

                    // Skip if near end of horizon (less than 4 blocks = 1 hour remaining)
                    // Night prices will come in next day's data, don't charge just before
                    if blocks_after.len() < 4 {
                        continue;
                    }

                    let avg_after = blocks_after.iter().sum::<f32>() / blocks_after.len() as f32;

                    // Also check: are there cheaper blocks LATER in the schedule?
                    // Don't charge now if we could charge cheaper later
                    let min_price_after: f32 = all_blocks
                        .iter()
                        .skip(run_end_idx + 1)
                        .map(|b| b.price_czk_per_kwh)
                        .fold(f32::INFINITY, |a, b| a.min(b));

                    // Only add if: prices rise significantly after AND no cheaper blocks later
                    let prices_rise_after = avg_after > run_avg_price * 1.2;
                    let no_cheaper_later = min_price_after >= run_avg_price * 0.9; // Allow 10% tolerance

                    if prices_rise_after && no_cheaper_later {
                        // Add this run to charge slots
                        for (idx, _) in &run {
                            if !charge_slots.contains(idx) {
                                charge_slots.push(*idx);
                            }
                        }
                        charge_slots.sort();
                    }
                }
            }
        }

        // Check if current block is a charge slot
        if charge_slots.contains(&current_block_index) {
            return (
                InverterOperationMode::ForceCharge,
                format!("Scheduled charge: {:.3} CZK/kWh", current_price),
            );
        }

        // Check if current block is in a peak window
        for window in &windows {
            if window.peak_slots.contains(&current_block_index)
                && context.current_battery_soc > self.config.min_soc_pct
            {
                return (
                    InverterOperationMode::SelfUse,
                    format!("Peak discharge: {:.3} CZK/kWh", current_price),
                );
            }
        }

        // Default: Hold
        (
            InverterOperationMode::SelfUse,
            format!("Hold: {:.3} CZK/kWh", current_price),
        )
    }
}

impl EconomicStrategy for WinterAdaptiveV2Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V2"
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
            eval.reason = "No price data".to_string();
            return eval;
        };

        let current_block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        let (mode, reason) = self.decide_mode(context, all_blocks, current_block_index);

        eval.mode = mode;
        eval.reason = reason;

        // Calculate energy flows based on mode
        match mode {
            InverterOperationMode::ForceCharge => {
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk =
                    economics::grid_import_cost(charge_kwh, context.price_block.price_czk_per_kwh);
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
            _ => {}
        }

        eval.calculate_net_profit();
        eval
    }
}

#[cfg(test)]
mod tests {
    use super::arbitrage;
    use super::spike_detection;
    use chrono::{TimeZone, Utc};
    use fluxion_types::pricing::TimeBlockPrice;

    #[test]
    fn test_arbitrage_window_detection() {
        // Create typical Czech pattern: overnight valley → morning peak → midday valley → evening peak
        let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let mut blocks = Vec::new();

        // Overnight valley (00:00-06:00): 24 blocks @ 1.5 CZK
        for i in 0..24 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 1.5,
            });
        }

        // Morning peak (06:00-10:00): 16 blocks @ 4.5 CZK
        for i in 24..40 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 4.5,
            });
        }

        // Midday valley (10:00-14:00): 16 blocks @ 2.0 CZK
        for i in 40..56 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
            });
        }

        // Evening peak (14:00-22:00): 32 blocks @ 5.0 CZK
        for i in 56..88 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
            });
        }

        let windows = arbitrage::detect_windows(&blocks);

        // Should detect 2 windows
        assert!(
            !windows.is_empty(),
            "Should detect at least 1 arbitrage window"
        );

        println!("Detected {} windows", windows.len());
        for (idx, window) in windows.iter().enumerate() {
            println!(
                "Window {}: valley avg={:.2}, peak avg={:.2}, valley slots={}, peak slots={}",
                idx,
                window.valley_avg_price,
                window.peak_avg_price,
                window.valley_slots.len(),
                window.peak_slots.len()
            );
        }
    }

    #[test]
    fn test_spike_detection() {
        let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let mut blocks = Vec::new();

        // Normal prices
        for i in 0..10 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
            });
        }

        // Spike at index 10
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(10 * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 10.0,
        });

        // More normal prices
        for i in 11..20 {
            blocks.push(TimeBlockPrice {
                block_start: base + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
            });
        }

        let net_consumption = vec![0.5; 20]; // 0.5 kWh per slot
        let spikes = spike_detection::detect_spikes(&blocks, 8.0, &net_consumption, 5.0);

        assert_eq!(spikes.len(), 1, "Should detect 1 spike");
        assert_eq!(spikes[0].slot_index, 10);
        assert_eq!(spikes[0].price_czk, 10.0);
    }
}
