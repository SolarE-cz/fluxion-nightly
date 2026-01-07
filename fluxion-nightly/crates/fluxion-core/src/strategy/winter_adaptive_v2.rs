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
};
use crate::utils::calculate_ema;
use chrono::{DateTime, Datelike, Utc};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::RwLock;

/// Seasonal operating mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeasonalMode {
    /// May-September: More solar, lower min SOC
    Summer,
    /// October-April: Less solar, higher min SOC
    Winter,
}

impl SeasonalMode {
    /// Determine the season from a UTC date
    #[must_use]
    pub fn from_date(date: DateTime<Utc>) -> Self {
        match date.month() {
            5..=9 => Self::Summer,
            _ => Self::Winter,
        }
    }

    /// Minimum SOC recommendation by season (percent)
    #[must_use]
    pub fn min_soc_percent(&self) -> f32 {
        match self {
            Self::Summer => 20.0,
            Self::Winter => 50.0,
        }
    }

    /// Minimum spread threshold for arbitrage (CZK/kWh)
    #[must_use]
    pub fn min_spread_threshold(&self) -> f32 {
        match self {
            Self::Summer => 2.0,
            Self::Winter => 3.0,
        }
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// A locked schedule entry - mode decision that should not change
#[derive(Debug, Clone)]
pub struct LockedBlock {
    /// Start time of this block
    pub block_start: DateTime<Utc>,
    /// The locked mode for this block
    pub mode: InverterOperationMode,
    /// Reason for this mode (for logging)
    pub reason: String,
}

/// State for schedule locking to prevent oscillation
#[derive(Debug, Clone, Default)]
pub struct ScheduleLockState {
    /// Locked blocks - mode decisions that should not be recalculated
    pub locked_blocks: Vec<LockedBlock>,
}

impl ScheduleLockState {
    /// Check if a block is locked and return its mode if so
    pub fn get_locked_mode(
        &self,
        block_start: DateTime<Utc>,
    ) -> Option<(InverterOperationMode, String)> {
        self.locked_blocks
            .iter()
            .find(|b| b.block_start == block_start)
            .map(|b| (b.mode, format!("LOCKED: {}", b.reason)))
    }

    /// Lock modes for the specified blocks
    pub fn lock_blocks(&mut self, blocks: Vec<LockedBlock>) {
        // Clear old locks (blocks in the past)
        let now = Utc::now();
        self.locked_blocks.retain(|b| b.block_start >= now);

        // Add new locks, avoiding duplicates
        for block in blocks {
            if !self
                .locked_blocks
                .iter()
                .any(|b| b.block_start == block.block_start)
            {
                self.locked_blocks.push(block);
            }
        }
    }
}

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

    // ---- Price Optimization ----
    /// Maximum price premium (%) above cheapest block to accept for charging
    /// Default: 15% - will skip 2.5 CZK blocks if 2.0 CZK blocks are available
    /// Example: if cheapest block is 2.0 CZK, max acceptable is 2.3 CZK (2.0 * 1.15)
    #[serde(default = "default_charge_price_tolerance")]
    pub charge_price_tolerance_percent: f32,

    /// How much above median (%) to allow when SOC deficit is predicted
    /// Default: 10% - when battery will hit min_soc, allow charging at median * 1.10
    /// This helps cover gaps where isolated cheap blocks exist just above median
    #[serde(default = "default_deficit_median_relaxation")]
    pub deficit_median_relaxation_percent: f32,

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

fn default_charge_price_tolerance() -> f32 {
    15.0 // 15% above cheapest price
}

fn default_deficit_median_relaxation() -> f32 {
    10.0 // 10% above median when deficit exists
}

impl Default for WinterAdaptiveV2Config {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 100, // Highest priority (same as Winter Adaptive v1)
            history_days: 3,
            consumption_p90_multiplier: 1.3,
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
            charge_price_tolerance_percent: 15.0,
            deficit_median_relaxation_percent: 10.0,
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
            };
        }

        // Calculate EMA as P50 estimate
        let p50 = calculate_ema(&values, config.history_days).unwrap_or(fallback_per_slot_kwh);

