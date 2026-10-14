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
        let total_daily_avg: f32 = self
            .daily_summaries
            .iter()
            .map(|s| s.consumption_kwh)
            .sum::<f32>()
            / num_days as f32;
        let hourly_avg = total_daily_avg / 24.0;

        self.hourly_profile = Some(HourlyConsumptionProfile {
            hourly_avg_kwh: [hourly_avg; 24],
            days_averaged: num_days,
            hourly_p75_kwh: None,
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
    if history_points.is_empty() {
        return None;
    }

    // Max reading per absolute UTC hour. Deltas are taken against the previous consecutive
    // hour across date boundaries, because the "today" sensor resets at *local* midnight,
    // which is not a UTC date boundary.
    let mut by_hour: std::collections::BTreeMap<chrono::DateTime<chrono::Utc>, f32> =
        std::collections::BTreeMap::new();
    for point in history_points {
        let Some(hour_start) = point
            .timestamp
            .with_minute(0)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
        else {
            continue;
        };
        by_hour
            .entry(hour_start)
            .and_modify(|max| *max = max.max(point.value))
            .or_insert(point.value);
    }

    let mut hour_deltas: Vec<Vec<f32>> = vec![Vec::new(); 24];
    let mut days = std::collections::HashSet::new();
    let mut prev: Option<(chrono::DateTime<chrono::Utc>, f32)> = None;
    for (&hour_start, &max_this_hour) in &by_hour {
        days.insert(hour_start.date_naive());
        if let Some((prev_start, prev_max)) = prev
            && hour_start - prev_start == chrono::Duration::hours(1)
        {
            let delta = max_this_hour - prev_max;
            if delta >= 0.0 {
                hour_deltas[hour_start.hour() as usize].push(delta);
            } else if max_this_hour < 0.5 * prev_max {
                // A large drop means the sensor reset at the start of this hour: the reading is
                // the energy since the reset. Small drops are glitches and are skipped.
                hour_deltas[hour_start.hour() as usize].push(max_this_hour);
            }
        }
        prev = Some((hour_start, max_this_hour));
    }
    let total_days = days.len();

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
        hourly_p75_kwh: None,
    })
}

/// Minimum number of days of house-load data before its profile replaces the consumption one.
pub const MIN_HOUSE_LOAD_DAYS: usize = 3;

/// Gaps longer than this between house-load samples are not integrated.
const HOUSE_LOAD_MAX_GAP_MINUTES: i64 = 30;

/// Per-hour p75 is capped at this multiple of the hourly mean. The margin covers evening
/// variability; rare spikes (EV charging, heat-pump boosts on a few days) must not drive the
/// planner's load, so one or two such days cannot pull p75 far above a normal day.
const HOUSE_LOAD_P75_CAP_FACTOR: f32 = 1.5;

