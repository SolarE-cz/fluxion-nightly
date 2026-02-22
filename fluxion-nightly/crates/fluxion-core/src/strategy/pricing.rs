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

//! Shared pricing utilities for strategy evaluation
//!
//! Provides HDO tariff parsing and effective price calculation used by
//! V2, V3, and V4 strategies. This module consolidates common pricing logic
//! that is independent of strategy-specific decision making.
//!
//! ## HDO (Hromadné Dálkové Ovládání)
//!
//! HDO is a Czech grid tariff system that switches between low and high tariff
//! periods based on grid demand. The schedule is broadcast via power lines and
//! varies by distribution region and day.
//!
//! ## Effective Price
//!
//! The effective price for grid import is: `spot_price + grid_fee`
//! where grid_fee depends on whether the current time is in HDO low or high tariff.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use std::collections::HashMap;
use std::sync::RwLock;

// ============================================================================
// HDO Types
// ============================================================================

/// Parsed HDO time range for low tariff periods
#[derive(Debug, Clone)]
pub struct HdoTimeRange {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

/// Cached HDO schedule for a specific date
#[derive(Debug, Clone)]
pub struct HdoDaySchedule {
    pub date: NaiveDate,
    pub low_tariff_ranges: Vec<HdoTimeRange>,
}

/// Cache for HDO schedules with TTL
#[derive(Debug)]
pub struct HdoCache {
    schedules: RwLock<HashMap<NaiveDate, HdoDaySchedule>>,
    last_refresh: RwLock<Option<DateTime<Utc>>>,
    ttl_secs: u64,
}

impl HdoCache {
    /// Create a new HDO cache with specified TTL in seconds
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            schedules: RwLock::new(HashMap::new()),
            last_refresh: RwLock::new(None),
            ttl_secs,
        }
    }

    /// Check if cache needs refresh based on TTL
    pub fn needs_refresh(&self) -> bool {
        let last = self.last_refresh.read().unwrap();
        match *last {
            None => true,
            Some(ts) => {
                let elapsed = Utc::now().signed_duration_since(ts).num_seconds() as u64;
                elapsed > self.ttl_secs
            }
        }
    }

    /// Update cache with new HDO schedules
    pub fn update(&self, schedules: Vec<HdoDaySchedule>) {
        let mut cache = self.schedules.write().unwrap();
        cache.clear();
        for schedule in schedules {
            cache.insert(schedule.date, schedule);
        }
        *self.last_refresh.write().unwrap() = Some(Utc::now());
    }

    /// Check if a given time is in low tariff period
    /// Returns None if no data available for that date (caller should default to high tariff)
    pub fn is_low_tariff(&self, dt: DateTime<Utc>) -> Option<bool> {
        let date = dt.date_naive();
        let time = dt.time();

        let cache = self.schedules.read().unwrap();
        let schedule = cache.get(&date)?;

        Some(schedule.low_tariff_ranges.iter().any(|range| {
            if range.start <= range.end {
                // Normal range: e.g., 06:00-12:00
                time >= range.start && time < range.end
            } else {
                // Overnight range: e.g., 22:00-06:00 (spans midnight)
                time >= range.start || time < range.end
            }
        }))
    }
}

impl Default for HdoCache {
    fn default() -> Self {
        Self::new(3600) // 1 hour default TTL
    }
}

// ============================================================================
// HDO Parsing Functions
// ============================================================================

/// Parse Czech date format "DD.MM.YYYY"
pub fn parse_czech_date(s: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let day: u32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let year: i32 = parts[2].parse().ok()?;

    NaiveDate::from_ymd_opt(year, month, day)
}

/// Parse time ranges from HDO format "HH:MM-HH:MM; HH:MM-HH:MM"
pub fn parse_time_ranges(s: &str) -> Vec<HdoTimeRange> {
    let mut ranges = Vec::new();

    for range_str in s.split(';') {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            continue;
        }

        if let Some(range) = parse_single_time_range(range_str) {
            ranges.push(range);
        }
    }

    ranges
}

