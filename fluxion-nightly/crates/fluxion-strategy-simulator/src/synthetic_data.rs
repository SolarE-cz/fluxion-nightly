// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Synthetic test day generation for strategy simulation.
//!
//! This module provides configurable consumption and solar profiles
//! that can be combined with price scenarios to create realistic
//! test days for strategy evaluation.

use crate::price_scenarios::PriceScenario;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Configuration for generating a synthetic test day
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticDayConfig {
    /// Date for the synthetic day (used for timestamps)
    pub date: NaiveDate,

    /// Consumption profile configuration
    pub consumption: ConsumptionProfile,

    /// Solar generation profile (optional - defaults to zero for winter testing)
    pub solar: SolarProfile,

    /// Price scenario to use
    pub price_scenario: PriceScenario,

    /// Initial battery state of charge (0-100%)
    pub initial_soc: f32,

    /// Battery capacity in kWh
    pub battery_capacity_kwh: f32,

    /// HDO low tariff periods as (start_hour, end_hour) tuples
    /// If None, uses default Czech HDO schedule
    pub hdo_periods: Option<Vec<(u8, u8)>>,

    /// HDO low tariff grid fee (CZK/kWh)
    pub hdo_low_tariff_czk: f32,

    /// HDO high tariff grid fee (CZK/kWh)
    pub hdo_high_tariff_czk: f32,
}

impl Default for SyntheticDayConfig {
    fn default() -> Self {
        Self {
            date: Utc::now().date_naive(),
            consumption: ConsumptionProfile::default(),
            solar: SolarProfile::default(),
            price_scenario: PriceScenario::UsualDay,
            initial_soc: 50.0,
            battery_capacity_kwh: 10.0,
            // Default Czech HDO low tariff periods (typical winter schedule)
            hdo_periods: Some(vec![
                (0, 6),   // Night: 00:00-06:00
                (13, 15), // Midday: 13:00-15:00
                (20, 22), // Evening: 20:00-22:00
            ]),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        }
    }
}

/// Consumption profile types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConsumptionProfile {
    /// Constant consumption throughout the day
    Constant {
        /// Load in kW (constant power draw)
        load_kw: f32,
    },

    /// Configurable peak-based profile (default)
    /// Base load during off-peak, higher load during peak hours
    PeakBased {
        /// Base load during non-peak hours (kW)
        base_load_kw: f32,
        /// Load during peak hours (kW)
        peak_load_kw: f32,
        /// Peak hours as list of (start_hour, end_hour)
        peak_periods: Vec<(u8, u8)>,
    },

    /// Realistic residential pattern with distinct periods
    Residential {
        /// Morning peak load (6-9) in kW
        morning_load_kw: f32,
        /// Midday base load (9-17) in kW
        midday_load_kw: f32,
        /// Evening peak load (17-22) in kW
        evening_load_kw: f32,
        /// Night base load (22-6) in kW
        night_load_kw: f32,
    },

    /// Custom per-block consumption (96 values for 15-minute blocks)
    Custom {
        /// 96 values for each 15-minute block (kW)
        blocks_kw: Vec<f32>,
    },
}

impl Default for ConsumptionProfile {
    fn default() -> Self {
        // User requirement: base 1.0 kWh/hour overnight, 4.0 kWh/hour peaks
        Self::PeakBased {
            base_load_kw: 1.0, // 1.0 kW = 0.25 kWh per 15-min block
            peak_load_kw: 4.0, // 4.0 kW = 1.0 kWh per 15-min block
            peak_periods: vec![
                (7, 8),   // 7:00-8:00
                (10, 11), // 10:00-11:00
                (14, 15), // 14:00-15:00
                (17, 18), // 17:00-18:00
            ],
        }
    }
}

impl ConsumptionProfile {
    /// Generate consumption for a specific block (in kWh for 15-minute period)
    pub fn consumption_for_block(&self, block_index: usize) -> f32 {
        let hour = block_index / 4; // 4 blocks per hour

        match self {
            Self::Constant { load_kw } => load_kw * 0.25, // 15 min = 0.25 hour

            Self::PeakBased {
                base_load_kw,
                peak_load_kw,
                peak_periods,
            } => {
                let is_peak = peak_periods
                    .iter()
                    .any(|&(start, end)| hour >= start as usize && hour < end as usize);

                let load_kw = if is_peak {
                    *peak_load_kw
                } else {
                    *base_load_kw
                };
                load_kw * 0.25 // Convert to kWh for 15-min block
            }

            Self::Residential {
                morning_load_kw,
                midday_load_kw,
                evening_load_kw,
                night_load_kw,
            } => {
                let load_kw = match hour {
                    0..=5 => *night_load_kw,
                    6..=8 => *morning_load_kw,
                    9..=16 => *midday_load_kw,
                    17..=21 => *evening_load_kw,
                    22..=23 => *night_load_kw,
                    _ => *night_load_kw,
                };
                load_kw * 0.25
            }

            Self::Custom { blocks_kw } => blocks_kw.get(block_index).copied().unwrap_or(1.0) * 0.25,
        }
    }

