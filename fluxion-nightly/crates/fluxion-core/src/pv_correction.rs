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

//! Self-correcting solar forecast (brief H34).
//!
//! The Open-Meteo forecast stays as the site configured it; FluxION scales the PV forecast its
//! planner uses by `Σ measured PV / Σ forecast PV` over the valid hours of the last 7 complete
//! local days, clamped to [0.5, 1.5].
//!
//! - Forecast: the hourly forecast of each local day as the planner had it when the day started
//!   (the first forecast fetch after local midnight, before 06:00), persisted as a daily
//!   snapshot. Days before the first snapshot are bootstrapped from the recorded history of the
//!   provider's total-today sensors when that history carries the per-period forecast.
//! - Measured: master PV plus the `pv_extra_prefixes` units, the same entities as live.
//! - Excluded hours: missing data in any of their blocks (forecast, any PV unit, SOC, export,
//!   or the export limit when export curtailment is enabled; a power sensor must have data for
//!   80 % of a block), and hours where PV was likely curtailed (export at the cap whatever the
//!   battery, export curtailment active, or export disabled with a full battery).
//! - A day counts when its valid forecast is at least 0.5 kWh per kWp-equivalent (the highest
//!   hourly forecast seen); at least 3 such days are needed, else the factor is 1.0.
//!
//! Read-only: only recorder history is read, nothing is written to HA or the inverter.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::backtest::{
    BLOCK_MINUTES, HistoryError, HistoryReader, HistoryState, Timeline, day_block_starts,
    local_midnight_utc,
};

/// Where the daily forecast snapshots (and the last factor) are persisted
pub const SNAPSHOT_PATH: &str = "/data/pv_forecast_snapshots.json";
/// Complete local days in the window
pub const WINDOW_DAYS: u32 = 7;
/// Fewest valid days for a factor other than 1.0
pub const MIN_VALID_DAYS: usize = 3;
/// Factor bounds
pub const FACTOR_MIN: f32 = 0.5;
pub const FACTOR_MAX: f32 = 1.5;
/// A day counts when its valid forecast is at least this many kWh per kWp-equivalent
pub const MIN_DAY_KWH_PER_KWP: f32 = 0.5;
/// The battery counts as full from `max_battery_soc` minus this many percent
pub const FULL_SOC_MARGIN_PCT: f32 = 1.0;
/// Export counts as at the cap from this share of it
pub const EXPORT_AT_CAP_RATIO: f32 = 0.95;
/// An export limit at or below this many watts means export is off
pub const EXPORT_OFF_MIN_W: u32 = 100;
/// The day's snapshot is taken from local midnight until this local hour
pub const SNAPSHOT_UNTIL_HOUR: u32 = 6;
/// Days kept in the snapshot store
const KEEP_DAYS: usize = 14;
/// Blocks per hour
const BLOCKS_PER_HOUR: usize = 4;
/// Share of a block a power sensor must have data for; the rest of the block counts as missing
pub const MIN_BLOCK_COVERAGE: f64 = 0.8;

/// Where a day's forecast came from
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastSource {
    /// FluxION's own per-block forecast, snapshotted after local midnight
    Snapshot,
    /// The provider sensors' recorded history (bootstrap)
    HaHistory,
}

/// Forecast PV energy of one hour (UTC start)
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HourForecast {
    pub start: DateTime<Utc>,
    pub kwh: f32,
}

/// The hourly forecast of one local day as the planner had it when the day started. Hours
/// without a complete forecast are left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayForecast {
    pub date: NaiveDate,
    pub taken_at: DateTime<Utc>,
    pub source: ForecastSource,
    pub hours: Vec<HourForecast>,
    /// Forecast sensors that contributed to every kept hour (0 in records written before this
    /// field existed, so a complete snapshot replaces them)
    #[serde(default)]
    pub sensors: usize,
}

impl DayForecast {
    /// Forecast of the hour starting at `start`, `None` when the hour has none
    #[must_use]
    pub fn hour(&self, start: DateTime<Utc>) -> Option<f32> {
        self.hours.iter().find(|h| h.start == start).map(|h| h.kwh)
    }

    fn peak_hour_kwh(&self) -> f32 {
        self.hours.iter().map(|h| h.kwh).fold(0.0, f32::max)
    }
}

/// A computed correction
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CorrectionResult {
    /// Factor applied to the PV forecast (1.0 = none)
    pub factor: f32,
    /// Valid days the factor rests on
    pub days_used: usize,
    /// Σ measured PV over the valid hours of the valid days (kWh)
    pub measured_kwh: f32,
    /// Σ forecast PV over the same hours (kWh)
    pub forecast_kwh: f32,
}

impl CorrectionResult {
    /// No correction
    #[must_use]
    pub const fn neutral() -> Self {
        Self {
            factor: 1.0,
            days_used: 0,
            measured_kwh: 0.0,
            forecast_kwh: 0.0,
        }
    }
}

impl CorrectionResult {
    /// The factor rests on enough valid days to scale the forecast
    #[must_use]
    pub fn is_applied(&self) -> bool {
        self.days_used >= MIN_VALID_DAYS
    }

    /// A factor that is not finite or lies outside [`FACTOR_MIN`, `FACTOR_MAX`] (a damaged
    /// file, a bad message) becomes no correction
    #[must_use]
    pub fn sanitized(self) -> Self {
        if self.factor.is_finite() && (FACTOR_MIN..=FACTOR_MAX).contains(&self.factor) {
            self
        } else {
            Self {
                factor: 1.0,
                ..self
            }
        }
    }
}

impl Default for CorrectionResult {
    fn default() -> Self {
        Self::neutral()
    }
}

/// The last correction and the local day it was computed on
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredCorrection {
    pub computed_on: NaiveDate,
    pub result: CorrectionResult,
}

/// Persisted snapshots
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastStore {
    #[serde(default)]
    pub days: Vec<DayForecast>,
    /// Highest hourly forecast ever seen (kWh in one hour, i.e. mean kW): the kWp-equivalent
    #[serde(default)]
    pub peak_hour_kwh: f32,
    #[serde(default)]
    pub last: Option<StoredCorrection>,
}

