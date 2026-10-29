// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Backtest page and API (brief H28): one past local day of this site, read from Home
//! Assistant history, with what actually happened, plain self-use and the Winter Reserve replay.
//!
//! The history is read on the web server's runtime (never on the scheduling loop) and only with
//! GET requests; the replay runs on a blocking thread. Days are cached in memory, so reloading
//! a day does not read HA again: complete days until evicted, days whose result can still change
//! (horizon cut at now, HDO lookup failed) for `PROVISIONAL_TTL`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use chrono::{DateTime, NaiveDate, Utc};
use fluxion_core::backtest::{
    AvailableDay, BacktestSite, DayReport, HistoryReader, LoadError, available_days, build_report,
    date_in_range, earliest_backtest_date, load_day,
};
use fluxion_core::scheduling::backup_planner_floor;
use fluxion_i18n::I18n;
use fluxion_types::UserControlState;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::{debug, error, info, warn};

/// How far back the day list looks; HA's recorder keeps 10 days by default.
const LOOKBACK_DAYS: i64 = 14;
/// The day list is read again after this long.
const DAYS_CACHE_TTL: Duration = Duration::from_secs(600);
/// Finished days kept in memory.
const MAX_CACHED_DAYS: usize = 32;
/// Lifetime of a cached day whose result can still change: its next day had not ended when it
/// was loaded (typically yesterday, the day the page opens on), or the HDO lookup failed.
const PROVISIONAL_TTL: chrono::Duration = chrono::Duration::minutes(15);
/// Longest wait for the one-at-a-time load permit before answering 503.
const LOAD_WAIT: Duration = Duration::from_secs(120);

/// A cached report: the day and the floor (bits of the f32 percentage) it was replayed with.
type ReportKey = (NaiveDate, Option<u32>);

/// A cached report and, for a provisional one, when it expires (on the state's clock).
type CachedReport = (Arc<DayReport>, Option<DateTime<Utc>>);

/// Wall clock of the handlers (replaceable in tests).
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The day list and when it was read.
type CachedDays = (Instant, Vec<AvailableDay>);

/// State of the backtest handlers.
#[derive(Clone)]
pub struct BacktestState {
    reader: Arc<dyn HistoryReader>,
    /// The site's live configuration, or why the backtest cannot run on it
    site: Result<Arc<BacktestSite>, String>,
    i18n: Arc<I18n>,
    /// Live dashboard state: its default mode sets the Back Up floor of the replay
    user_control: Option<Arc<parking_lot::RwLock<UserControlState>>>,
    /// Reports per day and floor used (the floor follows the current default mode)
    reports: Arc<Mutex<HashMap<ReportKey, CachedReport>>>,
    /// One day load at a time: HA history reads are heavy, and a second request for the same
    /// day waits and then takes the cached result
    loads: Arc<tokio::sync::Semaphore>,
    /// Longest wait for that permit
    load_wait: Duration,
    days: Arc<Mutex<Option<CachedDays>>>,
    clock: Clock,
}

impl std::fmt::Debug for BacktestState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BacktestState")
            .field("site", &self.site.as_ref().map(|s| s.timezone))
            .field("cached_days", &self.reports.lock().len())
            .finish_non_exhaustive()
    }
}

impl BacktestState {
    /// `site` is `Err` with a reason when the configuration lacks what the backtest reads.
    #[must_use]
    pub fn new(
        reader: Arc<dyn HistoryReader>,
        site: Result<BacktestSite, String>,
        i18n: Arc<I18n>,
    ) -> Self {
        Self {
            reader,
            site: site.map(Arc::new),
            i18n,
            user_control: None,
            reports: Arc::new(Mutex::new(HashMap::new())),
            loads: Arc::new(tokio::sync::Semaphore::new(1)),
            load_wait: LOAD_WAIT,
            days: Arc::new(Mutex::new(None)),
            clock: Arc::new(Utc::now),
        }
    }

