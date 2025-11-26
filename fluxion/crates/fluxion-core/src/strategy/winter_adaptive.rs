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

use crate::components::{InverterOperationMode, TimeBlockPrice};
use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, SeasonalMode, economics,
};
use crate::utils::calculate_ema;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Historical day data for seasonal mode detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayEnergyBalance {
    /// Date of this record
    pub date: DateTime<Utc>,
    /// Total solar production (kWh)
    pub solar_production_kwh: f32,
    /// Total grid import (kWh)
    pub grid_import_kwh: f32,
}

impl DayEnergyBalance {
    /// Calculate energy deficit ratio
    /// Returns positive value for deficit (more import than solar)
    /// Returns negative value for surplus (more solar than import)
    pub fn deficit_ratio(&self) -> f32 {
        if self.grid_import_kwh == 0.0 {
            return -1.0; // Full surplus
        }
        (self.grid_import_kwh - self.solar_production_kwh) / self.grid_import_kwh
    }

    /// Check if this day has at least 20% deficit (winter condition)
    pub fn is_deficit_day(&self) -> bool {
        self.deficit_ratio() >= 0.20
    }

    /// Check if this day has at least 20% surplus (summer condition)
    pub fn is_surplus_day(&self) -> bool {
        self.deficit_ratio() <= -0.20
    }
}

/// Configuration for winter adaptive strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveConfig {
    /// Enable/disable the strategy
    pub enabled: bool,

    /// Number of days to track for EMA calculation
    pub ema_period_days: usize,

    /// Minimum solar production percentage to consider (winter ignores solar if < 10%)
    pub min_solar_percentage: f32,

    /// Target battery SOC (%)
    pub target_battery_soc: f32,

    /// Critical battery SOC threshold for protection (%)
    pub critical_battery_soc: f32,

    /// Number of most expensive blocks to target for discharge
    pub top_expensive_blocks: usize,

    /// Number of days to track for seasonal mode detection
    pub seasonal_history_days: usize,

    /// Historical consumption data (last N days of total consumption in kWh)
    #[serde(skip)]
    pub consumption_history_kwh: VecDeque<f32>,

    /// Historical daily energy balance for seasonal mode detection
    #[serde(skip)]
    pub energy_balance_history: VecDeque<DayEnergyBalance>,

    /// Current seasonal mode
    pub seasonal_mode: SeasonalMode,

    // Plan 1: Tomorrow preservation
    /// Threshold for tomorrow vs today peak comparison (default: 1.2)
    pub tomorrow_preservation_threshold: f32,

    // Plan 2: Grid export on spikes
    /// Price threshold for export consideration (default: 8.0 CZK/kWh)
    pub grid_export_price_threshold: f32,
    /// Minimum SOC to keep for self-use during export (default: 50.0%)
    pub min_soc_for_export: f32,
    /// Multiplier over average to trigger export (default: 2.5)
    pub export_trigger_multiplier: f32,

    // Plan 3: Negative prices
    /// Enable negative price handling (default: true)
    pub negative_price_handling_enabled: bool,
    /// Charge even when full on negative prices (default: false)
    pub charge_on_negative_even_if_full: bool,
}

impl Default for WinterAdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ema_period_days: 7,
            min_solar_percentage: 0.10,
            target_battery_soc: 90.0,
            critical_battery_soc: 40.0,
            top_expensive_blocks: 12,
            seasonal_history_days: 3,
            consumption_history_kwh: VecDeque::new(),
            energy_balance_history: VecDeque::new(),
            seasonal_mode: SeasonalMode::Winter,
            tomorrow_preservation_threshold: 1.2,
            grid_export_price_threshold: 8.0,
            min_soc_for_export: 50.0,
            export_trigger_multiplier: 2.5,
            negative_price_handling_enabled: true,
            charge_on_negative_even_if_full: false,
        }
    }
}

impl WinterAdaptiveConfig {
    /// Update seasonal mode based on historical energy balance
    /// Returns true if mode changed
    pub fn update_seasonal_mode(&mut self, now: DateTime<Utc>) -> bool {
        // Seasonal mode switching rules:
        // - After Sept 1: switch to Winter if 3 consecutive days with 20% deficit
        // - After Feb 1: switch to Summer if 3 consecutive days with 20% surplus

        if self.energy_balance_history.len() < self.seasonal_history_days {
            return false; // Not enough data yet
        }

        let month = now.month();
        let last_3_days: Vec<_> = self
            .energy_balance_history
            .iter()
            .rev()
            .take(self.seasonal_history_days)
            .collect();

        // Check for winter mode switch (after September 1)
        if month >= 9 || month <= 2 {
            // September to February
            let all_deficit = last_3_days.iter().all(|day| day.is_deficit_day());
            if all_deficit && self.seasonal_mode != SeasonalMode::Winter {
                tracing::info!(
                    "Switching to Winter mode: 3 consecutive days with 20%+ deficit detected"
                );
                self.seasonal_mode = SeasonalMode::Winter;
                return true;
            }
        }

        // Check for summer mode switch (after February 1)
        if (2..=9).contains(&month) {
            // February to September
            let all_surplus = last_3_days.iter().all(|day| day.is_surplus_day());
            if all_surplus && self.seasonal_mode != SeasonalMode::Summer {
                tracing::info!(
                    "Switching to Summer mode: 3 consecutive days with 20%+ surplus detected"
                );
                self.seasonal_mode = SeasonalMode::Summer;
                return true;
            }
        }

        false
    }

    /// Add a new day's energy balance to history
    pub fn add_energy_balance(&mut self, balance: DayEnergyBalance) {
        self.energy_balance_history.push_back(balance);

        // Keep only the necessary history
        while self.energy_balance_history.len() > self.seasonal_history_days {
            self.energy_balance_history.pop_front();
        }
    }

    /// Add a new day's consumption to history
    pub fn add_consumption(&mut self, consumption_kwh: f32) {
        self.consumption_history_kwh.push_back(consumption_kwh);

        // Keep only the necessary history
        while self.consumption_history_kwh.len() > self.ema_period_days {
            self.consumption_history_kwh.pop_front();
        }
    }

