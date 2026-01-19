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

use crate::strategy::{
    Assumptions, BlockEvaluation, EconomicStrategy, EvaluationContext, economics,
};
use crate::utils::calculate_ema;
use chrono::{DateTime, Datelike, Timelike, Utc};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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

    /// Priority for conflict resolution (0-100, higher wins)
    pub priority: u8,

    /// Number of days to track for EMA calculation
    pub ema_period_days: usize,

    /// Minimum solar production percentage to consider (winter ignores solar if < 10%)
    pub min_solar_percentage: f32,

    /// Daily charging target SOC (%) - how full the battery should be charged each day
    pub daily_charging_target_soc: f32,

    /// Conservation threshold SOC (%) - below this level, be more careful about discharging
    pub conservation_threshold_soc: f32,

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

    // Anti-oscillation settings
    /// Minimum consecutive charge blocks to schedule together (default: 2)
    /// This prevents rapid on/off cycling that damages the inverter
    pub min_consecutive_charge_blocks: usize,

    /// Price tolerance for block consolidation (default: 0.30 = 30%)
    /// When selecting charge blocks, prefer consecutive blocks even if
    /// they're up to this percentage more expensive than the absolute cheapest.
    /// Higher tolerance (30%) allows capturing more of the cheap overnight window
    /// even when blocks have small price variations.
    pub charge_block_consolidation_tolerance: f32,

    /// Safety multiplier for charge calculation (default: 1.3)
    /// Increases the calculated charge requirement to avoid under-charging
    pub charge_safety_multiplier: f32,
}

