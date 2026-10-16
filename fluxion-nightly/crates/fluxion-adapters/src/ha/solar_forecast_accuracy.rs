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

//! Solar forecast accuracy: each local day the day-ahead forecast is recorded, the measured PV
//! energy is read from HA long-term statistics in the evening, and the 7-day bias is published as

//! Solar forecast accuracy: each local day the day-ahead forecast is recorded, the measured PV
//! energy is read from HA long-term statistics in the evening, and the 7-day bias is published as
//! `sensor.fluxion_solar_forecast_bias`. Read-only with respect to the inverter.

use crate::ha::client::HomeAssistantClient;
use crate::ha::ws::{HaWsSession, websocket_url};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Where the forecast records are persisted (survives restarts)
pub const BIAS_STATE_PATH: &str = "/data/solar_forecast_accuracy.json";
/// Published bias sensor
pub const BIAS_ENTITY_ID: &str = "sensor.fluxion_solar_forecast_bias";
/// Stable id of the persistent notification (one per day at most)
pub const BIAS_NOTIFICATION_ID: &str = "fluxion_solar_forecast_bias";

/// The day-ahead forecast is taken at the first fetch from this local hour...
const FORECAST_FROM_HOUR: u32 = 5;
/// ...and only before this local hour; a day without a complete forecast by then is skipped
const FORECAST_UNTIL_HOUR: u32 = 12;
/// Measurements are read from this local hour on
const READ_FROM_HOUR: u32 = 22;
/// A day is finalised only when its hourly rows reach this local hour
const FINALISE_HOUR: u32 = 21;
/// Days kept in the store
const KEEP_DAYS: usize = 14;
/// Unmeasured days older than this are dropped
const PENDING_MAX_AGE_DAYS: i64 = 2;
/// Days in the bias window
const BIAS_WINDOW_DAYS: usize = 7;
/// Fewest days with a measurement before a bias notice is sent
const MIN_DAYS_FOR_NOTICE: usize = 5;
/// Bias (%) above which a notice is sent
const NOTICE_BIAS_PCT: f32 = 10.0;
/// Pause between statistics reads
const ACTUAL_RETRY: Duration = Duration::from_secs(15 * 60);
/// The bias state is re-sent at least this often, so it returns after an HA restart
const PUBLISH_REFRESH: Duration = Duration::from_secs(15 * 60);

/// One local day: the forecast taken in the morning and the measured energy in the evening
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayRecord {
    pub date: NaiveDate,
    pub forecast_kwh: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_kwh: Option<f32>,
}

/// Persisted accuracy state
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccuracyStore {
    #[serde(default)]
    pub days: Vec<DayRecord>,
    /// Day of the last bias notice
    #[serde(default)]
    pub last_notified: Option<NaiveDate>,
}

/// Forecast error of one measured day: (forecast - actual) / actual in percent
#[derive(Debug, Clone, PartialEq)]
pub struct DayError {
    pub date: NaiveDate,
    pub forecast_kwh: f32,
    pub actual_kwh: f32,
    /// `None` when the measured energy is zero
    pub error_pct: Option<f32>,
}

/// The bias over the last 7 measured days, and the rows shown as attributes
#[derive(Debug, Clone, PartialEq)]
pub struct BiasSummary {
    /// (Σ forecast − Σ actual) / Σ actual × 100 over the window; positive = forecast too high
    pub bias_pct: f32,
    /// Mean absolute error of the shown rows
    pub mean_abs_error_pct: Option<f32>,
    /// Measured days in the window
    pub days: usize,
    /// Every measured day kept (oldest first)
    pub rows: Vec<DayError>,
}

fn day_error(day: &DayRecord, actual_kwh: f32) -> DayError {
    let error_pct =
        (actual_kwh > 0.0).then(|| (day.forecast_kwh - actual_kwh) / actual_kwh * 100.0);
    DayError {
        date: day.date,
        forecast_kwh: day.forecast_kwh,
        actual_kwh,
        error_pct,
    }
}

