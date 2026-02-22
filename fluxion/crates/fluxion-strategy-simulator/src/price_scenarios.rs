// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Price scenario definitions for strategy simulation.
//!
//! This module provides pre-defined price patterns that represent
//! typical market conditions:
//!
//! - **Usual Day**: Cheap overnight, elevated day, noon dip, evening peak
//! - **Elevated Day**: Cheap only at night, high prices throughout the day
//! - **Volatile**: Large price swings with arbitrage opportunities
//! - **Negative Prices**: Contains negative price periods (renewable surplus)

use chrono::{NaiveDate, TimeZone, Utc};
use fluxion_types::pricing::TimeBlockPrice;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Price scenario types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PriceScenario {
    /// "Usual day" - cheap overnight, elevated day, noon dip, evening peak
    UsualDay,

    /// "Elevated day" - cheap only at night, uniformly high during day
    ElevatedDay,

    /// Volatile prices with significant swings (0.5-8 CZK range)
    Volatile,

    /// Contains negative price periods (typically midday)
    NegativePrices,

    /// HDO-optimized schedule (follows Czech HDO pattern)
    HdoOptimized,

    /// Custom price curve with explicit prices
    Custom {
        /// 96 prices for each 15-minute block (CZK/kWh)
        prices: Vec<f32>,
        /// Optional HDO override periods
        hdo_periods: Option<Vec<(u8, u8)>>,
    },

    /// Import from historical database (marker - actual loading done externally)
    Historical {
        /// Date to load from database
        date: NaiveDate,
    },
}

impl PriceScenario {
    /// Get the human-readable name of this scenario
    pub fn name(&self) -> &str {
        match self {
            Self::UsualDay => "Usual Day",
            Self::ElevatedDay => "Elevated Day",
            Self::Volatile => "Volatile Prices",
            Self::NegativePrices => "Negative Prices",
            Self::HdoOptimized => "HDO Optimized",
            Self::Custom { .. } => "Custom",
            Self::Historical { .. } => "Historical",
        }
    }

    /// Get a description of this scenario
    pub fn description(&self) -> &str {
        match self {
            Self::UsualDay => {
                "Cheap overnight (0-6), elevated day (6-12, 14-20), noon dip (12-14), evening peak (17-20)"
            }
            Self::ElevatedDay => "Cheap only at night (0-6), uniformly high during day (6-24)",
            Self::Volatile => {
                "Large price swings throughout the day, testing arbitrage opportunities"
            }
            Self::NegativePrices => {
                "Contains negative price periods during midday (high renewable generation)"
            }
            Self::HdoOptimized => "Follows Czech HDO low-tariff schedule",
            Self::Custom { .. } => "User-defined custom price curve",
            Self::Historical { .. } => "Historical prices from database",
        }
    }

    /// Generate 96 price blocks for a day
    pub fn generate_prices(&self, date: NaiveDate) -> Vec<TimeBlockPrice> {
        match self {
            Self::UsualDay => generate_usual_day_prices(date),
            Self::ElevatedDay => generate_elevated_day_prices(date),
            Self::Volatile => generate_volatile_prices(date),
            Self::NegativePrices => generate_negative_prices(date),
            Self::HdoOptimized => generate_hdo_optimized_prices(date),
            Self::Custom { prices, .. } => prices_to_blocks(date, prices),
            Self::Historical { .. } => {
                // Historical data must be loaded externally
                // Return empty - caller should handle this case
                Vec::new()
            }
        }
    }
}

/// Price scenario preset with metadata
#[derive(Debug, Clone)]
pub struct PriceScenarioPreset {
    /// Unique identifier
    pub id: &'static str,
    /// Display name
    pub name: &'static str,
    /// Description
    pub description: &'static str,
    /// The scenario
    pub scenario: PriceScenario,
}

/// Available price scenario presets
pub const PRICE_PRESETS: &[PriceScenarioPreset] = &[
    PriceScenarioPreset {
        id: "usual_day",
        name: "Usual Day",
        description: "Cheap overnight (0-6), elevated day (6-12, 14-20), noon dip (12-14), evening peak (17-20)",
        scenario: PriceScenario::UsualDay,
    },
    PriceScenarioPreset {
        id: "elevated_day",
        name: "Elevated Day",
        description: "Cheap only at night (0-6), uniformly high during day",
        scenario: PriceScenario::ElevatedDay,
    },
    PriceScenarioPreset {
        id: "volatile",
        name: "Volatile",
        description: "Large price swings throughout the day, testing arbitrage opportunities",
        scenario: PriceScenario::Volatile,
    },
    PriceScenarioPreset {
        id: "negative",
        name: "Negative Prices",
        description: "Includes negative price periods during midday (high renewable generation)",
        scenario: PriceScenario::NegativePrices,
    },
    PriceScenarioPreset {
        id: "hdo_optimized",
        name: "HDO Optimized",
        description: "Price pattern aligned with Czech HDO low-tariff schedule",
        scenario: PriceScenario::HdoOptimized,
    },
];