impl Default for WinterAdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 100, // Highest priority by default
            ema_period_days: 7,
            min_solar_percentage: 0.10,
            daily_charging_target_soc: 90.0,
            conservation_threshold_soc: 75.0,
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
            min_consecutive_charge_blocks: 2,
            charge_block_consolidation_tolerance: 0.30, // 30% tolerance for better overnight window capture
            charge_safety_multiplier: 1.3,
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

    /// Consolidate cheapest blocks into consecutive groups to avoid oscillation.
    ///
    /// This function takes a list of blocks sorted by price (cheapest first) and
    /// reorganizes them to prefer consecutive sequences. It will include slightly
    /// more expensive blocks if they help form a consecutive run.
    ///
    /// # Arguments
    /// * `blocks_by_price` - Block indices sorted by price (cheapest first), with their prices
    /// * `count_needed` - Number of blocks to select
    /// * `tolerance` - Price tolerance (e.g., 0.15 = 15% above cheapest is acceptable)
    /// * `min_consecutive` - Minimum consecutive blocks to aim for
    fn consolidate_charge_blocks(
        blocks_by_price: &[(usize, f32)],
        count_needed: usize,
        tolerance: f32,
        min_consecutive: usize,
    ) -> Vec<usize> {
        if blocks_by_price.is_empty() || count_needed == 0 {
            return Vec::new();
        }

        // If only 1-2 blocks needed, just return the cheapest ones
        if count_needed < min_consecutive {
            return blocks_by_price
                .iter()
                .take(count_needed)
                .map(|(idx, _)| *idx)
                .collect();
        }

        // Find the price threshold: cheapest * (1 + tolerance)
        let cheapest_price = blocks_by_price[0].1;
        let price_threshold = if cheapest_price < 0.0 {
            // For negative prices, we want to include all blocks below the threshold
            cheapest_price * (1.0 - tolerance)
        } else {
            cheapest_price * (1.0 + tolerance)
        };

        // Collect all blocks within tolerance
        let eligible_blocks: Vec<(usize, f32)> = blocks_by_price
            .iter()
            .filter(|(_, price)| *price <= price_threshold)
            .cloned()
            .collect();

        // If we don't have enough eligible blocks, fall back to taking cheapest
        if eligible_blocks.len() < count_needed {
            return blocks_by_price
                .iter()
                .take(count_needed)
                .map(|(idx, _)| *idx)
                .collect();
        }

        // Sort eligible blocks by index to find consecutive runs
        let mut eligible_by_idx: Vec<(usize, f32)> = eligible_blocks.clone();
        eligible_by_idx.sort_by_key(|(idx, _)| *idx);

        // Find all consecutive runs within eligible blocks
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

        // Score runs: prefer longer runs at cheaper prices
        // Calculate average price for each run
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

        // Sort by: (1) runs >= min_consecutive first, (2) longer runs, (3) cheaper avg price
        scored_runs.sort_by(|a, b| {
            let a_meets_min = a.0 >= min_consecutive;
            let b_meets_min = b.0 >= min_consecutive;
            if a_meets_min != b_meets_min {
                return b_meets_min.cmp(&a_meets_min); // Prefer runs meeting minimum
            }
            // Among same category, prefer longer runs
            if a.0 != b.0 {
                return b.0.cmp(&a.0); // Longer first
            }
            // Same length: prefer cheaper
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        });

        // Greedily select blocks from runs until we have enough
        let mut selected: Vec<usize> = Vec::new();
        for (_, _, run) in scored_runs {
            if selected.len() >= count_needed {
                break;
            }
            // Add blocks from this run
            for idx in run {
                if selected.len() >= count_needed {
                    break;
                }
                if !selected.contains(&idx) {
                    selected.push(idx);
                }
            }
        }

        // If we still don't have enough, add remaining cheapest blocks
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

    /// Optimal charge block selection using just-in-time charging strategy.
    ///
    /// This method works backwards from the charging deadline to select the absolute
    /// cheapest blocks within the available time window. It avoids the issue where
    /// the battery fills up before reaching the cheapest blocks.
    ///
    /// # Arguments
    /// * `all_blocks` - All price blocks with their indices and prices
    /// * `current_block_index` - Current block index
    /// * `current_soc` - Current battery state of charge (%)
    /// * `target_soc` - Target state of charge (%)
    /// * `battery_capacity_kwh` - Battery capacity in kWh
    /// * `charge_rate_kw` - Charging rate in kW
    /// * `deadline_block_index` - Latest block index by which charging must be complete
    ///
    /// IMPROVED: Instead of just picking the cheapest N blocks (which may be scattered
    /// and get removed by post-processing), prefer consecutive runs of cheap blocks.
    /// This ensures the charging blocks form a continuous sequence that won't be
    /// filtered out as "isolated" blocks.
    fn select_optimal_charge_blocks(
        all_blocks: &[TimeBlockPrice],
        current_block_index: usize,
        current_soc: f32,
        target_soc: f32,
        battery_capacity_kwh: f32,
        charge_rate_kw: f32,
        deadline_block_index: usize,
    ) -> Vec<usize> {
        if all_blocks.is_empty() || current_soc >= target_soc {
            return Vec::new();
        }

        // Calculate energy needed to charge
        let energy_needed_kwh = battery_capacity_kwh * (target_soc - current_soc) / 100.0;
        if energy_needed_kwh <= 0.0 {
            return Vec::new();
        }

        // Calculate blocks needed (15 minutes = 0.25 hours)
        let charge_per_block = charge_rate_kw * 0.25;
        if charge_per_block <= 0.0 {
            return Vec::new();
        }

        let blocks_needed = (energy_needed_kwh / charge_per_block).ceil() as usize;
        if blocks_needed == 0 {
            return Vec::new();
        }

        // Collect all available blocks between now and deadline
        let available_blocks: Vec<(usize, f32)> = all_blocks
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx > current_block_index && *idx <= deadline_block_index)
            .map(|(idx, block)| (idx, block.price_czk_per_kwh))
            .collect();

        if available_blocks.is_empty() {
            return Vec::new();
        }

        // Find cheapest price
        let cheapest_price = available_blocks
            .iter()
            .map(|(_, p)| *p)
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(f32::MAX);

        // Use 30% tolerance to include blocks in the cheap price range
        let tolerance = 0.30;
        let threshold = cheapest_price * (1.0 + tolerance);

        // Get all blocks within tolerance, sorted by index for run detection
        let mut within_tolerance: Vec<(usize, f32)> = available_blocks
            .iter()
            .filter(|(_, p)| *p <= threshold)
            .cloned()
            .collect();
        within_tolerance.sort_by_key(|(idx, _)| *idx);

        // Find consecutive runs within tolerance blocks
        let mut runs: Vec<Vec<(usize, f32)>> = Vec::new();
        let mut current_run: Vec<(usize, f32)> = Vec::new();

        for (idx, price) in &within_tolerance {
            if current_run.is_empty() || *idx == current_run.last().unwrap().0 + 1 {
                current_run.push((*idx, *price));
            } else {
                if !current_run.is_empty() {
                    runs.push(current_run);
                }
                current_run = vec![(*idx, *price)];
            }
        }
        if !current_run.is_empty() {
            runs.push(current_run);
        }

        // Score runs: prefer runs that meet min_consecutive requirement (2+), then cheaper average
        runs.sort_by(|a, b| {
            let a_len_ok = a.len() >= 2;
            let b_len_ok = b.len() >= 2;
            if a_len_ok != b_len_ok {
                return b_len_ok.cmp(&a_len_ok);
            }
            // Same category: prefer cheaper average
            let a_avg: f32 = a.iter().map(|(_, p)| p).sum::<f32>() / a.len() as f32;
            let b_avg: f32 = b.iter().map(|(_, p)| p).sum::<f32>() / b.len() as f32;
            a_avg
                .partial_cmp(&b_avg)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Greedily select blocks from best runs
        let mut selected: Vec<usize> = Vec::new();

        for run in &runs {
            if selected.len() >= blocks_needed {
                break;
            }
            // Take blocks from this run
            for (idx, _) in run {
                if selected.len() >= blocks_needed {
                    break;
                }
                if !selected.contains(idx) {
                    selected.push(*idx);
                }
            }
        }

        // If not enough from runs, fall back to cheapest individual blocks
        if selected.len() < blocks_needed {
            let mut sorted_by_price = available_blocks;
            sorted_by_price
                .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            for (idx, _) in &sorted_by_price {
                if selected.len() >= blocks_needed {
                    break;
                }
                if !selected.contains(idx) {
                    selected.push(*idx);
                }
            }
        }

        selected
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

        // OVERNIGHT CHARGING FIX: Extend the charging window to include overnight blocks
        // whenever it makes sense for planning purposes. This treats 12:00-08:00 next day
        // as the relevant window for "today's" charging decisions.
        //
        // The window is extended when:
        // 1. It's afternoon or evening (12:00+) - gives enough time to plan overnight charging
        // 2. Or it's early morning (before 08:00) - we're in the overnight charging window
        let current_hour = current_block_start.hour();
        let should_extend_overnight = !(8..12).contains(&current_hour);

        // Calculate the end of the charging window
        let charging_window_end = if should_extend_overnight {
            // Include blocks until 8 AM tomorrow for overnight charging planning
            let window_end_date = if current_hour < 8 {
                // Early morning: use today's 8 AM
                now_date
            } else {
                // Afternoon/evening: use tomorrow's 8 AM
                tomorrow_date
            };
            let window_end_8am = window_end_date.and_hms_opt(8, 0, 0).unwrap();
            DateTime::<Utc>::from_naive_utc_and_offset(window_end_8am, Utc)
        } else {
            // Morning (08:00-12:00): just today's remaining blocks
            let today_end = now_date.and_hms_opt(23, 59, 59).unwrap();
            DateTime::<Utc>::from_naive_utc_and_offset(today_end, Utc)
        };

        // Find cheapest blocks in the charging window (TODAY + early morning if evening)
        let mut price_indexed_today: Vec<(usize, f32)> = upcoming_blocks
            .iter()
            .filter(|(_, b)| b.block_start <= charging_window_end)
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
        // Use dynamic prediction: if we are consuming more than expected, adjust the forecast
        let current_hour = context.price_block.block_start.hour() as f32;
        let current_minute = context.price_block.block_start.minute() as f32;
        let hours_passed = current_hour + (current_minute / 60.0);

        let remaining_consumption_today = if hours_passed > 0.5 {
            // If we have some history today, check our run rate
            let avg_hourly_consumption = today_consumed_so_far_kwh / hours_passed;
            let remaining_hours = 24.0 - hours_passed;
            let dynamic_remaining = avg_hourly_consumption * remaining_hours;

            // Use the greater of static prediction or dynamic run-rate
            // This ensures we handle days that are heavier than average
            let static_remaining = (predicted_consumption_kwh - today_consumed_so_far_kwh).max(0.0);
            dynamic_remaining.max(static_remaining)
        } else {
            // Early in the day, rely on static prediction
            (predicted_consumption_kwh - today_consumed_so_far_kwh).max(0.0)
        };

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
            * (self.config.daily_charging_target_soc - context.current_battery_soc)
            / 100.0;
        let energy_to_target_soc = energy_to_target_soc.max(0.0);

        // Total charge needed is the max of deficit or target
        let mut total_charge_needed = total_deficit.max(energy_to_target_soc);

        // To avoid under-charging (which is more damaging than a slight
        // over-charge, since max SOC is enforced elsewhere), apply a
        // small safety multiplier. This biases the strategy to plan
        // slightly more charge blocks than the bare minimum estimate.
        total_charge_needed *= self.config.charge_safety_multiplier;

        // Calculate number of charging blocks needed
        // Energy charged per 15-minute block
        let charge_per_block = context.control_config.max_battery_charge_rate_kw * 0.25; // 15 minutes = 0.25 hours

        if charge_per_block <= 0.0 {
            return (0, 0);
        }

        let urgent_blocks_today = (energy_needed_today_urgent / charge_per_block).ceil() as usize;
        let total_blocks_needed = (total_charge_needed / charge_per_block).ceil() as usize;

        (urgent_blocks_today, total_blocks_needed)
    }

    /// Determine operation mode for current block
    /// Returns (mode, reason, decision_uid)
    fn determine_mode(
        &self,
        context: &EvaluationContext,
        analysis: &PriceHorizonAnalysis,
        current_block_index: usize,
        urgent_blocks_today: usize,
        total_blocks_needed: usize,
    ) -> (InverterOperationMode, String, String) {
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
                "wa1:force_discharge:export_opportunity".to_owned(),
            );
        }

        // Priority 0.5: Negative price - charge aggressively (Plan 3)
        if self.config.negative_price_handling_enabled && analysis.is_negative_price {
            if context.current_battery_soc < self.config.daily_charging_target_soc
                || (self.config.charge_on_negative_even_if_full
                    && context.current_battery_soc < 100.0)
            {
                return (
                    InverterOperationMode::ForceCharge,
                    format!(
                        "Negative price charging: {:.3} CZK/kWh (PAID to consume!)",
                        context.price_block.price_czk_per_kwh
                    ),
                    "wa1:force_charge:negative_price".to_owned(),
                );
            }
            // Battery full - at minimum don't export (we'd pay to export)
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "Negative price - avoiding export: {:.3} CZK/kWh",
                    context.price_block.price_czk_per_kwh
                ),
                "wa1:backup:negative_price_full".to_owned(),
            );
        }

        // Priority 1: Force Charge
        // Use block consolidation to avoid oscillation - prefer consecutive charge blocks
        let mut should_charge = false;
        let mut charge_reason = String::new();

        // Build price-indexed lists for consolidation
        // These are needed because analysis only stores indices, not prices
        let blocks_today_with_prices: Vec<(usize, f32)> = analysis
            .cheapest_blocks_today
            .iter()
            .filter_map(|idx| {
                context
                    .all_price_blocks
                    .and_then(|blocks| blocks.get(*idx))
                    .map(|b| (*idx, b.price_czk_per_kwh))
            })
            .collect();

        let _blocks_global_with_prices: Vec<(usize, f32)> = analysis
            .cheapest_blocks
            .iter()
            .filter_map(|idx| {
                context
                    .all_price_blocks
                    .and_then(|blocks| blocks.get(*idx))
                    .map(|b| (*idx, b.price_czk_per_kwh))
            })
            .collect();

        // Check 1: Urgent needs today (with consolidation)
        if urgent_blocks_today > 0 {
            // Use consolidation to select consecutive blocks where possible
            let urgent_slots = Self::consolidate_charge_blocks(
                &blocks_today_with_prices,
                urgent_blocks_today,
                self.config.charge_block_consolidation_tolerance,
                self.config.min_consecutive_charge_blocks,
            );

            if urgent_slots.contains(&current_block_index) {
                should_charge = true;
                charge_reason = format!(
                    "Urgent charge for today ({:.3} CZK/kWh)",
                    context.price_block.price_czk_per_kwh
                );
            }
        }

        // Check 2: General needs (target SOC / tomorrow) with optimal timing
        if !should_charge
            && total_blocks_needed > 0
            && let Some(all_blocks) = context.all_price_blocks
        {
            // SMART DEADLINE CALCULATION: Instead of a fixed 8-hour deadline,
            // analyze price patterns to find when prices rise significantly,
            // and ensure we're fully charged BEFORE that price rise.
            let soc_deficit = self.config.daily_charging_target_soc - context.current_battery_soc;
            let is_significantly_below_target = soc_deficit > 30.0;

            let deadline_blocks_ahead = if is_significantly_below_target {
                // Find when prices start rising significantly and charge before that
                let horizon_blocks = 96.min(all_blocks.len() - current_block_index); // 24 hours max

                // Calculate average price in the horizon
                let horizon_prices: Vec<f32> = all_blocks[current_block_index..]
                    .iter()
                    .take(horizon_blocks)
                    .map(|b| b.price_czk_per_kwh)
                    .collect();

                let avg_price = if !horizon_prices.is_empty() {
                    horizon_prices.iter().sum::<f32>() / horizon_prices.len() as f32
                } else {
                    2.5 // Fallback
                };

                // Find the first sustained expensive period (3+ consecutive blocks above avg + 0.5)
                let expensive_threshold = avg_price + 0.5;
                let mut expensive_start_idx: Option<usize> = None;
                let mut consecutive_expensive = 0;

                for (offset, price) in horizon_prices.iter().enumerate() {
                    if *price > expensive_threshold {
                        consecutive_expensive += 1;
                        if consecutive_expensive >= 3 && expensive_start_idx.is_none() {
                            // Found sustained expensive period
                            expensive_start_idx = Some(offset - 2); // Start of the 3-block sequence
                        }
                    } else {
                        consecutive_expensive = 0;
                    }
                }

                // If we found an expensive period, set deadline to be fully charged before it
                // Otherwise, use 24 hours as fallback
                if let Some(expensive_offset) = expensive_start_idx {
                    // Ensure we have enough cheap blocks to charge
                    let min_blocks_for_deadline = (total_blocks_needed + 4).max(8); // Need at least N blocks
                    expensive_offset.max(min_blocks_for_deadline)
                } else {
                    // No sustained expensive period found, use 24 hours
                    96
                }
            } else {
                // Normal: charge before tomorrow 6 AM
                let tomorrow_date = context
                    .price_block
                    .block_start
                    .date_naive()
                    .succ_opt()
                    .unwrap_or(context.price_block.block_start.date_naive());

                let tomorrow_6am = tomorrow_date.and_hms_opt(6, 0, 0).unwrap();
                let tomorrow_6am_utc =
                    DateTime::<Utc>::from_naive_utc_and_offset(tomorrow_6am, Utc);

                // Find the block closest to 6 AM tomorrow
                all_blocks
                    .iter()
                    .enumerate()
                    .find(|(_, block)| block.block_start >= tomorrow_6am_utc)
                    .map(|(idx, _)| idx)
                    // Fallback: use 12 hours ahead
                    .unwrap_or(current_block_index + 48)
                    .saturating_sub(current_block_index)
            };

            let deadline_index = current_block_index + deadline_blocks_ahead;

            // IMPORTANT FIX: To prevent SOC prediction drift from causing the scheduler to
            // spread charging across multiple nights, we use a FIXED minimum SOC reference
            // when significantly below target. This ensures ALL blocks in the charging window
            // are evaluated with the same "need to charge" calculation, preventing the scheduler
            // from thinking "I only need 3 more blocks" halfway through the session.
            let reference_soc_for_planning = if is_significantly_below_target {
                // Use hardware minimum as stable reference for the entire charging session
                // This way, all 8 blocks are evaluated as "we need 8 blocks from 10% to 90%"
                // instead of recalculating on each block with the predicted SOC
                context.control_config.hardware_min_battery_soc
            } else {
                // Normal operation: use current predicted SOC
                context.current_battery_soc
            };

            // Use optimal charge block selection with stable reference
            let raw_optimal_slots = Self::select_optimal_charge_blocks(
                all_blocks,
                current_block_index,
                reference_soc_for_planning, // Use stable reference to prevent drift
                self.config.daily_charging_target_soc,
                context.control_config.battery_capacity_kwh,
                context.control_config.max_battery_charge_rate_kw,
                deadline_index,
            );

            // IMPORTANT: Apply consolidation to prevent isolated blocks that will be removed later
            // Build price list for consolidation
            let optimal_blocks_with_prices: Vec<(usize, f32)> = raw_optimal_slots
                .iter()
                .filter_map(|idx| all_blocks.get(*idx).map(|b| (*idx, b.price_czk_per_kwh)))
                .collect();

            // Consolidate to ensure consecutive blocks
            let consolidated_optimal_slots = if !optimal_blocks_with_prices.is_empty() {
                Self::consolidate_charge_blocks(
                    &optimal_blocks_with_prices,
                    raw_optimal_slots.len(),
                    self.config.charge_block_consolidation_tolerance,
                    self.config.min_consecutive_charge_blocks,
                )
            } else {
                Vec::new()
            };

            if consolidated_optimal_slots.contains(&current_block_index) {
                should_charge = true;
                charge_reason = if is_significantly_below_target {
                    format!(
                        "Aggressive charge to target ({:.1}% → {:.1}%, {:.3} CZK/kWh)",
                        context.current_battery_soc,
                        self.config.daily_charging_target_soc,
                        context.price_block.price_czk_per_kwh
                    )
                } else {
                    format!(
                        "Optimal charging for horizon/target ({:.3} CZK/kWh)",
                        context.price_block.price_czk_per_kwh
                    )
                };
            }
        }

        if should_charge {
            let decision_uid = if urgent_blocks_today > 0 {
                "wa1:force_charge:urgent_today".to_owned()
            } else {
                "wa1:force_charge:optimal_horizon".to_owned()
            };
            return (
                InverterOperationMode::ForceCharge,
                format!("{} (avg: {:.3})", charge_reason, analysis.avg_all_price),
                decision_uid,
            );
        }

        // Priority 1.5: Force Charge Proximity Check
        // If force charging is scheduled within next 3 hours, avoid backup mode entirely
        // to maximize battery discharge before optimal charging begins
        let force_charge_proximity_blocks = 32; // 8 hours at 15-minute intervals (very aggressive)
        let has_force_charge_soon = if urgent_blocks_today > 0 || total_blocks_needed > 0 {
            if let Some(all_blocks) = context.all_price_blocks {
                // Get the optimal charge slots that would be selected - use same deadline logic
                let tomorrow_date = context
                    .price_block
                    .block_start
                    .date_naive()
                    .succ_opt()
                    .unwrap_or(context.price_block.block_start.date_naive());

                let tomorrow_6am = tomorrow_date.and_hms_opt(6, 0, 0).unwrap();
                let tomorrow_6am_utc =
                    DateTime::<Utc>::from_naive_utc_and_offset(tomorrow_6am, Utc);

                let tomorrow_expensive_start = all_blocks
                    .iter()
                    .enumerate()
                    .find(|(_, block)| block.block_start >= tomorrow_6am_utc)
                    .map(|(idx, _)| idx)
                    .unwrap_or(current_block_index + 48);

                let optimal_slots = Self::select_optimal_charge_blocks(
                    all_blocks,
                    current_block_index,
                    context.current_battery_soc,
                    self.config.daily_charging_target_soc,
                    context.control_config.battery_capacity_kwh,
                    context.control_config.max_battery_charge_rate_kw,
                    tomorrow_expensive_start,
                );

                // Check if any optimal charge block is within the next 3 hours
                let end_proximity = current_block_index + force_charge_proximity_blocks;
                optimal_slots
                    .iter()
                    .any(|&idx| idx > current_block_index && idx <= end_proximity)
            } else {
                false
            }
        } else {
            false
        };

        // Additional fallback: Avoid backup mode during typical pre-charging evening hours
        // Even if optimal blocks aren't found within the window, we should avoid backup mode
        // during evening hours when force charging typically happens later
        let current_hour = context.price_block.block_start.hour();
        let is_evening_pre_charge_time = (19..=23).contains(&current_hour); // 7 PM - 11 PM

        let has_force_charge_soon = has_force_charge_soon || is_evening_pre_charge_time;

        // Priority 2: Battery Protection
        if context.current_battery_soc < context.backup_discharge_min_soc {
            // If we are not charging, but battery is critically low,
            // enter BackUpMode to prevent discharge
            return (
                InverterOperationMode::BackUpMode,
                format!(
                    "Battery critical ({:.1}% < {:.1}%)",
                    context.current_battery_soc, context.backup_discharge_min_soc
                ),
                "wa1:backup:battery_critical".to_owned(),
            );
        }

        // Priority 3: Discharge (Self-Use)
        // Only allow discharge if:
        // 1. We are in an "expensive" block (above average AND top N expensive today)
        // 2. OR we have excess battery (above target)
        // 3. OR we are in "Back Up Mode" (preserve battery) if price is cheap? No, BackUpMode prevents discharge.

        // Plan 1: Tomorrow Preservation (IMPROVED)
        // If tomorrow is much more expensive, be more conservative with discharge today
        // BUT: Don't preserve if we have cheap charging opportunities before tomorrow
        if analysis.should_preserve_for_tomorrow {
            // Only discharge if we are in the absolute most expensive blocks today
            // e.g., top 3 instead of top 12
            let is_top_peak = analysis
                .expensive_blocks_today
                .iter()
                .take(3)
                .any(|idx| *idx == current_block_index);

            if !is_top_peak && context.current_battery_soc <= self.config.conservation_threshold_soc
            {
                // NEW: Check if there are cheap charging hours coming before tomorrow
                // If yes, we can safely discharge now and recharge later
                let has_cheap_charging_before_tomorrow =
                    if let Some(blocks) = context.all_price_blocks {
                        // Find tomorrow's start (next day from current block)
                        let tomorrow_start = (context
                            .price_block
                            .block_start
                            .date_naive()
                            .succ_opt()
                            .unwrap_or(context.price_block.block_start.date_naive()))
                        .and_hms_opt(0, 0, 0)
                        .unwrap();
                        let tomorrow_start_utc =
                            DateTime::<Utc>::from_naive_utc_and_offset(tomorrow_start, Utc);

                        // Look for cheap charging blocks between now and tomorrow
                        // Cheap = below average price (relaxed threshold to catch force charging periods)
                        let avg_price = analysis.avg_all_price;
                        let cheap_threshold = avg_price * 0.85; // 15% below average (was 30%, too strict)

                        // Count how many cheap blocks we have before tomorrow
                        let cheap_blocks_count = blocks
                            .iter()
                            .enumerate()
                            .filter(|(idx, b)| {
                                *idx > current_block_index
                                    && b.block_start < tomorrow_start_utc
                                    && b.price_czk_per_kwh < cheap_threshold
                            })
                            .count();

                        // If we have at least N cheap blocks scheduled (from total_blocks_needed),
                        // we can safely discharge now
                        cheap_blocks_count >= total_blocks_needed.min(4) // At least 4 cheap blocks (1 hour)
                    } else {
                        false
                    };

                if !has_cheap_charging_before_tomorrow && !has_force_charge_soon {
                    return (
                        InverterOperationMode::BackUpMode,
                        format!(
                            "Preserving for tomorrow (avg {:.2} > {:.2})",
                            analysis.tomorrow_peak_avg.unwrap_or(0.0),
                            analysis.today_peak_avg
                        ),
                        "wa1:backup:preserve_tomorrow".to_owned(),
                    );
                }
                // Otherwise, allow discharge - we'll recharge tonight
            }
        }

        // Standard Discharge Logic
        if analysis.is_expensive_block {
            return (
                InverterOperationMode::SelfUse,
                format!(
                    "Discharging during expensive block ({:.3} CZK/kWh)",
                    context.price_block.price_czk_per_kwh
                ),
                "wa1:self_use:expensive_block".to_owned(),
            );
        }

        // If block is cheap (below average), consider holding charge
        // BUT: Use smarter logic to avoid unnecessary backup mode
        if analysis.is_cheap_block
            && context.current_battery_soc < self.config.conservation_threshold_soc
        {
            // Check if we should actually hold charge or just use battery

            // 1. Are there expensive blocks remaining today that we should save for?
            let has_expensive_blocks_ahead = analysis
                .expensive_blocks_today
                .iter()
                .any(|idx| *idx > current_block_index);

            // 2. Is cheap charging coming soon (within next 6 hours)?
            let has_cheap_charging_soon = if let Some(blocks) = context.all_price_blocks {
                let lookahead_blocks = 24; // 6 hours at 15-minute intervals (4 blocks/hour * 6 = 24)
                let end_idx = (current_block_index + lookahead_blocks).min(blocks.len());

                // Check if any of our planned charge blocks are in the next 6 hours
                analysis
                    .cheapest_blocks
                    .iter()
                    .any(|idx| *idx > current_block_index && *idx < end_idx)
            } else {
                false
            };

            // Only hold charge if we have a good reason:
            // - Expensive blocks are coming (need to save battery), OR
            // - No cheap charging soon (can't refill easily), AND
            // - No force charging within 3 hours (want battery as empty as possible)
            if (has_expensive_blocks_ahead || !has_cheap_charging_soon) && !has_force_charge_soon {
                return (
                    InverterOperationMode::BackUpMode,
                    format!(
                        "Holding charge during cheap block ({:.3} < {:.3})",
                        context.price_block.price_czk_per_kwh, analysis.avg_all_price
                    ),
                    "wa1:backup:hold_for_expensive".to_owned(),
                );
            }

            // Otherwise, allow self-use (battery will discharge if needed)
            // This handles the end-of-day case where charging is imminent
        }

        // Default: SelfUse (allow discharge if needed, but don't force it)
        (
            InverterOperationMode::SelfUse,
            "Standard operation".to_owned(),
            "wa1:self_use:standard".to_owned(),
        )
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

        // Fill assumptions
        eval.assumptions = Assumptions {
            solar_forecast_kwh: context.solar_forecast_kwh,
            consumption_forecast_kwh: context.consumption_forecast_kwh,
            current_battery_soc: context.current_battery_soc,
            battery_efficiency: context.control_config.battery_efficiency,
            battery_wear_cost_czk_per_kwh: context.control_config.battery_wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: context.price_block.price_czk_per_kwh,
            grid_export_price_czk_per_kwh: context.grid_export_price_czk_per_kwh,
        };

        // Need price data for analysis
        let Some(all_blocks) = context.all_price_blocks else {
            eval.reason = "No price data available".to_string();
            return eval;
        };

        // Find current block index in all_blocks
        // This assumes all_blocks contains the current block (it should)
        let current_block_index = all_blocks
            .iter()
            .position(|b| b.block_start == context.price_block.block_start)
            .unwrap_or(0);

        // 1. Analyze Prices
        let analysis = self.analyze_prices(
            all_blocks,
            context.price_block.block_start,
            current_block_index,
        );

        // 2. Predict Consumption (using config history or fallback)
        let predicted_daily_consumption = self
            .config
            .predict_daily_consumption()
            .unwrap_or(context.control_config.average_household_load_kw * 24.0);

        // 3. Calculate Energy Requirements
        // For now, assume we are at start of day if we don't have detailed history
        // In a real implementation, we'd track cumulative consumption/solar for the day
        let today_consumed_so_far = context.consumption_today_kwh.unwrap_or(0.0);
        let remaining_solar_today = context.solar_forecast_kwh; // Simplified
        let tomorrow_solar_estimate = context.solar_forecast_kwh; // Simplified persistence forecast

        let (urgent_blocks, total_blocks) = self.calculate_energy_requirements(
            context,
            predicted_daily_consumption,
            today_consumed_so_far,
            remaining_solar_today,
            tomorrow_solar_estimate,
        );

        // 4. Determine Mode
        let (mode, reason, decision_uid) = self.determine_mode(
            context,
            &analysis,
            current_block_index,
            urgent_blocks,
            total_blocks,
        );

        eval.mode = mode;
        eval.reason = reason;
        eval.decision_uid = Some(decision_uid);

        // 5. Calculate Financials (simplified for now)
        match mode {
            InverterOperationMode::ForceCharge => {
                // Energy charged in 15 minutes (0.25 hours)
                let charge_kwh = context.control_config.max_battery_charge_rate_kw * 0.25;
                eval.energy_flows.battery_charge_kwh = charge_kwh;
                eval.energy_flows.grid_import_kwh = charge_kwh;
                eval.cost_czk =
                    economics::grid_import_cost(charge_kwh, context.price_block.price_czk_per_kwh);
            }
            InverterOperationMode::ForceDischarge => {
                // Energy discharged in 15 minutes (0.25 hours)
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

                // Calculate how much battery will discharge to cover load
                let battery_discharge = usable_battery_kwh.min(context.consumption_forecast_kwh);

                eval.energy_flows.battery_discharge_kwh = battery_discharge;

                if battery_discharge >= context.consumption_forecast_kwh {
                    // Battery can fully cover consumption - show as avoided grid import cost
                    eval.revenue_czk = context.consumption_forecast_kwh * price;
                } else {
                    // Battery partially depleted - split between battery and grid
                    // Battery covers what it can (avoided cost = profit)
                    eval.revenue_czk = battery_discharge * price;
                    // Grid must cover the rest (actual cost)
                    eval.cost_czk = (context.consumption_forecast_kwh - battery_discharge) * price;
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
    use chrono::{TimeZone, Utc};
    use fluxion_types::config::ControlConfig;
    use fluxion_types::pricing::TimeBlockPrice;

    #[test]
    fn test_discharge_now_when_cheap_charging_tonight() {
        // Setup - Scenario from user: expensive hours now, but cheap charging before tomorrow
        let config = WinterAdaptiveConfig {
            enabled: true,
            daily_charging_target_soc: 80.0,
            conservation_threshold_soc: 70.0,
            tomorrow_preservation_threshold: 1.2, // Tomorrow is 20% more expensive
            top_expensive_blocks: 6,
            ..Default::default()
        };
        let strategy = WinterAdaptiveStrategy::new(config);

        // Create price blocks simulating the user's scenario:
        // Current time: 19:00 (expensive)
        // 22:00-06:00: cheap charging hours
        // Tomorrow morning: even more expensive
        let base_time = Utc.with_ymd_and_hms(2025, 11, 30, 19, 0, 0).unwrap();

        let mut blocks = Vec::new();

        // 19:00-22:00: Expensive hours NOW (where we should discharge)
        for i in 0..12 {
            // 3 hours = 12 blocks
            blocks.push(TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15 * i),
                duration_minutes: 15,
                price_czk_per_kwh: 4.5, // Expensive
                effective_price_czk_per_kwh: 4.5,
            });
        }

        // 22:00-06:00: Cheap overnight charging (8 hours = 32 blocks)
        for i in 12..44 {
            blocks.push(TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15 * i),
                duration_minutes: 15,
                price_czk_per_kwh: 1.2, // Very cheap
                effective_price_czk_per_kwh: 1.2,
            });
        }

        // Tomorrow 06:00-22:00: Very expensive (16 hours = 64 blocks)
        for i in 44..108 {
            blocks.push(TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15 * i),
                duration_minutes: 15,
                price_czk_per_kwh: 6.0, // Even more expensive (>20% higher than today)
                effective_price_czk_per_kwh: 6.0,
            });
        }

        // Context: currently in expensive hour with good battery
        // Key: battery is high enough that we don't need urgent charging
        let control_config = ControlConfig {
            average_household_load_kw: 0.5, // Low consumption
            battery_capacity_kwh: 10.0,
            max_battery_charge_rate_kw: 5.0,
            ..ControlConfig::default()
        };
        let context = EvaluationContext {
            price_block: &blocks[0], // 19:00 - expensive hour
            all_price_blocks: Some(&blocks),
            current_battery_soc: 75.0, // Good battery level, no urgent need
            control_config: &control_config,
            solar_forecast_kwh: 0.5,       // Winter - minimal solar
            consumption_forecast_kwh: 0.5, // Low consumption
            grid_export_price_czk_per_kwh: 0.1,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(15.0),
            consumption_today_kwh: None,
        };

        // Evaluate
        let eval = strategy.evaluate(&context);

        // Assert: Should discharge NOW (SelfUse) because:
        // 1. Current hour is expensive
        // 2. Tomorrow IS more expensive (triggers preservation check)
        // 3. BUT we have 32 cheap charging blocks tonight (plenty to recharge)
        // 4. Therefore, we should use battery now and recharge tonight
        println!("Mode: {:?}, Reason: {}", eval.mode, eval.reason);
        assert_eq!(
            eval.mode,
            InverterOperationMode::SelfUse,
            "Should discharge during expensive hours when cheap charging is available tonight"
        );
        assert_ne!(
            eval.mode,
            InverterOperationMode::BackUpMode,
            "Should NOT preserve battery when cheap charging is available before tomorrow"
        );
    }

    #[test]
    fn test_backup_mode_avoidance_when_charging_soon() {
        // Setup
        let config = WinterAdaptiveConfig {
            enabled: true,
            daily_charging_target_soc: 80.0, // High target to trigger potential backup
            conservation_threshold_soc: 75.0,
            ..Default::default()
        };
        let strategy = WinterAdaptiveStrategy::new(config);

        // Create price blocks
        // 22:45 (Current) - Cheap (2.215)
        // 23:00 - Cheap (2.374)
        // 23:15 - Cheap (2.283)
        // 23:30 - Very Cheap (1.500) -> Should be charging
        let base_time = Utc.timestamp_opt(1732920300, 0).unwrap(); // 2024-11-29 22:45:00 UTC (approx)

        let prices = [
            2.215, // 22:45
            2.374, // 23:00
            2.283, // 23:15
            1.500, // 23:30 (Cheap charging)
            1.500, // 23:45
            1.500, // 00:00
            1.500, // 00:15
            1.500, // 00:30
        ];

        let blocks: Vec<TimeBlockPrice> = prices
            .iter()
            .enumerate()
            .map(|(i, &price)| TimeBlockPrice {
                block_start: base_time + chrono::Duration::minutes(15 * i as i64),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            })
            .collect();

        // Context
        let control_config = ControlConfig {
            average_household_load_kw: 0.0,
            ..ControlConfig::default()
        };
        let context = EvaluationContext {
            price_block: &blocks[0],
            all_price_blocks: Some(&blocks),
            current_battery_soc: 79.0, // Just below target 80%
            control_config: &control_config,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.0, // No consumption to drive urgency
            grid_export_price_czk_per_kwh: 0.1,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(12.0),
            consumption_today_kwh: None,
        };

        // Evaluate
        let eval = strategy.evaluate(&context);

        // Assert
        // Should be SelfUse because cheap charging is coming at index 3 (within 8 blocks)
        println!("Mode: {:?}, Reason: {}", eval.mode, eval.reason);
        assert_eq!(
            eval.mode,
            InverterOperationMode::SelfUse,
            "Should be SelfUse when charging is imminent"
        );
        assert_ne!(
            eval.mode,
            InverterOperationMode::BackUpMode,
            "Should NOT be BackUpMode"
        );
    }
}