impl ForecastStore {
    /// Load the store; a missing or unreadable file gives an empty store
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// Persist the store as JSON (written to a temporary file, then renamed)
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    #[must_use]
    pub fn day(&self, date: NaiveDate) -> Option<&DayForecast> {
        self.days.iter().find(|d| d.date == date)
    }

    /// Add a day's forecast unless the day already has one from at least as many sensors (a
    /// complete forecast replaces a partial one). Returns true when added.
    pub fn insert(&mut self, day: DayForecast) -> bool {
        if let Some(existing) = self.day(day.date) {
            if existing.sensors >= day.sensors {
                return false;
            }
            self.days.retain(|d| d.date != day.date);
        }
        self.peak_hour_kwh = self.peak_hour_kwh.max(day.peak_hour_kwh());
        self.days.push(day);
        self.days.sort_by_key(|d| d.date);
        let excess = self.days.len().saturating_sub(KEEP_DAYS);
        self.days.drain(..excess);
        true
    }

    /// The snapshot of the local day of `now` is due: before `SNAPSHOT_UNTIL_HOUR` and the day
    /// has no forecast from all `sensors` yet
    #[must_use]
    pub fn snapshot_due(&self, local: NaiveDateTime, sensors: usize) -> bool {
        local.hour() < SNAPSHOT_UNTIL_HOUR
            && self.day(local.date()).is_none_or(|d| d.sensors < sensors)
    }

    /// Snapshot today's hourly forecast from the planner's per-block forecast, given per sensor
    /// (today and tomorrow sensors alike), when due. Taken only when exactly `sensors` sensors
    /// (the discovered total-today sensors, one per array) forecast the day; otherwise the next
    /// fetch in the window tries again. Returns true when a snapshot was taken.
    pub fn record_snapshot(
        &mut self,
        now: DateTime<Utc>,
        tz: Tz,
        per_sensor_blocks: &[Vec<(DateTime<Utc>, f32)>],
        sensors: usize,
    ) -> bool {
        let local = now.with_timezone(&tz).naive_local();
        if sensors == 0 || !self.snapshot_due(local, sensors) {
            return false;
        }
        let date = local.date();
        let per_sensor: Vec<Vec<HourForecast>> = per_sensor_blocks
            .iter()
            .map(|blocks| hourly_forecast(blocks, date, tz))
            .filter(|hours| !hours.is_empty())
            .collect();
        if per_sensor.len() != sensors {
            return false;
        }
        let hours = combine_sensor_hours(&per_sensor);
        if !hours.iter().any(|h| h.kwh > 0.0) {
            return false;
        }
        self.insert(DayForecast {
            date,
            taken_at: now,
            source: ForecastSource::Snapshot,
            hours,
            sensors,
        })
    }

    /// Days of the window before `today` without a forecast
    #[must_use]
    pub fn missing_days(&self, today: NaiveDate) -> Vec<NaiveDate> {
        window_days(today)
            .into_iter()
            .filter(|d| self.day(*d).is_none())
            .collect()
    }
}

/// The `WINDOW_DAYS` complete local days before `today`, oldest first
#[must_use]
pub fn window_days(today: NaiveDate) -> Vec<NaiveDate> {
    (1..=WINDOW_DAYS)
        .rev()
        .filter_map(|back| today.checked_sub_days(chrono::Days::new(u64::from(back))))
        .collect()
}

/// UTC starts of the hours of local day `date`: 23, 24 or 25 on DST days
#[must_use]
pub fn day_hour_starts(date: NaiveDate, tz: Tz) -> Vec<DateTime<Utc>> {
    day_block_starts(date, tz)
        .into_iter()
        .step_by(BLOCKS_PER_HOUR)
        .collect()
}

/// Per-block forecast (kWh) for each block start, `None` where no key covers it. A key covers
/// the starts up to (not including) one hour after it, so hourly keys fill their four blocks.
#[must_use]
pub fn covered_block_kwh(
    block_kwh: &[(DateTime<Utc>, f32)],
    block_starts: &[DateTime<Utc>],
) -> Vec<Option<f32>> {
    let by_key: BTreeMap<DateTime<Utc>, f32> = block_kwh.iter().copied().collect();
    block_starts
        .iter()
        .map(|start| {
            by_key
                .range(..=*start)
                .next_back()
                .filter(|(key, _)| *start - **key < Duration::minutes(60))
                .map(|(_, kwh)| *kwh)
        })
        .collect()
}

/// Hourly forecast of local day `date` from per-block values: an hour is kept only when all its
/// blocks are covered
#[must_use]
pub fn hourly_forecast(
    block_kwh: &[(DateTime<Utc>, f32)],
    date: NaiveDate,
    tz: Tz,
) -> Vec<HourForecast> {
    let starts = day_block_starts(date, tz);
    let covered = covered_block_kwh(block_kwh, &starts);
    starts
        .chunks(BLOCKS_PER_HOUR)
        .zip(covered.chunks(BLOCKS_PER_HOUR))
        .filter_map(|(hour_starts, values)| {
            let sum = values.iter().copied().sum::<Option<f32>>()?;
            (values.len() == BLOCKS_PER_HOUR).then_some(HourForecast {
                start: hour_starts[0],
                kwh: sum,
            })
        })
        .collect()
}

