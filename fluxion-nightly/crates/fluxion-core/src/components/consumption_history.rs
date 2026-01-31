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

use bevy_ecs::prelude::*;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// Import ConsumptionHistoryConfig and HourlyConsumptionProfile from fluxion-types
pub use fluxion_types::history::{ConsumptionHistoryConfig, HourlyConsumptionProfile};

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

    /// Cached hourly consumption profile, computed from daily summaries
    hourly_profile: Option<HourlyConsumptionProfile>,
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
            hourly_profile: None,
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
        self.hourly_profile = None;
    }

    /// Get the cached hourly consumption profile, if available
    pub fn hourly_profile(&self) -> Option<&HourlyConsumptionProfile> {
        self.hourly_profile.as_ref()
    }

    /// Set the hourly consumption profile
    pub fn set_hourly_profile(&mut self, profile: HourlyConsumptionProfile) {
        self.hourly_profile = Some(profile);
    }

    /// Recompute the hourly consumption profile from daily summaries.
    ///
    /// Since we only have daily totals, this distributes each day's consumption
    /// evenly across 24 hours and averages over all available days.
    /// Returns `None` if there are no summaries.
    pub fn recompute_hourly_profile(&mut self) {
        if self.daily_summaries.is_empty() {
            self.hourly_profile = None;
            return;
        }

        let num_days = self.daily_summaries.len();
        // With only daily totals, distribute consumption uniformly across hours
        let total_daily_avg: f32 =
            self.daily_summaries.iter().map(|s| s.consumption_kwh).sum::<f32>() / num_days as f32;
        let hourly_avg = total_daily_avg / 24.0;

        self.hourly_profile = Some(HourlyConsumptionProfile {
            hourly_avg_kwh: [hourly_avg; 24],
            days_averaged: num_days,
        });
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
    history_points: &[crate::traits::HistoryDataPoint],
    solar_points: &[crate::traits::HistoryDataPoint],
) -> Vec<DailyEnergySummary> {
    use std::collections::HashMap;

    // Helper to get daily max values
    let get_daily_max = |points: &[crate::traits::HistoryDataPoint]| -> HashMap<String, f32> {
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
                tracing::warn!(
                    "Skipping unreasonable consumption value: {:.2} kWh for {}",
                    consumption,
                    date_str
                );
                None
            }
        })
        .collect();

    // Sort by date (newest first)
    summaries.sort_by(|a, b| b.date.cmp(&a.date));

    summaries
}