/// Bias summary of the stored days. `None` when no measured day has energy.
#[must_use]
pub fn bias_summary(days: &[DayRecord]) -> Option<BiasSummary> {
    let mut measured: Vec<(&DayRecord, f32)> = days
        .iter()
        .filter_map(|d| d.actual_kwh.map(|a| (d, a)))
        .collect();
    measured.sort_by_key(|(d, _)| d.date);

    let window = &measured[measured.len().saturating_sub(BIAS_WINDOW_DAYS)..];
    let forecast: f32 = window.iter().map(|(d, _)| d.forecast_kwh).sum();
    let actual: f32 = window.iter().map(|(_, a)| a).sum();
    if actual <= 0.0 {
        return None;
    }

    let rows: Vec<DayError> = measured.iter().map(|(d, a)| day_error(d, *a)).collect();
    let errors: Vec<f32> = rows
        .iter()
        .filter_map(|r| r.error_pct.map(f32::abs))
        .collect();
    let mean_abs_error_pct =
        (!errors.is_empty()).then(|| errors.iter().sum::<f32>() / errors.len() as f32);

    Some(BiasSummary {
        bias_pct: (forecast - actual) / actual * 100.0,
        mean_abs_error_pct,
        days: window.len(),
        rows,
    })
}

/// A bias notice is due when at least 5 days are measured, the bias is beyond 10 %, and no notice
/// was sent today
#[must_use]
pub fn bias_notice_due(
    summary: Option<&BiasSummary>,
    today: NaiveDate,
    last_notified: Option<NaiveDate>,
) -> bool {
    let Some(summary) = summary else {
        return false;
    };
    summary.days >= MIN_DAYS_FOR_NOTICE
        && summary.bias_pct.abs() > NOTICE_BIAS_PCT
        && last_notified != Some(today)
}

/// State and attributes of `sensor.fluxion_solar_forecast_bias`
#[must_use]
pub fn bias_payload(days: &[DayRecord]) -> (String, Value) {
    let summary = bias_summary(days);
    let state = summary
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |s| format!("{:.1}", s.bias_pct));
    let attributes = match summary.as_ref() {
        Some(s) => json!({
            "unit_of_measurement": "%",
            "friendly_name": "FluxION solar forecast bias",
            "window_days": BIAS_WINDOW_DAYS,
            "days": s.days,
            "mean_abs_error_pct": s.mean_abs_error_pct.map(round1),
            "rows": s.rows.iter().map(|r| json!({
                "date": r.date.to_string(),
                "forecast_kwh": round1(r.forecast_kwh),
                "actual_kwh": round1(r.actual_kwh),
                "error_pct": r.error_pct.map(round1),
            })).collect::<Vec<_>>(),
        }),
        None => json!({
            "unit_of_measurement": "%",
            "friendly_name": "FluxION solar forecast bias",
            "window_days": BIAS_WINDOW_DAYS,
            "days": 0,
            "rows": [],
        }),
    };
    (state, attributes)
}

/// Publish when the body changed or the last publish is older than `PUBLISH_REFRESH`
#[must_use]
pub fn publish_due(last: Option<(&str, Instant)>, body: &str, now: Instant) -> bool {
    match last {
        None => true,
        Some((previous, at)) => previous != body || now.duration_since(at) >= PUBLISH_REFRESH,
    }
}

/// The morning forecast is recorded from `FORECAST_FROM_HOUR` until `FORECAST_UNTIL_HOUR` and only
/// when the day has no record yet
#[must_use]
pub fn forecast_due(local: NaiveDateTime, has_day: bool) -> bool {
    !has_day && (FORECAST_FROM_HOUR..FORECAST_UNTIL_HOUR).contains(&local.hour())
}

/// UTC bounds (start, end) of a local day
#[must_use]
pub fn local_day_utc_bounds(date: NaiveDate, tz: Tz) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = local_at(date, 0, tz)?;
    let end = local_at(date.succ_opt()?, 0, tz)?;
    Some((start, end))
}

/// The UTC instant of a local hour on a date
fn local_at(date: NaiveDate, hour: u32, tz: Tz) -> Option<DateTime<Utc>> {
    tz.from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
}

