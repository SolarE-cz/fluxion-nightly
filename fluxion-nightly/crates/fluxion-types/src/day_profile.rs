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

use serde::{Deserialize, Serialize};

/// Raw price statistics computed from time block prices.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceStats {
    pub avg_czk: f32,
    pub min_czk: f32,
    pub max_czk: f32,
    pub std_dev_czk: f32,
    pub median_czk: f32,
    pub block_count: usize,
}

impl Default for PriceStats {
    fn default() -> Self {
        Self {
            avg_czk: 0.0,
            min_czk: 0.0,
            max_czk: 0.0,
            std_dev_czk: 0.0,
            median_czk: 0.0,
            block_count: 0,
        }
    }
}

/// Measurable day characterization — all fields are independent, composable metrics.
/// A day can be volatile + have negative prices + high solar simultaneously.
/// Computed from price blocks + solar forecasts + consumption data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DayMetrics {
    // === Solar ===
    /// solar_forecast / daily_consumption (>1.1 = high, 0.9-1.1 = medium, <0.9 = low)
    pub solar_ratio: f32,

    // === Price Volatility ===
    /// Coefficient of Variation: std_dev / avg (standard statistical volatility measure)
    pub price_cv: f32,
    /// (max_price - min_price) / avg_price (price range as fraction of average)
    pub price_spread_ratio: f32,

    // === Price Level ===
    /// (avg_price - charge_cost) / charge_cost (normalized deviation from charge basis)
    pub price_level_vs_charge_cost: f32,

    // === Special Conditions ===
    /// Fraction of blocks with negative effective prices (0.0 to 1.0)
    pub negative_price_fraction: f32,

    // === Tomorrow Outlook ===
    /// avg_tomorrow / avg_today (None if tomorrow prices unavailable)
    pub tomorrow_price_ratio: Option<f32>,
    /// solar_forecast_tomorrow / daily_consumption
    pub tomorrow_solar_ratio: f32,

    // === Raw Statistics ===
    pub price_stats: PriceStats,
}

impl Default for DayMetrics {
    fn default() -> Self {
        Self {
            solar_ratio: 0.0,
            price_cv: 0.0,
            price_spread_ratio: 0.0,
            price_level_vs_charge_cost: 0.0,
            negative_price_fraction: 0.0,
            tomorrow_price_ratio: None,
            tomorrow_solar_ratio: 0.0,
            price_stats: PriceStats::default(),
        }
    }
}

impl DayMetrics {
    // --- Solar (independent axis) ---

    /// Solar generation exceeds consumption by >10%
    pub fn is_high_solar(&self) -> bool {
        self.solar_ratio > 1.1
    }

    /// Solar roughly balanced with consumption (within +/-10%)
    pub fn is_medium_solar(&self) -> bool {
        (0.9..=1.1).contains(&self.solar_ratio)
    }

    /// Solar covers less than 90% of consumption
    pub fn is_low_solar(&self) -> bool {
        self.solar_ratio < 0.9
    }

    // --- Volatility (independent axis) ---

    /// Price CV exceeds the given threshold (e.g. 0.35 for high volatility)
    pub fn is_volatile(&self, cv_threshold: f32) -> bool {
        self.price_cv > cv_threshold
    }

    /// Price CV below the given threshold (e.g. 0.15 for very stable)
    pub fn is_stable(&self, cv_threshold: f32) -> bool {
        self.price_cv < cv_threshold
    }

    // --- Price level (independent axis) ---

    /// Average price is above charge cost by more than `level_threshold` fraction
    pub fn is_expensive(&self, level_threshold: f32) -> bool {
        self.price_level_vs_charge_cost > level_threshold
    }

    /// Average price is below charge cost by more than `level_threshold` fraction (use negative)
    pub fn is_cheap(&self, level_threshold: f32) -> bool {
        self.price_level_vs_charge_cost < level_threshold
    }

    // --- Negative prices (independent axis) ---

    /// Any blocks have negative effective prices
    pub fn has_negative_prices(&self) -> bool {
        self.negative_price_fraction > 0.0
    }

    /// Negative price blocks exceed the given fraction threshold (e.g. 0.10 for significant)
    pub fn significant_negative_prices(&self, fraction_threshold: f32) -> bool {
        self.negative_price_fraction > fraction_threshold
    }

    // --- Tomorrow outlook (independent axis) ---

    /// Tomorrow's average price exceeds today's by more than `ratio_threshold` (e.g. 1.3)
    pub fn is_tomorrow_expensive(&self, ratio_threshold: f32) -> bool {
        self.tomorrow_price_ratio
            .is_some_and(|r| r > ratio_threshold)
    }

    /// Tomorrow's average price is below today's by less than `ratio_threshold` (e.g. 0.7)
    pub fn is_tomorrow_cheap(&self, ratio_threshold: f32) -> bool {
        self.tomorrow_price_ratio
            .is_some_and(|r| r < ratio_threshold)
    }

    /// Tomorrow has high solar generation relative to consumption
    pub fn is_tomorrow_sunny(&self) -> bool {
        self.tomorrow_solar_ratio > 1.1
    }
}