    /// Get total daily consumption in kWh
    pub fn total_daily_consumption_kwh(&self) -> f32 {
        (0..96).map(|i| self.consumption_for_block(i)).sum()
    }
}

/// Solar generation profile
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SolarProfile {
    /// No solar (winter/cloudy day testing)
    #[default]
    None,

    /// Bell curve centered on midday
    Typical {
        /// Peak generation in kW
        peak_kw: f32,
        /// Hour when generation starts (e.g., 7 for 7:00)
        sunrise_hour: u8,
        /// Hour when generation ends (e.g., 17 for 17:00)
        sunset_hour: u8,
    },

    /// Custom per-block generation (96 values)
    Custom {
        /// 96 values for each 15-minute block (kW)
        blocks_kw: Vec<f32>,
    },
}

impl SolarProfile {
    /// No solar generation (winter/cloudy day)
    pub fn none() -> Self {
        SolarProfile::None
    }

    /// Moderate solar - typical spring/fall day
    /// ~3 kW peak, 7am-6pm, ~12 kWh/day total
    pub fn moderate() -> Self {
        SolarProfile::Typical {
            peak_kw: 3.0,
            sunrise_hour: 7,
            sunset_hour: 18,
        }
    }

    /// High solar - summer day
    /// ~5 kW peak, 5am-9pm, ~25 kWh/day total
    pub fn high() -> Self {
        SolarProfile::Typical {
            peak_kw: 5.0,
            sunrise_hour: 5,
            sunset_hour: 21,
        }
    }

    /// Generate solar production for a specific block (in kWh for 15-minute period)
    pub fn generation_for_block(&self, block_index: usize) -> f32 {
        match self {
            Self::None => 0.0,

            Self::Typical {
                peak_kw,
                sunrise_hour,
                sunset_hour,
            } => {
                let hour = block_index / 4;
                let minute_in_day = block_index * 15;

                let sunrise_minute = (*sunrise_hour as usize) * 60;
                let sunset_minute = (*sunset_hour as usize) * 60;

                if hour < *sunrise_hour as usize || hour >= *sunset_hour as usize {
                    return 0.0;
                }

                // Bell curve: sin-based generation profile
                let day_length = sunset_minute - sunrise_minute;
                let progress = (minute_in_day - sunrise_minute) as f32 / day_length as f32;

                // Sin curve from 0 to PI gives smooth bell shape
                let factor = (progress * std::f32::consts::PI).sin();

                peak_kw * factor * 0.25 // Convert to kWh for 15-min block
            }

            Self::Custom { blocks_kw } => blocks_kw.get(block_index).copied().unwrap_or(0.0) * 0.25,
        }
    }

    /// Get total daily generation in kWh
    pub fn total_daily_generation_kwh(&self) -> f32 {
        (0..96).map(|i| self.generation_for_block(i)).sum()
    }
}

/// Generated synthetic day data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticDay {
    /// Date of this synthetic day
    pub date: NaiveDate,

    /// 96 blocks (15-minute intervals for 24 hours)
    pub blocks: Vec<SyntheticBlock>,

    /// Name of the price scenario used
    pub price_scenario_name: String,

    /// Total consumption for the day (kWh)
    pub total_consumption_kwh: f32,

    /// Total solar generation for the day (kWh)
    pub total_solar_kwh: f32,

    /// Initial battery SOC for the day
    pub initial_soc: f32,

    /// Battery capacity (kWh)
    pub battery_capacity_kwh: f32,
}

/// A single 15-minute block of synthetic data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticBlock {
    /// Block index (0-95)
    pub index: usize,

    /// Timestamp for this block
    pub timestamp: DateTime<Utc>,

    /// Consumption for this block (kWh)
    pub consumption_kwh: f32,

    /// Solar generation for this block (kWh)
    pub solar_kwh: f32,

    /// Spot price for this block (CZK/kWh)
    pub price_czk_per_kwh: f32,

    /// Grid fee for this block (CZK/kWh) - HDO dependent
    pub grid_fee_czk_per_kwh: f32,

    /// Effective price (spot + grid fee) (CZK/kWh)
    pub effective_price_czk_per_kwh: f32,

    /// Whether this block is in HDO low-tariff period
    pub is_hdo_low_tariff: bool,
}

/// Generator for synthetic test days
pub struct SyntheticDayGenerator;