/// Per-block PV forecast (UTC block start, kWh) from the attributes of an Open-Meteo forecast
/// sensor: `watts` (W at each block start), `wh_period_15m` (Wh per 15 minutes) or `wh_period`
/// (Wh per hour, split over its four blocks). The integration keeps `watts` and `wh_period` out
/// of the recorder, so in history only `wh_period_15m` is found (integration versions from
/// 2026-08 on).
#[must_use]
pub fn forecast_blocks_from_attributes(
    attributes: &serde_json::Value,
) -> Vec<(DateTime<Utc>, f32)> {
    let entries = |key: &str| -> Option<Vec<(DateTime<Utc>, f64)>> {
        let map = attributes.get(key)?.as_object()?;
        let parsed: Vec<(DateTime<Utc>, f64)> = map
            .iter()
            .filter_map(|(ts, value)| {
                let at = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
                Some((at, value.as_f64()?))
            })
            .collect();
        (!parsed.is_empty()).then_some(parsed)
    };
    if let Some(watts) = entries("watts") {
        return watts
            .into_iter()
            .map(|(at, w)| (at, w as f32 / 1000.0 * 0.25))
            .collect();
    }
    if let Some(wh_15m) = entries("wh_period_15m") {
        return wh_15m
            .into_iter()
            .map(|(at, wh)| (at, wh as f32 / 1000.0))
            .collect();
    }
    if let Some(wh_hour) = entries("wh_period") {
        return wh_hour
            .into_iter()
            .flat_map(|(at, wh)| {
                (0..4).map(move |q| {
                    (
                        at + Duration::minutes(BLOCK_MINUTES * q),
                        wh as f32 / 4000.0,
                    )
                })
            })
            .collect();
    }
    Vec::new()
}

/// Bootstrap a past day's forecast from the recorded states of the provider's total-today
/// sensors (one list of states per sensor). For each sensor the first state recorded from local
/// midnight until `SNAPSHOT_UNTIL_HOUR` whose attributes forecast the day is used, like a live
/// snapshot. `None` when any sensor has no such state (the forecast cannot be reconstructed).
#[must_use]
pub fn day_forecast_from_history(
    states_per_sensor: &[Vec<HistoryState>],
    date: NaiveDate,
    tz: Tz,
) -> Option<DayForecast> {
    if states_per_sensor.is_empty() {
        return None;
    }
    let day_start = local_midnight_utc(date, tz);
    let until = day_start + Duration::hours(i64::from(SNAPSHOT_UNTIL_HOUR));
    let mut per_sensor = Vec::with_capacity(states_per_sensor.len());
    let mut taken_at = day_start;
    for states in states_per_sensor {
        let (at, hours) = states
            .iter()
            .filter(|s| s.at >= day_start && s.at < until)
            .find_map(|s| {
                let blocks = forecast_blocks_from_attributes(s.attributes.as_ref()?);
                let hours = hourly_forecast(&blocks, date, tz);
                hours.iter().any(|h| h.kwh > 0.0).then_some((s.at, hours))
            })?;
        taken_at = taken_at.max(at);
        per_sensor.push(hours);
    }
    let hours = combine_sensor_hours(&per_sensor);
    (!hours.is_empty()).then_some(DayForecast {
        date,
        taken_at,
        source: ForecastSource::HaHistory,
        hours,
        sensors: per_sensor.len(),
    })
}

/// Sum the sensors' hourly forecasts, keeping only the hours every sensor forecast
#[must_use]
pub fn combine_sensor_hours(per_sensor: &[Vec<HourForecast>]) -> Vec<HourForecast> {
    let mut per_hour: BTreeMap<DateTime<Utc>, (f32, usize)> = BTreeMap::new();
    for hours in per_sensor {
        for hour in hours {
            let entry = per_hour.entry(hour.start).or_insert((0.0, 0));
            entry.0 += hour.kwh;
            entry.1 += 1;
        }
    }
    per_hour
        .into_iter()
        .filter(|(_, (_, count))| *count == per_sensor.len())
        .map(|(start, (kwh, _))| HourForecast { start, kwh })
        .collect()
}

/// What counts as curtailment at the site
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurtailLimits {
    /// `control.max_battery_soc` (%)
    pub max_soc_pct: f32,
    /// Configured export cap: the lower of `maximum_export_power_w` and the hard cap (W)
    pub export_cap_w: u32,
    /// An export limit at or below this means export is off (W)
    pub export_off_w: u32,
    /// FluxION's price-based export curtailment is enabled: the recorded export limit is then
    /// needed to tell curtailed hours
    pub export_curtailment_enabled: bool,
}

/// Everything the correction reads, from the live configuration
#[derive(Debug, Clone)]
pub struct CorrectionSite {
    pub timezone: Tz,
    /// Master PV power first, then the `pv_extra_prefixes` units; summed
    pub pv: Vec<String>,
    pub soc: String,
    /// Grid export power (W)
    pub grid_export: String,
    /// Export limit number entity (W)
    pub export_limit: Option<String>,
    pub limits: CurtailLimits,
}

/// The correction site built from the live configuration; `None` when it cannot be built
/// (timezone unknown, no inverter, no export sensor), and the correction then stays at 1.0
#[derive(Debug, Clone, bevy_ecs::prelude::Resource)]
pub struct PvCorrectionSiteResource(pub Option<CorrectionSite>);

/// One 15-minute block as recorded. `None` = no data.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BlockObservation {
    /// PV energy of every unit (kWh); `None` when any unit has no data
    pub pv_kwh: Option<f32>,
    /// Highest SOC seen at the block's start and end (%)
    pub soc_pct: Option<f32>,
    /// Grid export energy (kWh)
    pub export_kwh: Option<f32>,
    /// Lowest export limit in force during the block (W)
    pub export_limit_w: Option<f32>,
}

/// How a block or an hour counts
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MeasuredStatus {
    /// Some data is missing
    Missing,
    /// PV was likely curtailed
    Curtailed,
    /// Measured PV (kWh)
    Valid(f32),
}

/// Classify one block
#[must_use]
pub fn block_status(block: &BlockObservation, limits: &CurtailLimits) -> MeasuredStatus {
    let (Some(pv), Some(soc), Some(export_kwh)) = (block.pv_kwh, block.soc_pct, block.export_kwh)
    else {
        return MeasuredStatus::Missing;
    };
    if limits.export_curtailment_enabled && block.export_limit_w.is_none() {
        return MeasuredStatus::Missing;
    }
    let full = soc >= limits.max_soc_pct - FULL_SOC_MARGIN_PCT;
    let off_w = limits.export_off_w as f32;
    let config_cap_w = limits.export_cap_w as f32;
    let export_disabled = config_cap_w <= off_w;
    // FluxION's export curtailment (or anyone) brought the limit down to (near) zero
    let curtailment_active = !export_disabled && block.export_limit_w.is_some_and(|w| w <= off_w);
    let cap_w = block
        .export_limit_w
        .map_or(config_cap_w, |w| w.min(config_cap_w));
    let export_w = export_kwh * 4000.0;
    let at_cap = !export_disabled && export_w >= EXPORT_AT_CAP_RATIO * cap_w;
    // Export at the cap clips PV whatever the battery does (it may be charging at its maximum
    // rate); a full battery clips it when export is disabled
    if curtailment_active || at_cap || (full && export_disabled) {
        MeasuredStatus::Curtailed
    } else {
        MeasuredStatus::Valid(pv)
    }
}