    /// Get predicted daily consumption based on EMA
    pub fn predict_daily_consumption(&self) -> Option<f32> {
        if self.consumption_history_kwh.is_empty() {
            return None;
        }

        let history: Vec<f32> = self.consumption_history_kwh.iter().copied().collect();
        calculate_ema(&history, self.ema_period_days)
    }
}

/// Price analysis results for different time horizons
#[derive(Debug, Clone)]
pub struct PriceHorizonAnalysis {
    /// Average price for the next 8 hours
    pub avg_8h_price: f32,
    /// Average price for all available data
    pub avg_all_price: f32,
    /// Indices of cheapest blocks for charging (global horizon)
    pub cheapest_blocks: Vec<usize>,
    /// Indices of cheapest blocks for charging TODAY only
    pub cheapest_blocks_today: Vec<usize>,
    /// Indices of top N most expensive blocks for today
    pub expensive_blocks_today: Vec<usize>,
    /// Is current block in cheap zone (below average)
    pub is_cheap_block: bool,
    /// Is current block in expensive zone (top N today)
    pub is_expensive_block: bool,

    /// Average of top expensive blocks TODAY
    pub today_peak_avg: f32,
    /// Average of top expensive blocks TOMORROW (if data available)
    pub tomorrow_peak_avg: Option<f32>,
    /// True if tomorrow's peak is significantly higher than today's
    pub should_preserve_for_tomorrow: bool,

    /// True if current price is extreme enough for grid export
    pub is_export_opportunity: bool,

    /// True if current price is negative (we're paid to consume)
    pub is_negative_price: bool,

    /// Percentile rank of current block's price among remaining TODAY blocks (0.0 = cheapest, 1.0 = most expensive)
    /// Used for tiered SOC-based discharge decisions
    pub current_price_percentile_today: f32,

    /// Number of remaining blocks today (for logging/debugging)
    pub remaining_blocks_today: usize,
}

/// Winter Adaptive Strategy
///
/// A comprehensive strategy optimized for winter conditions with low solar production.
/// Features:
/// - Automatic seasonal mode detection (winter/summer)
/// - EMA-based consumption forecasting
/// - Multi-horizon price analysis (8h, full horizon)
/// - Intelligent battery charge planning
/// - Mode switching based on price analysis and battery state
/// - Battery protection when SOC drops below 40% before peak hours
#[derive(Debug, Clone)]
pub struct WinterAdaptiveStrategy {
    config: WinterAdaptiveConfig,
}

impl WinterAdaptiveStrategy {
    /// Create a new Winter Adaptive strategy
    pub fn new(config: WinterAdaptiveConfig) -> Self {
        Self { config }
    }

    /// Analyze prices across different time horizons
    fn analyze_prices(
        &self,
        all_blocks: &[TimeBlockPrice],
        current_block_start: DateTime<Utc>,
        current_block_index: usize,
    ) -> PriceHorizonAnalysis {
        let now_date = current_block_start.date_naive();

        // Find all upcoming blocks
        // NOTE: We use LOCAL indices (0, 1, 2...) relative to all_blocks slice passed to us,
        // NOT original indices from the full price data. This is important because the scheduler
        // passes us only remaining blocks starting from the current block.
        let upcoming_blocks: Vec<(usize, &TimeBlockPrice)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.block_start >= current_block_start)
            .collect();

        // Calculate 8-hour average
        let blocks_8h: Vec<f32> = upcoming_blocks
            .iter()
            .take(32) // 8 hours = 32 blocks (15-min blocks)
            .map(|(_, b)| b.price_czk_per_kwh)
            .collect();
        let avg_8h_price = if blocks_8h.is_empty() {
            0.0
        } else {
            blocks_8h.iter().sum::<f32>() / blocks_8h.len() as f32
        };

        // Calculate average for all upcoming blocks
        let all_prices: Vec<f32> = upcoming_blocks
            .iter()
            .map(|(_, b)| b.price_czk_per_kwh)
            .collect();
        let avg_all_price = if all_prices.is_empty() {
            0.0
        } else {
            all_prices.iter().sum::<f32>() / all_prices.len() as f32
        };

        let current_price = all_blocks
            .get(current_block_index)
            .map(|b| b.price_czk_per_kwh)
            .unwrap_or(avg_all_price);

        // Find top N most expensive blocks for TODAY only
        // Use local indices from upcoming_blocks to ensure consistency
        let mut today_blocks: Vec<(usize, f32)> = upcoming_blocks
            .iter()
            .filter(|(_, b)| b.block_start.date_naive() == now_date)
            .map(|(idx, b)| (*idx, b.price_czk_per_kwh))
            .collect();

        today_blocks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let expensive_blocks_today: Vec<usize> = today_blocks
            .iter()
            .take(self.config.top_expensive_blocks)
            .map(|(idx, _)| *idx)
            .collect();

        let today_peak_avg = if !today_blocks.is_empty() {
            let count = today_blocks.len().min(self.config.top_expensive_blocks);
            today_blocks.iter().take(count).map(|(_, p)| p).sum::<f32>() / count as f32
        } else {
            0.0
        };

        // Analyze Tomorrow's prices (Plan 1)
        let tomorrow_date = now_date.succ_opt().unwrap_or(now_date);
        let mut tomorrow_blocks: Vec<f32> = upcoming_blocks
            .iter()
            .filter(|(_, b)| b.block_start.date_naive() == tomorrow_date)
            .map(|(_, b)| b.price_czk_per_kwh)
            .collect();

        tomorrow_blocks.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

        let tomorrow_peak_avg = if !tomorrow_blocks.is_empty() {
            let count = tomorrow_blocks.len().min(self.config.top_expensive_blocks);
            Some(tomorrow_blocks.iter().take(count).sum::<f32>() / count as f32)
        } else {
            None
        };

        let should_preserve_for_tomorrow = if let Some(tomorrow_avg) = tomorrow_peak_avg {
            tomorrow_avg > today_peak_avg * self.config.tomorrow_preservation_threshold
        } else {
            false
        };

        // Check for export opportunity (Plan 2)
        let is_export_opportunity = current_price > self.config.grid_export_price_threshold
            && current_price > avg_all_price * self.config.export_trigger_multiplier;