/// Parse a single time range "HH:MM-HH:MM"
fn parse_single_time_range(s: &str) -> Option<HdoTimeRange> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start = parse_time(parts[0])?;
    let end = parse_time(parts[1])?;

    Some(HdoTimeRange { start, end })
}

/// Parse time "HH:MM"
fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }

    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;

    // Handle 24:00 as end-of-day (convert to 23:59:59)
    if hour == 24 && minute == 0 {
        return NaiveTime::from_hms_opt(23, 59, 59);
    }

    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// Parse HDO sensor data from raw JSON format
///
/// Supports multiple formats:
/// 1. Home Assistant sensor structure: `attributes.response_json.data.signals`
/// 2. Direct structure: `data.signals`
///
/// Expected signal format:
/// ```json
/// {
///   "signal": "EVV1",
///   "datum": "14.01.2026",
///   "casy": "00:00-06:00; 07:00-09:00; 10:00-13:00"
/// }
/// ```
pub fn parse_hdo_sensor_data(raw_data: &str) -> Vec<HdoDaySchedule> {
    let mut schedules = Vec::new();

    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw_data) else {
        return schedules;
    };

    // Try multiple paths to find the signals array:
    // 1. attributes.raw_json.data.data.signals (new cez_hdo_raw_data sensor)
    // 2. attributes.response_json.data.signals (legacy cez_hdo_lowtariffstart sensor)
    // 3. data.signals (direct structure from some sensors)
    let signals = json
        .get("attributes")
        .and_then(|a| a.get("raw_json"))
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("data"))
        .and_then(|d| d.get("signals"))
        .and_then(|s| s.as_array())
        .or_else(|| {
            json.get("attributes")
                .and_then(|a| a.get("response_json"))
                .and_then(|r| r.get("data"))
                .and_then(|d| d.get("signals"))
                .and_then(|s| s.as_array())
        })
        .or_else(|| {
            json.get("data")
                .and_then(|d| d.get("signals"))
                .and_then(|s| s.as_array())
        });

    let Some(signals) = signals else {
        tracing::warn!(
            "HDO data has no signals array. Keys: {:?}",
            json.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        return schedules;
    };

    for signal in signals {
        if let (Some(datum), Some(casy)) = (
            signal.get("datum").and_then(|d| d.as_str()),
            signal.get("casy").and_then(|c| c.as_str()),
        ) && let Some(date) = parse_czech_date(datum)
        {
            let ranges = parse_time_ranges(casy);
            if !ranges.is_empty() {
                let range_count = ranges.len();
                schedules.push(HdoDaySchedule {
                    date,
                    low_tariff_ranges: ranges,
                });
                tracing::debug!(
                    "Parsed HDO schedule for {}: {} low tariff periods",
                    date,
                    range_count
                );
            }
        }
    }

    tracing::info!(
        "Parsed HDO data for {} days: {:?}",
        schedules.len(),
        schedules.iter().map(|s| s.date).collect::<Vec<_>>()
    );

    schedules
}

// ============================================================================
// Effective Price Calculation
// ============================================================================

