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

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::collections::{BTreeMap, HashMap};
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

/// Absolute interval `[start, end)` in UTC
type UtcInterval = (DateTime<Utc>, DateTime<Utc>);

/// One cached day: its local-day span and its low-tariff windows, all in UTC
#[derive(Debug, Clone)]
struct HdoDayIntervals {
    day: UtcInterval,
    low: Vec<UtcInterval>,
}

/// Cache for HDO schedules with TTL
///
/// HDO windows are local time. They are converted to UTC intervals once, when
/// the schedules are loaded (or the timezone changes), so queries compare UTC only.
#[derive(Debug)]
pub struct HdoCache {
    schedules: RwLock<HashMap<NaiveDate, HdoDaySchedule>>,
    days: RwLock<HashMap<NaiveDate, HdoDayIntervals>>,
    last_refresh: RwLock<Option<DateTime<Utc>>>,
    ttl_secs: u64,
    /// Local timezone of the HDO windows (CEZ publishes local time)
    timezone: RwLock<Tz>,
}

impl HdoCache {
    /// Create a new HDO cache with specified TTL in seconds
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            schedules: RwLock::new(HashMap::new()),
            days: RwLock::new(HashMap::new()),
            last_refresh: RwLock::new(None),
            ttl_secs,
            timezone: RwLock::new(chrono_tz::Europe::Prague),
        }
    }

    /// Set the local timezone of the HDO windows and convert the cached schedules again
    pub fn set_timezone(&self, tz: Tz) {
        *self.timezone.write().unwrap() = tz;
        self.rebuild();
    }

    /// Recompute the UTC intervals of every cached schedule
    fn rebuild(&self) {
        let tz = *self.timezone.read().unwrap();
        let days = {
            let schedules = self.schedules.read().unwrap();
            schedules
                .values()
                .filter_map(|schedule| {
                    day_intervals(tz, schedule).map(|intervals| (schedule.date, intervals))
                })
                .collect()
        };
        *self.days.write().unwrap() = days;
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
        {
            let mut cache = self.schedules.write().unwrap();
            cache.clear();
            for schedule in schedules {
                cache.insert(schedule.date, schedule);
            }
        }
        self.rebuild();
        *self.last_refresh.write().unwrap() = Some(Utc::now());
    }

    /// Check if a given time is in low tariff period
    /// Returns None if no data available for that date (caller should default to high tariff)
    pub fn is_low_tariff(&self, dt: DateTime<Utc>) -> Option<bool> {
        let days = self.days.read().unwrap();
        // Unknown unless dt falls inside a day the cache has data for
        let day = days
            .values()
            .find(|day| day.day.0 <= dt && dt < day.day.1)?;
        Some(day.low.iter().any(|(start, end)| *start <= dt && dt < *end))
    }
}

/// Convert a local wall-clock time to UTC.
/// Ambiguous times (fall-back) take the earlier instant. Times skipped by a
/// spring-forward gap use the offset in force before the gap.
fn local_to_utc(tz: Tz, local: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&local).earliest() {
        Some(t) => t.with_timezone(&Utc),
        None => tz
            .from_local_datetime(&(local - Duration::hours(1)))
            .earliest()
            .map(|t| t.with_timezone(&Utc) + Duration::hours(1))
            .unwrap_or_else(|| Utc.from_utc_datetime(&local)),
    }
}