/// Convert price array to `TimeBlockPrice` blocks
fn prices_to_blocks(date: NaiveDate, prices: &[f32]) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    prices
        .iter()
        .enumerate()
        .map(|(i, &price)| TimeBlockPrice {
            block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
            duration_minutes: 15,
            price_czk_per_kwh: price,
            effective_price_czk_per_kwh: price,
        })
        .collect()
}

/// Generate "Usual Day" price pattern
///
/// Pattern:
/// - 00:00-06:00: 1.50 CZK (cheap overnight)
/// - 06:00-12:00: 3.50 CZK (morning elevated)
/// - 12:00-14:00: 2.80 CZK (noon dip - solar surplus)
/// - 14:00-17:00: 3.20 CZK (afternoon)
/// - 17:00-20:00: 4.50 CZK (evening peak)
/// - 20:00-24:00: 2.50 CZK (late evening decline)
fn generate_usual_day_prices(date: NaiveDate) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    let mut rng = rand::thread_rng();

    (0..96)
        .map(|i| {
            let hour = i / 4;
            let base_price = match hour {
                0..=5 => 1.50,   // Cheap overnight
                6..=11 => 3.50,  // Morning elevated
                12..=13 => 2.80, // Noon dip
                14..=16 => 3.20, // Afternoon
                17..=19 => 4.50, // Evening peak
                20..=23 => 2.50, // Late evening decline
                _ => 2.50,
            };

            // Add small random noise (+/- 10%)
            let noise = rng.gen_range(-0.10..0.10);
            let price = base_price * (1.0 + noise);

            TimeBlockPrice {
                block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            }
        })
        .collect()
}

/// Generate "Elevated Day" price pattern
///
/// Pattern:
/// - 00:00-06:00: 1.50 CZK (cheap overnight)
/// - 06:00-24:00: 4.50 CZK (elevated all day)
fn generate_elevated_day_prices(date: NaiveDate) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    let mut rng = rand::thread_rng();

    (0..96)
        .map(|i| {
            let hour = i / 4;
            let base_price = if hour < 6 {
                1.50 // Cheap overnight
            } else {
                4.50 // Elevated all day
            };

            // Add small random noise (+/- 10%)
            let noise = rng.gen_range(-0.10..0.10);
            let price = base_price * (1.0 + noise);

            TimeBlockPrice {
                block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            }
        })
        .collect()
}

/// Generate "Volatile" price pattern
///
/// Large swings from 0.5 to 8.0 CZK, simulating high market volatility
fn generate_volatile_prices(date: NaiveDate) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    let mut rng = rand::thread_rng();

    // Create a base pattern with multiple peaks and valleys
    // Format: (start_block, end_block, low_price, high_price)
    let base_pattern: [(usize, usize, f32, f32); 22] = [
        // Night - cheap with occasional spikes
        (0, 4, 0.8, 1.2),   // 00:00-01:00 valley
        (4, 8, 0.8, 1.4),   // 01:00-02:00 slight rise
        (8, 12, 0.5, 0.8),  // 02:00-03:00 deep valley
        (12, 16, 1.5, 2.2), // 03:00-04:00 morning ramp
        (16, 20, 3.5, 4.2), // 04:00-05:00 morning peak
        (20, 24, 2.0, 2.7), // 05:00-06:00 dip
        // Morning - volatile
        (24, 28, 5.0, 6.2), // 06:00-07:00 high
        (28, 32, 3.0, 3.8), // 07:00-08:00 drop
        (32, 36, 6.5, 7.8), // 08:00-09:00 spike
        (36, 40, 4.0, 4.8), // 09:00-10:00 moderate
        // Midday - solar dip
        (40, 48, 2.0, 2.8), // 10:00-12:00 solar dip
        (48, 56, 1.5, 2.2), // 12:00-14:00 deep solar dip
        // Afternoon ramp
        (56, 60, 3.5, 4.2), // 14:00-15:00 ramp up
        (60, 64, 5.0, 5.8), // 15:00-16:00 rising
        (64, 68, 6.5, 7.2), // 16:00-17:00 peak building
        // Evening peak - extreme
        (68, 72, 7.5, 8.2), // 17:00-18:00 peak
        (72, 76, 8.0, 8.8), // 18:00-19:00 extreme peak
        (76, 80, 6.5, 7.2), // 19:00-20:00 decline
        // Night decline
        (80, 84, 4.0, 4.8), // 20:00-21:00 evening
        (84, 88, 2.5, 3.2), // 21:00-22:00 late evening
        (88, 92, 1.5, 2.2), // 22:00-23:00 night
        (92, 96, 1.0, 1.7), // 23:00-24:00 late night
    ];

    (0..96)
        .map(|i| {
            // Find the pattern range for this block
            let (base_low, base_high) = base_pattern
                .iter()
                .find(|&&(start, end, _, _)| i >= start && i < end)
                .map(|&(_, _, low, high)| (low, high))
                .unwrap_or((2.0, 3.0));

            // Generate price within range (ensure low < high)
            let price = rng.gen_range(base_low..base_high.max(base_low + 0.1));

            TimeBlockPrice {
                block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            }
        })
        .collect()
}