/// Classify an hour from its blocks: missing when any block is, curtailed when any block is
#[must_use]
pub fn hour_status(blocks: &[BlockObservation], limits: &CurtailLimits) -> MeasuredStatus {
    if blocks.len() != BLOCKS_PER_HOUR {
        return MeasuredStatus::Missing;
    }
    let mut total = 0.0;
    let mut curtailed = false;
    for block in blocks {
        match block_status(block, limits) {
            MeasuredStatus::Missing => return MeasuredStatus::Missing,
            MeasuredStatus::Curtailed => curtailed = true,
            MeasuredStatus::Valid(kwh) => total += kwh,
        }
    }
    if curtailed {
        MeasuredStatus::Curtailed
    } else {
        MeasuredStatus::Valid(total)
    }
}

/// One hour of a day: its forecast and how its measurement counts
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HourInput {
    pub start: DateTime<Utc>,
    pub forecast_kwh: Option<f32>,
    pub measured: MeasuredStatus,
}

/// One local day of the window
#[derive(Debug, Clone, PartialEq)]
pub struct DayInput {
    pub date: NaiveDate,
    pub hours: Vec<HourInput>,
}

impl DayInput {
    /// Σ forecast and Σ measured over the valid hours
    #[must_use]
    pub fn valid_totals(&self) -> (f32, f32) {
        self.hours
            .iter()
            .filter_map(|h| match (h.forecast_kwh, h.measured) {
                (Some(f), MeasuredStatus::Valid(m)) => Some((f, m)),
                _ => None,
            })
            .fold((0.0, 0.0), |(fs, ms), (f, m)| (fs + f, ms + m))
    }
}

/// Correction factor over the days: Σ measured / Σ forecast over the valid hours of the days
/// whose valid forecast is at least `MIN_DAY_KWH_PER_KWP` × `kwp_equivalent`. Fewer than
/// `MIN_VALID_DAYS` such days give 1.0; the factor is clamped to [`FACTOR_MIN`, `FACTOR_MAX`].
#[must_use]
pub fn correction_factor(days: &[DayInput], kwp_equivalent: f32) -> CorrectionResult {
    let min_day_kwh = MIN_DAY_KWH_PER_KWP * kwp_equivalent;
    let mut result = CorrectionResult::neutral();
    for day in days {
        let (forecast, measured) = day.valid_totals();
        if kwp_equivalent > 0.0 && forecast > 0.0 && forecast >= min_day_kwh {
            result.days_used += 1;
            result.forecast_kwh += forecast;
            result.measured_kwh += measured;
        }
    }
    if result.days_used >= MIN_VALID_DAYS && result.forecast_kwh > 0.0 {
        result.factor = (result.measured_kwh / result.forecast_kwh).clamp(FACTOR_MIN, FACTOR_MAX);
    }
    result
}

/// Block observations from recorded timelines over `starts`
#[must_use]
pub fn observe_blocks(
    pv: &[Timeline],
    soc: &Timeline,
    grid_export: &Timeline,
    export_limit: Option<&Timeline>,
    starts: &[DateTime<Utc>],
) -> Vec<BlockObservation> {
    let to_kwh = |mean_w: f32| mean_w * 0.25 / 1000.0;
    starts
        .iter()
        .map(|&t| {
            let end = t + Duration::minutes(BLOCK_MINUTES);
            let last = end - Duration::seconds(1);
            let pv_kwh = if pv.is_empty() {
                None
            } else {
                pv.iter()
                    .map(|unit| unit.mean_covering(t, end, MIN_BLOCK_COVERAGE).map(to_kwh))
                    .sum::<Option<f32>>()
            };
            let soc_pct = match (soc.value_at(t), soc.value_at(last)) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            let export_limit_w =
                export_limit.and_then(|limit| match (limit.value_at(t), limit.value_at(last)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    _ => None,
                });
            BlockObservation {
                pv_kwh,
                soc_pct,
                export_kwh: grid_export
                    .mean_covering(t, end, MIN_BLOCK_COVERAGE)
                    .map(to_kwh),
                export_limit_w,
            }
        })
        .collect()
}

/// Day inputs of the window from the observations (aligned with the blocks from local midnight
/// of the first day) and the stored forecasts
#[must_use]
pub fn build_day_inputs(
    dates: &[NaiveDate],
    tz: Tz,
    forecasts: &ForecastStore,
    observe: impl Fn(&[DateTime<Utc>]) -> Vec<BlockObservation>,
    limits: &CurtailLimits,
) -> Vec<DayInput> {
    dates
        .iter()
        .map(|&date| {
            let starts = day_block_starts(date, tz);
            let blocks = observe(&starts);
            let forecast = forecasts.day(date);
            let hours = starts
                .chunks(BLOCKS_PER_HOUR)
                .zip(blocks.chunks(BLOCKS_PER_HOUR))
                .map(|(hour_starts, hour_blocks)| {
                    let start = hour_starts[0];
                    HourInput {
                        start,
                        forecast_kwh: forecast.and_then(|f| f.hour(start)),
                        measured: hour_status(hour_blocks, limits),
                    }
                })
                .collect();
            DayInput { date, hours }
        })
        .collect()
}

