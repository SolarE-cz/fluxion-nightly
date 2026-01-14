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

//! Winter Adaptive Strategy V3
//!
//! A simplified battery optimization strategy with HDO tariff integration.
//!
//! ## Key Features
//!
//! 1. **HDO Tariff Integration**: Uses dynamic grid fees from HDO sensor data
//! 2. **Effective Price Calculation**: total_price = spot_price + grid_fee
//! 3. **Winter Discharge Restriction**: SOC >= 50% AND top 4 expensive blocks today
//! 4. **Simplified Algorithm**: No arbitrage, P90, spikes, or feed-in complexity
//!
//! ## HDO Sensor Format
//!
//! The strategy parses HDO data from Home Assistant sensors in this format:
//! ```yaml
//! data:
//!   signals:
//!     - signal: EVV1
//!       datum: 14.01.2026
//!       casy: 00:00-06:00; 07:00-09:00; 10:00-13:00; 14:00-16:00; 17:00-24:00
//! ```

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

// Re-export SeasonalMode from V2 for consistency
pub use crate::strategy::winter_adaptive_v2::{DayEnergyBalance, SeasonalMode};

// ============================================================================
// HDO Parsing and Caching
// ============================================================================

/// Parsed HDO time range for low tariff periods
#[derive(Debug, Clone)]
pub struct HdoTimeRange {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

/// Cached HDO schedule for a specific date
#[derive(Debug, Clone)]
pub struct HdoDaySchedule {
    /// Date this schedule is for
    pub date: NaiveDate,
    /// Low tariff time ranges for this day
    pub low_tariff_ranges: Vec<HdoTimeRange>,
}

/// Cache for HDO schedules with TTL
#[derive(Debug)]
pub struct HdoCache {
    /// Cached schedules by date
    schedules: RwLock<HashMap<NaiveDate, HdoDaySchedule>>,
    /// Last refresh timestamp
    last_refresh: RwLock<Option<DateTime<Utc>>>,
    /// Cache TTL in seconds
    ttl_secs: u64,
}

impl HdoCache {
    /// Create a new HDO cache with specified TTL
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            schedules: RwLock::new(HashMap::new()),
            last_refresh: RwLock::new(None),
            ttl_secs,
        }
    }

    /// Check if cache needs refresh
    pub fn needs_refresh(&self) -> bool {
        let last = self.last_refresh.read().unwrap();
        match *last {
            None => true,
            Some(ts) => {
                let elapsed = Utc::now().signed_duration_since(ts).num_seconds() as u64;
                elapsed > self.ttl_secs
            }
        }
    }

    /// Update cache with parsed HDO data
    pub fn update(&self, schedules: Vec<HdoDaySchedule>) {
        let mut cache = self.schedules.write().unwrap();
        cache.clear();
        for schedule in schedules {
            cache.insert(schedule.date, schedule);
        }
        *self.last_refresh.write().unwrap() = Some(Utc::now());
    }

    /// Check if a given time is in low tariff period
    /// Returns None if no data available for that date (fallback to high tariff)
    pub fn is_low_tariff(&self, dt: DateTime<Utc>) -> Option<bool> {
        let date = dt.date_naive();
        let time = dt.time();

        let cache = self.schedules.read().unwrap();
        let schedule = cache.get(&date)?;

        Some(schedule.low_tariff_ranges.iter().any(|range| {
            if range.start <= range.end {
                // Normal range: e.g., 06:00-12:00
                time >= range.start && time < range.end
            } else {
                // Overnight range: e.g., 22:00-06:00 (spans midnight)
                time >= range.start || time < range.end
            }
        }))
    }
}

impl Default for HdoCache {
    fn default() -> Self {
        Self::new(3600) // 1 hour default TTL
    }
}

/// Parse Czech date format "DD.MM.YYYY"
pub fn parse_czech_date(s: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let day: u32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let year: i32 = parts[2].parse().ok()?;

    NaiveDate::from_ymd_opt(year, month, day)
}

