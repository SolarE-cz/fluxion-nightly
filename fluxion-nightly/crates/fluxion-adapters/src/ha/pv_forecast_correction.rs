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

//! PV forecast correction worker (brief H34): a tokio task off the scheduling loop that
//! recomputes the factor on start and once a day after local midnight from the recorder history
//! (GET only, 60 s timeout per request, the backtest's reader), bootstraps missing forecast days
//! from the provider sensors' history, and sends the factor to the ECS.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use fluxion_core::backtest::{HistoryReader, local_midnight_utc};
use fluxion_core::pv_correction::{
    CorrectionResult, CorrectionSite, ForecastStore, MIN_VALID_DAYS, SNAPSHOT_UNTIL_HOUR,
    StoredCorrection, compute_correction, day_forecast_from_history,
};
use fluxion_core::strategy::winter_reserve::{
    CAUTIOUS_PV_MIN_DAYS, CautiousPvThresholds, cautious_pv_signal,
};
use fluxion_types::CautiousPvSignal;
use tracing::{debug, info, warn};

use crate::ha::solar_forecast_accuracy::SharedCautiousPv;

/// Delay after local midnight before the daily recompute (the last hour's history settles)
const AFTER_MIDNIGHT: chrono::Duration = chrono::Duration::minutes(10);
/// Retry delay after a failed recompute
const RETRY: Duration = Duration::from_secs(30 * 60);

/// Shared snapshot store: the forecast fetcher adds snapshots, the worker reads them
pub type SharedForecastStore = Arc<Mutex<ForecastStore>>;
/// The provider's discovered total-today sensors, kept current by the fetcher (discovery may
/// finish or change after the worker started)
pub type SharedSensorList = Arc<Mutex<Vec<String>>>;

/// Local date of `now`
fn local_date(now: DateTime<Utc>, tz: Tz) -> NaiveDate {
    now.with_timezone(&tz).date_naive()
}

/// Time until the next daily recompute: the next local midnight plus `AFTER_MIDNIGHT`
#[must_use]
pub fn next_run_delay(now: DateTime<Utc>, tz: Tz) -> Duration {
    let today = local_date(now, tz);
    let mut next = local_midnight_utc(today, tz) + AFTER_MIDNIGHT;
    if next <= now {
        let tomorrow = today.succ_opt().unwrap_or(today);
        next = local_midnight_utc(tomorrow, tz) + AFTER_MIDNIGHT;
    }
    (next - now).to_std().unwrap_or(RETRY)
}

/// The stored factor applies at start when it was computed today or yesterday
#[must_use]
pub fn stored_factor_usable(stored: Option<&StoredCorrection>, today: NaiveDate) -> bool {
    stored.is_some_and(|s| s.usable_on(today))
}

/// The stored correction to apply at start: only a recent one, and its factor validated (a
/// damaged or edited file must not scale the forecast beyond the bounds)
#[must_use]
pub fn stored_correction_to_send(
    stored: Option<StoredCorrection>,
    today: NaiveDate,
) -> Option<CorrectionResult> {
    stored
        .filter(|s| stored_factor_usable(Some(s), today))
        .map(|s| s.result.sanitized())
}

/// The INFO line of a recompute
#[must_use]
pub fn recompute_log_line(result: &CorrectionResult) -> String {
    let basis = if result.days_used >= MIN_VALID_DAYS {
        format!("{} valid days", result.days_used)
    } else {
        format!(
            "only {} valid days (need {MIN_VALID_DAYS}), no correction",
            result.days_used
        )
    };
    format!(
        "☀️ PV forecast correction: factor {:.3} from {basis}; measured {:.1} kWh / forecast {:.1} kWh",
        result.factor, result.measured_kwh, result.forecast_kwh
    )
}

/// Record a recompute in the store: the result, and the cautious-PV state after its forecast
/// error (cleared when the error is unknown or rests on fewer than `CAUTIOUS_PV_MIN_DAYS` valid
/// days, so the fallback months decide). `thresholds` come from `strategies.winter_reserve`;
/// `None` leaves the state alone. Returns the signal to send.
pub fn record_recompute(
    store: &mut ForecastStore,
    today: NaiveDate,
    result: CorrectionResult,
    thresholds: Option<CautiousPvThresholds>,
) -> Option<CautiousPvSignal> {
    store.last = Some(StoredCorrection {
        computed_on: today,
        result,
    });
    // The worker sends this result; the planner applies it sanitized for the rest of the day
    store.record_applied_factor(today, result.sanitized().factor);
    let thresholds = thresholds?;
    let signal = cautious_pv_signal(
        result.forecast_error_pct,
        result.days_used,
        store.cautious_pv,
        thresholds,
    );
    store.cautious_pv = signal.is_some_and(|s| s.cautious);
    signal
}

