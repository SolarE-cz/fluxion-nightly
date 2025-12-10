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

use bevy_ecs::prelude::Resource;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Historical data point with timestamp and value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryDataPoint {
    /// Timestamp of the data point
    pub timestamp: DateTime<Utc>,
    /// Numeric value
    pub value: f32,
}

/// Configuration for consumption history tracking
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionHistoryConfig {
    /// Home Assistant entity ID for daily consumption (e.g., "sensor.solax_today_s_import_energy")
    pub consumption_entity: String,

    /// Home Assistant entity ID for daily solar production (e.g., "sensor.energy_production_today")
    pub solar_production_entity: String,

    /// Number of days to track for EMA calculation
    pub ema_days: usize,

    /// Number of days to track for seasonal mode detection
    pub seasonal_detection_days: usize,
}

impl Default for ConsumptionHistoryConfig {
    fn default() -> Self {
        Self {
            consumption_entity: "sensor.solax_today_s_import_energy".to_string(),
            solar_production_entity: "sensor.energy_production_today".to_string(),
            ema_days: 7,
            seasonal_detection_days: 3,
        }
    }
}

/// Daily energy summary for a specific date
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyEnergySummary {
    /// Date of this summary (midnight UTC)
    pub date: DateTime<Utc>,

    /// Total consumption for the day (kWh)
    pub consumption_kwh: f32,

    /// Total solar production for the day (kWh)
    pub solar_production_kwh: f32,

    /// Total grid import for the day (kWh)
    pub grid_import_kwh: f32,
}

impl DailyEnergySummary {
    /// Calculate energy balance ratio
    /// Positive = deficit (more import than solar)
    /// Negative = surplus (more solar than import)
    pub fn balance_ratio(&self) -> f32 {
        if self.grid_import_kwh == 0.0 {
            return -1.0; // Full surplus
        }
        (self.grid_import_kwh - self.solar_production_kwh) / self.grid_import_kwh
    }

    /// Check if this day meets winter criteria (20% deficit)
    pub fn is_winter_day(&self) -> bool {
        self.balance_ratio() >= 0.20
    }

    /// Check if this day meets summer criteria (20% surplus)
    pub fn is_summer_day(&self) -> bool {
        self.balance_ratio() <= -0.20
    }
}

/// Resource storing historical consumption and energy balance data
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionHistory {
    /// Daily energy summaries (newest first)
    daily_summaries: VecDeque<DailyEnergySummary>,

    /// Maximum number of days to keep
    max_days: usize,

    /// Last time history was updated from HA
    last_update: Option<DateTime<Utc>>,
}

impl Default for ConsumptionHistory {
    fn default() -> Self {
        Self::new(7) // Default to 7 days for EMA
    }
}

impl ConsumptionHistory {
    /// Create new consumption history with specified max days
    pub fn new(max_days: usize) -> Self {
        Self {
            daily_summaries: VecDeque::with_capacity(max_days),
            max_days,
            last_update: None,
        }
    }

    /// Add a daily summary
    pub fn add_summary(&mut self, summary: DailyEnergySummary) {
        // Check if we already have an entry for this date
        let date_str = summary.date.format("%Y-%m-%d").to_string();

        // Remove existing entry for this date if present
        self.daily_summaries
            .retain(|s| s.date.format("%Y-%m-%d").to_string() != date_str);

        // Add new entry at the front (newest first)
        self.daily_summaries.push_front(summary);

        // Maintain size limit
        while self.daily_summaries.len() > self.max_days {
            self.daily_summaries.pop_back();
        }

        self.last_update = Some(Utc::now());
    }

    /// Get all summaries (newest first)
    pub fn summaries(&self) -> &VecDeque<DailyEnergySummary> {
        &self.daily_summaries
    }

    /// Get consumption values for EMA calculation (newest first)
    pub fn consumption_values(&self) -> Vec<f32> {
        self.daily_summaries
            .iter()
            .map(|s| s.consumption_kwh)
            .collect()
    }

    /// Get the last N days for seasonal detection (newest first)
    pub fn last_n_days(&self, n: usize) -> Vec<&DailyEnergySummary> {
        self.daily_summaries.iter().take(n).collect()
    }

    /// Check if we have enough data for EMA calculation
    pub fn has_sufficient_data(&self, required_days: usize) -> bool {
        self.daily_summaries.len() >= required_days
    }

    /// Get last update time
    pub fn last_update(&self) -> Option<DateTime<Utc>> {
        self.last_update
    }

    /// Check if history needs refresh (older than 1 hour)
    pub fn needs_refresh(&self) -> bool {
        match self.last_update {
            Some(last) => {
                let age = Utc::now().signed_duration_since(last);
                age > Duration::hours(1)
            }
            None => true, // Never updated
        }
    }

    /// Clear all history
    pub fn clear(&mut self) {
        self.daily_summaries.clear();
        self.last_update = None;
    }
}

/// Utility function to calculate daily consumption from HA history data
///
/// For sensors that reset at midnight, this takes the LAST value before reset
/// (which represents the daily total) for each day.
///
/// # Arguments
/// * `history_points` - Historical data points
///
/// # Returns
/// Vector of daily summaries, newest first
pub fn aggregate_daily_consumption(
    history_points: &[HistoryDataPoint],
    solar_points: &[HistoryDataPoint],
) -> Vec<DailyEnergySummary> {
    use std::collections::HashMap;

    // Helper to get daily max values
    let get_daily_max = |points: &[HistoryDataPoint]| -> HashMap<String, f32> {
        let mut map: HashMap<String, f32> = HashMap::new();
        for point in points {
            let date_key = point.timestamp.format("%Y-%m-%d").to_string();
            map.entry(date_key)
                .and_modify(|max| *max = max.max(point.value))
                .or_insert(point.value);
        }
        map
    };

    let consumption_map = get_daily_max(history_points);
    let solar_map = get_daily_max(solar_points);

    // Convert to daily summaries
    let mut summaries: Vec<DailyEnergySummary> = consumption_map
        .into_iter()
        .filter_map(|(date_str, consumption)| {
            // Parse date
            let date = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)?
                .and_utc();

            let solar_production = *solar_map.get(&date_str).unwrap_or(&0.0);

            // Only include if consumption is reasonable (0-200 kWh per day)
            if (0.0..200.0).contains(&consumption) {
                Some(DailyEnergySummary {
                    date,
                    consumption_kwh: consumption,
                    solar_production_kwh: solar_production,
                    grid_import_kwh: consumption, // For now assuming consumption is import, logic can be refined
                })
            } else {
                // Warning would require tracing, skipping for now in types crate
                None
            }
        })
        .collect();

    // Sort by date (newest first)
    summaries.sort_by(|a, b| b.date.cmp(&a.date));

    summaries
}