/// Parse time ranges from HDO format "HH:MM-HH:MM; HH:MM-HH:MM"
pub fn parse_time_ranges(s: &str) -> Vec<HdoTimeRange> {
    let mut ranges = Vec::new();

    for range_str in s.split(';') {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            continue;
        }

        if let Some(range) = parse_single_time_range(range_str) {
            ranges.push(range);
        }
    }

    ranges
}

/// Parse a single time range "HH:MM-HH:MM"
fn parse_single_time_range(s: &str) -> Option<HdoTimeRange> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start = parse_time(parts[0])?;
    let end = parse_time(parts[1])?;

    Some(HdoTimeRange { start, end })
}

/// Parse time "HH:MM"
fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }

    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;

    // Handle 24:00 as end-of-day (convert to 23:59:59)
    if hour == 24 && minute == 0 {
        return NaiveTime::from_hms_opt(23, 59, 59);
    }

    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// Parse HDO sensor data from raw JSON/YAML-like format
/// Expected format:
/// ```yaml
/// data:
///   signals:
///     - signal: EVV1
///       datum: 14.01.2026
///       casy: 00:00-06:00; 07:00-09:00; ...
/// ```
pub fn parse_hdo_sensor_data(raw_data: &str) -> Vec<HdoDaySchedule> {
    let mut schedules = Vec::new();

    // Try to parse as JSON first
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw_data)
        && let Some(data) = json.get("data")
        && let Some(signals) = data.get("signals").and_then(|s| s.as_array())
    {
        for signal in signals {
            if let (Some(datum), Some(casy)) = (
                signal.get("datum").and_then(|d| d.as_str()),
                signal.get("casy").and_then(|c| c.as_str()),
            ) && let Some(date) = parse_czech_date(datum)
            {
                let ranges = parse_time_ranges(casy);
                if !ranges.is_empty() {
                    schedules.push(HdoDaySchedule {
                        date,
                        low_tariff_ranges: ranges,
                    });
                }
            }
        }
    }

    schedules
}

// ============================================================================
// Locked Block State (from V2)
// ============================================================================

/// A locked schedule entry - mode decision that should not change
#[derive(Debug, Clone)]
pub struct LockedBlock {
    pub block_start: DateTime<Utc>,
    pub mode: InverterOperationMode,
    pub reason: String,
}

/// State for schedule locking to prevent oscillation
#[derive(Debug, Clone, Default)]
pub struct ScheduleLockState {
    pub locked_blocks: Vec<LockedBlock>,
}

impl ScheduleLockState {
    pub fn get_locked_mode(
        &self,
        block_start: DateTime<Utc>,
    ) -> Option<(InverterOperationMode, String)> {
        self.locked_blocks
            .iter()
            .find(|b| b.block_start == block_start)
            .map(|b| (b.mode, format!("LOCKED: {}", b.reason)))
    }