/// Hourly house-load profile from a house-load power sensor (W).
///
/// Each sample holds until the next one (time-weighted). Negative readings are clamped to 0 and
/// intervals longer than 30 min are skipped. For each UTC day and hour the time-weighted mean power
/// in kW (which equals kWh/h) is taken over the covered part of that hour. `hourly_avg_kwh` is the
/// mean of those values across days and `hourly_p75_kwh` their 75th percentile across days, capped
/// at `HOUSE_LOAD_P75_CAP_FACTOR` times the mean. An hour with no samples on any day takes the mean
/// of its neighbouring hours (wrapping around midnight), or the mean of all observed hours when both
/// neighbours are empty; its p75 equals that value. Returns None if there are no samples.
pub fn aggregate_hourly_house_load(
    power_points: &[crate::traits::HistoryDataPoint],
) -> Option<HourlyConsumptionProfile> {
    use chrono::Timelike;
    use std::collections::BTreeMap;

    let mut sorted: Vec<&crate::traits::HistoryDataPoint> = power_points.iter().collect();
    sorted.sort_by_key(|p| p.timestamp);

    // (UTC day, hour) -> (energy kWh, covered hours)
    let mut cells: BTreeMap<(chrono::NaiveDate, u32), (f64, f64)> = BTreeMap::new();
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let gap = b.timestamp - a.timestamp;
        if gap <= Duration::zero() || gap > Duration::minutes(HOUSE_LOAD_MAX_GAP_MINUTES) {
            continue;
        }
        let kw = f64::from(a.value.max(0.0)) / 1000.0;
        let mut t = a.timestamp;
        while t < b.timestamp {
            let Some(hour_start) = t
                .date_naive()
                .and_hms_opt(t.hour(), 0, 0)
                .map(|d| d.and_utc())
            else {
                break;
            };
            let segment_end = (hour_start + Duration::hours(1)).min(b.timestamp);
            let hours = (segment_end - t).num_milliseconds() as f64 / 3_600_000.0;
            let cell = cells
                .entry((t.date_naive(), t.hour()))
                .or_insert((0.0, 0.0));
            cell.0 += kw * hours;
            cell.1 += hours;
            t = segment_end;
        }
    }

    // UTC day -> mean kW per hour (None where the day has no data for that hour)
    let mut days: BTreeMap<chrono::NaiveDate, [Option<f32>; 24]> = BTreeMap::new();
    for ((day, hour), (energy_kwh, hours)) in cells {
        if hours > 0.0 {
            days.entry(day).or_insert([None; 24])[hour as usize] =
                Some((energy_kwh / hours) as f32);
        }
    }
    if days.is_empty() {
        return None;
    }

    // Observed hours: (mean, capped p75)
    let mut observed: [Option<(f32, f32)>; 24] = [None; 24];
    for (h, slot) in observed.iter_mut().enumerate() {
        let mut values: Vec<f32> = days.values().filter_map(|d| d[h]).collect();
        if values.is_empty() {
            continue;
        }
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let p75 = percentile(&mut values, 0.75).min(mean * HOUSE_LOAD_P75_CAP_FACTOR);
        *slot = Some((mean, p75));
    }
    let observed_means: Vec<f32> = observed.iter().flatten().map(|(mean, _)| *mean).collect();
    let all_hours_mean = observed_means.iter().sum::<f32>() / observed_means.len() as f32;

    let mut hourly_avg_kwh = [0.0f32; 24];
    let mut hourly_p75_kwh = [0.0f32; 24];
    for h in 0..24 {
        let (mean, p75) = match observed[h] {
            Some(values) => values,
            None => {
                let prev = observed[(h + 23) % 24].map(|(mean, _)| mean);
                let next = observed[(h + 1) % 24].map(|(mean, _)| mean);
                let fill = match (prev, next) {
                    (Some(a), Some(b)) => (a + b) / 2.0,
                    (Some(a), None) | (None, Some(a)) => a,
                    (None, None) => all_hours_mean,
                };
                (fill, fill)
            }
        };
        hourly_avg_kwh[h] = mean;
        hourly_p75_kwh[h] = p75;
    }

    Some(HourlyConsumptionProfile {
        hourly_avg_kwh,
        days_averaged: days.len(),
        hourly_p75_kwh: Some(hourly_p75_kwh),
    })
}