    /// Read the live user control state (default mode, Back Up minimum) for every replay.
    #[must_use]
    pub fn with_user_control(
        mut self,
        user_control: Arc<parking_lot::RwLock<UserControlState>>,
    ) -> Self {
        self.user_control = Some(user_control);
        self
    }

    /// Floor the live planner uses now: `Some` in Back Up default mode.
    fn backup_floor(&self, site: &BacktestSite) -> Option<f32> {
        let state = self.user_control.as_ref().map(|uc| uc.read().clone());
        backup_planner_floor(
            state.as_ref(),
            site.control.min_battery_soc,
            site.control.max_battery_soc,
        )
    }

    /// A state whose routes answer with `reason` (no history source).
    #[must_use]
    pub fn unavailable(i18n: Arc<I18n>, reason: &str) -> Self {
        Self::new(Arc::new(NoHistory), Err(reason.to_owned()), i18n)
    }

    /// Replace the wall clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Replace the longest wait for the load permit (tests).
    #[must_use]
    pub fn with_load_wait(mut self, wait: Duration) -> Self {
        self.load_wait = wait;
        self
    }

    /// The cached report for `key`, unless it has expired at `now` (an expired one is dropped).
    fn cached(&self, key: &ReportKey, now: DateTime<Utc>) -> Option<Arc<DayReport>> {
        let mut reports = self.reports.lock();
        match reports.get(key) {
            Some((report, expires)) if expires.is_none_or(|at| now < at) => {
                Some(Arc::clone(report))
            }
            Some(_) => {
                reports.remove(key);
                None
            }
            None => None,
        }
    }

    /// Cache `report`; `expires` is `Some` for a provisional result. Keeps at most
    /// `MAX_CACHED_DAYS`: expired entries go first, then the oldest day.
    fn store(
        &self,
        key: ReportKey,
        report: Arc<DayReport>,
        expires: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) {
        let mut reports = self.reports.lock();
        reports.retain(|_, (_, at)| at.is_none_or(|at| now < at));
        if reports.len() >= MAX_CACHED_DAYS
            && !reports.contains_key(&key)
            && let Some(oldest) = reports.keys().min().copied()
        {
            reports.remove(&oldest);
        }
        reports.insert(key, (report, expires));
    }
}

/// History source of a state without Home Assistant: never read (the site is `Err`).
struct NoHistory;

#[async_trait::async_trait]
impl HistoryReader for NoHistory {
    async fn history(
        &self,
        _entity_id: &str,
        _start: DateTime<Utc>,
        _end: DateTime<Utc>,
        _with_attributes: bool,
    ) -> Result<Vec<fluxion_core::backtest::HistoryState>, fluxion_core::backtest::HistoryError>
    {
        Err(fluxion_core::backtest::HistoryError::Unreachable)
    }
}

/// Backtest page template
#[derive(Template)]
#[template(path = "backtest.html")]
pub struct BacktestTemplate {
    #[expect(dead_code)]
    pub i18n: Arc<I18n>,
    pub ingress_path: String,
    pub sides: Vec<String>,
}

/// JSON error body: a message for the owner, no URL, path or token.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

/// GET /backtest. The days and the report are fetched by the page, so rendering never waits
/// for Home Assistant.
pub async fn backtest_page_handler(
    State(state): State<BacktestState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let template = BacktestTemplate {
        i18n: state.i18n,
        ingress_path: extract_ingress_path(&headers),
        sides: vec!["left".to_owned(), "right".to_owned()],
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Backtest template render error: {e}");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the backtest page could not be rendered",
            )
        }
    }
}

/// Response of `GET /api/backtest/days`
#[derive(Serialize)]
pub struct AvailableDaysResponse {
    /// Past local days with history, newest first
    pub days: Vec<AvailableDay>,
    pub timezone: String,
}