    pub fn lock_blocks(&mut self, blocks: Vec<LockedBlock>) {
        let now = Utc::now();
        self.locked_blocks.retain(|b| b.block_start >= now);

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

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for Winter Adaptive V3 strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV3Config {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Priority for conflict resolution (0-100, higher wins)
    pub priority: u8,

    /// Target battery SOC for charging (default: 90%)
    pub daily_charging_target_soc: f32,

    /// Home Assistant entity for HDO tariff schedule
    pub hdo_sensor_entity: String,

    /// Grid fee during HDO low tariff periods (CZK/kWh)
    pub hdo_low_tariff_czk: f32,

    /// Grid fee during HDO high tariff periods (CZK/kWh)
    pub hdo_high_tariff_czk: f32,

    /// Minimum SOC for winter discharge (default: 50%)
    pub winter_discharge_min_soc: f32,

    /// Number of top expensive blocks per day to allow discharge (default: 4)
    pub top_discharge_blocks_per_day: usize,

    /// Minimum arbitrage buffer above median+high_grid_fee for discharge to be worthwhile
    /// Default: 1.0 CZK (use 0.05 for EUR)
    /// Discharge only allowed if: effective_price > (median_spot + high_grid_fee + buffer)
    pub discharge_arbitrage_buffer: f32,

    /// Minimum consecutive charge blocks (default: 2)
    pub min_consecutive_charge_blocks: usize,

    /// Price tolerance for charge block selection (default: 15%)
    pub charge_price_tolerance_percent: f32,

    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,

    /// Current seasonal mode
    pub seasonal_mode: SeasonalMode,

    /// HDO cache TTL in seconds (default: 3600)
    pub hdo_cache_ttl_secs: u64,
}

impl Default for WinterAdaptiveV3Config {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 100,
            daily_charging_target_soc: 90.0,
            hdo_sensor_entity: "sensor.cez_hdo_lowtariffstart".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
            winter_discharge_min_soc: 50.0,
            top_discharge_blocks_per_day: 4,
            discharge_arbitrage_buffer: 1.0, // 1.0 CZK, use 0.05 for EUR
            min_consecutive_charge_blocks: 2,
            charge_price_tolerance_percent: 15.0,
            negative_price_handling_enabled: true,
            seasonal_mode: SeasonalMode::Winter,
            hdo_cache_ttl_secs: 3600,
        }
    }
}

// ============================================================================
// Simplified Scheduling (adapted from V2)
// ============================================================================

mod scheduling {
    /// Select cheapest consecutive blocks
    pub fn select_cheapest_blocks(
        blocks_with_prices: &[(usize, f32)],
        count_needed: usize,
        min_consecutive: usize,
        price_tolerance_percent: Option<f32>,
    ) -> Vec<usize> {
        if blocks_with_prices.is_empty() || count_needed == 0 {
            return Vec::new();
        }

        let price_map: std::collections::HashMap<usize, f32> =
            blocks_with_prices.iter().copied().collect();

        let mut by_idx = blocks_with_prices.to_vec();
        by_idx.sort_by_key(|(idx, _)| *idx);

        // Find consecutive ranges
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        if !by_idx.is_empty() {
            let mut range_start = by_idx[0].0;
            let mut prev_idx = by_idx[0].0;

            for (curr_idx, _) in by_idx.iter().skip(1) {
                if *curr_idx != prev_idx + 1 {
                    ranges.push((range_start, prev_idx + 1));
                    range_start = *curr_idx;
                }
                prev_idx = *curr_idx;
            }
            ranges.push((range_start, prev_idx + 1));
        }

        let cheapest_price = by_idx
            .iter()
            .map(|(_, p)| *p)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0.0);

        let max_acceptable = if let Some(tolerance_pct) = price_tolerance_percent {
            cheapest_price * (1.0 + tolerance_pct / 100.0)
        } else {
            f32::MAX
        };

        // Find best consecutive window
        let mut best_window: Option<(f32, Vec<usize>)> = None;

        for (range_start, range_end) in &ranges {
            let range_len = range_end - range_start;
            if range_len >= count_needed {
                for window_start in *range_start..=(range_end - count_needed) {
                    let window: Vec<usize> = (window_start..window_start + count_needed).collect();
                    let avg: f32 = window
                        .iter()
                        .map(|idx| price_map.get(idx).copied().unwrap_or(f32::MAX))
                        .sum::<f32>()
                        / count_needed as f32;

                    let blocks_within_tolerance = window
                        .iter()
                        .filter(|idx| {
                            price_map.get(*idx).copied().unwrap_or(f32::MAX) <= max_acceptable
                        })
                        .count();

                    let tolerance_score = blocks_within_tolerance as f32 / count_needed as f32;

                    if let Some((best_avg, _)) = &best_window {
                        let current_tolerance_score = best_window
                            .as_ref()
                            .map(|(_, w)| {
                                w.iter()
                                    .filter(|idx| {
                                        price_map.get(*idx).copied().unwrap_or(f32::MAX)
                                            <= max_acceptable
                                    })
                                    .count() as f32
                                    / count_needed as f32
                            })
                            .unwrap_or(0.0);

                        if tolerance_score > current_tolerance_score
                            || (tolerance_score == current_tolerance_score && avg < *best_avg)
                        {
                            best_window = Some((avg, window));
                        }
                    } else {
                        best_window = Some((avg, window));
                    }
                }
            }
        }

