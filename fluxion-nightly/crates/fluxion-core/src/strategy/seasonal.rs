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

//! Seasonal mode detection and energy balance tracking
//!
//! Provides utilities for detecting whether the system is operating in
//! "summer" or "winter" mode based on solar production and grid import
//! patterns. This affects strategy behavior such as minimum SOC thresholds
//! and arbitrage spread requirements.
//!
//! ## Seasonal Modes
//!
//! - **Summer** (May-September): More solar production, lower minimum SOC
//! - **Winter** (October-April): Less solar, higher minimum SOC for safety

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

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
    pub date: DateTime<Utc>,
    pub solar_production_kwh: f32,
    pub grid_import_kwh: f32,
}

impl DayEnergyBalance {
    /// Calculate the deficit ratio: (import - solar) / import
    /// Positive means deficit (more import than solar)
    /// Negative means surplus (more solar than import)
    pub fn deficit_ratio(&self) -> f32 {
        if self.grid_import_kwh == 0.0 {
            return -1.0;
        }
        (self.grid_import_kwh - self.solar_production_kwh) / self.grid_import_kwh
    }

    /// Returns true if this was a deficit day (imported 20%+ more than solar produced)
    pub fn is_deficit_day(&self) -> bool {
        self.deficit_ratio() >= 0.20
    }

    /// Returns true if this was a surplus day (solar exceeded imports by 20%+)
    pub fn is_surplus_day(&self) -> bool {
        self.deficit_ratio() <= -0.20
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
    fn test_seasonal_mode_min_soc() {
        assert_eq!(SeasonalMode::Summer.min_soc_percent(), 20.0);
        assert_eq!(SeasonalMode::Winter.min_soc_percent(), 50.0);
    }

    #[test]
    fn test_seasonal_mode_min_spread() {
        assert_eq!(SeasonalMode::Summer.min_spread_threshold(), 2.0);
        assert_eq!(SeasonalMode::Winter.min_spread_threshold(), 3.0);
    }

    #[test]
    fn test_day_energy_balance_deficit() {
        let deficit_day = DayEnergyBalance {
            date: Utc::now(),
            solar_production_kwh: 5.0,
            grid_import_kwh: 10.0,
        };
        assert!(deficit_day.is_deficit_day());
        assert!(!deficit_day.is_surplus_day());

        let surplus_day = DayEnergyBalance {
            date: Utc::now(),
            solar_production_kwh: 15.0,
            grid_import_kwh: 10.0,
        };
        assert!(!surplus_day.is_deficit_day());
        assert!(surplus_day.is_surplus_day());
    }
}