/// Convert one local schedule day into UTC intervals.
/// A range with end 24:00 (stored as 23:59:59) runs to the next local midnight.
/// A range whose start is after its end wraps midnight and is kept day-keyed
/// (`[00:00, end)` and `[start, 24:00)` on the same date).
fn day_intervals(tz: Tz, schedule: &HdoDaySchedule) -> Option<HdoDayIntervals> {
    let date = schedule.date;
    let next = date.succ_opt()?;
    let midnight = |d: NaiveDate| local_to_utc(tz, d.and_time(NaiveTime::MIN));
    let end_of_day = NaiveTime::from_hms_opt(23, 59, 59)?;

    let mut low = Vec::new();
    for range in &schedule.low_tariff_ranges {
        let start = date.and_time(range.start);
        let end = if range.end == end_of_day {
            next.and_time(NaiveTime::MIN)
        } else {
            date.and_time(range.end)
        };
        if start < end {
            low.push((local_to_utc(tz, start), local_to_utc(tz, end)));
        } else if start > end {
            low.push((midnight(date), local_to_utc(tz, end)));
            low.push((local_to_utc(tz, start), midnight(next)));
        }
    }

    Some(HdoDayIntervals {
        day: (midnight(date), midnight(next)),
        low,
    })
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

/// Parse the CEZ HDO schedule sensor's `attributes.schedule[]` entries
/// (`{"start": "2026-10-14T17:00:00+02:00", "end": "2026-10-15T00:00:00+02:00", "tariff": "NT", "value": 1}`).
///
/// Times are taken as written (the local wall-clock of the entry's own offset).
/// NT entries (or `value == 1`) become low tariff ranges, grouped by local start date and merged.
/// An end on a later day runs to end of day, stored as 23:59:59 like `casy` "17:00-24:00".
pub fn parse_hdo_schedule_attribute(entries: &[serde_json::Value]) -> Vec<HdoDaySchedule> {
    let Some(end_of_day) = NaiveTime::from_hms_opt(23, 59, 59) else {
        return Vec::new();
    };

    let mut by_day: BTreeMap<NaiveDate, Vec<HdoTimeRange>> = BTreeMap::new();
    for entry in entries {
        let is_low = entry.get("tariff").and_then(|t| t.as_str()) == Some("NT")
            || entry.get("value").and_then(|v| v.as_i64()) == Some(1);
        if !is_low {
            continue;
        }

        let parse_rfc3339 = |key: &str| {
            entry
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        };
        let (Some(start), Some(end)) = (parse_rfc3339("start"), parse_rfc3339("end")) else {
            continue;
        };

        let date = start.date_naive();
        let end_time = if end.date_naive() > date {
            end_of_day
        } else {
            end.time()
        };
        if end_time <= start.time() {
            continue;
        }

        by_day.entry(date).or_default().push(HdoTimeRange {
            start: start.time(),
            end: end_time,
        });
    }

    by_day
        .into_iter()
        .map(|(date, ranges)| HdoDaySchedule {
            date,
            low_tariff_ranges: merge_time_ranges(ranges),
        })
        .collect()
}

/// Sort time ranges and merge overlapping or adjacent ones
fn merge_time_ranges(mut ranges: Vec<HdoTimeRange>) -> Vec<HdoTimeRange> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<HdoTimeRange> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => {
                if range.end > last.end {
                    last.end = range.end;
                }
            }
            _ => merged.push(range),
        }
    }
    merged
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

    // Preferred: the schedule sensor's attributes.schedule[] (RFC 3339 local times).
    // Falls through to the signals paths below when it yields no low tariff ranges.
    if let Some(entries) = json
        .get("attributes")
        .and_then(|a| a.get("schedule"))
        .and_then(|s| s.as_array())
    {
        schedules = parse_hdo_schedule_attribute(entries);
        if !schedules.is_empty() {
            return schedules;
        }
    }

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

    /// Prague local wall-clock time (HDO windows are local) as a UTC instant
    fn prague(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        chrono_tz::Europe::Prague
            .with_ymd_and_hms(2026, 1, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

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

        // Test within first low tariff range (03:00 local)
        assert_eq!(cache.is_low_tariff(prague(14, 3, 0)), Some(true));

        // Test within second low tariff range (12:00 local)
        assert_eq!(cache.is_low_tariff(prague(14, 12, 0)), Some(true));

        // Test outside low tariff ranges (high tariff, 08:00 local)
        assert_eq!(cache.is_low_tariff(prague(14, 8, 0)), Some(false));

        // Test date not in cache
        assert_eq!(cache.is_low_tariff(prague(15, 3, 0)), None);
    }

    #[test]
    fn test_hdo_cache_matches_local_time_not_utc() {
        let cache = HdoCache::new(3600);
        cache.update(vec![HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 1, 14).unwrap(),
            low_tariff_ranges: parse_time_ranges("22:00-24:00"),
        }]);

        // 21:30 UTC is 22:30 Prague (UTC+1 in winter): inside the low window
        let dt = Utc.with_ymd_and_hms(2026, 1, 14, 21, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(true));

        // 20:30 UTC is 21:30 Prague: outside the low window
        let dt = Utc.with_ymd_and_hms(2026, 1, 14, 20, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(false));
    }

    #[test]
    fn test_hdo_cache_timezone_is_configurable() {
        let cache = HdoCache::new(3600);
        cache.update(vec![HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 1, 14).unwrap(),
            low_tariff_ranges: parse_time_ranges("22:00-24:00"),
        }]);

        // Matched in UTC, 21:30 is outside the 22:00-24:00 window
        cache.set_timezone(chrono_tz::UTC);
        let dt = Utc.with_ymd_and_hms(2026, 1, 14, 21, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(false));
    }

    #[test]
    fn test_hdo_cache_dst_day_window_is_two_utc_hours() {
        // 2026-03-29: Prague clocks jump 02:00 -> 03:00 (CET -> CEST) at 01:00 UTC.
        // Window 01:00-04:00 local is 00:00-02:00 UTC, i.e. two hours, not three.
        let cache = HdoCache::new(3600);
        cache.update(vec![HdoDaySchedule {
            date: NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(),
            low_tariff_ranges: parse_time_ranges("01:00-04:00"),
        }]);

        // 23:30 UTC on 28th is 00:30 local: before the window
        let dt = Utc.with_ymd_and_hms(2026, 3, 28, 23, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(false));

        // 00:30 UTC is 01:30 CET: inside
        let dt = Utc.with_ymd_and_hms(2026, 3, 29, 0, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(true));

        // 01:30 UTC is 03:30 CEST: still inside
        let dt = Utc.with_ymd_and_hms(2026, 3, 29, 1, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(true));

        // 02:00 UTC is 04:00 CEST: window end is exclusive
        let dt = Utc.with_ymd_and_hms(2026, 3, 29, 2, 0, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), Some(false));

        // Day not in cache
        let dt = Utc.with_ymd_and_hms(2026, 3, 30, 0, 30, 0).unwrap();
        assert_eq!(cache.is_low_tariff(dt), None);
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
    fn test_parse_hdo_schedule_attribute_matches_casy() {
        // attributes.schedule[] of sensor.cez_hdo_schedule_*; NT entries are the low tariff
        let schedule_json = r#"{
            "state": "2026-10-14T00:00:00+02:00",
            "attributes": {
                "signal": "EVV1",
                "days": 7,
                "schedule": [
                    {"start": "2026-10-14T00:00:00+02:00", "end": "2026-10-14T06:00:00+02:00", "tariff": "NT", "value": 1},
                    {"start": "2026-10-14T06:00:00+02:00", "end": "2026-10-14T07:00:00+02:00", "tariff": "VT", "value": 0},
                    {"start": "2026-10-14T07:00:00+02:00", "end": "2026-10-14T09:00:00+02:00", "tariff": "NT", "value": 1},
                    {"start": "2026-10-14T09:00:00+02:00", "end": "2026-10-14T17:00:00+02:00", "tariff": "VT", "value": 0},
                    {"start": "2026-10-14T17:00:00+02:00", "end": "2026-10-15T00:00:00+02:00", "tariff": "NT", "value": 1},
                    {"start": "2026-10-15T00:00:00+02:00", "end": "2026-10-15T06:00:00+02:00", "tariff": "NT", "value": 1}
                ]
            }
        }"#;
        let casy_json = r#"{
            "data": {"signals": [
                {"signal": "EVV1", "datum": "14.10.2026", "casy": "00:00-06:00;   07:00-09:00;   17:00-24:00"},
                {"signal": "EVV1", "datum": "15.10.2026", "casy": "00:00-06:00"}
            ]}
        }"#;

        let from_schedule = parse_hdo_sensor_data(schedule_json);
        let from_casy = parse_hdo_sensor_data(casy_json);
        assert_eq!(from_schedule.len(), 2);
        assert_eq!(from_schedule.len(), from_casy.len());
        for (a, b) in from_schedule.iter().zip(from_casy.iter()) {
            assert_eq!(a.date, b.date);
            let a_ranges: Vec<_> = a
                .low_tariff_ranges
                .iter()
                .map(|r| (r.start, r.end))
                .collect();
            let b_ranges: Vec<_> = b
                .low_tariff_ranges
                .iter()
                .map(|r| (r.start, r.end))
                .collect();
            assert_eq!(a_ranges, b_ranges);
        }
        // 17:00 -> next-day 00:00 is stored as 17:00-23:59:59 on the start date
        let last = from_schedule[0].low_tariff_ranges.last().unwrap();
        assert_eq!(last.start, NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        assert_eq!(last.end, NaiveTime::from_hms_opt(23, 59, 59).unwrap());
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

        // During low tariff (03:00 local)
        let dt_low = prague(14, 3, 0);
        let eff_low =
            calculate_effective_price(spot_price, dt_low, &cache, low_tariff, high_tariff);
        assert!((eff_low - 2.50).abs() < 0.001); // 2.0 + 0.50

        // During high tariff (08:00 local)
        let dt_high = prague(14, 8, 0);
        let eff_high =
            calculate_effective_price(spot_price, dt_high, &cache, low_tariff, high_tariff);
        assert!((eff_high - 3.80).abs() < 0.001); // 2.0 + 1.80

        // Unknown date (defaults to high)
        let dt_unknown = prague(15, 3, 0);
        let eff_unknown =
            calculate_effective_price(spot_price, dt_unknown, &cache, low_tariff, high_tariff);
        assert!((eff_unknown - 3.80).abs() < 0.001); // 2.0 + 1.80 (defaults to high)
    }
}