        if let Some((_, window)) = best_window {
            return window;
        }

        // Fallback: combine smaller windows
        let mut all_windows: Vec<(f32, Vec<usize>)> = Vec::new();
        for (range_start, range_end) in &ranges {
            let range_len = range_end - range_start;
            if range_len >= min_consecutive {
                for window_start in *range_start..=(range_end - min_consecutive) {
                    let window: Vec<usize> =
                        (window_start..window_start + min_consecutive).collect();
                    let avg: f32 = window
                        .iter()
                        .map(|idx| price_map.get(idx).copied().unwrap_or(f32::MAX))
                        .sum::<f32>()
                        / min_consecutive as f32;
                    all_windows.push((avg, window));
                }
            }
        }

        all_windows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut selected: Vec<usize> = Vec::new();
        for (_, window) in &all_windows {
            if selected.len() >= count_needed {
                break;
            }
            if window.iter().any(|idx| selected.contains(idx)) {
                continue;
            }
            for &idx in window {
                if selected.len() < count_needed {
                    selected.push(idx);
                }
            }
        }

        selected.sort();
        selected
    }
}

// ============================================================================
// Main Strategy Implementation
// ============================================================================

#[derive(Debug)]
pub struct WinterAdaptiveV3Strategy {
    config: WinterAdaptiveV3Config,
    hdo_cache: HdoCache,
    lock_state: RwLock<ScheduleLockState>,
}

impl WinterAdaptiveV3Strategy {
    pub fn new(config: WinterAdaptiveV3Config) -> Self {
        let hdo_cache = HdoCache::new(config.hdo_cache_ttl_secs);
        Self {
            config,
            hdo_cache,
            lock_state: RwLock::new(ScheduleLockState::default()),
        }
    }

    /// Update HDO cache with new sensor data
    pub fn update_hdo_cache(&self, raw_data: &str) {
        let schedules = parse_hdo_sensor_data(raw_data);
        if !schedules.is_empty() {
            let count = schedules.len();
            self.hdo_cache.update(schedules);
            tracing::debug!("V3: Updated HDO cache with {} day schedules", count);
        }
    }

    /// Calculate effective price = spot_price + grid_fee
    fn calculate_effective_price(&self, spot_price: f32, block_time: DateTime<Utc>) -> f32 {
        let grid_fee = match self.hdo_cache.is_low_tariff(block_time) {
            Some(true) => self.config.hdo_low_tariff_czk,
            Some(false) | None => self.config.hdo_high_tariff_czk, // Default to high if unknown
        };
        spot_price + grid_fee
    }

    /// Check if discharge is allowed based on SOC, block ranking, and arbitrage efficiency
    fn is_discharge_allowed(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
    ) -> (bool, String) {
        // Summer mode: no restrictions
        if self.config.seasonal_mode == SeasonalMode::Summer {
            return (true, "Summer mode - no restrictions".to_string());
        }

        // Winter: Check SOC >= min threshold
        if context.current_battery_soc < self.config.winter_discharge_min_soc {
            return (
                false,
                format!(
                    "SOC {:.1}% < {:.1}% minimum",
                    context.current_battery_soc, self.config.winter_discharge_min_soc
                ),
            );
        }

        // Winter: Check if in top N expensive blocks TODAY
        let current_block = &all_blocks[current_idx];
        let today = current_block.block_start.date_naive();

        // Filter blocks for today and calculate effective prices
        let mut today_blocks: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.block_start.date_naive() == today)
            .map(|(idx, b)| {
                let effective_price =
                    self.calculate_effective_price(b.price_czk_per_kwh, b.block_start);
                (idx, effective_price)
            })
            .collect();