        // Check for negative price (Plan 3)
        let is_negative_price = current_price < 0.0;

        // Find cheapest blocks for charging (from all upcoming blocks)
        let mut price_indexed: Vec<(usize, f32)> = upcoming_blocks
            .iter()
            .map(|(idx, b)| (*idx, b.price_czk_per_kwh))
            .collect();
        price_indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Global cheapest blocks (up to 96 to allow full filling if needed, filtered later by count)
        let cheapest_blocks: Vec<usize> = price_indexed.iter().map(|(idx, _)| *idx).collect();

        // Find cheapest blocks TODAY
        let mut price_indexed_today: Vec<(usize, f32)> = upcoming_blocks
            .iter()
            .filter(|(_, b)| b.block_start.date_naive() == now_date)
            .map(|(idx, b)| (*idx, b.price_czk_per_kwh))
            .collect();
        price_indexed_today
            .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let cheapest_blocks_today: Vec<usize> =
            price_indexed_today.iter().map(|(idx, _)| *idx).collect();

        let current_price = all_blocks
            .get(current_block_index)
            .map(|b| b.price_czk_per_kwh)
            .unwrap_or(avg_all_price);

        // Calculate percentile rank of current price among remaining TODAY blocks
        // 0.0 = cheapest, 1.0 = most expensive
        // today_blocks is already sorted DESC by price from earlier
        let remaining_blocks_today = today_blocks.len();
        let current_price_percentile_today = if remaining_blocks_today <= 1 {
            // Only one block or no blocks - treat as most expensive (discharge allowed)
            1.0
        } else {
            // Count how many blocks have lower price than current
            let cheaper_count = today_blocks
                .iter()
                .filter(|(_, price)| *price < current_price)
                .count();
            // Percentile = position from bottom / (total - 1)
            // e.g., if 3 blocks are cheaper out of 10, percentile = 3/9 ≈ 0.33 (33rd percentile)
            // Higher percentile = more expensive = higher priority for discharge
            cheaper_count as f32 / (remaining_blocks_today - 1) as f32
        };

        // A block is "expensive" only if:
        // 1. It's among the top N expensive blocks today, AND
        // 2. Its price is actually above average (not just "least cheap" when all blocks are cheap)
        let is_expensive_block =
            expensive_blocks_today.contains(&current_block_index) && current_price >= avg_all_price;