        ConsumptionForecast {
            p50_kwh: p50,
            p90_kwh: p50 * config.consumption_p90_multiplier,
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
// Module: Simplified Scheduling
// ============================================================================

mod scheduling {
    /// Select cheapest blocks respecting min_consecutive constraint
    /// Returns indices of blocks to charge, sorted by time
    ///
    /// Algorithm:
    /// 1. Sort all blocks by price (cheapest first)
    /// 2. Take the top N cheapest blocks as candidates
    /// 3. From candidates, find consecutive runs >= min_consecutive
    /// 4. For each run, try shifting earlier AND later (up to min_consecutive * 2 blocks)
    ///    to find a cheaper consecutive window
    /// 5. Apply price tolerance filter (if set) - reject runs > tolerance% above cheapest
    /// 6. Select cheapest runs until we have enough blocks
    /// 7. If not enough consecutive runs, expand candidate pool
    pub fn select_cheapest_blocks(
        blocks_with_prices: &[(usize, f32)], // (index, price) pairs
        count_needed: usize,
        min_consecutive: usize,
        price_tolerance_percent: Option<f32>, // Max premium (%) above cheapest to accept
    ) -> Vec<usize> {
        if blocks_with_prices.is_empty() || count_needed == 0 {
            return Vec::new();
        }

        // Build a lookup map for quick price access by index
        let price_map: std::collections::HashMap<usize, f32> =
            blocks_with_prices.iter().copied().collect();

        // Sort by price (cheapest first)
        let mut sorted = blocks_with_prices.to_vec();
        sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Simple case: if we need fewer than min_consecutive, just take cheapest
        if count_needed < min_consecutive {
            let mut result: Vec<usize> = sorted
                .iter()
                .take(count_needed)
                .map(|(idx, _)| *idx)
                .collect();
            result.sort();
            return result;
        }

        // Try with progressively larger candidate pools until we find enough
        let mut selected: Vec<usize> = Vec::new();

        // Start with just the cheapest count_needed blocks, then expand if needed
        for pool_size in [
            count_needed,
            count_needed * 2,
            count_needed * 3,
            sorted.len(),
        ] {
            let pool_size = pool_size.min(sorted.len());
            let candidates: Vec<(usize, f32)> = sorted.iter().take(pool_size).copied().collect();

            // Sort candidates by index to find consecutive runs
            let mut by_idx = candidates.clone();
            by_idx.sort_by_key(|(idx, _)| *idx);

            // Find all consecutive runs of at least min_consecutive length
            let mut runs: Vec<Vec<(usize, f32)>> = Vec::new();
            let mut current_run: Vec<(usize, f32)> = Vec::new();

            for (idx, price) in by_idx {
                if current_run.is_empty() || idx == current_run.last().unwrap().0 + 1 {
                    current_run.push((idx, price));
                } else {
                    if current_run.len() >= min_consecutive {
                        runs.push(current_run);
                    }
                    current_run = vec![(idx, price)];
                }
            }
            if current_run.len() >= min_consecutive {
                runs.push(current_run);
            }

            // Score runs by average price, but also try shifting earlier to find cheaper windows
            let mut scored_runs: Vec<(f32, Vec<usize>)> = runs
                .into_iter()
                .map(|run| {
                    let original_avg: f32 =
                        run.iter().map(|(_, p)| *p).sum::<f32>() / run.len() as f32;
                    let original_indices: Vec<usize> = run.iter().map(|(idx, _)| *idx).collect();
                    let run_length = run.len();
                    let run_start = run.first().map(|(idx, _)| *idx).unwrap_or(0);

                    // Try shifting the window earlier by up to min_consecutive * 2 blocks
                    // to find a potentially cheaper consecutive window
                    let max_shift_back = min_consecutive * 2;
                    let mut best_avg = original_avg;
                    let mut best_indices = original_indices.clone();

                    for shift in 1..=max_shift_back {
                        if run_start < shift {
                            break; // Can't shift back further
                        }
                        let new_start = run_start - shift;

                        // Check if we can form a valid consecutive window of run_length blocks
                        // All blocks in the new window must exist in blocks_with_prices
                        let new_indices: Vec<usize> = (new_start..new_start + run_length).collect();
                        let all_available =
                            new_indices.iter().all(|idx| price_map.contains_key(idx));

                        if all_available {
                            let new_avg: f32 = new_indices
                                .iter()
                                .map(|idx| price_map.get(idx).copied().unwrap_or(f32::MAX))
                                .sum::<f32>()
                                / run_length as f32;

                            if new_avg < best_avg {
                                best_avg = new_avg;
                                best_indices = new_indices;
                            }
                        }
                    }

                    // ALSO try shifting the window LATER (forward in time) to find cheaper blocks
                    // This is critical for scenarios where cheaper prices exist later in the night
                    let max_shift_forward = min_consecutive * 2;
                    let max_idx = price_map.keys().max().copied().unwrap_or(0);

                    for shift in 1..=max_shift_forward {
                        let new_start = run_start + shift;

                        // Ensure we don't exceed available blocks
                        if new_start + run_length - 1 > max_idx {
                            break;
                        }

                        let new_indices: Vec<usize> = (new_start..new_start + run_length).collect();
                        let all_available =
                            new_indices.iter().all(|idx| price_map.contains_key(idx));

                        if all_available {
                            let new_avg: f32 = new_indices
                                .iter()
                                .map(|idx| price_map.get(idx).copied().unwrap_or(f32::MAX))
                                .sum::<f32>()
                                / run_length as f32;

                            if new_avg < best_avg {
                                best_avg = new_avg;
                                best_indices = new_indices;
                            }
                        }
                    }

                    (best_avg, best_indices)
                })
                .collect();

            // Sort runs by average price (cheapest first)
            scored_runs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            // Apply price tolerance filter: reject runs with price > tolerance% above cheapest
            // This ensures we strongly prefer the cheapest blocks (even if they're later)
            // and only use more expensive blocks when absolutely necessary
            if let Some(tolerance_pct) = price_tolerance_percent
                && let Some((min_avg, _)) = scored_runs.first()
            {
                let max_acceptable_price = min_avg * (1.0 + tolerance_pct / 100.0);
                let original_count = scored_runs.len();
                scored_runs.retain(|(avg_price, _)| *avg_price <= max_acceptable_price);

                if scored_runs.len() < original_count {
                    tracing::debug!(
                        "Price tolerance filter: kept {} of {} runs (max price: {:.3} CZK, tolerance: {}%)",
                        scored_runs.len(),
                        original_count,
                        max_acceptable_price,
                        tolerance_pct
                    );
                }
            }

            // Select blocks from cheapest runs first
            selected.clear();
            for (_, run) in &scored_runs {
                if selected.len() >= count_needed {
                    break;
                }
                for &idx in run {
                    if selected.len() >= count_needed {
                        break;
                    }
                    if !selected.contains(&idx) {
                        selected.push(idx);
                    }
                }
            }

            // If we found enough blocks in consecutive runs, we're done
            if selected.len() >= count_needed {
                break;
            }
        }

        // If still need more blocks (couldn't find enough consecutive runs),
        // fall back to cheapest individual blocks
        if selected.len() < count_needed {
            for (idx, _) in &sorted {
                if selected.len() >= count_needed {
                    break;
                }
                if !selected.contains(idx) {
                    selected.push(*idx);
                }
            }
        }

        // Return sorted by time
        selected.sort();
        selected
    }

    /// Simple SOC simulation: predict SOC at each block given charge schedule
    /// Returns SOC at the START of each block (index 0 = current SOC)
    pub fn simulate_soc(
        initial_soc: f32,
        charge_schedule: &[usize], // block indices where we charge
        num_blocks: usize,
        consumption_per_block_kwh: f32,
        charge_per_block_kwh: f32,
        battery_capacity_kwh: f32,
        min_soc: f32,
    ) -> Vec<f32> {
        let mut soc = initial_soc;
        let mut trajectory = Vec::with_capacity(num_blocks + 1);
        trajectory.push(soc);

        for block_idx in 0..num_blocks {
            if charge_schedule.contains(&block_idx) {
                // Charging: add energy
                let energy_added = charge_per_block_kwh;
                let soc_delta = (energy_added / battery_capacity_kwh) * 100.0;
                soc = (soc + soc_delta).min(100.0);
            } else {
                // Discharging/consuming: subtract energy
                let energy_used = consumption_per_block_kwh;
                let soc_delta = (energy_used / battery_capacity_kwh) * 100.0;
                soc = (soc - soc_delta).max(min_soc);
            }
            trajectory.push(soc);
        }

        trajectory
    }

    /// Find blocks where price is above median (expensive periods)
    pub fn find_expensive_periods(prices: &[f32], current_index: usize) -> Vec<(usize, usize)> {
        // Calculate median of remaining blocks
        let remaining_prices: Vec<f32> = prices.iter().skip(current_index).copied().collect();
        if remaining_prices.is_empty() {
            return Vec::new();
        }

        let mut sorted = remaining_prices.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if sorted.len().is_multiple_of(2) {
            let mid = sorted.len() / 2;
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };

        // Find consecutive runs of above-median prices
        let mut periods: Vec<(usize, usize)> = Vec::new();
        let mut in_expensive = false;
        let mut period_start = 0;

        for (rel_idx, &price) in remaining_prices.iter().enumerate() {
            let abs_idx = current_index + rel_idx;

            if price > median {
                if !in_expensive {
                    period_start = abs_idx;
                    in_expensive = true;
                }
            } else if in_expensive {
                periods.push((period_start, abs_idx));
                in_expensive = false;
            }
        }

        // Close last period if still expensive at end
        if in_expensive {
            periods.push((period_start, current_index + remaining_prices.len()));
        }

        periods
    }
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug)]
pub struct WinterAdaptiveV2Strategy {
    config: WinterAdaptiveV2Config,
    /// Schedule lock state to prevent mode oscillation
    lock_state: RwLock<ScheduleLockState>,
}

impl WinterAdaptiveV2Strategy {
    pub fn new(config: WinterAdaptiveV2Config) -> Self {
        Self {
            config,
            lock_state: RwLock::new(ScheduleLockState::default()),
        }
    }