        // Sort by effective price descending (most expensive first)
        today_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Get top N indices
        let top_n: Vec<usize> = today_blocks
            .iter()
            .take(self.config.top_discharge_blocks_per_day)
            .map(|(idx, _)| *idx)
            .collect();

        let current_effective_price = self
            .calculate_effective_price(current_block.price_czk_per_kwh, current_block.block_start);

        // Check if in top N expensive blocks
        if !top_n.contains(&current_idx) {
            return (
                false,
                format!(
                    "Not in top {} expensive blocks, eff price {:.3} CZK",
                    self.config.top_discharge_blocks_per_day, current_effective_price
                ),
            );
        }

        // =====================================================================
        // NEW: Arbitrage efficiency check
        // Only discharge if: effective_price > (median_spot + high_grid_fee + buffer)
        // This ensures discharge is worthwhile vs. just using grid directly
        // =====================================================================
        let today_spot_prices: Vec<f32> = all_blocks
            .iter()
            .filter(|b| b.block_start.date_naive() == today)
            .map(|b| b.price_czk_per_kwh)
            .collect();

        let median_spot = Self::calculate_median(&today_spot_prices);
        let arbitrage_threshold =
            median_spot + self.config.hdo_high_tariff_czk + self.config.discharge_arbitrage_buffer;

        if current_effective_price <= arbitrage_threshold {
            return (
                false,
                format!(
                    "Arbitrage not efficient: eff {:.3} <= threshold {:.3} (median {:.3} + high fee {:.3} + buffer {:.3})",
                    current_effective_price,
                    arbitrage_threshold,
                    median_spot,
                    self.config.hdo_high_tariff_czk,
                    self.config.discharge_arbitrage_buffer
                ),
            );
        }

