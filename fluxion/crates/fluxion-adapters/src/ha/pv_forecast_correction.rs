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
use tracing::{debug, info, warn};

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
    stored.is_some_and(|s| s.computed_on <= today && (today - s.computed_on).num_days() <= 1)
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

/// The correction worker
pub struct CorrectionWorker<R: HistoryReader> {
    pub reader: R,
    pub site: CorrectionSite,
    pub store: SharedForecastStore,
    pub path: PathBuf,
    /// The provider's total-today sensors (bootstrap source), read at each bootstrap
    pub forecast_entities: SharedSensorList,
    pub sender: crossbeam_channel::Sender<CorrectionResult>,
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
            let _ = self.sender.send(result);
        }
        loop {
            let wait = if self.recompute(Utc::now()).await {
                next_run_delay(Utc::now(), tz)
            } else {
                RETRY
            };
            tokio::time::sleep(wait).await;
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
                self.with_store(|s| {
                    s.last = Some(StoredCorrection {
                        computed_on: today,
                        result,
                    });
                });
                let _ = self.sender.send(result);
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
        };
        assert!(!worker.recompute(Utc::now()).await);
        assert!(rx.try_recv().is_err());
    }
}