impl SyntheticDayGenerator {
    /// Generate a synthetic day from configuration
    pub fn generate(config: &SyntheticDayConfig) -> Result<SyntheticDay> {
        let date = config.date;
        let base_time = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid date"))?;
        let base_dt = Utc.from_utc_datetime(&base_time);

        // Generate spot prices from scenario
        let spot_prices = config.price_scenario.generate_prices(date);

        // Build blocks
        let mut blocks = Vec::with_capacity(96);
        let mut total_consumption = 0.0;
        let mut total_solar = 0.0;

        for i in 0..96 {
            let timestamp = base_dt + chrono::Duration::minutes(i as i64 * 15);
            let hour = i / 4;

            // Get consumption and solar for this block
            let consumption_kwh = config.consumption.consumption_for_block(i);
            let solar_kwh = config.solar.generation_for_block(i);

            total_consumption += consumption_kwh;
            total_solar += solar_kwh;

            // Get spot price
            let price_czk_per_kwh = spot_prices
                .get(i)
                .map(|p| p.price_czk_per_kwh)
                .unwrap_or(2.5);

            // Determine HDO tariff
            let is_hdo_low = config.hdo_periods.as_ref().is_some_and(|periods| {
                periods
                    .iter()
                    .any(|&(start, end)| hour >= start as usize && hour < end as usize)
            });

            let grid_fee_czk_per_kwh = if is_hdo_low {
                config.hdo_low_tariff_czk
            } else {
                config.hdo_high_tariff_czk
            };

            let effective_price_czk_per_kwh = price_czk_per_kwh + grid_fee_czk_per_kwh;

            blocks.push(SyntheticBlock {
                index: i,
                timestamp,
                consumption_kwh,
                solar_kwh,
                price_czk_per_kwh,
                grid_fee_czk_per_kwh,
                effective_price_czk_per_kwh,
                is_hdo_low_tariff: is_hdo_low,
            });
        }

        Ok(SyntheticDay {
            date,
            blocks,
            price_scenario_name: config.price_scenario.name().to_string(),
            total_consumption_kwh: total_consumption,
            total_solar_kwh: total_solar,
            initial_soc: config.initial_soc,
            battery_capacity_kwh: config.battery_capacity_kwh,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_consumption_profile_generates_expected_peaks() {
        let profile = ConsumptionProfile::default();

        // Block 28 = 7:00 (start of peak)
        let peak_consumption = profile.consumption_for_block(28);
        // Block 0 = 0:00 (off-peak)
        let base_consumption = profile.consumption_for_block(0);

        // Peak should be 4x base (4kW vs 1kW)
        assert!(
            (peak_consumption / base_consumption - 4.0).abs() < 0.01,
            "Peak should be 4x base: {} / {} = {}",
            peak_consumption,
            base_consumption,
            peak_consumption / base_consumption
        );
    }

    #[test]
    fn test_synthetic_day_generates_96_blocks() {
        let config = SyntheticDayConfig::default();
        let day = SyntheticDayGenerator::generate(&config).unwrap();

        assert_eq!(
            day.blocks.len(),
            96,
            "Should generate 96 blocks for 24 hours"
        );
    }

    #[test]
    fn test_hdo_periods_affect_grid_fee() {
        let config = SyntheticDayConfig::default();
        let day = SyntheticDayGenerator::generate(&config).unwrap();

        // Block 8 = 2:00 (should be low tariff - within 0-6)
        let night_block = &day.blocks[8];
        assert!(
            night_block.is_hdo_low_tariff,
            "Block at 2:00 should be low tariff"
        );
        assert!(
            (night_block.grid_fee_czk_per_kwh - 0.50).abs() < 0.01,
            "Low tariff should be 0.50 CZK"
        );

        // Block 40 = 10:00 (should be high tariff)
        let day_block = &day.blocks[40];
        assert!(
            !day_block.is_hdo_low_tariff,
            "Block at 10:00 should be high tariff"
        );
        assert!(
            (day_block.grid_fee_czk_per_kwh - 1.80).abs() < 0.01,
            "High tariff should be 1.80 CZK"
        );
    }

    #[test]
    fn test_typical_solar_profile_bell_curve() {
        let solar = SolarProfile::Typical {
            peak_kw: 5.0,
            sunrise_hour: 7,
            sunset_hour: 17,
        };

        // Before sunrise - no generation
        assert_eq!(
            solar.generation_for_block(20), // 5:00
            0.0,
            "No generation before sunrise"
        );

        // At midday - peak generation
        let midday_gen = solar.generation_for_block(48); // 12:00
        assert!(
            midday_gen > 1.0,
            "Should have significant generation at midday: {}",
            midday_gen
        );

        // After sunset - no generation
        assert_eq!(
            solar.generation_for_block(72), // 18:00
            0.0,
            "No generation after sunset"
        );
    }
}