        let rank = top_n
            .iter()
            .position(|&idx| idx == current_idx)
            .unwrap_or(0)
            + 1;
        (
            true,
            format!(
                "Top {} block (#{} of {}), eff {:.3} > threshold {:.3} CZK",
                self.config.top_discharge_blocks_per_day,
                rank,
                today_blocks.len(),
                current_effective_price,
                arbitrage_threshold
            ),
        )
    }

    /// Calculate median of a slice of f32 values
    fn calculate_median(values: &[f32]) -> f32 {
        if values.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let len = sorted.len();
        if len.is_multiple_of(2) {
            (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
        } else {
            sorted[len / 2]
        }
    }

    /// Select charge blocks based on effective prices
    fn select_charge_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
        blocks_needed: usize,
    ) -> Vec<usize> {
        // Calculate effective prices for remaining blocks
        let blocks_with_eff_prices: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .skip(current_idx)
            .map(|(idx, b)| {
                let effective_price =
                    self.calculate_effective_price(b.price_czk_per_kwh, b.block_start);
                (idx, effective_price)
            })
            .collect();

        // Calculate median effective price
        let mut prices: Vec<f32> = blocks_with_eff_prices.iter().map(|(_, p)| *p).collect();
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if prices.is_empty() {
            0.0
        } else if prices.len().is_multiple_of(2) {
            let mid = prices.len() / 2;
            (prices[mid - 1] + prices[mid]) / 2.0
        } else {
            prices[prices.len() / 2]
        };

        // Filter to affordable blocks (below median)
        let affordable_blocks: Vec<(usize, f32)> = blocks_with_eff_prices
            .iter()
            .filter(|(_, price)| *price < median)
            .copied()
            .collect();

        tracing::debug!(
            "V3: Median eff price: {:.3} CZK, {} of {} blocks affordable",
            median,
            affordable_blocks.len(),
            blocks_with_eff_prices.len()
        );

        scheduling::select_cheapest_blocks(
            &affordable_blocks,
            blocks_needed,
            self.config.min_consecutive_charge_blocks,
            Some(self.config.charge_price_tolerance_percent),
        )
    }

    /// Main decision logic
    fn decide_mode(
        &self,
        context: &EvaluationContext,
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
    ) -> (InverterOperationMode, String, String) {
        let current_price = context.price_block.price_czk_per_kwh;
        let current_block_start = context.price_block.block_start;
        let effective_price = self.calculate_effective_price(current_price, current_block_start);

        // =====================================================================
        // PRIORITY 0: Check locked blocks
        // =====================================================================
        {
            let lock_state = self.lock_state.read().unwrap();
            if let Some((locked_mode, locked_reason)) =
                lock_state.get_locked_mode(current_block_start)
            {
                tracing::debug!(
                    "V3: Block {} is locked to {:?}",
                    current_block_start,
                    locked_mode
                );
                return (
                    locked_mode,
                    locked_reason,
                    "winter_adaptive_v3:locked_block".to_string(),
                );
            }
        }

        // =====================================================================
        // PRIORITY 1: Negative prices - always charge (free energy!)
        // =====================================================================
        if self.config.negative_price_handling_enabled && current_price < 0.0 {
            if context.current_battery_soc < 100.0 {
                return (
                    InverterOperationMode::ForceCharge,
                    format!("Negative price: {:.3} CZK/kWh", current_price),
                    "winter_adaptive_v3:negative_price_charge".to_string(),
                );
            }
            return (
                InverterOperationMode::SelfUse,
                format!("Negative price, battery full: {:.3} CZK/kWh", current_price),
                "winter_adaptive_v3:negative_price_full".to_string(),
            );
        }

        // =====================================================================
        // STEP 2: Calculate charge blocks needed
        // =====================================================================
        let battery_capacity_kwh = context.control_config.battery_capacity_kwh;
        let charge_per_block_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
        let target_soc = self.config.daily_charging_target_soc;
        let soc_deficit = (target_soc - context.current_battery_soc).max(0.0);
        let energy_needed_kwh = (soc_deficit / 100.0) * battery_capacity_kwh;
        let blocks_for_soc = (energy_needed_kwh / charge_per_block_kwh).ceil() as usize;

        let config_charge_blocks = context.control_config.force_charge_hours * 4;
        let blocks_needed = config_charge_blocks.max(blocks_for_soc);

        tracing::debug!(
            "V3: SOC {:.1}% -> {:.1}% target, need {} blocks (config min: {})",
            context.current_battery_soc,
            target_soc,
            blocks_for_soc,
            config_charge_blocks
        );

        // =====================================================================
        // STEP 3: Select cheapest charge blocks by EFFECTIVE price
        // =====================================================================
        let charge_schedule =
            self.select_charge_blocks(all_blocks, current_block_index, blocks_needed);

        // =====================================================================
        // STEP 4: Check if current block should charge
        // =====================================================================
        let should_charge = charge_schedule.contains(&current_block_index);

        if should_charge && context.current_battery_soc < target_soc {
            // Lock next blocks to prevent oscillation
            self.lock_upcoming_blocks(
                all_blocks,
                current_block_index,
                &charge_schedule,
                context.control_config.min_consecutive_force_blocks,
            );

            return (
                InverterOperationMode::ForceCharge,
                format!(
                    "Scheduled charge: spot {:.3} + grid {:.3} = {:.3} CZK/kWh",
                    current_price,
                    effective_price - current_price,
                    effective_price
                ),
                "winter_adaptive_v3:scheduled_charge".to_string(),
            );
        }

        // =====================================================================
        // STEP 5: Check discharge restriction (Winter mode)
        // =====================================================================
        let (discharge_allowed, discharge_reason) =
            self.is_discharge_allowed(context, all_blocks, current_block_index);

        if discharge_allowed {
            return (
                InverterOperationMode::SelfUse,
                format!("Discharge allowed: {}", discharge_reason),
                "winter_adaptive_v3:discharge_allowed".to_string(),
            );
        }

        // =====================================================================
        // STEP 6: Default - SelfUse with discharge prevention
        // =====================================================================
        (
            InverterOperationMode::SelfUse,
            format!(
                "Hold: {} (eff {:.3} CZK)",
                discharge_reason, effective_price
            ),
            "winter_adaptive_v3:hold".to_string(),
        )
    }

    /// Lock upcoming blocks to prevent oscillation
    fn lock_upcoming_blocks(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_idx: usize,
        charge_schedule: &[usize],
        min_consecutive: usize,
    ) {
        let now = Utc::now();
        let current_block_start = all_blocks[current_idx].block_start;
        let block_age_seconds = (now - current_block_start).num_seconds();

        // Only lock if evaluating current block
        let is_current_block = (0..900).contains(&block_age_seconds);
        if !is_current_block {
            return;
        }

        let mut lock_state = self.lock_state.write().unwrap();
        let mut blocks_to_lock = Vec::new();

        for i in 0..min_consecutive {
            let block_idx = current_idx + i;
            if block_idx < all_blocks.len() {
                let block = &all_blocks[block_idx];
                let block_mode = if charge_schedule.contains(&block_idx) {
                    InverterOperationMode::ForceCharge
                } else {
                    InverterOperationMode::SelfUse
                };
                let block_reason = if charge_schedule.contains(&block_idx) {
                    format!("Scheduled charge: {:.3} CZK", block.price_czk_per_kwh)
                } else {
                    format!("Hold: {:.3} CZK", block.price_czk_per_kwh)
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
            "V3: Locked {} blocks starting at {}",
            blocks_locked,
            current_block_start
        );
    }
}

impl EconomicStrategy for WinterAdaptiveV3Strategy {
    fn name(&self) -> &str {
        "Winter-Adaptive-V3"
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
                // Use effective price for cost calculation
                let effective_price = self.calculate_effective_price(
                    context.price_block.price_czk_per_kwh,
                    context.price_block.block_start,
                );
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

                let effective_price = self.calculate_effective_price(
                    context.price_block.price_czk_per_kwh,
                    context.price_block.block_start,
                );

                if usable_battery_kwh >= context.consumption_forecast_kwh {
                    eval.revenue_czk = context.consumption_forecast_kwh * effective_price;
                } else {
                    eval.revenue_czk = usable_battery_kwh * effective_price;
                    eval.cost_czk =
                        (context.consumption_forecast_kwh - usable_battery_kwh) * effective_price;
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

    #[test]
    fn test_parse_czech_date() {
        assert_eq!(
            parse_czech_date("14.01.2026"),
            Some(NaiveDate::from_ymd_opt(2026, 1, 14).unwrap())
        );
        assert_eq!(
            parse_czech_date("31.12.2025"),
            Some(NaiveDate::from_ymd_opt(2025, 12, 31).unwrap())
        );
        assert_eq!(parse_czech_date("invalid"), None);
        assert_eq!(parse_czech_date("14-01-2026"), None);
    }

    #[test]
    fn test_parse_time_ranges() {
        let ranges = parse_time_ranges("00:00-06:00; 07:00-09:00; 10:00-13:00");
        assert_eq!(ranges.len(), 3);

        assert_eq!(ranges[0].start, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        assert_eq!(ranges[0].end, NaiveTime::from_hms_opt(6, 0, 0).unwrap());

        assert_eq!(ranges[1].start, NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert_eq!(ranges[1].end, NaiveTime::from_hms_opt(9, 0, 0).unwrap());

        assert_eq!(ranges[2].start, NaiveTime::from_hms_opt(10, 0, 0).unwrap());
        assert_eq!(ranges[2].end, NaiveTime::from_hms_opt(13, 0, 0).unwrap());
    }

    #[test]
    fn test_parse_time_ranges_with_24_hour() {
        let ranges = parse_time_ranges("17:00-24:00");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        // 24:00 should be converted to 23:59:59
        assert_eq!(ranges[0].end, NaiveTime::from_hms_opt(23, 59, 59).unwrap());
    }

    #[test]
    fn test_hdo_cache_is_low_tariff() {
        let cache = HdoCache::new(3600);

        let schedule = HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 1, 14).unwrap(),
            low_tariff_ranges: vec![
                HdoTimeRange {
                    start: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
                },
                HdoTimeRange {
                    start: NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(14, 0, 0).unwrap(),
                },
            ],
        };
        cache.update(vec![schedule]);

        // Test within first low tariff range
        let dt1 = Utc.with_ymd_and_hms(2026, 1, 14, 3, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt1), Some(true));

        // Test within second low tariff range
        let dt2 = Utc.with_ymd_and_hms(2026, 1, 14, 12, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt2), Some(true));

        // Test outside low tariff ranges (high tariff)
        let dt3 = Utc.with_ymd_and_hms(2026, 1, 14, 8, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt3), Some(false));

        // Test date not in cache
        let dt4 = Utc.with_ymd_and_hms(2026, 1, 15, 3, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt4), None);
    }

    #[test]
    fn test_parse_hdo_sensor_data() {
        let json_data = r#"{
            "data": {
                "signals": [
                    {
                        "signal": "EVV1",
                        "datum": "14.01.2026",
                        "casy": "00:00-06:00; 07:00-09:00"
                    }
                ]
            }
        }"#;

        let schedules = parse_hdo_sensor_data(json_data);
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 14).unwrap()
        );
        assert_eq!(schedules[0].low_tariff_ranges.len(), 2);
    }

    #[test]
    fn test_seasonal_mode_from_date() {
        // Winter months
        let winter_date = Utc.with_ymd_and_hms(2026, 1, 14, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(winter_date), SeasonalMode::Winter);

        let winter_date2 = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(winter_date2), SeasonalMode::Winter);

        // Summer months
        let summer_date = Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(summer_date), SeasonalMode::Summer);

        let summer_date2 = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        assert_eq!(SeasonalMode::from_date(summer_date2), SeasonalMode::Summer);
    }

    #[test]
    fn test_calculate_median() {
        // Odd number of elements
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[1.0, 3.0, 2.0]),
            2.0
        );
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[5.0, 1.0, 9.0, 3.0, 7.0]),
            5.0
        );

        // Even number of elements
        assert_eq!(
            WinterAdaptiveV3Strategy::calculate_median(&[1.0, 2.0, 3.0, 4.0]),
            2.5
        );
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[1.0, 2.0]), 1.5);

        // Empty
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[]), 0.0);

        // Single element
        assert_eq!(WinterAdaptiveV3Strategy::calculate_median(&[42.0]), 42.0);
    }

    #[test]
    fn test_arbitrage_threshold_calculation() {
        // Given:
        // - median_spot = 2.0 CZK/kWh
        // - high_grid_fee = 1.80 CZK/kWh
        // - buffer = 1.0 CZK
        // Threshold = 2.0 + 1.80 + 1.0 = 4.80 CZK/kWh
        //
        // For discharge to be efficient:
        // - effective_price must be > 4.80

        let config = WinterAdaptiveV3Config {
            enabled: true,
            hdo_high_tariff_czk: 1.80,
            discharge_arbitrage_buffer: 1.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveV3Strategy::new(config);

        // Test median calculation for threshold
        let spot_prices = vec![1.0, 2.0, 3.0]; // median = 2.0
        let median = WinterAdaptiveV3Strategy::calculate_median(&spot_prices);
        assert_eq!(median, 2.0);

        let threshold = median
            + strategy.config.hdo_high_tariff_czk
            + strategy.config.discharge_arbitrage_buffer;
        assert!((threshold - 4.80).abs() < 0.001);

        // Effective price of 5.0 should pass (5.0 > 4.80)
        // Effective price of 4.80 should fail (4.80 <= 4.80)
        // Effective price of 4.0 should fail (4.0 <= 4.80)
    }
}
