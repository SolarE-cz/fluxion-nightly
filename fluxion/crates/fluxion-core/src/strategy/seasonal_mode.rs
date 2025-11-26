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

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

/// Seasonal operating mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeasonalMode {
    /// May-September
    Summer,
    /// October-April
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
            Self::Summer => 20.0, // Aggressive
            Self::Winter => 50.0, // Conservative
        }
    }

    /// Minimum spread threshold for arbitrage (CZK/kWh)
    #[must_use]
    pub fn min_spread_threshold(&self) -> f32 {
        match self {
            Self::Summer => 2.0, // Lower bar
            Self::Winter => 3.0, // Higher bar for profitability
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seasonal_detection_october() {
        let date = DateTime::parse_from_rfc3339("2025-10-14T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(SeasonalMode::from_date(date), SeasonalMode::Winter);
    }

    #[test]
    fn test_seasonal_detection_july() {
        let date = DateTime::parse_from_rfc3339("2025-07-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(SeasonalMode::from_date(date), SeasonalMode::Summer);
    }
}