/// Generate "Negative Prices" pattern
///
/// Contains negative prices during midday (11:00-14:00) simulating
/// high renewable generation periods
fn generate_negative_prices(date: NaiveDate) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    let mut rng = rand::thread_rng();

    (0..96)
        .map(|i| {
            let hour = i / 4;
            let base_price: f32 = match hour {
                0..=5 => 1.50,    // Cheap overnight
                6..=10 => 3.00,   // Morning
                11..=13 => -0.50, // NEGATIVE - solar surplus
                14..=16 => 2.50,  // Afternoon recovery
                17..=20 => 4.50,  // Evening peak
                21..=23 => 2.50,  // Night decline
                _ => 2.50,
            };

            // Add random noise (smaller for negative prices)
            let noise_range: f32 = if base_price < 0.0 { 0.3 } else { 0.15 };
            let noise: f32 = rng.gen_range(-noise_range..noise_range);
            let price = base_price + (base_price.abs() * noise);

            TimeBlockPrice {
                block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            }
        })
        .collect()
}

/// Generate "HDO Optimized" price pattern
///
/// Prices aligned with typical Czech HDO low-tariff periods:
/// - Low prices during HDO low-tariff: 00:00-06:00, 13:00-15:00, 20:00-22:00
/// - Higher prices during high-tariff periods
fn generate_hdo_optimized_prices(date: NaiveDate) -> Vec<TimeBlockPrice> {
    let base_time = date.and_hms_opt(0, 0, 0).unwrap();
    let base_dt = Utc.from_utc_datetime(&base_time);

    let mut rng = rand::thread_rng();

    // HDO low-tariff periods (typical winter)
    let is_hdo_low = |hour: usize| -> bool { matches!(hour, 0..=5 | 13..=14 | 20..=21) };

    (0..96)
        .map(|i| {
            let hour = i / 4;

            let base_price = if is_hdo_low(hour) {
                1.20 // Low price during HDO low-tariff
            } else {
                match hour {
                    6..=8 => 3.50,   // Morning ramp
                    9..=12 => 3.00,  // Midday
                    15..=17 => 3.50, // Afternoon
                    18..=19 => 4.50, // Evening peak
                    22..=23 => 2.50, // Late night
                    _ => 3.00,
                }
            };

            // Add small random noise (+/- 8%)
            let noise = rng.gen_range(-0.08..0.08);
            let price = base_price * (1.0 + noise);

            TimeBlockPrice {
                block_start: base_dt + chrono::Duration::minutes(i as i64 * 15),
                duration_minutes: 15,
                price_czk_per_kwh: price,
                effective_price_czk_per_kwh: price,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usual_day_has_correct_pattern() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let prices = generate_usual_day_prices(date);

        assert_eq!(prices.len(), 96, "Should generate 96 blocks");

        // Night should be cheaper than evening peak
        let night_avg: f32 = prices[0..24]
            .iter()
            .map(|p| p.price_czk_per_kwh)
            .sum::<f32>()
            / 24.0;
        let evening_avg: f32 = prices[68..80]
            .iter()
            .map(|p| p.price_czk_per_kwh)
            .sum::<f32>()
            / 12.0;

        assert!(
            evening_avg > night_avg * 2.0,
            "Evening peak ({:.2}) should be at least 2x overnight ({:.2})",
            evening_avg,
            night_avg
        );
    }

    #[test]
    fn test_negative_prices_scenario() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let prices = generate_negative_prices(date);

        // Check for negative prices during midday (blocks 44-56 = 11:00-14:00)
        let has_negative = prices[44..56].iter().any(|p| p.price_czk_per_kwh < 0.0);

        assert!(has_negative, "Should have negative prices during midday");
    }

    #[test]
    fn test_volatile_prices_have_high_range() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let prices = generate_volatile_prices(date);

        let min = prices
            .iter()
            .map(|p| p.price_czk_per_kwh)
            .fold(f32::INFINITY, f32::min);
        let max = prices
            .iter()
            .map(|p| p.price_czk_per_kwh)
            .fold(f32::NEG_INFINITY, f32::max);

        let range = max - min;
        assert!(
            range > 5.0,
            "Volatile prices should have range > 5 CZK, got {:.2} (min: {:.2}, max: {:.2})",
            range,
            min,
            max
        );
    }

    #[test]
    fn test_all_presets_are_valid() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        for preset in PRICE_PRESETS {
            let prices = preset.scenario.generate_prices(date);
            assert_eq!(
                prices.len(),
                96,
                "Preset '{}' should generate 96 blocks",
                preset.id
            );
        }
    }
}