        PriceHorizonAnalysis {
            avg_8h_price,
            avg_all_price,
            cheapest_blocks,
            cheapest_blocks_today,
            expensive_blocks_today: expensive_blocks_today.clone(),
            is_cheap_block: current_price < avg_all_price,
            is_expensive_block,
            today_peak_avg,
            tomorrow_peak_avg,
            should_preserve_for_tomorrow,
            is_export_opportunity,
            is_negative_price,
            current_price_percentile_today,
            remaining_blocks_today,
        }
    }

    /// Calculate energy deficit and required charging
    /// Returns (urgent_blocks_today, total_blocks_needed)
    fn calculate_energy_requirements(
        &self,
        context: &EvaluationContext,
        predicted_consumption_kwh: f32,
        today_consumed_so_far_kwh: f32,
        remaining_solar_today_kwh: f32,
        tomorrow_solar_estimate_kwh: f32,
    ) -> (usize, usize) {
        // Calculate remaining consumption for today
        let remaining_consumption_today =
            (predicted_consumption_kwh - today_consumed_so_far_kwh).max(0.0);

        // Estimate tomorrow's consumption (same as predicted daily)
        let tomorrow_consumption = predicted_consumption_kwh;

        // Calculate solar contribution
        // In winter, only count solar if it's > 10% of consumption
        let solar_contribution_today = if remaining_solar_today_kwh
            > remaining_consumption_today * self.config.min_solar_percentage
        {
            remaining_solar_today_kwh
        } else {
            0.0
        };

        let solar_contribution_tomorrow = if tomorrow_solar_estimate_kwh
            > tomorrow_consumption * self.config.min_solar_percentage
        {
            tomorrow_solar_estimate_kwh
        } else {
            0.0
        };

        // Calculate current battery energy available
        // Usable capacity: battery capacity * (current SOC - min SOC) / 100
        // Consider min SOC as 10% (empty)
        let battery_energy_available = context.control_config.battery_capacity_kwh
            * (context.current_battery_soc - 10.0)
            / 100.0;
        let battery_energy_available = battery_energy_available.max(0.0);

        // 1. Urgent Needs (Today)
        // If we don't have enough battery to cover TODAY's consumption, we MUST charge today.
        let energy_needed_today_urgent =
            (remaining_consumption_today - solar_contribution_today - battery_energy_available)
                .max(0.0);

        // 2. Total Horizon Needs (Today + Tomorrow)
        // Do we have enough for both days?
        let total_consumption = remaining_consumption_today + tomorrow_consumption;
        let total_solar = solar_contribution_today + solar_contribution_tomorrow;

        let total_deficit = (total_consumption - total_solar - battery_energy_available).max(0.0);

        // Also consider target SOC (we want to end up with some charge)
        let energy_to_target_soc = context.control_config.battery_capacity_kwh
            * (self.config.target_battery_soc - context.current_battery_soc)
            / 100.0;
        let energy_to_target_soc = energy_to_target_soc.max(0.0);

        // Total charge needed is the max of deficit or target
        let mut total_charge_needed = total_deficit.max(energy_to_target_soc);

        // To avoid under-charging (which is more damaging than a slight
        // over-charge, since max SOC is enforced elsewhere), apply a
        // small safety multiplier. This biases the strategy to plan
        // slightly more charge blocks than the bare minimum estimate.
        const CHARGE_SAFETY_MULTIPLIER: f32 = 1.3; // 30% safety margin
        total_charge_needed *= CHARGE_SAFETY_MULTIPLIER;

        // Calculate number of charging blocks needed
        let charge_per_block = context.control_config.max_battery_charge_rate_kw * 0.25; // 15 minutes

        if charge_per_block <= 0.0 {
            return (0, 0);
        }

        let urgent_blocks_today = (energy_needed_today_urgent / charge_per_block).ceil() as usize;
        let total_blocks_needed = (total_charge_needed / charge_per_block).ceil() as usize;

        (urgent_blocks_today, total_blocks_needed)
    }

    /// Determine operation mode for current block
    fn determine_mode(
        &self,
        context: &EvaluationContext,
        analysis: &PriceHorizonAnalysis,
        current_block_index: usize,
        urgent_blocks_today: usize,
        total_blocks_needed: usize,
    ) -> (InverterOperationMode, String) {
        // Priority 0: Export to grid during extreme price spikes (Plan 2)
        if analysis.is_export_opportunity
            && context.current_battery_soc > self.config.min_soc_for_export
        {
            let exportable_soc = context.current_battery_soc - self.config.min_soc_for_export;
            return (
                InverterOperationMode::ForceDischarge,
                format!(
                    "Grid export: {:.2} CZK/kWh (>{:.1}x avg), exportable: {:.1}%",
                    context.price_block.price_czk_per_kwh,
                    self.config.export_trigger_multiplier,
                    exportable_soc
                ),
            );
        }

        // Priority 0.5: Negative price - charge aggressively (Plan 3)
        if self.config.negative_price_handling_enabled && analysis.is_negative_price {
            if context.current_battery_soc < self.config.target_battery_soc
                || (self.config.charge_on_negative_even_if_full
                    && context.current_battery_soc < 100.0)
            {
                return (
                    InverterOperationMode::ForceCharge,
                    format!(
                        "Negative price charging: {:.3} CZK/kWh (PAID to consume!)",
                        context.price_block.price_czk_per_kwh
                    ),
                );
            }
            // Battery full - at minimum don't export (we'd pay to export)
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "Negative price - avoiding export: {:.3} CZK/kWh",
                    context.price_block.price_czk_per_kwh
                ),
            );
        }

        // Priority 1: Force Charge
        let mut should_charge = false;
        let mut charge_reason = String::new();

        // Check 1: Urgent needs today
        if urgent_blocks_today > 0 {
            // Take top N cheapest blocks TODAY
            let urgent_slots: Vec<usize> = analysis
                .cheapest_blocks_today
                .iter()
                .take(urgent_blocks_today)
                .cloned()
                .collect();

            if urgent_slots.contains(&current_block_index) {
                should_charge = true;
                charge_reason = format!(
                    "Urgent charge for today ({:.3} CZK/kWh)",
                    context.price_block.price_czk_per_kwh
                );
            }
        }

        // Check 2: General needs (target SOC / tomorrow)
        if !should_charge && total_blocks_needed > 0 {
            // Take top N cheapest blocks GLOBALLY (horizon)
            // Note: We might have already used some slots for urgent charge, but that's fine,
            // we just check if current block is in the top N global slots.
            let global_slots: Vec<usize> = analysis
                .cheapest_blocks
                .iter()
                .take(total_blocks_needed)
                .cloned()
                .collect();

            if global_slots.contains(&current_block_index) {
                should_charge = true;
                charge_reason = format!(
                    "Charging for horizon/target ({:.3} CZK/kWh)",
                    context.price_block.price_czk_per_kwh
                );
            }
        }

        if should_charge {
            return (
                InverterOperationMode::ForceCharge,
                format!("{} (avg: {:.3})", charge_reason, analysis.avg_all_price),
            );
        }

        // Priority 2: Tiered SOC-based discharge control
        // As battery depletes, we become more selective about when to use it.
        // This ensures battery power is reserved for the most expensive blocks.
        //
        // Tiers (fixed thresholds):
        //   SOC > 50%:   Discharge allowed on any expensive block (normal operation)
        //   SOC 30-50%:  Discharge only on top 40% most expensive remaining blocks today
        //   SOC 20-30%:  Discharge only on top 20% most expensive remaining blocks today
        //   SOC 10-20%:  Discharge only on top 10% most expensive remaining blocks today
        //   SOC <= min:  No discharge (hardware minimum, typically 10%)

        let soc = context.current_battery_soc;
        let min_soc = context.control_config.min_battery_soc; // Usually 10%
        let current_percentile = analysis.current_price_percentile_today;

        // Determine discharge threshold based on SOC tier
        // threshold = minimum percentile required to allow discharge
        // e.g., 0.60 means only top 40% (percentile >= 0.60) can discharge
        let (discharge_allowed, tier_name, required_percentile) = if soc > 50.0 {
            // High SOC: discharge allowed on expensive blocks
            (analysis.is_expensive_block, "high (>50%)", 0.0)
        } else if soc > 30.0 {
            // Medium SOC (30-50%): only top 40% most expensive
            let required = 0.60; // 60th percentile = top 40%
            (current_percentile >= required, "medium (30-50%)", required)
        } else if soc > 20.0 {
            // Low SOC (20-30%): only top 20% most expensive
            let required = 0.80; // 80th percentile = top 20%
            (current_percentile >= required, "low (20-30%)", required)
        } else if soc > min_soc {
            // Critical SOC (min-20%): only top 10% most expensive
            let required = 0.90; // 90th percentile = top 10%
            (
                current_percentile >= required,
                "critical (10-20%)",
                required,
            )
        } else {
            // Below minimum: never discharge
            (false, "depleted", 1.0)
        };

        // If SOC is 50% or below and discharge is not allowed for this block, use BackUpMode
        if soc <= 50.0 && !discharge_allowed {
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "SOC {:.1}% ({}) - preserving for top {:.0}% expensive (current: {:.0}th pctl, {} blocks left)",
                    soc,
                    tier_name,
                    (1.0 - required_percentile) * 100.0,
                    current_percentile * 100.0,
                    analysis.remaining_blocks_today
                ),
            );
        }

        // Priority 3: Allow discharge when SOC > 50% on expensive blocks, or when tier allows
        if (soc > 50.0 && analysis.is_expensive_block) || (soc <= 50.0 && discharge_allowed) {
            // Check: Reduce aggression if tomorrow is more expensive
            if analysis.should_preserve_for_tomorrow && soc < 70.0 {
                return (
                    InverterOperationMode::BackUpMode,
                    format!(
                        "Preserving battery for tomorrow (today: {:.2}, tomorrow: {:.2} CZK/kWh)",
                        analysis.today_peak_avg,
                        analysis.tomorrow_peak_avg.unwrap_or(0.0)
                    ),
                );
            }

            return (
                InverterOperationMode::SelfUse,
                format!(
                    "Using battery ({:.1}% SOC, {:.0}th pctl, tier: {}) at {:.3} CZK/kWh",
                    soc,
                    current_percentile * 100.0,
                    tier_name,
                    context.price_block.price_czk_per_kwh
                ),
            );
        }

        // Priority 4: Back Up Mode during cheap prices (preserve battery)
        // This applies when SOC > 50% and block is not expensive
        if analysis.is_cheap_block {
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "Preserving battery in cheap block ({:.3} CZK/kWh < avg {:.3})",
                    context.price_block.price_czk_per_kwh, analysis.avg_all_price
                ),
            );
        }

        // Default: Self Use Mode (use battery as needed)
        (
            InverterOperationMode::SelfUse,
            format!(
                "Normal self-use ({:.3} CZK/kWh)",
                context.price_block.price_czk_per_kwh
            ),
        )
    }
}