/// GET /api/backtest/days
pub async fn available_days_handler(State(state): State<BacktestState>) -> Response {
    let site = match &state.site {
        Ok(site) => Arc::clone(site),
        Err(reason) => return json_error(StatusCode::SERVICE_UNAVAILABLE, reason.clone()),
    };
    if let Some((at, days)) = state.days.lock().as_ref()
        && at.elapsed() < DAYS_CACHE_TTL
    {
        return Json(AvailableDaysResponse {
            days: days.clone(),
            timezone: site.timezone.name().to_owned(),
        })
        .into_response();
    }
    match available_days(state.reader.as_ref(), &site, (state.clock)(), LOOKBACK_DAYS).await {
        Ok(days) => {
            *state.days.lock() = Some((Instant::now(), days.clone()));
            Json(AvailableDaysResponse {
                days,
                timezone: site.timezone.name().to_owned(),
            })
            .into_response()
        }
        Err(e) => {
            warn!("Backtest day list unavailable: {e}");
            json_error(StatusCode::BAD_GATEWAY, e.to_string())
        }
    }
}

/// GET /api/backtest/day/{date}: actual, self-use and Winter Reserve for one local day.
pub async fn day_handler(
    State(state): State<BacktestState>,
    Path(date_str): Path<String>,
) -> Response {
    let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") else {
        return json_error(StatusCode::BAD_REQUEST, "the date must be YYYY-MM-DD");
    };
    if !date_in_range(date, (state.clock)()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "the date must be between {} and tomorrow",
                earliest_backtest_date()
            ),
        );
    }
    let site = match &state.site {
        Ok(site) => Arc::clone(site),
        Err(reason) => return json_error(StatusCode::SERVICE_UNAVAILABLE, reason.clone()),
    };
    let floor = state.backup_floor(&site);
    let key = (date, floor.map(f32::to_bits));
    if let Some(report) = state.cached(&key, (state.clock)()) {
        debug!("Backtest {date} served from cache");
        return Json(report.as_ref()).into_response();
    }
    // One load at a time, waiting at most `load_wait`; whoever waited for the permit may find
    // the result cached now
    let _permit = match tokio::time::timeout(state.load_wait, state.loads.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the backtest is shutting down",
            );
        }
        Err(_) => {
            info!("Backtest {date}: another day is still loading, request turned away");
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "another backtest day is loading, try again",
            );
        }
    };
    if let Some(report) = state.cached(&key, (state.clock)()) {
        debug!("Backtest {date} served from cache after waiting");
        return Json(report.as_ref()).into_response();
    }

    let started = Instant::now();
    let loaded = match load_day(state.reader.as_ref(), &site, date, (state.clock)()).await {
        Ok(loaded) => loaded,
        Err(err @ (LoadError::NotFinished | LoadError::OutOfRange)) => {
            return json_error(StatusCode::BAD_REQUEST, err.to_string());
        }
        Err(LoadError::History(e)) => {
            warn!("Backtest {date}: history unavailable: {e}");
            return json_error(StatusCode::BAD_GATEWAY, e.to_string());
        }
    };
    // A horizon cut at `now` (the day after has not ended) or a failed HDO lookup can still
    // change: cache it only briefly
    let provisional = !loaded.horizon_complete || loaded.hdo_lookup_error.is_some();
    let replay_site = Arc::clone(&site);
    let report =
        match tokio::task::spawn_blocking(move || build_report(&loaded, &replay_site, floor)).await
        {
            Ok(Ok(report)) => Arc::new(report),
            Ok(Err(message)) => return json_error(StatusCode::UNPROCESSABLE_ENTITY, message),
            Err(e) => {
                error!("Backtest {date}: replay task failed: {e}");
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "the replay failed");
            }
        };
    info!(
        "Backtest {date}: {} of {} blocks usable, replayed in {:.1} s",
        report.data.usable_blocks,
        report.data.blocks,
        started.elapsed().as_secs_f32()
    );
    let now = (state.clock)();
    let expires = provisional.then(|| now + PROVISIONAL_TTL);
    state.store(key, Arc::clone(&report), expires, now);
    Json(report.as_ref()).into_response()
}

/// Extract ingress path from request headers
fn extract_ingress_path(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("X-Ingress-Path")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}