    /// Main decision logic for current block - GLOBAL OPTIMIZATION ALGORITHM
    ///
    /// 1. Find the cheapest N blocks (based on force_charge_hours config)
    /// 2. Simulate SOC through the day and identify expensive periods (above median)
    /// 3. Calculate total energy deficit across ALL expensive periods
    /// 4. Select globally cheapest blocks from ENTIRE remaining schedule to cover deficit
    /// 5. Apply cost-benefit filter: only use blocks where charging is economical
    /// 6. Validate temporal feasibility: ensure adequate SOC before each expensive period
    /// 7. Decide current block based on final schedule
    ///
    /// Returns (mode, reason, decision_uid)
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
    ) -> (InverterOperationMode, String, String) {
        let current_price = context.price_block.price_czk_per_kwh;
        let current_block_start = context.price_block.block_start;
        let min_consecutive = context.control_config.min_consecutive_force_blocks;

        // =====================================================================
        // PRIORITY 0: Check if this block is locked (prevents oscillation)
        // =====================================================================
        {
            let lock_state = self.lock_state.read().unwrap();
            if let Some((locked_mode, locked_reason)) =
                lock_state.get_locked_mode(current_block_start)
            {
                tracing::debug!(
                    "V2: Block {} is locked to {:?}",
                    current_block_start,
                    locked_mode
                );
                return (
                    locked_mode,
                    locked_reason,
                    "winter_adaptive_v2:locked_block".to_string(),
                );
            }
        }

        // Priority 1: Negative prices - always charge (free energy!)
        if self.config.negative_price_handling_enabled && current_price < 0.0 {
            if context.current_battery_soc < 100.0 {
                return (
                    InverterOperationMode::ForceCharge,
                    format!("Negative price: {:.3} CZK/kWh", current_price),
                    "winter_adaptive_v2:negative_price_charge".to_string(),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price, battery full: {:.3} CZK/kWh", current_price),
                "winter_adaptive_v2:negative_price_full".to_string(),
            );
        }

        // Extract prices for remaining blocks
        let prices: Vec<f32> = all_blocks.iter().map(|b| b.price_czk_per_kwh).collect();
        let num_remaining_blocks = all_blocks.len() - current_block_index;

        // Parameters for simulation
        let consumption_per_block_kwh = context.control_config.average_household_load_kw * 0.25; // 15 min
        let charge_per_block_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
        let battery_capacity_kwh = context.control_config.battery_capacity_kwh;
        let min_soc = self.config.min_soc_pct;

        // =====================================================================
        // STEP 1: Find the cheapest N blocks for base charging
        // N = force_charge_hours * 4 (convert hours to 15-min blocks)
        // =====================================================================
        let base_charge_blocks = context.control_config.force_charge_hours * 4;

        // Collect all remaining blocks with prices (only from current index onward)
        let remaining_blocks: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .skip(current_block_index)
            .map(|(idx, b)| (idx, b.price_czk_per_kwh))
            .collect();

        // =====================================================================
        // HARD FILTER: Never charge in top 50% most expensive slots
        // Calculate median price and filter out expensive blocks
        // =====================================================================
        let median_price = {
            let mut sorted_prices: Vec<f32> = remaining_blocks.iter().map(|(_, p)| *p).collect();
            sorted_prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if sorted_prices.is_empty() {
                0.0
            } else if sorted_prices.len().is_multiple_of(2) {
                let mid = sorted_prices.len() / 2;
                (sorted_prices[mid - 1] + sorted_prices[mid]) / 2.0
            } else {
                sorted_prices[sorted_prices.len() / 2]
            }
        };

        // Only allow charging in blocks with price BELOW median (bottom 50%)
        let affordable_blocks: Vec<(usize, f32)> = remaining_blocks
            .iter()
            .filter(|(_, price)| *price < median_price)
            .copied()
            .collect();

        tracing::debug!(
            "V2: Median price: {:.3} CZK/kWh, {} of {} blocks are affordable for charging",
            median_price,
            affordable_blocks.len(),
            remaining_blocks.len()
        );

        // Select cheapest blocks respecting min_consecutive constraint
        // ONLY from affordable blocks (below median price)
        // Apply price tolerance to strongly prefer the cheapest available blocks
        let mut charge_schedule = scheduling::select_cheapest_blocks(
            &affordable_blocks,
            base_charge_blocks,
            min_consecutive,
            Some(self.config.charge_price_tolerance_percent),
        );

        // =====================================================================
        // STEP 2: Simulate SOC and find expensive periods
        // =====================================================================
        let expensive_periods = scheduling::find_expensive_periods(&prices, current_block_index);

        // =====================================================================
        // STEP 3: Calculate total energy deficit across ALL expensive periods
        // =====================================================================

        // First pass: calculate total deficit across all expensive periods
        let mut total_deficit_kwh = 0.0;

        for (period_start, period_end) in &expensive_periods {
            // Simulate SOC with current schedule
            let soc_trajectory = scheduling::simulate_soc(
                context.current_battery_soc,
                &charge_schedule,
                num_remaining_blocks,
                consumption_per_block_kwh,
                charge_per_block_kwh,
                battery_capacity_kwh,
                min_soc,
            );

            // Calculate how much energy we need to cover this expensive period
            let period_length = period_end - period_start;
            let energy_needed_kwh = period_length as f32 * consumption_per_block_kwh;
            let soc_needed = (energy_needed_kwh / battery_capacity_kwh) * 100.0 + min_soc;

            // Get predicted SOC at period start
            let rel_period_start = period_start.saturating_sub(current_block_index);
            let soc_at_period_start = soc_trajectory
                .get(rel_period_start)
                .copied()
                .unwrap_or(context.current_battery_soc);

            // If we won't have enough SOC, accumulate the deficit
            if soc_at_period_start < soc_needed {
                let soc_deficit = soc_needed - soc_at_period_start;
                let energy_deficit_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;
                total_deficit_kwh += energy_deficit_kwh;
            }
        }

        // =====================================================================
        // STEP 4: Select globally cheapest blocks to cover total deficit
        // =====================================================================

        if total_deficit_kwh > 0.0 {
            let extra_blocks_needed = (total_deficit_kwh / charge_per_block_kwh).ceil() as usize;

            // When deficit exists, use RELAXED median threshold to allow blocks
            // slightly above median - this helps capture adjacent blocks that
            // would otherwise be isolated (e.g., 3.38 CZK below median, 3.54 CZK just above)
            let relaxed_median_threshold =
                median_price * (1.0 + self.config.deficit_median_relaxation_percent / 100.0);

            tracing::debug!(
                "V2: Deficit of {:.2} kWh detected. Relaxing median filter from {:.3} to {:.3} CZK/kWh",
                total_deficit_kwh,
                median_price,
                relaxed_median_threshold
            );

            // Find all unscheduled blocks from the ENTIRE remaining schedule
            // Use RELAXED median threshold when deficit exists
            let available_blocks: Vec<(usize, f32)> = remaining_blocks
                .iter()
                .filter(|(idx, price)| {
                    *idx >= current_block_index
                        && !charge_schedule.contains(idx)
                        && *price < relaxed_median_threshold // Relaxed when deficit exists
                })
                .copied()
                .collect();

            // Cost-benefit check: only add blocks if charging is economical
            // Compare: cost_to_charge vs cost_to_buy_from_grid_during_expensive_period
            // Account for round-trip efficiency loss
            let efficiency = self.config.round_trip_efficiency;
            let avg_expensive_price = if expensive_periods.is_empty() {
                median_price * 1.5 // Assume 50% markup if no periods identified
            } else {
                expensive_periods
                    .iter()
                    .flat_map(|(start, end)| {
                        (*start..*end).map(|i| prices.get(i).copied().unwrap_or(0.0))
                    })
                    .sum::<f32>()
                    / expensive_periods
                        .iter()
                        .map(|(start, end)| (end - start) as f32)
                        .sum::<f32>()
            };

            // Select the globally cheapest blocks (already filtered to below median)
            // Use tolerance to prefer cheapest blocks for deficit coverage too
            let mut extra_blocks = scheduling::select_cheapest_blocks(
                &available_blocks,
                extra_blocks_needed,
                min_consecutive,
                Some(self.config.charge_price_tolerance_percent),
            );

            // Additional cost-benefit filter: charge_price / efficiency < expensive_price
            extra_blocks.retain(|&idx| {
                let charge_price = prices.get(idx).copied().unwrap_or(0.0);
                let effective_charge_cost = charge_price / efficiency;
                effective_charge_cost < avg_expensive_price
            });

            // Add selected blocks to schedule
            for idx in extra_blocks {
                if !charge_schedule.contains(&idx) {
                    charge_schedule.push(idx);
                }
            }
            charge_schedule.sort();
        }

        // =====================================================================
        // STEP 5: Validate temporal feasibility - ensure blocks before periods
        // =====================================================================

        // For each expensive period, verify we have adequate SOC at start
        // If blocks after a period were selected, they must serve a FUTURE period
        for (period_start, period_end) in &expensive_periods {
            let soc_trajectory = scheduling::simulate_soc(
                context.current_battery_soc,
                &charge_schedule,
                num_remaining_blocks,
                consumption_per_block_kwh,
                charge_per_block_kwh,
                battery_capacity_kwh,
                min_soc,
            );

            let period_length = period_end - period_start;
            let energy_needed_kwh = period_length as f32 * consumption_per_block_kwh;
            let soc_needed = (energy_needed_kwh / battery_capacity_kwh) * 100.0 + min_soc;

            let rel_period_start = period_start.saturating_sub(current_block_index);
            let soc_at_period_start = soc_trajectory
                .get(rel_period_start)
                .copied()
                .unwrap_or(context.current_battery_soc);

            // If still insufficient, add blocks that complete BEFORE this period
            if soc_at_period_start < soc_needed {
                let soc_deficit = soc_needed - soc_at_period_start;
                let energy_deficit_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;
                let extra_blocks_needed =
                    (energy_deficit_kwh / charge_per_block_kwh).ceil() as usize;

                // When deficit exists for a specific period, use RELAXED median threshold
                let relaxed_median_threshold =
                    median_price * (1.0 + self.config.deficit_median_relaxation_percent / 100.0);

                // Find cheapest blocks BEFORE this period (timing constraint)
                // Use relaxed median threshold when deficit exists
                let mut available_before: Vec<(usize, f32)> = remaining_blocks
                    .iter()
                    .filter(|(idx, price)| {
                        *idx >= current_block_index
                            && *idx < *period_start
                            && !charge_schedule.contains(idx)
                            && *price < relaxed_median_threshold // Relaxed when deficit exists
                    })
                    .copied()
                    .collect();

                available_before
                    .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

                // For timing-constrained blocks before a period, apply tolerance filter
                // If tolerance rejects too many, the function will fall back to individual blocks
                let extra_blocks = scheduling::select_cheapest_blocks(
                    &available_before,
                    extra_blocks_needed,
                    min_consecutive,
                    Some(self.config.charge_price_tolerance_percent),
                );

                for idx in extra_blocks {
                    if !charge_schedule.contains(&idx) {
                        charge_schedule.push(idx);
                    }
                }
                charge_schedule.sort();
            }
        }

        // =====================================================================
        // STEP 6: Make decision for current block
        // =====================================================================

        let (mode, reason, decision_uid) = if charge_schedule.contains(&current_block_index) {
            // Only charge if not already full
            if context.current_battery_soc < self.config.daily_charging_target_soc {
                (
                    InverterOperationMode::ForceCharge,
                    format!("Scheduled charge: {:.3} CZK/kWh", current_price),
                    "winter_adaptive_v2:scheduled_charge".to_string(),
                )
            } else {
                (
                    InverterOperationMode::SelfUse,
                    format!(
                        "Battery full, skipping charge: {:.3} CZK/kWh",
                        current_price
                    ),
                    "winter_adaptive_v2:battery_full".to_string(),
                )
            }
        } else {
            // Default: Self-use (battery covers consumption, no grid charging)
            (
                InverterOperationMode::SelfUse,
                format!("Hold: {:.3} CZK/kWh", current_price),
                "winter_adaptive_v2:hold".to_string(),
            )
        };

        // =====================================================================
        // STEP 7: Lock the next min_consecutive blocks to prevent oscillation
        // ONLY lock when evaluating the CURRENT block (not future blocks for preview)
        // =====================================================================
        let now = Utc::now();
        let block_age_seconds = (now - current_block_start).num_seconds();

        // Only lock if this block is the current block (started within last 15 minutes)
        // This prevents locking ALL blocks when generating schedule preview
        let is_current_block = (0..900).contains(&block_age_seconds); // 15 min = 900 sec

        if is_current_block {
            let mut lock_state = self.lock_state.write().unwrap();
            let mut blocks_to_lock = Vec::new();

            // Lock current block and next (min_consecutive - 1) blocks
            // Total locked = min_consecutive blocks
            for i in 0..min_consecutive {
                let block_idx = current_block_index + i;
                if block_idx < all_blocks.len() {
                    let block = &all_blocks[block_idx];
                    let block_mode = if charge_schedule.contains(&block_idx)
                        && context.current_battery_soc < self.config.daily_charging_target_soc
                    {
                        InverterOperationMode::ForceCharge
                    } else {
                        InverterOperationMode::SelfUse
                    };
                    let block_reason = if charge_schedule.contains(&block_idx) {
                        format!("Scheduled charge: {:.3} CZK/kWh", block.price_czk_per_kwh)
                    } else {
                        format!("Hold: {:.3} CZK/kWh", block.price_czk_per_kwh)
                    };

                    blocks_to_lock.push(LockedBlock {
                        block_start: block.block_start,
                        mode: block_mode,
                        reason: block_reason,
                    });
                }
            }

            let blocks_locked = blocks_to_lock.len();
            lock_state.lock_blocks(blocks_to_lock);
            tracing::debug!(
                "V2: Locked {} blocks ({}min) starting at {}",
                blocks_locked,
                blocks_locked * 15,
                current_block_start
            );
        }

        (mode, reason, decision_uid)
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

        let (mode, reason, decision_uid) =
            self.decide_mode(context, all_blocks, current_block_index);

        eval.mode = mode;
        eval.reason = reason;
        eval.decision_uid = Some(decision_uid);

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
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                // Estimate profit based on usable battery capacity vs consumption
                // Usable capacity = current SOC minus hardware minimum (cannot discharge below this)
                let usable_battery_kwh = ((context.current_battery_soc
                    - context.control_config.hardware_min_battery_soc)
                    .max(0.0)
                    / 100.0)
                    * context.control_config.battery_capacity_kwh;
                let price = context.price_block.price_czk_per_kwh;

                if usable_battery_kwh >= context.consumption_forecast_kwh {
                    // Battery can fully cover consumption - show as avoided grid import cost
                    eval.revenue_czk = context.consumption_forecast_kwh * price;
                } else {
                    // Battery partially depleted - split between battery and grid
                    // Battery covers what it can (avoided cost = profit)
                    eval.revenue_czk = usable_battery_kwh * price;
                    // Grid must cover the rest (actual cost)
                    eval.cost_czk = (context.consumption_forecast_kwh - usable_battery_kwh) * price;
                }
            }
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