impl Default for WinterAdaptiveStrategy {
    fn default() -> Self {
        Self::new(WinterAdaptiveConfig::default())
    }
}

impl EconomicStrategy for WinterAdaptiveStrategy {
    fn name(&self) -> &str {
        "Winter-Adaptive"
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

        // Fill in assumptions
        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        // Check if we have price data
        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available for winter adaptive strategy".to_string();
            return eval;
        };

        // Find current block index
        let current_block_idx = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start);

        let Some(current_idx) = current_block_idx else {
            eval.reason = "Could not find current block in price data".to_string();
            return eval;
        };

        // Perform price analysis
        let analysis =
            self.analyze_prices(all_blocks, context.price_block.block_start, current_idx);

        // Get predicted consumption
        // Use average household load as fallback if history is empty/unreliable
        // context.consumption_forecast_kwh is per-block, so it's not a good daily predictor on its own
        let predicted_daily_consumption = self
            .config
            .predict_daily_consumption()
            .unwrap_or(context.control_config.average_household_load_kw * 24.0);

        // Estimate consumption so far today (simplified - in practice read from HA)
        let blocks_so_far_today = context.price_block.block_start.hour() * 4
            + context.price_block.block_start.minute() / 15;
        // Use the same fallback logic for "consumed so far" estimation if we don't have real data
        // (In this simulation context we don't have real accumulated data, so we estimate)
        let estimated_consumption_per_block = predicted_daily_consumption / 96.0;
        let consumption_so_far = estimated_consumption_per_block * blocks_so_far_today as f32;

        // Calculate blocks remaining today for solar estimation
        let blocks_remaining_today = 96 - blocks_so_far_today;
        let estimated_remaining_solar = context.solar_forecast_kwh * blocks_remaining_today as f32;

        // Calculate energy requirements
        let (urgent_blocks, total_blocks) = self.calculate_energy_requirements(
            context,
            predicted_daily_consumption,
            consumption_so_far,
            estimated_remaining_solar,         // Remaining solar today
            context.solar_forecast_kwh * 96.0, // Tomorrow's solar estimate (very rough)
        );

        tracing::debug!(
            "WinterAdaptive: predicted_daily={:.2}kWh, SOC={:.1}%, urgent_blocks={}, total_blocks={}, cheapest_today={:?}",
            predicted_daily_consumption,
            context.current_battery_soc,
            urgent_blocks,
            total_blocks,
            analysis
                .cheapest_blocks_today
                .iter()
                .take(5)
                .collect::<Vec<_>>()
        );

        // Determine operation mode
        let (mode, reason) =
            self.determine_mode(context, &analysis, current_idx, urgent_blocks, total_blocks);

        eval.mode = mode;
        eval.reason = reason;

        // Calculate economics based on mode
        match mode {
            InverterOperationMode::ForceCharge => {
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh =
                    charge_kwh * context.control_config.battery_efficiency;
                eval.energy_flows.grid_import_kwh = charge_kwh + context.consumption_forecast_kwh;

                // Calculate costs
                eval.cost_czk = economics::grid_import_cost(
                    eval.energy_flows.grid_import_kwh,
                    context.price_block.price_czk_per_kwh,
                ) + economics::battery_degradation_cost(
                    charge_kwh,
                    context.control_config.battery_wear_cost_czk_per_kwh,
                );

                // Calculate revenue: value of stored energy for later use
                // Assume we'll use this stored energy during expensive blocks
                // Use the average of today's peak prices as conservative estimate
                let future_use_price = analysis.today_peak_avg.max(analysis.avg_all_price * 1.5);
                eval.revenue_czk = economics::grid_import_cost(
                    eval.energy_flows.battery_charge_kwh,
                    future_use_price,
                );
            }
            InverterOperationMode::SelfUse => {
                // Normal self-use: use battery to cover deficit
                let deficit = context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if deficit > 0.0 {
                    eval.energy_flows.battery_discharge_kwh =
                        deficit.min(context.control_config.max_battery_charge_rate_kw * 0.25);
                    eval.cost_czk = economics::battery_degradation_cost(
                        eval.energy_flows.battery_discharge_kwh,
                        context.control_config.battery_wear_cost_czk_per_kwh,
                    );
                }
            }
            InverterOperationMode::BackUpMode => {
                // Back up mode: import from grid, preserve battery
                let deficit = context.consumption_forecast_kwh - context.solar_forecast_kwh;
                if deficit > 0.0 {
                    eval.energy_flows.grid_import_kwh = deficit;
                    eval.cost_czk =
                        economics::grid_import_cost(deficit, context.price_block.price_czk_per_kwh);
                }
            }
            InverterOperationMode::ForceDischarge => {
                let discharge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                let exportable_kwh = discharge_kwh * context.control_config.battery_efficiency;

                eval.energy_flows.battery_discharge_kwh = discharge_kwh;
                eval.energy_flows.grid_export_kwh =
                    (exportable_kwh - context.consumption_forecast_kwh).max(0.0);

                eval.revenue_czk = economics::grid_export_revenue(
                    eval.energy_flows.grid_export_kwh,
                    context.grid_export_price_czk_per_kwh,
                );
                eval.cost_czk = economics::battery_degradation_cost(
                    discharge_kwh,
                    context.control_config.battery_wear_cost_czk_per_kwh,
                );
            }
        }

        eval.calculate_net_profit();
        eval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_day_energy_balance_deficit() {
        let day = DayEnergyBalance {
            date: Utc::now(),
            solar_production_kwh: 5.0,
            grid_import_kwh: 25.0,
        };

        assert!(day.is_deficit_day());
        assert!(!day.is_surplus_day());
        assert!(day.deficit_ratio() > 0.0);
    }

    #[test]
    fn test_day_energy_balance_surplus() {
        let day = DayEnergyBalance {
            date: Utc::now(),
            solar_production_kwh: 30.0,
            grid_import_kwh: 20.0,
        };

        assert!(!day.is_deficit_day());
        assert!(day.is_surplus_day());
        assert!(day.deficit_ratio() < 0.0);
    }

    #[test]
    fn test_ema_prediction() {
        let mut config = WinterAdaptiveConfig::default();
        config.add_consumption(20.0);
        config.add_consumption(22.0);
        config.add_consumption(21.0);
        config.add_consumption(23.0);
        config.add_consumption(24.0);
        config.add_consumption(22.0);
        config.add_consumption(23.0);

        let prediction = config.predict_daily_consumption();
        assert!(prediction.is_some());
        let value = prediction.unwrap();
        assert!(value > 20.0 && value < 25.0);
    }

    #[test]
    fn test_winter_adaptive_strategy_creation() {
        let strategy = WinterAdaptiveStrategy::default();
        assert_eq!(strategy.name(), "Winter-Adaptive");
        assert!(strategy.is_enabled());
    }

    // Helper to create context for testing
    fn create_test_context<'a>(
        price_block: &'a TimeBlockPrice,
        control_config: &'a crate::resources::ControlConfig,
        all_blocks: Option<&'a [TimeBlockPrice]>,
        soc: f32,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            price_block,
            control_config,
            current_battery_soc: soc,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.5, // Cheap export usually
            all_price_blocks: all_blocks,
        }
    }

    #[test]
    fn test_tomorrow_preservation() {
        let config = WinterAdaptiveConfig {
            top_expensive_blocks: 1,
            tomorrow_preservation_threshold: 1.2, // 20% increase
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig::default();

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let tomorrow = now + chrono::Duration::days(1);

        // Today price 5.0, Tomorrow price 7.0 (40% higher)
        let blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
            },
            TimeBlockPrice {
                block_start: tomorrow,
                duration_minutes: 15,
                price_czk_per_kwh: 7.0,
            },
        ];

        let current_block = &blocks[0];

        let context = create_test_context(current_block, &control_config, Some(&blocks), 60.0);

        // Analyze prices manually to check internal state
        let analysis = strategy.analyze_prices(&blocks, now, 0);
        assert!(analysis.should_preserve_for_tomorrow);
        assert_eq!(analysis.today_peak_avg, 5.0);
        assert_eq!(analysis.tomorrow_peak_avg, Some(7.0));

        // Determine mode
        let (mode, reason) = strategy.determine_mode(&context, &analysis, 0, 0, 0);
        assert_eq!(mode, InverterOperationMode::BackUpMode);
        assert!(reason.contains("Preserving battery"));
    }

    #[test]
    fn test_grid_export_opportunity() {
        let config = WinterAdaptiveConfig {
            grid_export_price_threshold: 8.0,
            export_trigger_multiplier: 2.0,
            min_soc_for_export: 50.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig::default();

        let now = Utc::now();
        // High price 10.0, Avg 3.0
        let blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 10.0,
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 0.0,
            },
        ];

        let current_block = &blocks[0];
        let context = create_test_context(current_block, &control_config, Some(&blocks), 80.0);

        let analysis = strategy.analyze_prices(&blocks, now, 0);
        assert!(analysis.is_export_opportunity);

        let (mode, reason) = strategy.determine_mode(&context, &analysis, 0, 0, 0);
        assert_eq!(mode, InverterOperationMode::ForceDischarge);
        assert!(reason.contains("Grid export"));
    }

    #[test]
    fn test_negative_price_handling() {
        let config = WinterAdaptiveConfig {
            negative_price_handling_enabled: true,
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig::default();

        let now = Utc::now();
        // Negative price
        let blocks = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: -1.0,
        }];

        let current_block = &blocks[0];

        // Case 1: Low SOC -> Force Charge
        let context_low = create_test_context(current_block, &control_config, Some(&blocks), 50.0);
        let analysis = strategy.analyze_prices(&blocks, now, 0);
        assert!(analysis.is_negative_price);

        let (mode, _reason) = strategy.determine_mode(&context_low, &analysis, 0, 0, 0);
        assert_eq!(mode, InverterOperationMode::ForceCharge);

        // Case 2: High SOC -> BackUpMode (avoid export)
        let config_full = WinterAdaptiveConfig {
            negative_price_handling_enabled: true,
            target_battery_soc: 90.0,
            ..Default::default()
        };
        let strategy_full = WinterAdaptiveStrategy::new(config_full);

        let context_full = create_test_context(current_block, &control_config, Some(&blocks), 95.0);
        let (mode, reason) = strategy_full.determine_mode(&context_full, &analysis, 0, 0, 0);
        assert_eq!(mode, InverterOperationMode::BackUpMode);
        assert!(reason.contains("avoiding export"));
    }

    #[test]
    fn test_charging_scheduled_for_cheapest_blocks() {
        // Scenario: Low SOC (30%), need to charge, multiple price blocks.
        // The strategy should select cheapest blocks for charging.
        let config = WinterAdaptiveConfig {
            enabled: true,
            target_battery_soc: 90.0,
            critical_battery_soc: 40.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            battery_capacity_kwh: 20.0,
            max_battery_charge_rate_kw: 10.0,
            average_household_load_kw: 0.5, // 12 kWh/day
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(2, 0, 0)
            .unwrap()
            .and_utc(); // 2 AM

        // Create blocks with varying prices (index 0 = current block)
        // Block 0: 2.0 CZK (cheap)
        // Block 1: 5.0 CZK (expensive)
        // Block 2: 1.5 CZK (cheapest)
        // Block 3: 3.0 CZK (medium)
        let blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 2.0,
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 1.5,
            },
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(45),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
            },
        ];

        // Low SOC - should need charging
        let current_soc = 30.0;

        let current_block = &blocks[0];
        let context =
            create_test_context(current_block, &control_config, Some(&blocks), current_soc);

        // Evaluate the strategy
        let eval = strategy.evaluate(&context);

        // At 2 AM with 30% SOC and 12 kWh/day predicted consumption:
        // - Battery available: (30-10)% * 20 kWh = 4 kWh
        // - Remaining consumption today: ~10 kWh (12 * 22/24)
        // - Deficit: 10 - 4 = 6 kWh minimum to charge
        // - Plus target SOC (90%): (90-30)% * 20 = 12 kWh to charge
        //
        // Current block (index 0, 2.0 CZK) is among the cheapest,
        // so it should be selected for charging.

        assert_eq!(
            eval.mode,
            InverterOperationMode::ForceCharge,
            "Should charge at low SOC on cheap block. Reason: {}",
            eval.reason
        );
    }

    #[test]
    fn test_charging_respects_total_needs() {
        // When SOC is high but we still have horizon needs (tomorrow's consumption),
        // the strategy should still plan charging if we're in a cheap block.
        // This is correct behavior - we're preparing for future consumption.
        let config = WinterAdaptiveConfig {
            enabled: true,
            target_battery_soc: 90.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            battery_capacity_kwh: 20.0,
            max_battery_charge_rate_kw: 10.0,
            average_household_load_kw: 0.5, // 12 kWh/day
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(2, 0, 0)
            .unwrap()
            .and_utc();

        // Single cheap block
        let blocks = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 1.0,
        }];

        // High SOC but still have horizon needs (today + tomorrow = ~24 kWh)
        // Battery available: (95-10)% * 20 = 17 kWh
        // So we still need ~7 kWh to cover both days
        let current_soc = 95.0;

        let current_block = &blocks[0];
        let context =
            create_test_context(current_block, &control_config, Some(&blocks), current_soc);

        let eval = strategy.evaluate(&context);

        // Even at 95% SOC, if we have future consumption needs and this is the cheapest
        // block, it makes economic sense to charge.
        // The strategy correctly identifies horizon needs.
        assert_eq!(
            eval.mode,
            InverterOperationMode::ForceCharge,
            "Should charge in cheapest block even at high SOC when horizon needs exist. Reason: {}",
            eval.reason
        );
    }

    #[test]
    fn test_backup_mode_on_cheap_block_when_no_charge_needed() {
        // When SOC is very high AND we have very low consumption forecast,
        // there should be no charging needed, so we go to BackUpMode.
        let config = WinterAdaptiveConfig {
            enabled: true,
            target_battery_soc: 90.0,
            ..Default::default()
        };

        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            battery_capacity_kwh: 20.0,
            max_battery_charge_rate_kw: 10.0,
            average_household_load_kw: 0.1, // Very low: 2.4 kWh/day
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(23, 0, 0)
            .unwrap()
            .and_utc(); // 11 PM

        // Create blocks with clear cheap/expensive distinction
        // Current block at 1.0 CZK is cheap compared to average of (1+5+4)/3 = 3.33
        // Threshold = 3.33 * 0.5 = 1.67 CZK, so 1.0 is below threshold (cheap)
        let blocks = vec![
            TimeBlockPrice {
                block_start: now,
                duration_minutes: 15,
                price_czk_per_kwh: 1.0,
            }, // cheap
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
            }, // expensive
            TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(30),
                duration_minutes: 15,
                price_czk_per_kwh: 4.0,
            }, // expensive
        ];

        // Very high SOC and low consumption
        // Battery available: (98-10)% * 20 = 17.6 kWh
        // Remaining today: ~0.1 kWh (only 1 hour left)
        // Tomorrow: 2.4 kWh
        // Total: ~2.5 kWh needed, have 17.6 kWh -> no deficit
        let current_soc = 98.0;

        let current_block = &blocks[0];
        let context =
            create_test_context(current_block, &control_config, Some(&blocks), current_soc);

        let eval = strategy.evaluate(&context);

        // At 98% SOC with very low consumption, no charging needed
        // Should be BackUpMode (cheap block, preserve battery)
        assert_eq!(
            eval.mode,
            InverterOperationMode::BackUpMode,
            "Should use BackUpMode on cheap block when no charging needed. Reason: {}",
            eval.reason
        );
    }

    #[test]
    fn test_tiered_discharge_at_40_percent_soc() {
        // At 40% SOC (medium tier 30-50%), should only discharge on top 40% expensive blocks
        let config = WinterAdaptiveConfig::default();
        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            min_battery_soc: 10.0,
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(14, 0, 0)
            .unwrap()
            .and_utc();

        // Create 10 blocks with prices 1-10 CZK
        // Percentile is calculated among REMAINING blocks from current position
        let blocks: Vec<TimeBlockPrice> = (0..10)
            .map(|i| TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: (i + 1) as f32, // 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
            })
            .collect();

        // Test cheap block (3.0 CZK) - should preserve
        // At block 2, remaining blocks are [3,4,5,6,7,8,9,10] (8 blocks)
        // Price 3.0 is the cheapest among remaining -> 0th percentile -> should NOT discharge
        let cheap_block = &blocks[2]; // price = 3.0
        let context_cheap = create_test_context(cheap_block, &control_config, Some(&blocks), 40.0);
        let analysis = strategy.analyze_prices(&blocks, cheap_block.block_start, 2);

        let (mode, reason) = strategy.determine_mode(&context_cheap, &analysis, 2, 0, 0);
        assert_eq!(
            mode,
            InverterOperationMode::BackUpMode,
            "At 40% SOC, cheap block should trigger BackUpMode. Reason: {}",
            reason
        );

        // Test most expensive block among remaining
        // At block 9, only block 10 remains, percentile = 1.0 (100th) -> should discharge
        let expensive_block = &blocks[9]; // price = 10.0
        let context_expensive =
            create_test_context(expensive_block, &control_config, Some(&blocks), 40.0);
        let analysis_exp = strategy.analyze_prices(&blocks, expensive_block.block_start, 9);

        // With only 1 block remaining, percentile = 1.0, so discharge allowed
        let (mode_exp, reason_exp) =
            strategy.determine_mode(&context_expensive, &analysis_exp, 9, 0, 0);
        assert_eq!(
            mode_exp,
            InverterOperationMode::SelfUse,
            "At 40% SOC, most expensive remaining block should allow discharge. Reason: {}",
            reason_exp
        );

        // Test a block in the middle tier: at block 0, remaining = all 10
        // Price 7.0 (block 6) is at 60th percentile (6 cheaper out of 10) -> exactly at threshold
        let mid_block = &blocks[6]; // price = 7.0
        let context_mid = create_test_context(mid_block, &control_config, Some(&blocks), 40.0);
        let analysis_mid = strategy.analyze_prices(&blocks, blocks[0].block_start, 6);

        // Percentile = 6/9 = 0.67, which is >= 0.60, so should discharge
        let (mode_mid, reason_mid) = strategy.determine_mode(&context_mid, &analysis_mid, 6, 0, 0);
        assert_eq!(
            mode_mid,
            InverterOperationMode::SelfUse,
            "At 40% SOC, 67th percentile block should allow discharge. Reason: {}",
            reason_mid
        );
    }

    #[test]
    fn test_tiered_discharge_at_25_percent_soc() {
        // At 25% SOC (low tier 20-30%), should only discharge on top 20% expensive blocks
        let config = WinterAdaptiveConfig::default();
        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            min_battery_soc: 10.0,
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(14, 0, 0)
            .unwrap()
            .and_utc();

        // Create 10 blocks with prices 1-10 CZK
        let blocks: Vec<TimeBlockPrice> = (0..10)
            .map(|i| TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: (i + 1) as f32,
            })
            .collect();

        // Test 70th percentile block - should NOT discharge at 25% SOC (needs 80th+)
        let block_70pctl = &blocks[6]; // price = 7.0 (70th percentile)
        let context_70 = create_test_context(block_70pctl, &control_config, Some(&blocks), 25.0);
        let analysis_70 = strategy.analyze_prices(&blocks, block_70pctl.block_start, 6);

        let (mode_70, reason_70) = strategy.determine_mode(&context_70, &analysis_70, 6, 0, 0);
        assert_eq!(
            mode_70,
            InverterOperationMode::BackUpMode,
            "At 25% SOC, 70th percentile block should trigger BackUpMode. Reason: {}",
            reason_70
        );

        // Test most expensive block (10.0 CZK = ~100th percentile) - should discharge
        let most_expensive = &blocks[9]; // price = 10.0
        let context_top = create_test_context(most_expensive, &control_config, Some(&blocks), 25.0);
        let analysis_top = strategy.analyze_prices(&blocks, most_expensive.block_start, 9);

        let (mode_top, reason_top) = strategy.determine_mode(&context_top, &analysis_top, 9, 0, 0);
        assert_eq!(
            mode_top,
            InverterOperationMode::SelfUse,
            "At 25% SOC, top percentile block should allow discharge. Reason: {}",
            reason_top
        );
    }

    #[test]
    fn test_tiered_discharge_at_15_percent_soc() {
        // At 15% SOC (critical tier 10-20%), should only discharge on top 10% expensive blocks
        let config = WinterAdaptiveConfig::default();
        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            min_battery_soc: 10.0,
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(14, 0, 0)
            .unwrap()
            .and_utc();

        // Create 10 blocks with prices 1-10 CZK
        let blocks: Vec<TimeBlockPrice> = (0..10)
            .map(|i| TimeBlockPrice {
                block_start: now + chrono::Duration::minutes(i * 15),
                duration_minutes: 15,
                price_czk_per_kwh: (i + 1) as f32,
            })
            .collect();

        // Test 85th percentile block - should NOT discharge at 15% SOC (needs 90th+)
        let block_85pctl = &blocks[7]; // price = 8.0 (~78th percentile)
        let context_85 = create_test_context(block_85pctl, &control_config, Some(&blocks), 15.0);
        let analysis_85 = strategy.analyze_prices(&blocks, block_85pctl.block_start, 7);

        let (mode_85, reason_85) = strategy.determine_mode(&context_85, &analysis_85, 7, 0, 0);
        assert_eq!(
            mode_85,
            InverterOperationMode::BackUpMode,
            "At 15% SOC, 85th percentile block should trigger BackUpMode. Reason: {}",
            reason_85
        );

        // Test most expensive block - should discharge even at 15% SOC
        let most_expensive = &blocks[9]; // price = 10.0 (100th percentile)
        let context_top = create_test_context(most_expensive, &control_config, Some(&blocks), 15.0);
        let analysis_top = strategy.analyze_prices(&blocks, most_expensive.block_start, 9);

        let (mode_top, reason_top) = strategy.determine_mode(&context_top, &analysis_top, 9, 0, 0);
        assert_eq!(
            mode_top,
            InverterOperationMode::SelfUse,
            "At 15% SOC, top 10% block should allow discharge. Reason: {}",
            reason_top
        );
    }

    #[test]
    fn test_no_discharge_below_min_soc() {
        // At or below min SOC (10%), should never discharge regardless of price
        let config = WinterAdaptiveConfig::default();
        let strategy = WinterAdaptiveStrategy::new(config);
        let control_config = crate::resources::ControlConfig {
            min_battery_soc: 10.0,
            ..Default::default()
        };

        let now = Utc::now()
            .date_naive()
            .and_hms_opt(14, 0, 0)
            .unwrap()
            .and_utc();

        let blocks = vec![TimeBlockPrice {
            block_start: now,
            duration_minutes: 15,
            price_czk_per_kwh: 10.0,
        }];

        // Even with most expensive block, at 10% SOC should not discharge
        let context = create_test_context(&blocks[0], &control_config, Some(&blocks), 10.0);
        let analysis = strategy.analyze_prices(&blocks, now, 0);

        let (mode, reason) = strategy.determine_mode(&context, &analysis, 0, 0, 0);
        assert_eq!(
            mode,
            InverterOperationMode::BackUpMode,
            "At min SOC, should never discharge even on expensive block. Reason: {}",
            reason
        );
    }
}