/// Linear-interpolated percentile `q` (0..=1) of a non-empty slice. Sorts the slice.
fn percentile(values: &mut [f32], q: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    let pos = q * (values.len() - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    values[lo] + (values[hi] - values[lo]) * (pos - lo as f32)
}

/// Picks the hourly profile used by the planner and the predictor: the house-load profile when it
/// has at least `MIN_HOUSE_LOAD_DAYS` days of data, otherwise the consumption-based profile.
pub fn pick_hourly_profile(
    house_load: Option<HourlyConsumptionProfile>,
    consumption: Option<HourlyConsumptionProfile>,
) -> Option<HourlyConsumptionProfile> {
    match house_load {
        Some(profile) if profile.days_averaged >= MIN_HOUSE_LOAD_DAYS => Some(profile),
        _ => consumption,
    }
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

    #[test]
    fn test_hourly_profile_handles_local_midnight_reset() {
        use chrono::{TimeZone, Utc};
        // Constant 1 kWh/h; the cumulative "today" sensor resets at 23:00 UTC (local midnight, CET).
        let mut points = Vec::new();
        let start = Utc.with_ymd_and_hms(2026, 1, 10, 23, 0, 0).unwrap();
        for i in 0..(5 * 24 * 4) {
            let t = start + chrono::Duration::minutes(15 * i);
            let minutes_since_reset = (t - start).num_minutes() % (24 * 60);
            points.push(crate::traits::HistoryDataPoint {
                timestamp: t,
                value: (minutes_since_reset + 15) as f32 / 60.0,
            });
        }
        let profile = aggregate_hourly_consumption(&points).unwrap();
        for (h, kwh) in profile.hourly_avg_kwh.iter().enumerate() {
            assert!((kwh - 1.0).abs() < 0.05, "hour {h}: {kwh}");
        }
    }

    fn power_point(day: u32, hour: u32, minute: u32, watts: f32) -> HistoryDataPoint {
        use chrono::TimeZone;
        HistoryDataPoint {
            timestamp: chrono::Utc
                .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
                .unwrap(),
            value: watts,
        }
    }

    #[test]
    fn house_load_is_time_weighted_and_skips_gaps() {
        // 10:00 1 kW for 20 min, 10:20 3 kW for 30 min, then a 50 min gap (not integrated)
        let points = vec![
            power_point(14, 10, 0, 1000.0),
            power_point(14, 10, 20, 3000.0),
            power_point(14, 10, 50, 2000.0),
            power_point(14, 11, 40, 500.0),
        ];
        let profile = aggregate_hourly_house_load(&points).unwrap();
        assert_eq!(profile.days_averaged, 1);
        // Energy 1/3 + 1.5 = 11/6 kWh over 5/6 h covered = 2.2 kW
        assert!((profile.hourly_avg_kwh[10] - 2.2).abs() < 1e-3);
        // Hour 11 has no samples on its own: it takes hour 10 (the only observed neighbour)
        assert!((profile.hourly_avg_kwh[11] - 2.2).abs() < 1e-3);
        assert!((profile.hourly_p75_kwh.unwrap()[11] - 2.2).abs() < 1e-3);
    }

    #[test]
    fn house_load_p75_across_seven_days() {
        // Hour 8 holds a constant 1..7 kW on each of 7 days
        let mut points = Vec::new();
        for (i, day) in (14..=20).enumerate() {
            let kw = (i + 1) as f32 * 1000.0;
            points.push(power_point(day, 8, 0, kw));
            points.push(power_point(day, 8, 30, kw));
            points.push(power_point(day, 9, 0, kw));
        }
        let profile = aggregate_hourly_house_load(&points).unwrap();
        assert_eq!(profile.days_averaged, 7);
        assert!((profile.hourly_avg_kwh[8] - 4.0).abs() < 1e-3);
        // Linear interpolation: position 0.75 * 6 = 4.5 between 5 and 6
        let p75 = profile.hourly_p75_kwh.unwrap();
        assert!((p75[8] - 5.5).abs() < 1e-3, "p75 {}", p75[8]);
    }

    /// Samples at h:00 and h:30 with constant `watts` for each (hour, watts) pair on one day.
    fn hourly_samples(day: u32, hours: &[(u32, f32)]) -> Vec<HistoryDataPoint> {
        hours
            .iter()
            .flat_map(|&(h, watts)| {
                [
                    power_point(day, h, 0, watts),
                    power_point(day, h, 30, watts),
                ]
            })
            .collect()
    }

    #[test]
    fn house_load_empty_hour_takes_neighbour_mean() {
        // Hours 0-23 with (h+1) kW, except hour 12 which has no samples: neighbours 11 (12 kW)
        // and 13 (14 kW) give 13 kW
        let hours: Vec<(u32, f32)> = (0..24)
            .filter(|h| *h != 12)
            .map(|h| (h, (h + 1) as f32 * 1000.0))
            .collect();
        let profile = aggregate_hourly_house_load(&hourly_samples(14, &hours)).unwrap();
        assert!((profile.hourly_avg_kwh[12] - 13.0).abs() < 1e-3);
        assert!((profile.hourly_p75_kwh.unwrap()[12] - 13.0).abs() < 1e-3);
    }

    #[test]
    fn house_load_empty_hour_wraps_around_midnight() {
        // Hour 0 has no samples; its neighbours are 23 (4 kW) and 1 (2 kW)
        let points = hourly_samples(14, &[(23, 4000.0), (1, 2000.0)]);
        let profile = aggregate_hourly_house_load(&points).unwrap();
        assert!((profile.hourly_avg_kwh[0] - 3.0).abs() < 1e-3);
    }

    #[test]
    fn house_load_empty_hour_without_observed_neighbours_uses_all_hours_mean() {
        // Only hours 5 (2 kW) and 17 (6 kW) have data; hour 12 has no observed neighbour
        let points = hourly_samples(14, &[(5, 2000.0), (17, 6000.0)]);
        let profile = aggregate_hourly_house_load(&points).unwrap();
        assert!((profile.hourly_avg_kwh[12] - 4.0).abs() < 1e-3);
        assert!((profile.hourly_p75_kwh.unwrap()[12] - 4.0).abs() < 1e-3);
    }

    #[test]
    fn house_load_p75_is_capped_at_1_5_times_mean_on_spike_days() {
        // Seven days at hour 8: five days at 0.8 kW, two spike days at 11 kW.
        // Raw p75 = 5.9 kW; mean = 3.714 kW, so the cap 1.5 x mean = 5.571 kW applies.
        let mut points = Vec::new();
        for (i, day) in (14..=20).enumerate() {
            let watts = if i >= 5 { 11000.0 } else { 800.0 };
            points.push(power_point(day, 8, 0, watts));
            points.push(power_point(day, 8, 30, watts));
            points.push(power_point(day, 9, 0, watts));
        }
        let profile = aggregate_hourly_house_load(&points).unwrap();
        let mean = (5.0 * 0.8 + 2.0 * 11.0) / 7.0;
        assert!((profile.hourly_avg_kwh[8] - mean).abs() < 1e-3);
        let p75 = profile.hourly_p75_kwh.unwrap()[8];
        assert!((p75 - 1.5 * mean).abs() < 1e-3, "p75 {p75}");
    }

    #[test]
    fn house_load_negative_is_clamped_to_zero() {
        let points = vec![
            power_point(14, 8, 0, -500.0),
            power_point(14, 8, 30, -500.0),
            power_point(14, 9, 0, 0.0),
        ];
        let profile = aggregate_hourly_house_load(&points).unwrap();
        assert_eq!(profile.hourly_avg_kwh[8], 0.0);
        assert_eq!(profile.hourly_p75_kwh.unwrap()[8], 0.0);
    }

    #[test]
    fn house_load_empty_is_none() {
        assert!(aggregate_hourly_house_load(&[]).is_none());
    }

    #[test]
    fn pick_profile_falls_back_below_three_days() {
        let profile = |days| HourlyConsumptionProfile {
            hourly_avg_kwh: [1.0; 24],
            days_averaged: days,
            hourly_p75_kwh: None,
        };
        let consumption = Some(profile(7));
        let picked = pick_hourly_profile(Some(profile(2)), consumption.clone()).unwrap();
        assert_eq!(picked.days_averaged, 7);
        let picked = pick_hourly_profile(Some(profile(3)), consumption).unwrap();
        assert_eq!(picked.days_averaged, 3);
        assert!(pick_hourly_profile(None, None).is_none());
    }
}