async fn read_timeline(
    reader: &dyn HistoryReader,
    entity: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Timeline, HistoryError> {
    let states = reader.history(entity, start, end, false).await?;
    Ok(Timeline::from_states(&states))
}

/// Read the window's measurements and compute the factor. Only GET requests to the recorder
/// history; nothing is written.
pub async fn compute_correction(
    reader: &dyn HistoryReader,
    site: &CorrectionSite,
    forecasts: &ForecastStore,
    today: NaiveDate,
) -> Result<CorrectionResult, HistoryError> {
    let tz = site.timezone;
    let dates = window_days(today);
    let (Some(first), Some(_)) = (dates.first(), dates.last()) else {
        return Ok(CorrectionResult::neutral());
    };
    let start = local_midnight_utc(*first, tz);
    let end = local_midnight_utc(today, tz);
    let mut pv = Vec::with_capacity(site.pv.len());
    for entity in &site.pv {
        pv.push(read_timeline(reader, entity, start, end).await?);
    }
    let soc = read_timeline(reader, &site.soc, start, end).await?;
    let grid_export = read_timeline(reader, &site.grid_export, start, end).await?;
    // The export limit is needed only when export curtailment is enabled; without it every
    // block of such a site counts as missing, which leaves the factor at 1.0
    let export_limit = match &site.export_limit {
        Some(entity) => match read_timeline(reader, entity, start, end).await {
            Ok(timeline) => Some(timeline),
            Err(e) => {
                tracing::warn!("☀️ PV forecast correction: export limit history unavailable ({e})");
                None
            }
        },
        None => None,
    };
    let days = build_day_inputs(
        &dates,
        tz,
        forecasts,
        |starts| observe_blocks(&pv, &soc, &grid_export, export_limit.as_ref(), starts),
        &site.limits,
    );
    Ok(correction_factor(&days, forecasts.peak_hour_kwh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const TZ: Tz = chrono_tz::Europe::Prague;

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    fn limits() -> CurtailLimits {
        CurtailLimits {
            max_soc_pct: 100.0,
            export_cap_w: 10_000,
            export_off_w: EXPORT_OFF_MIN_W,
            export_curtailment_enabled: false,
        }
    }

    /// A clear-ish day shape: forecast kWh per local hour 07..17, peak 5 kWh at noon
    fn shape(local_hour: u32) -> f32 {
        match local_hour {
            7..=17 => 5.0 - (local_hour as f32 - 12.0).abs() * 0.8,
            _ => 0.0,
        }
    }

    /// A day whose every hour is valid and measured = forecast × `ratio`
    fn day(date: NaiveDate, ratio: f32) -> DayInput {
        let hours = day_hour_starts(date, TZ)
            .into_iter()
            .map(|start| {
                let f = shape(start.with_timezone(&TZ).hour());
                HourInput {
                    start,
                    forecast_kwh: Some(f),
                    measured: MeasuredStatus::Valid(f * ratio),
                }
            })
            .collect();
        DayInput { date, hours }
    }

    fn week(ratio: f32) -> Vec<DayInput> {
        (11..=17).map(|d| day(date(d), ratio)).collect()
    }

    /// kWp-equivalent of the synthetic shape: its highest hour
    const KWP: f32 = 5.0;

    fn block(pv: f32, soc: f32, export_w: f32, limit_w: Option<f32>) -> BlockObservation {
        BlockObservation {
            pv_kwh: Some(pv),
            soc_pct: Some(soc),
            export_kwh: Some(export_w / 4000.0),
            export_limit_w: limit_w,
        }
    }

    #[test]
    fn factor_from_a_synthetic_week_of_plus_seventeen_percent() {
        let r = correction_factor(&week(1.17), KWP);
        assert!((r.factor - 1.17).abs() < 1e-4, "{}", r.factor);
        assert_eq!(r.days_used, 7);
        assert!((r.measured_kwh / r.forecast_kwh - 1.17).abs() < 1e-4);
    }

    #[test]
    fn curtailed_hours_are_excluded() {
        // Every day: the noon hour was curtailed and measured only half of the forecast
        let mut days = week(1.1);
        for d in &mut days {
            for h in &mut d.hours {
                if h.start.with_timezone(&TZ).hour() == 12 {
                    h.measured = MeasuredStatus::Curtailed;
                }
            }
        }
        let r = correction_factor(&days, KWP);
        assert!((r.factor - 1.1).abs() < 1e-4, "{}", r.factor);
    }

    #[test]
    fn missing_hours_are_excluded() {
        let mut days = week(0.9);
        for d in &mut days {
            // A forecast hour without measurement, and a measured hour without forecast
            d.hours[10].measured = MeasuredStatus::Missing;
            d.hours[11].forecast_kwh = None;
            d.hours[11].measured = MeasuredStatus::Valid(100.0);
        }
        let r = correction_factor(&days, KWP);
        assert!((r.factor - 0.9).abs() < 1e-4, "{}", r.factor);
    }

    #[test]
    fn fewer_than_three_valid_days_give_one() {
        let two: Vec<DayInput> = (11..=12).map(|d| day(date(d), 1.3)).collect();
        let r = correction_factor(&two, KWP);
        assert_eq!(r.factor, 1.0);
        assert_eq!(r.days_used, 2);
        let three: Vec<DayInput> = (11..=13).map(|d| day(date(d), 1.3)).collect();
        assert!((correction_factor(&three, KWP).factor - 1.3).abs() < 1e-4);
        // No kWp-equivalent known yet: no day can count
        assert_eq!(correction_factor(&week(1.3), 0.0).factor, 1.0);
    }

    #[test]
    fn factor_is_clamped_at_both_ends() {
        assert_eq!(correction_factor(&week(2.4), KWP).factor, FACTOR_MAX);
        assert_eq!(correction_factor(&week(0.2), KWP).factor, FACTOR_MIN);
    }

    #[test]
    fn dark_days_below_the_minimum_do_not_count() {
        // Three normal days at ×1.2, four dark days (forecast 1/20 of normal) at ×3
        let mut days: Vec<DayInput> = (11..=13).map(|d| day(date(d), 1.2)).collect();
        for d in 14..=17 {
            let mut dark = day(date(d), 3.0);
            for h in &mut dark.hours {
                h.forecast_kwh = h.forecast_kwh.map(|f| f / 20.0);
                if let MeasuredStatus::Valid(m) = h.measured {
                    h.measured = MeasuredStatus::Valid(m / 20.0);
                }
            }
            let (forecast, _) = dark.valid_totals();
            assert!(forecast < MIN_DAY_KWH_PER_KWP * KWP, "{forecast}");
            days.push(dark);
        }
        let r = correction_factor(&days, KWP);
        assert_eq!(r.days_used, 3);
        assert!((r.factor - 1.2).abs() < 1e-4, "{}", r.factor);
    }

    #[test]
    fn dst_day_has_twenty_five_hours_and_counts() {
        // 2026-10-25: CEST ends, the local day has 25 hours
        let dst = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        assert_eq!(day_hour_starts(dst, TZ).len(), 25);
        let spring = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert_eq!(day_hour_starts(spring, TZ).len(), 23);

        // A snapshot of the DST day keeps both 02:00 hours
        let blocks: Vec<(DateTime<Utc>, f32)> = day_block_starts(dst, TZ)
            .into_iter()
            .map(|t| (t, 0.25))
            .collect();
        let hours = hourly_forecast(&blocks, dst, TZ);
        assert_eq!(hours.len(), 25);
        assert!(hours.iter().all(|h| (h.kwh - 1.0).abs() < 1e-6));

        // The window ending on the DST day: its 25 hours all count
        let days: Vec<DayInput> = (21..=25)
            .map(|d| day(NaiveDate::from_ymd_opt(2026, 10, d).unwrap(), 1.15))
            .collect();
        assert_eq!(days[4].hours.len(), 25);
        let r = correction_factor(&days, KWP);
        assert_eq!(r.days_used, 5);
        assert!((r.factor - 1.15).abs() < 1e-4, "{}", r.factor);
    }

    #[test]
    fn invalid_factors_become_one_and_few_days_do_not_apply() {
        let r = |factor, days_used| CorrectionResult {
            factor,
            days_used,
            measured_kwh: 1.0,
            forecast_kwh: 1.0,
        };
        assert_eq!(r(1.17, 7).sanitized().factor, 1.17);
        assert_eq!(r(FACTOR_MIN, 7).sanitized().factor, FACTOR_MIN);
        assert_eq!(r(FACTOR_MAX, 7).sanitized().factor, FACTOR_MAX);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.51, 0.49, 0.0] {
            assert_eq!(r(bad, 7).sanitized().factor, 1.0, "{bad}");
        }
        assert!(r(1.0, 3).is_applied());
        assert!(!r(1.0, 2).is_applied());
    }

    #[test]
    fn block_curtailment_rules() {
        let l = limits();
        // Normal block
        assert_eq!(
            block_status(&block(1.0, 60.0, 5000.0, None), &l),
            MeasuredStatus::Valid(1.0)
        );
        // Full battery and export at 95 % of the cap
        assert_eq!(
            block_status(&block(1.0, 99.0, 9500.0, None), &l),
            MeasuredStatus::Curtailed
        );
        // Full battery, export below the cap: PV was not held back
        assert_eq!(
            block_status(&block(1.0, 100.0, 9000.0, None), &l),
            MeasuredStatus::Valid(1.0)
        );
        // Export at the cap with the battery not full (charging at its maximum rate while PV
        // clips at the hard cap): curtailed whatever the SOC
        assert_eq!(
            block_status(&block(1.0, 98.0, 10_000.0, None), &l),
            MeasuredStatus::Curtailed
        );
        assert_eq!(
            block_status(&block(1.0, 30.0, 9500.0, None), &l),
            MeasuredStatus::Curtailed
        );
        // A 27.5 kW hard cap: at 95 % of it
        let hard_cap = CurtailLimits {
            export_cap_w: 27_500,
            ..l
        };
        assert_eq!(
            block_status(&block(1.0, 60.0, 26_200.0, None), &hard_cap),
            MeasuredStatus::Curtailed
        );
        assert_eq!(
            block_status(&block(1.0, 60.0, 25_000.0, None), &hard_cap),
            MeasuredStatus::Valid(1.0)
        );
        // Export limit lowered to 4 kW: export at it clips PV, battery not full either
        assert_eq!(
            block_status(&block(1.0, 50.0, 3900.0, Some(4000.0)), &l),
            MeasuredStatus::Curtailed
        );
        // Export limit lowered to 4 kW (hard cap): full + export at it
        assert_eq!(
            block_status(&block(1.0, 100.0, 3900.0, Some(4000.0)), &l),
            MeasuredStatus::Curtailed
        );
        // Export curtailment active (limit 10 W), whatever the battery
        assert_eq!(
            block_status(&block(1.0, 40.0, 0.0, Some(10.0)), &l),
            MeasuredStatus::Curtailed
        );
        // Export disabled by configuration: curtailed only with a full battery
        let disabled = CurtailLimits {
            export_cap_w: 0,
            ..l
        };
        assert_eq!(
            block_status(&block(1.0, 50.0, 0.0, None), &disabled),
            MeasuredStatus::Valid(1.0)
        );
        assert_eq!(
            block_status(&block(1.0, 99.5, 0.0, None), &disabled),
            MeasuredStatus::Curtailed
        );
    }

    #[test]
    fn block_with_missing_data_is_missing() {
        let l = limits();
        let mut b = block(1.0, 50.0, 0.0, None);
        b.pv_kwh = None;
        assert_eq!(block_status(&b, &l), MeasuredStatus::Missing);
        let mut b = block(1.0, 50.0, 0.0, None);
        b.soc_pct = None;
        assert_eq!(block_status(&b, &l), MeasuredStatus::Missing);
        let mut b = block(1.0, 50.0, 0.0, None);
        b.export_kwh = None;
        assert_eq!(block_status(&b, &l), MeasuredStatus::Missing);
        // With export curtailment enabled the export limit is needed
        let curtailing = CurtailLimits {
            export_curtailment_enabled: true,
            ..l
        };
        assert_eq!(
            block_status(&block(1.0, 50.0, 0.0, None), &curtailing),
            MeasuredStatus::Missing
        );
        assert_eq!(
            block_status(&block(1.0, 50.0, 0.0, Some(10_000.0)), &curtailing),
            MeasuredStatus::Valid(1.0)
        );
    }

    #[test]
    fn hour_is_missing_or_curtailed_when_any_block_is() {
        let l = limits();
        let ok = block(0.5, 50.0, 0.0, None);
        assert_eq!(hour_status(&[ok; 4], &l), MeasuredStatus::Valid(2.0));
        let mut blocks = [ok; 4];
        blocks[2] = block(0.5, 100.0, 10_000.0, None);
        assert_eq!(hour_status(&blocks, &l), MeasuredStatus::Curtailed);
        blocks[1].pv_kwh = None;
        assert_eq!(hour_status(&blocks, &l), MeasuredStatus::Missing);
        assert_eq!(hour_status(&[ok; 3], &l), MeasuredStatus::Missing);
    }

    /// Recorded history of power sensors, one constant value over the window
    fn constant(at: DateTime<Utc>, value: &str) -> Timeline {
        Timeline::from_states(&[HistoryState::from_raw(at, value, None)])
    }

    #[test]
    fn observed_pv_sums_every_unit_and_needs_all_of_them() {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap();
        let starts = [t0];
        let master = constant(t0 - Duration::hours(1), "4000");
        let extra = constant(t0 - Duration::hours(1), "2000");
        let soc = constant(t0 - Duration::hours(1), "55");
        let export = constant(t0 - Duration::hours(1), "0");
        let obs = observe_blocks(&[master.clone(), extra], &soc, &export, None, &starts);
        assert!((obs[0].pv_kwh.unwrap() - 1.5).abs() < 1e-6);
        // The extra unit unavailable: PV missing (not the master alone)
        let gap = Timeline::from_states(&[HistoryState::from_raw(
            t0 - Duration::hours(1),
            "unavailable",
            None,
        )]);
        let obs = observe_blocks(&[master, gap], &soc, &export, None, &starts);
        assert_eq!(obs[0].pv_kwh, None);
    }

    #[test]
    fn block_needs_data_over_most_of_its_span() {
        // 1 min at 8000 W, then 14 min unavailable: not 2.0 kWh, the block is missing
        let t0 = Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap();
        let sparse = Timeline::from_states(&[
            HistoryState::from_raw(t0, "8000", None),
            HistoryState::from_raw(t0 + Duration::minutes(1), "unavailable", None),
        ]);
        let soc = constant(t0 - Duration::hours(1), "55");
        let export = constant(t0 - Duration::hours(1), "0");
        let obs = observe_blocks(std::slice::from_ref(&sparse), &soc, &export, None, &[t0]);
        assert_eq!(obs[0].pv_kwh, None);
        assert_eq!(block_status(&obs[0], &limits()), MeasuredStatus::Missing);
        // The same gap in the export sensor makes the block missing too
        let pv = constant(t0 - Duration::hours(1), "4000");
        let obs = observe_blocks(&[pv], &soc, &sparse, None, &[t0]);
        assert_eq!(obs[0].export_kwh, None);
        // 13 of 15 minutes with data (87 %) is enough; the mean is over the covered part
        let mostly = Timeline::from_states(&[
            HistoryState::from_raw(t0, "unavailable", None),
            HistoryState::from_raw(t0 + Duration::minutes(2), "4000", None),
        ]);
        let obs = observe_blocks(&[mostly], &soc, &export, None, &[t0]);
        assert!((obs[0].pv_kwh.unwrap() - 1.0).abs() < 1e-6);
        // The backtest's own mean is unchanged: any data counts
        assert!((sparse.mean(t0, t0 + Duration::minutes(15)).unwrap() - 8000.0).abs() < 1e-3);
    }

    #[test]
    fn snapshot_is_taken_once_after_midnight() {
        let mut store = ForecastStore::default();
        let day_blocks: Vec<(DateTime<Utc>, f32)> = day_block_starts(date(14), TZ)
            .into_iter()
            .map(|t| (t, shape(t.with_timezone(&TZ).hour()) / 4.0))
            .collect();
        // 00:30 local on the 14th: due and taken
        let at = TZ
            .with_ymd_and_hms(2026, 10, 14, 0, 30, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert!(store.record_snapshot(at, TZ, std::slice::from_ref(&day_blocks), 1));
        let snap = store.day(date(14)).unwrap();
        assert_eq!(snap.source, ForecastSource::Snapshot);
        assert_eq!(snap.hours.len(), 24);
        assert!((store.peak_hour_kwh - 5.0).abs() < 1e-6);
        // A later fetch the same day does not replace it
        let scaled: Vec<(DateTime<Utc>, f32)> =
            day_blocks.iter().map(|(t, k)| (*t, k * 2.0)).collect();
        assert!(!store.record_snapshot(
            at + Duration::hours(1),
            TZ,
            std::slice::from_ref(&scaled),
            1
        ));
        // From 06:00 local no snapshot is taken for a day without one
        let late = TZ
            .with_ymd_and_hms(2026, 10, 15, 6, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert!(!store.record_snapshot(late, TZ, std::slice::from_ref(&scaled), 1));
        // An empty forecast is not a snapshot
        let early = TZ
            .with_ymd_and_hms(2026, 10, 15, 0, 5, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert!(!store.record_snapshot(early, TZ, &[], 1));
    }

    #[test]
    fn store_keeps_fourteen_days_and_round_trips() {
        let mut store = ForecastStore::default();
        for d in 1..=20 {
            store.insert(DayForecast {
                date: date(d),
                taken_at: Utc::now(),
                source: ForecastSource::Snapshot,
                hours: vec![HourForecast {
                    start: local_midnight_utc(date(d), TZ),
                    kwh: d as f32,
                }],
                sensors: 1,
            });
        }
        assert_eq!(store.days.len(), KEEP_DAYS);
        assert_eq!(store.days[0].date, date(7));
        assert_eq!(store.peak_hour_kwh, 20.0);
        store.last = Some(StoredCorrection {
            computed_on: date(20),
            result: CorrectionResult {
                factor: 1.17,
                days_used: 7,
                measured_kwh: 117.0,
                forecast_kwh: 100.0,
            },
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pv_forecast_snapshots.json");
        store.save(&path).unwrap();
        assert_eq!(ForecastStore::load(&path), store);
        assert_eq!(store.missing_days(date(21)), Vec::<NaiveDate>::new());
        assert_eq!(store.missing_days(date(23)), vec![date(21), date(22)]);
        assert_eq!(
            ForecastStore::load(&dir.path().join("absent.json")),
            ForecastStore::default()
        );
    }

    #[test]
    fn bootstrap_from_recorded_wh_period_15m() {
        // The today sensor as recorded: the 23:50 state still forecasts the 13th; the 00:40
        // state forecasts the 14th (anonymised, shape of the Open-Meteo integration)
        let day14: serde_json::Map<String, serde_json::Value> = day_block_starts(date(14), TZ)
            .into_iter()
            .map(|t| {
                let local = t.with_timezone(&TZ);
                (
                    local.to_rfc3339(),
                    serde_json::json!(shape(local.hour()) * 250.0),
                )
            })
            .collect();
        let day13: serde_json::Map<String, serde_json::Value> = day_block_starts(date(13), TZ)
            .into_iter()
            .map(|t| (t.with_timezone(&TZ).to_rfc3339(), serde_json::json!(999.0)))
            .collect();
        let at = |h: u32, m: u32, d: u32| {
            TZ.with_ymd_and_hms(2026, 10, d, h, m, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        let states = vec![
            HistoryState::from_raw(
                at(23, 50, 13),
                "40.1",
                Some(serde_json::json!({ "wh_period_15m": day13 })),
            ),
            HistoryState::from_raw(
                at(0, 40, 14),
                "41.0",
                Some(serde_json::json!({ "wh_period_15m": day14, "unit_of_measurement": "kWh" })),
            ),
        ];
        let f = day_forecast_from_history(std::slice::from_ref(&states), date(14), TZ).unwrap();
        assert_eq!(f.source, ForecastSource::HaHistory);
        assert_eq!(f.taken_at, at(0, 40, 14));
        let noon = TZ
            .with_ymd_and_hms(2026, 10, 14, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert!((f.hour(noon).unwrap() - 5.0).abs() < 1e-5);
        let total: f32 = f.hours.iter().map(|h| h.kwh).sum();
        let expected: f32 = (0..24).map(shape).sum();
        assert!((total - expected).abs() < 1e-3);

        // Without the per-period attribute (watts / wh_period are not recorded): not rebuilt
        let bare = vec![HistoryState::from_raw(
            at(0, 40, 14),
            "41.0",
            Some(serde_json::json!({ "unit_of_measurement": "kWh" })),
        )];
        assert!(day_forecast_from_history(&[bare], date(14), TZ).is_none());
        // Two sensors: both must be rebuilt
        assert!(day_forecast_from_history(&[states, Vec::new()], date(14), TZ).is_none());
    }

    /// Recorded history served from memory
    struct FakeHistory(std::collections::HashMap<String, Vec<HistoryState>>);

    #[async_trait::async_trait]
    impl HistoryReader for FakeHistory {
        async fn history(
            &self,
            entity_id: &str,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _with_attributes: bool,
        ) -> Result<Vec<HistoryState>, HistoryError> {
            self.0
                .get(entity_id)
                .cloned()
                .ok_or(HistoryError::Status(404))
        }
    }

    #[tokio::test]
    async fn compute_from_history_and_snapshots() {
        // Seven days 11..17: PV follows 1.17 × the snapshot forecast; on the 16th the battery
        // was full and export at the cap from 11:00 to 14:00 (curtailed hours, PV low)
        let today = date(18);
        let mut store = ForecastStore::default();
        let mut pv_states = Vec::new();
        let mut soc_states = Vec::new();
        let mut export_states = Vec::new();
        for d in 11..=17 {
            let blocks: Vec<(DateTime<Utc>, f32)> = day_block_starts(date(d), TZ)
                .into_iter()
                .map(|t| (t, shape(t.with_timezone(&TZ).hour()) / 4.0))
                .collect();
            let first = local_midnight_utc(date(d), TZ);
            assert!(store.record_snapshot(
                first + Duration::minutes(5),
                TZ,
                std::slice::from_ref(&blocks),
                1
            ));
            for start in day_hour_starts(date(d), TZ) {
                let hour = start.with_timezone(&TZ).hour();
                let curtailed = d == 16 && (11..14).contains(&hour);
                let pv_w = if curtailed {
                    1000.0
                } else {
                    shape(hour) * 1.17 * 1000.0
                };
                pv_states.push(HistoryState::from_raw(start, &pv_w.to_string(), None));
                let (soc, export) = if curtailed {
                    ("100", "9800")
                } else {
                    ("60", "0")
                };
                soc_states.push(HistoryState::from_raw(start, soc, None));
                export_states.push(HistoryState::from_raw(start, export, None));
            }
        }
        let reader = FakeHistory(
            [
                ("sensor.inv_pv_power_total".to_owned(), pv_states),
                ("sensor.inv_battery_capacity".to_owned(), soc_states),
                ("sensor.inv_grid_export".to_owned(), export_states),
            ]
            .into_iter()
            .collect(),
        );
        let site = CorrectionSite {
            timezone: TZ,
            pv: vec!["sensor.inv_pv_power_total".to_owned()],
            soc: "sensor.inv_battery_capacity".to_owned(),
            grid_export: "sensor.inv_grid_export".to_owned(),
            export_limit: Some("number.inv_export_limit".to_owned()),
            limits: limits(),
        };
        let r = compute_correction(&reader, &site, &store, today)
            .await
            .unwrap();
        assert_eq!(r.days_used, 7);
        assert!((r.factor - 1.17).abs() < 1e-3, "{}", r.factor);

        // A missing snapshot day does not count
        store.days.retain(|d| d.date != date(13));
        let r = compute_correction(&reader, &site, &store, today)
            .await
            .unwrap();
        assert_eq!(r.days_used, 6);

        // The SOC history unreadable: an error, the caller keeps its factor
        let mut broken = site.clone();
        broken.soc = "sensor.absent".to_owned();
        assert!(
            compute_correction(&reader, &broken, &store, today)
                .await
                .is_err()
        );
    }
}