/// Compute average hourly consumption from raw HA history data points.
///
/// The sensor is cumulative (resets at midnight), so we compute hourly deltas
/// by finding the max value in each hour and subtracting the previous hour's max.
///
/// Returns None if insufficient data (no points).
pub fn aggregate_hourly_consumption(
    history_points: &[crate::traits::HistoryDataPoint],
) -> Option<HourlyConsumptionProfile> {
    use chrono::Timelike;
    use std::collections::HashMap;

    if history_points.is_empty() {
        return None;
    }

    // Group points by date (YYYY-MM-DD string key)
    let mut by_date: HashMap<String, Vec<&crate::traits::HistoryDataPoint>> = HashMap::new();
    for point in history_points {
        let date_key = point.timestamp.format("%Y-%m-%d").to_string();
        by_date.entry(date_key).or_default().push(point);
    }

    // For each hour across all days, collect deltas
    let mut hour_deltas: Vec<Vec<f32>> = vec![Vec::new(); 24];
    let mut total_days = 0usize;

    for points in by_date.values() {
        // Group points by hour within this day
        let mut by_hour: HashMap<u32, f32> = HashMap::new();
        for point in points {
            let hour = point.timestamp.hour();
            by_hour
                .entry(hour)
                .and_modify(|max| *max = max.max(point.value))
                .or_insert(point.value);
        }

        if by_hour.is_empty() {
            continue;
        }
        total_days += 1;

        // Compute deltas for each hour
        for h in 0..24u32 {
            if let Some(&max_this_hour) = by_hour.get(&h) {
                let delta = if h == 0 {
                    // Sensor starts at 0 after midnight reset
                    max_this_hour
                } else {
                    // Find the previous hour that has data
                    let mut prev_max = None;
                    for prev_h in (0..h).rev() {
                        if let Some(&val) = by_hour.get(&prev_h) {
                            prev_max = Some(val);
                            break;
                        }
                    }
                    match prev_max {
                        Some(prev) => max_this_hour - prev,
                        None => max_this_hour, // No previous hour data, treat like hour 0
                    }
                };

                // Only include non-negative deltas (cumulative sensor should only go up)
                if delta >= 0.0 {
                    hour_deltas[h as usize].push(delta);
                }
            }
        }
    }

    // Average each hour's deltas
    let mut hourly_avg_kwh = [0.0f32; 24];
    for (h, deltas) in hour_deltas.iter().enumerate() {
        if !deltas.is_empty() {
            let sum: f32 = deltas.iter().sum();
            hourly_avg_kwh[h] = sum / deltas.len() as f32;
        }
    }

    Some(HourlyConsumptionProfile {
        hourly_avg_kwh,
        days_averaged: total_days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::HistoryDataPoint;
    use chrono::Timelike;

    #[test]
    fn test_consumption_history_add() {
        let mut history = ConsumptionHistory::new(7);

        let summary = DailyEnergySummary {
            date: Utc::now(),
            consumption_kwh: 25.0,
            solar_production_kwh: 5.0,
            grid_import_kwh: 20.0,
        };

        history.add_summary(summary);
        assert_eq!(history.summaries().len(), 1);
        assert!(history.last_update().is_some());
    }

    #[test]
    fn test_consumption_history_size_limit() {
        let mut history = ConsumptionHistory::new(3);

        for i in 0..5 {
            let summary = DailyEnergySummary {
                date: Utc::now() - Duration::days(i),
                consumption_kwh: 20.0 + i as f32,
                solar_production_kwh: 5.0,
                grid_import_kwh: 15.0,
            };
            history.add_summary(summary);
        }

        assert_eq!(history.summaries().len(), 3);
    }

    #[test]
    fn test_daily_energy_balance() {
        let winter_day = DailyEnergySummary {
            date: Utc::now(),
            consumption_kwh: 25.0,
            solar_production_kwh: 5.0,
            grid_import_kwh: 25.0,
        };

        assert!(winter_day.is_winter_day());
        assert!(!winter_day.is_summer_day());

        let summer_day = DailyEnergySummary {
            date: Utc::now(),
            consumption_kwh: 20.0,
            solar_production_kwh: 30.0,
            grid_import_kwh: 15.0,
        };

        assert!(!summer_day.is_winter_day());
        assert!(summer_day.is_summer_day());
    }

    #[test]
    fn test_aggregate_daily_consumption() {
        let now = Utc::now();
        let history_points = vec![
            HistoryDataPoint {
                timestamp: now.with_hour(0).unwrap(),
                value: 2.5,
            }, // Morning
            HistoryDataPoint {
                timestamp: now.with_hour(12).unwrap(),
                value: 12.5,
            }, // Midday
            HistoryDataPoint {
                timestamp: now.with_hour(23).unwrap(),
                value: 25.0,
            }, // End of day (just before reset)
        ];
        let solar_points = vec![];

        let summaries = aggregate_daily_consumption(&history_points, &solar_points);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].consumption_kwh, 25.0); // Max value (last before reset)
    }

    #[test]
    fn test_aggregate_multiple_days() {
        let today = Utc::now();
        let yesterday = today - Duration::days(1);
        let two_days_ago = today - Duration::days(2);

        let history_points = vec![
            // Two days ago
            HistoryDataPoint {
                timestamp: two_days_ago.with_hour(23).unwrap(),
                value: 20.0,
            },
            // Yesterday
            HistoryDataPoint {
                timestamp: yesterday.with_hour(23).unwrap(),
                value: 22.0,
            },
            // Today
            HistoryDataPoint {
                timestamp: today.with_hour(12).unwrap(),
                value: 10.0,
            },
            HistoryDataPoint {
                timestamp: today.with_hour(23).unwrap(),
                value: 25.0,
            },
        ];
        let solar_points = vec![];

        let summaries = aggregate_daily_consumption(&history_points, &solar_points);
        assert_eq!(summaries.len(), 3);

        // Should be sorted newest first
        assert_eq!(summaries[0].consumption_kwh, 25.0); // Today
        assert_eq!(summaries[1].consumption_kwh, 22.0); // Yesterday
        assert_eq!(summaries[2].consumption_kwh, 20.0); // Two days ago
    }
}