/// `recorder/statistics_during_period` request for hourly mean power of the entities
#[must_use]
pub fn statistics_request(
    entity_ids: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Value {
    json!({
        "type": "recorder/statistics_during_period",
        "start_time": start.to_rfc3339(),
        "end_time": end.to_rfc3339(),
        "statistic_ids": entity_ids,
        "period": "hour",
        "types": ["mean"],
    })
}

/// A statistics row time: epoch milliseconds (the HA websocket shape), or RFC 3339 text
fn stat_time(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(ms) = value.as_i64() {
        return Utc.timestamp_millis_opt(ms).single();
    }
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Energy (kWh) of a day from the hourly statistics, summed over the entities.
///
/// Every entity must have rows, and its last row must end at or after `finalise_at`; otherwise
/// the day is not final and `None` is returned. Each hourly mean power (W) over its duration gives
/// W·h / 1000 kWh.
#[must_use]
pub fn actual_kwh_from_statistics(
    result: &Value,
    entity_ids: &[String],
    finalise_at: DateTime<Utc>,
) -> Option<f32> {
    let series = result.as_object()?;
    if entity_ids.is_empty() {
        return None;
    }
    let mut total_kwh = 0.0_f32;
    for entity_id in entity_ids {
        let rows = series.get(entity_id)?.as_array()?;
        let mut covered_until: Option<DateTime<Utc>> = None;
        let mut counted = 0_usize;
        let mut kwh = 0.0_f32;
        for row in rows {
            let (Some(start), Some(end)) = (stat_time(&row["start"]), stat_time(&row["end"]))
            else {
                continue;
            };
            covered_until = Some(covered_until.map_or(end, |c| c.max(end)));
            if let Some(mean_w) = row["mean"].as_f64() {
                let hours = (end - start).num_milliseconds() as f32 / 3_600_000.0;
                kwh += mean_w as f32 * hours / 1000.0;
                counted += 1;
            }
        }
        if counted == 0 || covered_until.is_none_or(|c| c < finalise_at) {
            return None;
        }
        total_kwh += kwh;
    }
    Some(total_kwh)
}

impl AccuracyStore {
    /// Load the store; a missing or unreadable file gives an empty store
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// Persist the store as JSON
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    #[must_use]
    pub fn has_day(&self, date: NaiveDate) -> bool {
        self.days.iter().any(|d| d.date == date)
    }

    /// Record the day's forecast once. Returns true when a record was added.
    pub fn record_forecast(&mut self, date: NaiveDate, forecast_kwh: f32) -> bool {
        if self.has_day(date) {
            return false;
        }
        self.days.push(DayRecord {
            date,
            forecast_kwh,
            actual_kwh: None,
        });
        self.days.sort_by_key(|d| d.date);
        let excess = self.days.len().saturating_sub(KEEP_DAYS);
        self.days.drain(..excess);
        true
    }

    /// Unmeasured days up to `today` that are not older than `PENDING_MAX_AGE_DAYS`
    #[must_use]
    pub fn pending_actual(&self, today: NaiveDate) -> Vec<NaiveDate> {
        self.days
            .iter()
            .filter(|d| d.actual_kwh.is_none() && pending_age_ok(d.date, today))
            .map(|d| d.date)
            .collect()
    }

    /// Drop unmeasured days older than `PENDING_MAX_AGE_DAYS`. Returns true when any was dropped.
    pub fn drop_stale_pending(&mut self, today: NaiveDate) -> bool {
        let before = self.days.len();
        self.days
            .retain(|d| d.actual_kwh.is_some() || pending_age_ok(d.date, today));
        before != self.days.len()
    }

    /// Store the measured energy of a day. Returns true when the day is in the store.
    pub fn set_actual(&mut self, date: NaiveDate, actual_kwh: f32) -> bool {
        match self.days.iter_mut().find(|d| d.date == date) {
            Some(day) => {
                day.actual_kwh = Some(actual_kwh);
                true
            }
            None => false,
        }
    }
}

/// A day is pending no older than the limit (and never in the future)
fn pending_age_ok(date: NaiveDate, today: NaiveDate) -> bool {
    date <= today && (today - date).num_days() <= PENDING_MAX_AGE_DAYS
}

/// Runs the monitor inside the solar forecast fetcher loop
pub struct AccuracyMonitor {
    path: PathBuf,
    tz: Tz,
    entities: Vec<String>,
    store: AccuracyStore,
    last_actual_try: Option<Instant>,
    /// Last published body and when it was sent
    last_publish: Option<(String, Instant)>,
}

impl AccuracyMonitor {
    #[must_use]
    pub fn new(path: PathBuf, tz: Tz, entities: Vec<String>) -> Self {
        let store = AccuracyStore::load(&path);
        Self {
            path,
            tz,
            entities,
            store,
            last_actual_try: None,
            last_publish: None,
        }
    }

    /// One fetch cycle: record the morning forecast, read evening measurements, publish the bias
    /// and send at most one notice per day. `forecast_total_kwh` is the complete total-today
    /// forecast, `None` when a sensor was missing.
    pub async fn tick(
        &mut self,
        client: &HomeAssistantClient,
        now: DateTime<Utc>,
        forecast_total_kwh: Option<f32>,
    ) {
        let local = now.with_timezone(&self.tz).naive_local();
        let today = local.date();
        let mut changed = false;

        if forecast_due(local, self.store.has_day(today))
            && let Some(kwh) = forecast_total_kwh.filter(|k| *k > 0.0)
        {
            self.store.record_forecast(today, kwh);
            info!("☀️ Solar forecast for {}: {:.1} kWh recorded", today, kwh);
            changed = true;
        }

        if self.store.drop_stale_pending(today) {
            changed = true;
        }

        if local.hour() >= READ_FROM_HOUR && self.actual_read_due() {
            let pending = self.store.pending_actual(today);
            if !pending.is_empty() {
                for (date, actual_kwh) in self.read_actuals(client, &pending).await {
                    self.store.set_actual(date, actual_kwh);
                    info!("☀️ Measured PV energy for {}: {:.1} kWh", date, actual_kwh);
                    changed = true;
                }
            }
        }

        self.publish(client).await;

        let summary = bias_summary(&self.store.days);
        if bias_notice_due(summary.as_ref(), today, self.store.last_notified)
            && let Some(summary) = summary.as_ref()
            && self.notify(client, summary.bias_pct).await
        {
            self.store.last_notified = Some(today);
            changed = true;
        }

        if changed && let Err(e) = self.store.save(&self.path) {
            warn!("⚠️ Failed to save solar forecast accuracy records: {}", e);
        }
    }

    /// Throttles statistics reads: at most one attempt per `ACTUAL_RETRY`
    fn actual_read_due(&mut self) -> bool {
        let instant = Instant::now();
        if self
            .last_actual_try
            .is_some_and(|t| instant.duration_since(t) < ACTUAL_RETRY)
        {
            return false;
        }
        self.last_actual_try = Some(instant);
        true
    }

    /// Measured PV energy of the given days, read over one websocket session. A day is returned
    /// only when it is final (see `actual_kwh_from_statistics`).
    async fn read_actuals(
        &self,
        client: &HomeAssistantClient,
        dates: &[NaiveDate],
    ) -> Vec<(NaiveDate, f32)> {
        let mut finished = Vec::new();
        if self.entities.is_empty() {
            return finished;
        }
        let Some(url) = websocket_url(client.base_url()) else {
            warn!("⚠️ No websocket URL for the HA base URL, solar accuracy not read");
            return finished;
        };
        let mut ws = match HaWsSession::connect(&url, client.token()).await {
            Ok(ws) => ws,
            Err(e) => {
                warn!("⚠️ Failed to connect for PV statistics: {}", e);
                return finished;
            }
        };
        for &date in dates {
            let (Some((start, end)), Some(finalise_at)) = (
                local_day_utc_bounds(date, self.tz),
                local_at(date, FINALISE_HOUR, self.tz),
            ) else {
                continue;
            };
            match ws
                .call(statistics_request(&self.entities, start, end))
                .await
            {
                Ok(result) => {
                    match actual_kwh_from_statistics(&result, &self.entities, finalise_at) {
                        Some(kwh) => finished.push((date, kwh)),
                        None => debug!("PV statistics for {} not final yet", date),
                    }
                }
                Err(e) => {
                    warn!("⚠️ Failed to read PV statistics for {}: {}", date, e);
                    break;
                }
            }
        }
        finished
    }

    async fn publish(&mut self, client: &HomeAssistantClient) {
        let (state, attributes) = bias_payload(&self.store.days);
        let body = json!({ "state": state, "attributes": attributes }).to_string();
        let now = Instant::now();
        let last = self.last_publish.as_ref().map(|(b, t)| (b.as_str(), *t));
        if !publish_due(last, &body, now) {
            return;
        }
        match client.set_state(BIAS_ENTITY_ID, &state, attributes).await {
            Ok(()) => self.last_publish = Some((body, now)),
            Err(e) => warn!("⚠️ Failed to publish {}: {}", BIAS_ENTITY_ID, e),
        }
    }

    /// Send the persistent notice; `true` when HA accepted it
    async fn notify(&self, client: &HomeAssistantClient, bias_pct: f32) -> bool {
        let result = client
            .call_service(
                "persistent_notification.create",
                json!({
                    "title": "FluxION: předpověď FVE nesedí",
                    "message": format!(
                        "Předpověď výroby se za 7 dní liší od skutečnosti o {:.0} %. Upravte nastavení Open-Meteo (výkon panelů, orientace).",
                        bias_pct.abs()
                    ),
                    "notification_id": BIAS_NOTIFICATION_ID,
                }),
            )
            .await;
        match result {
            Ok(()) => true,
            Err(e) => {
                warn!("⚠️ Failed to send solar forecast notice: {}", e);
                false
            }
        }
    }
}

fn round1(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    fn measured(day: u32, forecast: f32, actual: f32) -> DayRecord {
        DayRecord {
            date: date(day),
            forecast_kwh: forecast,
            actual_kwh: Some(actual),
        }
    }

    /// Epoch milliseconds of a UTC time, as the HA statistics rows carry them
    fn ms(y: i32, mo: u32, d: u32, h: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn bias_sign_positive_when_forecast_is_high() {
        let s = bias_summary(&[measured(1, 12.0, 10.0)]).unwrap();
        assert!((s.bias_pct - 20.0).abs() < 1e-3, "{}", s.bias_pct);
        let s = bias_summary(&[measured(1, 8.0, 10.0)]).unwrap();
        assert!((s.bias_pct + 20.0).abs() < 1e-3, "{}", s.bias_pct);
    }

    #[test]
    fn bias_uses_only_the_last_seven_measured_days() {
        // Two old days with a large error are outside the window
        let mut days = vec![measured(1, 50.0, 10.0), measured(2, 50.0, 10.0)];
        days.extend((3..=9).map(|d| measured(d, 10.0, 10.0)));
        let s = bias_summary(&days).unwrap();
        assert_eq!(s.days, 7);
        assert!(s.bias_pct.abs() < 1e-3, "{}", s.bias_pct);
        assert_eq!(s.rows.len(), 9);
    }

    #[test]
    fn bias_ignores_days_without_measurement() {
        let days = [
            DayRecord {
                date: date(1),
                forecast_kwh: 30.0,
                actual_kwh: None,
            },
            measured(2, 11.0, 10.0),
        ];
        let s = bias_summary(&days).unwrap();
        assert_eq!(s.days, 1);
        assert!((s.bias_pct - 10.0).abs() < 1e-3);
    }

    #[test]
    fn no_bias_without_energy() {
        assert!(bias_summary(&[]).is_none());
        assert!(bias_summary(&[measured(1, 3.0, 0.0)]).is_none());
    }

    #[test]
    fn notice_needs_five_days() {
        let four: Vec<DayRecord> = (1..=4).map(|d| measured(d, 13.0, 10.0)).collect();
        let s = bias_summary(&four).unwrap();
        assert!(!bias_notice_due(Some(&s), date(10), None));
        let five: Vec<DayRecord> = (1..=5).map(|d| measured(d, 13.0, 10.0)).collect();
        let s = bias_summary(&five).unwrap();
        assert!(bias_notice_due(Some(&s), date(10), None));
    }

    #[test]
    fn notice_only_beyond_ten_percent() {
        let within: Vec<DayRecord> = (1..=5).map(|d| measured(d, 10.9, 10.0)).collect();
        let s = bias_summary(&within).unwrap();
        assert!(!bias_notice_due(Some(&s), date(10), None));
        let beyond: Vec<DayRecord> = (1..=5).map(|d| measured(d, 8.9, 10.0)).collect();
        let s = bias_summary(&beyond).unwrap();
        assert!(
            bias_notice_due(Some(&s), date(10), None),
            "negative bias counts too"
        );
    }

    #[test]
    fn notice_once_per_day() {
        let days: Vec<DayRecord> = (1..=5).map(|d| measured(d, 13.0, 10.0)).collect();
        let s = bias_summary(&days).unwrap();
        assert!(!bias_notice_due(Some(&s), date(10), Some(date(10))));
        assert!(bias_notice_due(Some(&s), date(11), Some(date(10))));
        assert!(!bias_notice_due(None, date(10), None));
    }

    #[test]
    fn payload_state_is_unknown_without_data_and_rounded_otherwise() {
        let (state, _) = bias_payload(&[]);
        assert_eq!(state, "unknown");
        let (state, attributes) = bias_payload(&[measured(1, 12.0, 10.0)]);
        assert_eq!(state, "20.0");
        assert_eq!(attributes["days"], 1);
    }

    #[test]
    fn publish_when_changed_or_refresh_due() {
        let now = Instant::now();
        assert!(publish_due(None, "a", now));
        assert!(!publish_due(Some(("a", now)), "a", now));
        assert!(publish_due(Some(("a", now)), "b", now));
        let later = now + PUBLISH_REFRESH;
        assert!(publish_due(Some(("a", now)), "a", later));
    }

    #[test]
    fn forecast_only_from_five_until_noon_and_once_per_day() {
        let at = |h| date(15).and_hms_opt(h, 0, 0).expect("valid time");
        assert!(!forecast_due(at(4), false));
        assert!(forecast_due(at(5), false));
        assert!(forecast_due(at(11), false));
        assert!(!forecast_due(at(12), false), "skipped after noon");
        assert!(!forecast_due(at(9), true), "one record per day");
    }

    #[test]
    fn store_records_each_day_once_and_keeps_fourteen() {
        let mut store = AccuracyStore::default();
        assert!(store.record_forecast(date(1), 10.0));
        assert!(!store.record_forecast(date(1), 99.0));
        assert_eq!(store.days[0].forecast_kwh, 10.0);
        for d in 2..=20 {
            store.record_forecast(date(d), 10.0);
        }
        assert_eq!(store.days.len(), KEEP_DAYS);
        assert_eq!(store.days[0].date, date(7));
    }

    #[test]
    fn pending_days_are_at_most_two_days_old() {
        let mut store = AccuracyStore::default();
        for d in 10..=13 {
            store.record_forecast(date(d), 10.0);
        }
        // Today is the 13th: the 10th is three days old, the 11th and 12th are pending
        assert_eq!(
            store.pending_actual(date(13)),
            vec![date(11), date(12), date(13)]
        );
        assert!(store.drop_stale_pending(date(13)));
        assert_eq!(store.days.len(), 3);
        assert!(!store.drop_stale_pending(date(13)));
    }

    #[test]
    fn set_actual_marks_a_day_measured() {
        let mut store = AccuracyStore::default();
        store.record_forecast(date(1), 10.0);
        assert!(store.set_actual(date(1), 9.0));
        assert!(store.pending_actual(date(1)).is_empty());
        assert!(!store.set_actual(date(9), 1.0));
    }

    /// Statistics rows of one hour each, in the live shape: epoch-ms start and end
    fn hour_rows(hours: &[(i64, i64, f64)]) -> Value {
        let rows: Vec<Value> = hours
            .iter()
            .map(|&(start, end, mean)| json!({"start": start, "end": end, "mean": mean}))
            .collect();
        Value::Array(rows)
    }

    #[test]
    fn actual_kwh_sums_hourly_mean_watts_over_entities() {
        // 1000 W and 2000 W in one entity, 500 W in another: each row is one hour of energy
        let result = json!({
            "sensor.solax_pv_power_total": hour_rows(&[
                (ms(2026, 10, 14, 10), ms(2026, 10, 14, 11), 1000.0),
                (ms(2026, 10, 14, 11), ms(2026, 10, 14, 12), 2000.0),
                (ms(2026, 10, 14, 20), ms(2026, 10, 14, 21), 0.0),
            ]),
            "sensor.other_pv_power_total": hour_rows(&[
                (ms(2026, 10, 14, 10), ms(2026, 10, 14, 11), 500.0),
                (ms(2026, 10, 14, 20), ms(2026, 10, 14, 21), 0.0),
            ]),
        });
        let entities = [
            "sensor.solax_pv_power_total".to_string(),
            "sensor.other_pv_power_total".to_string(),
        ];
        let finalise_at = Utc.with_ymd_and_hms(2026, 10, 14, 19, 0, 0).unwrap();
        let kwh = actual_kwh_from_statistics(&result, &entities, finalise_at).unwrap();
        assert!((kwh - 3.5).abs() < 1e-4, "{kwh}");
    }

    #[test]
    fn actual_needs_every_configured_entity() {
        // The second entity returned no rows: the day stays pending, no undercount is stored
        let result = json!({
            "sensor.solax_pv_power_total": hour_rows(&[
                (ms(2026, 10, 14, 10), ms(2026, 10, 14, 11), 1000.0),
                (ms(2026, 10, 14, 20), ms(2026, 10, 14, 21), 0.0),
            ]),
            "sensor.other_pv_power_total": [],
        });
        let entities = [
            "sensor.solax_pv_power_total".to_string(),
            "sensor.other_pv_power_total".to_string(),
        ];
        let finalise_at = Utc.with_ymd_and_hms(2026, 10, 14, 19, 0, 0).unwrap();
        assert!(actual_kwh_from_statistics(&result, &entities, finalise_at).is_none());
        // A configured entity missing from the answer is the same
        let one = [
            "sensor.solax_pv_power_total".to_string(),
            "sensor.absent".to_string(),
        ];
        assert!(actual_kwh_from_statistics(&result, &one, finalise_at).is_none());
    }

    #[test]
    fn actual_is_final_only_when_rows_reach_twenty_one() {
        // 15 Oct in Prague: 21:00 local is 19:00 UTC
        let entities = ["sensor.solax_pv_power_total".to_string()];
        let finalise_at = local_at(date(15), FINALISE_HOUR, chrono_tz::Europe::Prague).unwrap();
        // The last row ends at 21:00 local: final
        let final_day = json!({
            "sensor.solax_pv_power_total": hour_rows(&[
                (ms(2026, 10, 15, 17), ms(2026, 10, 15, 18), 1000.0),
                (ms(2026, 10, 15, 18), ms(2026, 10, 15, 19), 0.0),
            ]),
        });
        assert!(actual_kwh_from_statistics(&final_day, &entities, finalise_at).is_some());
        // The last row ends at 20:00 local: not final yet
        let early = json!({
            "sensor.solax_pv_power_total": hour_rows(&[
                (ms(2026, 10, 15, 16), ms(2026, 10, 15, 17), 1000.0),
                (ms(2026, 10, 15, 17), ms(2026, 10, 15, 18), 0.0),
            ]),
        });
        assert!(actual_kwh_from_statistics(&early, &entities, finalise_at).is_none());
    }

    #[test]
    fn local_day_bounds_follow_the_timezone() {
        let (start, end) = local_day_utc_bounds(date(15), chrono_tz::Europe::Prague).unwrap();
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 10, 14, 22, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 10, 15, 22, 0, 0).unwrap());
    }

    #[test]
    fn store_round_trips_through_its_file() {
        let path =
            std::env::temp_dir().join(format!("fluxion-accuracy-test-{}.json", std::process::id()));
        let mut store = AccuracyStore::default();
        store.record_forecast(date(1), 10.0);
        store.set_actual(date(1), 9.5);
        store.last_notified = Some(date(1));
        store.save(&path).unwrap();
        assert_eq!(AccuracyStore::load(&path), store);
        let _ = std::fs::remove_file(&path);
        assert_eq!(AccuracyStore::load(&path), AccuracyStore::default());
    }
}