/// Calculate effective price = spot_price + grid_fee based on HDO tariff
///
/// # Arguments
/// * `spot_price` - The spot market price in CZK/kWh
/// * `block_time` - The time of the price block
/// * `hdo_cache` - Cache of HDO schedules
/// * `low_tariff_czk` - Grid fee during low tariff periods (CZK/kWh)
/// * `high_tariff_czk` - Grid fee during high tariff periods (CZK/kWh)
///
/// # Returns
/// The effective price (spot + grid fee) in CZK/kWh
pub fn calculate_effective_price(
    spot_price: f32,
    block_time: DateTime<Utc>,
    hdo_cache: &HdoCache,
    low_tariff_czk: f32,
    high_tariff_czk: f32,
) -> f32 {
    let grid_fee = match hdo_cache.is_low_tariff(block_time) {
        Some(true) => low_tariff_czk,
        Some(false) | None => high_tariff_czk, // Default to high if unknown
    };
    spot_price + grid_fee
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_parse_czech_date() {
        assert_eq!(
            parse_czech_date("14.01.2026"),
            Some(NaiveDate::from_ymd_opt(2026, 1, 14).unwrap())
        );
        assert_eq!(
            parse_czech_date("31.12.2025"),
            Some(NaiveDate::from_ymd_opt(2025, 12, 31).unwrap())
        );
        assert_eq!(parse_czech_date("invalid"), None);
        assert_eq!(parse_czech_date("14-01-2026"), None);
    }

    #[test]
    fn test_parse_time_ranges() {
        let ranges = parse_time_ranges("00:00-06:00; 07:00-09:00; 10:00-13:00");
        assert_eq!(ranges.len(), 3);

        assert_eq!(ranges[0].start, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        assert_eq!(ranges[0].end, NaiveTime::from_hms_opt(6, 0, 0).unwrap());

        assert_eq!(ranges[1].start, NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert_eq!(ranges[1].end, NaiveTime::from_hms_opt(9, 0, 0).unwrap());

        assert_eq!(ranges[2].start, NaiveTime::from_hms_opt(10, 0, 0).unwrap());
        assert_eq!(ranges[2].end, NaiveTime::from_hms_opt(13, 0, 0).unwrap());
    }

    #[test]
    fn test_parse_time_ranges_with_24_hour() {
        let ranges = parse_time_ranges("17:00-24:00");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        // 24:00 should be converted to 23:59:59
        assert_eq!(ranges[0].end, NaiveTime::from_hms_opt(23, 59, 59).unwrap());
    }

    #[test]
    fn test_hdo_cache_is_low_tariff() {
        let cache = HdoCache::new(3600);

        let schedule = HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 1, 14).unwrap(),
            low_tariff_ranges: vec![
                HdoTimeRange {
                    start: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
                },
                HdoTimeRange {
                    start: NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(14, 0, 0).unwrap(),
                },
            ],
        };
        cache.update(vec![schedule]);

        // Test within first low tariff range
        let dt1 = Utc.with_ymd_and_hms(2026, 1, 14, 3, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt1), Some(true));

        // Test within second low tariff range
        let dt2 = Utc.with_ymd_and_hms(2026, 1, 14, 12, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt2), Some(true));

        // Test outside low tariff ranges (high tariff)
        let dt3 = Utc.with_ymd_and_hms(2026, 1, 14, 8, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt3), Some(false));

        // Test date not in cache
        let dt4 = Utc.with_ymd_and_hms(2026, 1, 15, 3, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt4), None);
    }

    #[test]
    fn test_parse_hdo_sensor_data() {
        let json_data = r#"{
            "data": {
                "signals": [
                    {
                        "signal": "EVV1",
                        "datum": "14.01.2026",
                        "casy": "00:00-06:00; 07:00-09:00"
                    }
                ]
            }
        }"#;

        let schedules = parse_hdo_sensor_data(json_data);
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 14).unwrap()
        );
        assert_eq!(schedules[0].low_tariff_ranges.len(), 2);
    }

    #[test]
    fn test_parse_hdo_sensor_data_ha_structure() {
        // Test the actual Home Assistant sensor structure
        let json_data = r#"{
            "entity_id": "sensor.cez_hdo_raw_data",
            "state": "17:00:00",
            "attributes": {
                "response_json": {
                    "data": {
                        "signals": [
                            {
                                "signal": "EVV1",
                                "den": "Čtvrtek",
                                "datum": "15.01.2026",
                                "casy": "00:00-06:00;   07:00-09:00;   10:00-13:00;   14:00-16:00;   17:00-24:00"
                            },
                            {
                                "signal": "EVV1",
                                "den": "Pátek",
                                "datum": "16.01.2026",
                                "casy": "00:00-06:00;   07:00-09:00"
                            }
                        ]
                    }
                },
                "friendly_name": "cez_hdo_LowTariffStart"
            }
        }"#;

        let schedules = parse_hdo_sensor_data(json_data);
        assert_eq!(schedules.len(), 2);
        assert_eq!(
            schedules[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()
        );
        assert_eq!(schedules[0].low_tariff_ranges.len(), 5);
        assert_eq!(
            schedules[1].date,
            NaiveDate::from_ymd_opt(2026, 1, 16).unwrap()
        );
        assert_eq!(schedules[1].low_tariff_ranges.len(), 2);
    }

    #[test]
    fn test_parse_hdo_sensor_data_raw_json_structure() {
        // Test the new cez_hdo_raw_data sensor structure (raw_json.data.data.signals)
        let json_data = r#"{
            "entity_id": "sensor.cez_hdo_raw_data",
            "state": "21.01.2026 19:34",
            "attributes": {
                "raw_json": {
                    "timestamp": "2026-01-21T19:34:57.476005",
                    "data": {
                        "data": {
                            "signals": [
                                {
                                    "signal": "EVV1",
                                    "den": "Středa",
                                    "datum": "21.01.2026",
                                    "casy": "00:00-06:00;   07:00-09:00;   10:00-13:00;   14:00-16:00;   17:00-24:00"
                                },
                                {
                                    "signal": "EVV1",
                                    "den": "Čtvrtek",
                                    "datum": "22.01.2026",
                                    "casy": "00:00-06:00;   07:00-09:00"
                                }
                            ],
                            "amm": false,
                            "switchClock": false
                        },
                        "statusCode": 200
                    }
                },
                "icon": "mdi:home-clock",
                "friendly_name": "ČEZ HDO surová data"
            }
        }"#;

        let schedules = parse_hdo_sensor_data(json_data);
        assert_eq!(schedules.len(), 2);
        assert_eq!(
            schedules[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 21).unwrap()
        );
        assert_eq!(schedules[0].low_tariff_ranges.len(), 5);
        assert_eq!(
            schedules[1].date,
            NaiveDate::from_ymd_opt(2026, 1, 22).unwrap()
        );
        assert_eq!(schedules[1].low_tariff_ranges.len(), 2);
    }

    #[test]
    fn test_calculate_effective_price() {
        let cache = HdoCache::new(3600);

        let schedule = HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 1, 14).unwrap(),
            low_tariff_ranges: vec![HdoTimeRange {
                start: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                end: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
            }],
        };
        cache.update(vec![schedule]);

        let low_tariff = 0.50;
        let high_tariff = 1.80;
        let spot_price = 2.0;

        // During low tariff (3:00)
        let dt_low = Utc.with_ymd_and_hms(2026, 1, 14, 3, 0, 0).unwrap();
        let eff_low =
            calculate_effective_price(spot_price, dt_low, &cache, low_tariff, high_tariff);
        assert!((eff_low - 2.50).abs() < 0.001); // 2.0 + 0.50

        // During high tariff (8:00)
        let dt_high = Utc.with_ymd_and_hms(2026, 1, 14, 8, 0, 0).unwrap();
        let eff_high =
            calculate_effective_price(spot_price, dt_high, &cache, low_tariff, high_tariff);
        assert!((eff_high - 3.80).abs() < 0.001); // 2.0 + 1.80

        // Unknown date (defaults to high)
        let dt_unknown = Utc.with_ymd_and_hms(2026, 1, 15, 3, 0, 0).unwrap();
        let eff_unknown =
            calculate_effective_price(spot_price, dt_unknown, &cache, low_tariff, high_tariff);
        assert!((eff_unknown - 3.80).abs() < 0.001); // 2.0 + 1.80 (defaults to high)
    }
}