/// The cautious-PV signal to send before the first recompute: the stored state with the stored
/// error, while the stored correction is recent; else none (fallback months)
#[must_use]
pub fn stored_cautious_pv_to_send(
    store: &ForecastStore,
    today: NaiveDate,
) -> Option<CautiousPvSignal> {
    store
        .last
        .filter(|s| stored_factor_usable(Some(s), today))
        .filter(|s| s.result.days_used >= CAUTIOUS_PV_MIN_DAYS)
        .and_then(|s| s.result.forecast_error_pct)
        .map(|mae_pct| CautiousPvSignal {
            mae_pct,
            cautious: store.cautious_pv,
        })
}

/// Where the cautious-PV signal goes
pub struct CautiousPvOutput {
    /// `strategies.winter_reserve.cautious_pv_mae_pct` / `cautious_pv_off_pct`, read at startup
    pub thresholds: CautiousPvThresholds,
    pub sender: crossbeam_channel::Sender<Option<CautiousPvSignal>>,
    /// Shown on the bias sensor
    pub view: SharedCautiousPv,
}

impl CautiousPvOutput {
    fn send(&self, signal: Option<CautiousPvSignal>) {
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = signal;
        if let Err(e) = self.sender.try_send(signal) {
            debug!("Cautious PV signal not sent: {}", e);
        }
    }
}

/// The PV forecast correction worker (brief H34): after local midnight it bootstraps missing
/// forecast days from HA history, computes the correction factor and the forecast error over the
/// window, saves both (and the cautious-PV state) in the snapshot store, and sends the factor to
/// the planner and the cautious-PV signal to the planner and the bias sensor. At start it sends
/// the stored ones while they are recent.
pub struct CorrectionWorker<R: HistoryReader> {
    /// Recorder history (GET only)
    pub reader: R,
    pub site: CorrectionSite,
    /// Snapshot store shared with the fetcher, which records the daily snapshots
    pub store: SharedForecastStore,
    /// Where the store is saved
    pub path: PathBuf,
    /// The provider's total-today sensors (bootstrap source), read at each bootstrap
    pub forecast_entities: SharedSensorList,
    pub sender: crossbeam_channel::Sender<CorrectionResult>,
    /// Cautious PV for Winter Reserve from the forecast error; `None` = not wired
    pub cautious_pv: Option<CautiousPvOutput>,
}

impl<R: HistoryReader> CorrectionWorker<R> {
    fn store_snapshot(&self) -> ForecastStore {
        self.store
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut ForecastStore) -> T) -> T {
        let mut guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let out = f(&mut guard);
        if let Err(e) = guard.save(&self.path) {
            warn!("⚠️ Failed to save PV forecast snapshots: {}", e);
        }
        out
    }

    /// Run forever: the stored factor first (when recent), then a recompute now and once a day
    pub async fn run(self) {
        let tz = self.site.timezone;
        let today = local_date(Utc::now(), tz);
        if let Some(result) = stored_correction_to_send(self.store_snapshot().last, today) {
            debug!(
                "PV forecast correction: stored factor {:.3} until the recompute",
                result.factor
            );
            self.with_store(|s| s.record_applied_factor(today, result.factor));
            let _ = self.sender.send(result);
        }
        self.send_stored_cautious_pv(today);
        loop {
            let wait = if self.recompute(Utc::now()).await {
                next_run_delay(Utc::now(), tz)
            } else {
                RETRY
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Send the stored cautious-PV signal when the stored correction is recent; a stale or
    /// missing one sends nothing (the planner starts on the fallback months)
    pub fn send_stored_cautious_pv(&self, today: NaiveDate) {
        if let Some(out) = &self.cautious_pv
            && let Some(signal) = stored_cautious_pv_to_send(&self.store_snapshot(), today)
        {
            out.send(Some(signal));
        }
    }

    /// Bootstrap missing days, compute and send the factor. False when the history failed.
    pub async fn recompute(&self, now: DateTime<Utc>) -> bool {
        let today = local_date(now, self.site.timezone);
        self.bootstrap(today).await;
        let store = self.store_snapshot();
        match compute_correction(&self.reader, &self.site, &store, today).await {
            Ok(result) => {
                info!("{}", recompute_log_line(&result));
                let thresholds = self.cautious_pv.as_ref().map(|c| c.thresholds);
                let signal = self.with_store(|s| record_recompute(s, today, result, thresholds));
                let _ = self.sender.send(result);
                if let Some(out) = &self.cautious_pv {
                    match signal {
                        Some(s) => info!(
                            "☀️ Cautious PV {}: over-forecast error {:.1} % (on at {:.1} %, off below {:.1} %)",
                            if s.cautious { "on" } else { "off" },
                            s.mae_pct,
                            out.thresholds.on_pct,
                            out.thresholds.off_pct
                        ),
                        None => info!(
                            "☀️ Cautious PV: forecast error unknown (fewer than {CAUTIOUS_PV_MIN_DAYS} valid days), fallback months decide"
                        ),
                    }
                    out.send(signal);
                }
                true
            }
            Err(e) => {
                warn!(
                    "⚠️ PV forecast correction not recomputed ({}), keeping the current factor; retry in 30 min",
                    e
                );
                false
            }
        }
    }

    /// Rebuild the window's days that have no snapshot from the provider sensors' history
    async fn bootstrap(&self, today: NaiveDate) {
        let forecast_entities = self
            .forecast_entities
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|e| e.into_inner().clone());
        if forecast_entities.is_empty() {
            debug!("PV forecast bootstrap: no forecast sensors discovered yet");
            return;
        }
        let tz = self.site.timezone;
        let missing = self.store_snapshot().missing_days(today);
        for date in missing {
            let start = local_midnight_utc(date, tz);
            let end = start + chrono::Duration::hours(i64::from(SNAPSHOT_UNTIL_HOUR));
            let mut per_sensor = Vec::with_capacity(forecast_entities.len());
            for entity in &forecast_entities {
                match self.reader.history(entity, start, end, true).await {
                    Ok(states) => per_sensor.push(states),
                    Err(e) => {
                        warn!(
                            "⚠️ PV forecast bootstrap for {}: history failed ({})",
                            date, e
                        );
                        return;
                    }
                }
            }
            match day_forecast_from_history(&per_sensor, date, tz) {
                Some(day) => {
                    let kwh: f32 = day.hours.iter().map(|h| h.kwh).sum();
                    info!(
                        "☀️ PV forecast for {} rebuilt from HA history: {:.1} kWh over {} hours",
                        date,
                        kwh,
                        day.hours.len()
                    );
                    self.with_store(|s| s.insert(day));
                }
                None => debug!(
                    "PV forecast for {} not in HA history (no per-period attribute recorded)",
                    date
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use fluxion_core::backtest::{HistoryError, HistoryState};

    const TZ: Tz = chrono_tz::Europe::Prague;

    #[test]
    fn next_run_is_ten_minutes_after_the_next_local_midnight() {
        // 23:00 local: 70 minutes
        let now = TZ
            .with_ymd_and_hms(2026, 10, 18, 23, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next_run_delay(now, TZ), Duration::from_secs(70 * 60));
        // 00:05 local: today's run is still ahead
        let now = TZ
            .with_ymd_and_hms(2026, 10, 19, 0, 5, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next_run_delay(now, TZ), Duration::from_secs(5 * 60));
        // On the 25-hour DST day: 00:20 CEST to 00:10 CET next day is 24 h 50 min
        let now = TZ
            .with_ymd_and_hms(2026, 10, 25, 0, 20, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            next_run_delay(now, TZ),
            Duration::from_secs(24 * 3600 + 50 * 60)
        );
    }

    #[test]
    fn stored_factor_applies_for_a_day() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        let stored = StoredCorrection {
            computed_on: d(18),
            result: CorrectionResult::neutral(),
        };
        assert!(stored_factor_usable(Some(&stored), d(18)));
        assert!(stored_factor_usable(Some(&stored), d(19)));
        assert!(!stored_factor_usable(Some(&stored), d(20)));
        assert!(!stored_factor_usable(Some(&stored), d(17)));
        assert!(!stored_factor_usable(None, d(18)));
    }

    #[test]
    fn stored_factor_is_validated_before_it_is_sent() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        let stored = |factor| StoredCorrection {
            computed_on: d(18),
            result: CorrectionResult {
                factor,
                days_used: 7,
                measured_kwh: 100.0,
                forecast_kwh: 100.0,
                forecast_error_pct: None,
            },
        };
        let sent = |factor| {
            stored_correction_to_send(Some(stored(factor)), d(19))
                .unwrap()
                .factor
        };
        assert!((sent(1.17) - 1.17).abs() < 1e-6);
        assert_eq!(sent(f32::NAN), 1.0);
        assert_eq!(sent(f32::INFINITY), 1.0);
        assert_eq!(sent(7.0), 1.0);
        assert_eq!(sent(0.1), 1.0);
        assert_eq!(sent(-1.0), 1.0);
        // Too old: nothing is sent
        assert!(stored_correction_to_send(Some(stored(1.17)), d(21)).is_none());
    }

    #[test]
    fn log_line_names_factor_days_and_energies() {
        let line = recompute_log_line(&CorrectionResult {
            factor: 1.17,
            days_used: 7,
            measured_kwh: 812.34,
            forecast_kwh: 694.3,
            forecast_error_pct: None,
        });
        assert!(line.contains("factor 1.170"), "{line}");
        assert!(line.contains("7 valid days"), "{line}");
        assert!(
            line.contains("measured 812.3 kWh / forecast 694.3 kWh"),
            "{line}"
        );
        let line = recompute_log_line(&CorrectionResult {
            days_used: 2,
            ..CorrectionResult::neutral()
        });
        assert!(line.contains("only 2 valid days"), "{line}");
    }

    struct FailingHistory;

    #[async_trait::async_trait]
    impl HistoryReader for FailingHistory {
        async fn history(
            &self,
            _entity_id: &str,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _with_attributes: bool,
        ) -> Result<Vec<HistoryState>, HistoryError> {
            Err(HistoryError::Timeout)
        }
    }

    #[tokio::test]
    async fn failed_history_keeps_the_factor_and_sends_nothing() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let dir = tempfile::tempdir().unwrap();
        let worker = CorrectionWorker {
            reader: FailingHistory,
            site: CorrectionSite {
                timezone: TZ,
                pv: vec!["sensor.inv_pv_power_total".to_owned()],
                soc: "sensor.inv_battery_capacity".to_owned(),
                grid_export: "sensor.inv_grid_export".to_owned(),
                export_limit: None,
                limits: fluxion_core::pv_correction::CurtailLimits {
                    max_soc_pct: 100.0,
                    export_cap_w: 10_000,
                    export_off_w: 100,
                    export_curtailment_enabled: false,
                },
            },
            store: Arc::new(Mutex::new(ForecastStore::default())),
            path: dir.path().join("snapshots.json"),
            forecast_entities: Arc::new(Mutex::new(vec![
                "sensor.energy_production_today".to_owned(),
            ])),
            sender: tx,
            cautious_pv: None,
        };
        assert!(!worker.recompute(Utc::now()).await);
        assert!(rx.try_recv().is_err());
    }

    // ---- Cautious PV from the forecast error (review #652 C-1, C-2, C-5, D-1, D-2, D-4) ----

    use chrono::Timelike;
    use fluxion_core::backtest::{day_block_starts, local_midnight_utc};
    use fluxion_core::pv_correction::{
        DayInput, HourInput, MeasuredStatus, correction_factor, day_hour_starts,
    };
    use fluxion_core::strategy::winter_reserve::cautious_pv_at;

    /// kWp-equivalent of the test site: days with a valid forecast under 5 kWh are dim
    const KWP: f32 = 10.0;

    /// The defaults: on at 25 %, off below 20 %
    const T: CautiousPvThresholds = CautiousPvThresholds {
        on_pct: 25.0,
        off_pct: 20.0,
    };

    fn d(i: usize) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 5, 1).unwrap() + chrono::Days::new(i as u64)
    }

    /// A day with one valid hour (forecast f, measured a) and optionally a curtailed hour; the
    /// planner's factor is unknown until `walk` sets it
    fn day_input(i: usize, f: f32, a: f32, curtailed_forecast: Option<f32>) -> DayInput {
        let start = d(i).and_hms_opt(10, 0, 0).unwrap().and_utc();
        let mut hours = vec![HourInput {
            start,
            forecast_kwh: Some(f),
            measured: MeasuredStatus::Valid(a),
        }];
        if let Some(cf) = curtailed_forecast {
            hours.push(HourInput {
                start: start + chrono::Duration::hours(1),
                forecast_kwh: Some(cf),
                measured: MeasuredStatus::Curtailed,
            });
        }
        DayInput {
            date: d(i),
            hours,
            planner_factor: None,
        }
    }

    /// Feed the days one by one in the live order: day j's snapshot is taken at 00:05 with the
    /// factor the planner holds, then the recompute at 00:10 over the 7 days before replaces it
    /// with the new factor the planner applies for the rest of day j. Returns (error %, cautious,
    /// signal known) after each complete day.
    fn walk(days: &[DayInput], t: CautiousPvThresholds) -> Vec<(Option<f32>, bool, bool)> {
        let mut days = days.to_vec();
        let mut store = ForecastStore::default();
        let mut states = Vec::new();
        for j in 0..=days.len() {
            let window = &days[j.saturating_sub(7)..j];
            let result = correction_factor(window, KWP);
            let signal = record_recompute(&mut store, d(j), result, Some(t));
            if j > 0 {
                states.push((
                    result.forecast_error_pct,
                    store.cautious_pv,
                    signal.is_some(),
                ));
            }
            if let Some(day) = days.get_mut(j) {
                day.planner_factor = day.planner_factor.or(Some(store.factor_in_force()));
            }
        }
        states
    }

    fn pairs(list: &[(f32, f32)]) -> Vec<DayInput> {
        list.iter()
            .enumerate()
            .map(|(i, &(f, a))| day_input(i, f, a, None))
            .collect()
    }

    fn on_days(states: &[(Option<f32>, bool, bool)]) -> usize {
        states.iter().filter(|s| s.1).count()
    }

    #[test]
    fn probe_a_steady_bias_is_corrected_away() {
        // Forecast 30 % high every day, and the planner already scaled it by H34's 0.77: the
        // forecast it had was right, the error is 0
        let mut days = pairs(&[(130.0, 100.0); 10]);
        for day in &mut days {
            day.planner_factor = Some(100.0 / 130.0);
        }
        let states = walk(&days, T);
        for (error, on, _) in &states[2..] {
            assert!(error.unwrap() < 0.1, "{error:?}");
            assert!(!on);
        }
        // Live, from no correction: the three days planned at face value (23 % high) leave the
        // window before cautious PV could turn on
        assert_eq!(on_days(&walk(&pairs(&[(130.0, 100.0); 12]), T)), 0);
    }

    #[test]
    fn under_forecast_does_not_count() {
        // Actual twice the forecast: k clamps at 1.5 and the forecast is still too low. More sun
        // than forecast is not what cautious PV guards against (D-4): 0 %, off.
        let states = walk(&pairs(&[(30.0, 60.0); 10]), T);
        for (error, on, _) in &states[2..] {
            assert!(error.unwrap() < 0.1, "{error:?}");
            assert!(!on);
        }
    }

    #[test]
    fn probe_b_three_rainy_may_days_stay_on_only_while_in_the_window() {
        // Good days at -5 %, three rainy days (forecast 60, actual 15), then good days. Planned
        // with the factor of the days before, the rain scores up to 29.3 %: on for the 5 days
        // the error stays at or above 20 %, all while a rainy day is in the window
        let mut list = vec![(60.0, 57.0); 5];
        list.extend([(60.0, 15.0); 3]);
        list.extend([(60.0, 57.0); 20]);
        let states = walk(&pairs(&list), T);
        for (i, (error, on, _)) in states.iter().enumerate() {
            let today = i + 1;
            if !(6..=14).contains(&today) {
                assert!(!on, "today {today}: error {error:?}");
            }
        }
        assert_eq!(on_days(&states), 5, "{states:?}");
        let peak = states.iter().filter_map(|s| s.0).fold(0.0_f32, f32::max);
        assert!((peak - 29.3).abs() < 0.1, "{peak}");
    }

    #[test]
    fn a_single_snow_day_is_bounded_and_short() {
        // Ten good days, one snow day (60 / 1), good days after: cautious PV, if it turns on at
        // all, stays on no longer than the snow day is in the window
        let mut list = vec![(60.0, 57.0); 10];
        list.push((60.0, 1.0));
        list.extend([(60.0, 57.0); 15]);
        let states = walk(&pairs(&list), T);
        for (error, _, _) in &states {
            assert!(
                error.is_none_or(|e| (0.0..=100.0).contains(&e)),
                "{error:?}"
            );
        }
        for (i, (_, on, _)) in states.iter().enumerate() {
            let today = i + 1;
            if !(12..=18).contains(&today) {
                assert!(!on, "today {today}");
            }
        }
        assert!(on_days(&states) <= 7, "{states:?}");
    }

    #[test]
    fn a_dark_week_is_100_percent_and_threshold_100_is_off() {
        let dark = walk(
            &pairs(&[(30.0, 0.0); 7]),
            CautiousPvThresholds { on_pct: 100.0, ..T },
        );
        let (error, on, _) = dark[6];
        assert!((error.unwrap() - 100.0).abs() < 1e-3, "{error:?}");
        assert!(!on);
        // ...while the default threshold turns it on
        assert!(walk(&pairs(&[(30.0, 0.0); 7]), T)[6].1);
    }

    #[test]
    fn fewer_than_five_valid_days_leave_it_to_the_months() {
        // Four very bad days: the error is known (>= 3 days) but rests on too few days
        let states = walk(&pairs(&[(60.0, 5.0); 6]), T);
        assert!(states[3].0.unwrap() > 25.0);
        assert!(!states[3].2 && !states[3].1, "{:?}", states[3]);
        // The fifth day decides
        assert!(states[4].2 && states[4].1, "{:?}", states[4]);
    }

    #[test]
    fn probe_d_curtailed_hours_do_not_count() {
        // Export-capped summer: sunny days lose PV in a curtailed noon hour, cloudy days are not
        // curtailed; the valid hours match the forecast
        let days: Vec<DayInput> = (0..8)
            .map(|i| {
                if i % 4 == 3 {
                    day_input(i, 100.0, 100.0, None)
                } else {
                    day_input(i, 200.0, 200.0, Some(50.0))
                }
            })
            .collect();
        for (error, on, _) in &walk(&days, T)[2..] {
            assert!(error.unwrap() < 0.1, "{error:?}");
            assert!(!on);
        }
    }

    #[test]
    fn dim_days_do_not_count() {
        // Three normal days plus dim days (forecast 3 < 5 kWh) that produced nothing
        let mut list = vec![(60.0, 57.0); 3];
        list.extend([(3.0, 0.0); 4]);
        let result = correction_factor(&pairs(&list), KWP);
        assert_eq!(result.days_used, 3);
        assert!(result.forecast_error_pct.unwrap() < 0.1, "{result:?}");
    }

    /// Jakub's site, 30 Oct - 2 Nov 2026 (forecast / actual), planned with H34's 0.84, plus a
    /// fifth similar day
    fn jakub_days() -> Vec<DayInput> {
        let mut days = pairs(&[
            (63.0, 59.0),
            (45.0, 75.0),
            (117.0, 68.0),
            (132.0, 30.0),
            (110.0, 50.0),
        ]);
        for day in &mut days {
            day.planner_factor = Some(0.84);
        }
        days
    }

    #[test]
    fn jakub_days_score_the_forecast_the_planner_had() {
        let days = jakub_days();
        // The four days: 32.4 % with the factor the planner used (the window's own fit, 0.65,
        // would give 21.6 %), but four valid days do not decide yet
        let four = correction_factor(&days[..4], KWP);
        assert!((four.factor - 0.65).abs() < 0.01, "{four:?}");
        assert!(
            (four.forecast_error_pct.unwrap() - 32.4).abs() < 0.1,
            "{four:?}"
        );
        let mut store = ForecastStore::default();
        assert_eq!(record_recompute(&mut store, d(5), four, Some(T)), None);
        // November is a fallback month meanwhile
        let nov_2 = NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        assert!(cautious_pv_at(nov_2, None, &[10, 11, 12, 1, 2, 3], TZ));
        // The fifth day: the error decides, and it is on
        let five = correction_factor(&days, KWP);
        let signal = record_recompute(&mut store, d(6), five, Some(T)).unwrap();
        assert!((signal.mae_pct - 35.3).abs() < 0.1, "{signal:?}");
        assert!(signal.cautious);
    }

    #[test]
    fn unknown_error_clears_the_state_and_restart_keeps_a_known_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshots.json");
        let mut store = ForecastStore::default();
        let five = correction_factor(&pairs(&[(60.0, 15.0); 5]), KWP);
        let signal = record_recompute(&mut store, d(10), five, Some(T)).unwrap();
        assert!(signal.cautious);
        store.save(&path).unwrap();
        // After a restart the stored state and error are sent before the first recompute
        let loaded = ForecastStore::load(&path);
        assert!(loaded.cautious_pv);
        let sent = stored_cautious_pv_to_send(&loaded, d(11)).unwrap();
        assert!(sent.cautious);
        assert!((sent.mae_pct - signal.mae_pct).abs() < 1e-6);
        // Hysteresis across the restart: 22 % keeps it on
        let mut store = loaded;
        let mid = CorrectionResult {
            forecast_error_pct: Some(22.0),
            ..five
        };
        assert!(
            record_recompute(&mut store, d(11), mid, Some(T))
                .unwrap()
                .cautious
        );
        // Fewer than 5 valid days: no signal, the stored state is cleared (fallback months)
        let four = correction_factor(&pairs(&[(60.0, 15.0); 4]), KWP);
        assert!(four.forecast_error_pct.is_some());
        assert_eq!(record_recompute(&mut store, d(12), four, Some(T)), None);
        assert!(!store.cautious_pv);
        assert_eq!(stored_cautious_pv_to_send(&store, d(12)), None);
    }

    fn snapshot_blocks(date: NaiveDate) -> Vec<(DateTime<Utc>, f32)> {
        day_block_starts(date, TZ)
            .into_iter()
            .map(|t| (t, shape(t.with_timezone(&TZ).hour()) / 4.0))
            .collect()
    }

    #[test]
    fn the_day_carries_the_factor_of_its_recompute_not_the_snapshot_time_one() {
        // 18 Oct: the planner holds 0.84 from yesterday's recompute
        let mut store = ForecastStore::default();
        let yesterday = CorrectionResult {
            factor: 0.84,
            days_used: 7,
            ..CorrectionResult::neutral()
        };
        record_recompute(&mut store, oct(17), yesterday, Some(T));
        // 00:05: the first fetch takes the snapshot, still with 0.84
        let midnight = local_midnight_utc(oct(18), TZ);
        assert!(store.record_snapshot(
            midnight + chrono::Duration::minutes(5),
            TZ,
            &[snapshot_blocks(oct(18))],
            1
        ));
        assert_eq!(store.day(oct(18)).unwrap().planner_factor, Some(0.84));
        // 00:10: the recompute; the planner applies its factor (sanitized) for the rest of the
        // day, and the day is scored with that one
        let today = CorrectionResult {
            factor: 0.7,
            ..yesterday
        };
        record_recompute(&mut store, oct(18), today, Some(T));
        let applied = stored_correction_to_send(store.last, oct(18))
            .unwrap()
            .factor;
        assert_eq!(store.day(oct(18)).unwrap().planner_factor, Some(applied));
        assert!((applied - 0.7).abs() < 1e-6);
    }

    #[test]
    fn a_stale_factor_the_planner_still_holds_is_the_one_stored() {
        // The recompute fails for days: the planner keeps the last factor it got (0.65, from
        // 14 Oct), with no expiry, and the 18 Oct snapshot must carry that, not 1.0
        let mut store = ForecastStore::default();
        let old = CorrectionResult {
            factor: 0.65,
            days_used: 7,
            ..CorrectionResult::neutral()
        };
        record_recompute(&mut store, oct(14), old, Some(T));
        let midnight = local_midnight_utc(oct(18), TZ);
        assert!(store.record_snapshot(
            midnight + chrono::Duration::minutes(5),
            TZ,
            &[snapshot_blocks(oct(18))],
            1
        ));
        assert_eq!(store.day(oct(18)).unwrap().planner_factor, Some(0.65));
        // After a restart with that stale store, nothing is sent: the planner runs on 1.0
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshots.json");
        store.save(&path).unwrap();
        let mut restarted = ForecastStore::load(&path);
        assert_eq!(stored_correction_to_send(restarted.last, oct(19)), None);
        assert!(restarted.record_snapshot(
            local_midnight_utc(oct(19), TZ) + chrono::Duration::minutes(5),
            TZ,
            &[snapshot_blocks(oct(19))],
            1
        ));
        assert_eq!(restarted.day(oct(19)).unwrap().planner_factor, Some(1.0));
    }

    // ---- The worker sends the signal (D-1) ----

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

    fn oct(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    /// Forecast kWh per local hour 07..17, 31 kWh a day, peak 5 kWh
    fn shape(local_hour: u32) -> f32 {
        match local_hour {
            7..=17 => 5.0 - (local_hour as f32 - 12.0).abs() * 0.8,
            _ => 0.0,
        }
    }

    type CautiousRx = crossbeam_channel::Receiver<Option<CautiousPvSignal>>;

    fn worker_with<R: HistoryReader>(
        reader: R,
        store: ForecastStore,
        dir: &tempfile::TempDir,
    ) -> (CorrectionWorker<R>, CautiousRx, SharedCautiousPv) {
        let (tx, _factor_rx) = crossbeam_channel::bounded(16);
        let (ctx, crx) = crossbeam_channel::bounded(16);
        let view: SharedCautiousPv = Arc::new(Mutex::new(None));
        let worker = CorrectionWorker {
            reader,
            site: CorrectionSite {
                timezone: TZ,
                pv: vec!["sensor.inv_pv_power_total".to_owned()],
                soc: "sensor.inv_battery_capacity".to_owned(),
                grid_export: "sensor.inv_grid_export".to_owned(),
                export_limit: None,
                limits: fluxion_core::pv_correction::CurtailLimits {
                    max_soc_pct: 100.0,
                    export_cap_w: 10_000,
                    export_off_w: 100,
                    export_curtailment_enabled: false,
                },
            },
            store: Arc::new(Mutex::new(store)),
            path: dir.path().join("snapshots.json"),
            forecast_entities: Arc::new(Mutex::new(Vec::new())),
            sender: tx,
            cautious_pv: Some(CautiousPvOutput {
                thresholds: T,
                sender: ctx,
                view: view.clone(),
            }),
        };
        (worker, crx, view)
    }

    #[tokio::test]
    async fn recompute_sends_the_signal_to_the_channel_and_the_view() {
        // 11..17 Oct: four days at 10 % of the forecast, three at 100 %
        let mut store = ForecastStore::default();
        let (mut pv, mut soc, mut export) = (Vec::new(), Vec::new(), Vec::new());
        for day in 11..=17 {
            let blocks: Vec<(DateTime<Utc>, f32)> = day_block_starts(oct(day), TZ)
                .into_iter()
                .map(|t| (t, shape(t.with_timezone(&TZ).hour()) / 4.0))
                .collect();
            let midnight = local_midnight_utc(oct(day), TZ);
            assert!(store.record_snapshot(
                midnight + chrono::Duration::minutes(5),
                TZ,
                std::slice::from_ref(&blocks),
                1
            ));
            let ratio = if day <= 14 { 0.1 } else { 1.0 };
            for start in day_hour_starts(oct(day), TZ) {
                let w = shape(start.with_timezone(&TZ).hour()) * ratio * 1000.0;
                pv.push(HistoryState::from_raw(start, &w.to_string(), None));
                soc.push(HistoryState::from_raw(start, "60", None));
                export.push(HistoryState::from_raw(start, "0", None));
            }
        }
        let reader = FakeHistory(
            [
                ("sensor.inv_pv_power_total".to_owned(), pv),
                ("sensor.inv_battery_capacity".to_owned(), soc),
                ("sensor.inv_grid_export".to_owned(), export),
            ]
            .into_iter()
            .collect(),
        );
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx, view) = worker_with(reader, store, &dir);
        let now = local_midnight_utc(oct(18), TZ) + chrono::Duration::minutes(20);
        assert!(worker.recompute(now).await);
        let sent = rx.try_recv().unwrap().expect("a signal");
        // The snapshots were planned with no correction (1.0):
        // 4 × (31 − 3.1) / (7 × 31) = 51.4 %
        assert!((sent.mae_pct - 51.4).abs() < 0.1, "{sent:?}");
        assert!(sent.cautious);
        assert_eq!(*view.lock().unwrap(), Some(sent));
        // And the state is persisted for the next start
        assert!(ForecastStore::load(&dir.path().join("snapshots.json")).cautious_pv);
    }

    fn stored(computed_on: NaiveDate) -> ForecastStore {
        ForecastStore {
            last: Some(StoredCorrection {
                computed_on,
                result: CorrectionResult {
                    factor: 0.8,
                    days_used: 7,
                    measured_kwh: 80.0,
                    forecast_kwh: 100.0,
                    forecast_error_pct: Some(33.0),
                },
            }),
            cautious_pv: true,
            ..ForecastStore::default()
        }
    }

    #[tokio::test]
    async fn run_sends_the_stored_signal_at_startup() {
        let today = Utc::now().with_timezone(&TZ).date_naive();
        let dir = tempfile::tempdir().unwrap();
        // The history fails, so only the startup send reaches the channel
        let (worker, rx, view) = worker_with(FailingHistory, stored(today), &dir);
        let store = worker.store.clone();
        let task = tokio::spawn(worker.run());
        let sent = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(signal) = rx.try_recv() {
                    return signal;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup signal");
        task.abort();
        let expected = CautiousPvSignal {
            mae_pct: 33.0,
            cautious: true,
        };
        assert_eq!(sent, Some(expected));
        assert_eq!(*view.lock().unwrap(), Some(expected));
        // The stored factor the planner now holds is what today's snapshot will carry
        assert_eq!(store.lock().unwrap().factor_in_force(), 0.8);
    }

    #[test]
    fn a_stale_store_sends_nothing_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx, view) = worker_with(FailingHistory, stored(oct(15)), &dir);
        worker.send_stored_cautious_pv(oct(18));
        assert!(rx.try_recv().is_err());
        assert_eq!(*view.lock().unwrap(), None);
        // The same store one day later is recent and is sent
        worker.send_stored_cautious_pv(oct(16));
        assert!(rx.try_recv().unwrap().unwrap().cautious);
    }
}
