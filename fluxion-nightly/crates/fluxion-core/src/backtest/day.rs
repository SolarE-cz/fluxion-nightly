// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Day loader: 15-minute blocks of one local day (and the days before it, for the load
//! forecast) built from Home Assistant history with the site's live entity ids.
//!
//! Port of `tools/winter_reserve/export_ha_history.py`: power sensors are time-weighted means
//! over the block, SOC is the state at the block start, prices the state one minute into the
//! block. Unavailable states are gaps, never filled. A block is usable only when spot, load, PV,
//! SOC, import and export all have data; otherwise it is reported missing and left out.
//!
//! Prices use the live planner's construction (`scheduling::calculate_effective_prices`): buy =
//! buy sensor (else spot) + trade fee + HDO fee; sell = spot - trade fee when selling at spot,
//! else the control export price. The HDO tariff per block comes from the recorded history of the
//! HDO sensor the live fetcher reads: `strategies.winter_adaptive_v3.hdo_sensor_entity`, or the
//! sensor found by content when that exact entity does not exist (same discovery as live), with
//! its schedule attributes as recorded on that day; a block without schedule is priced at the
//! high tariff, as live.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use crate::resources::GlobalHdoCache;
use crate::savings::RecordedBlock;
use crate::strategy::pricing::{HdoDaySchedule, parse_hdo_sensor_data};
use fluxion_types::config::{ControlConfig, PricingConfig};
use fluxion_types::pricing::TimeBlockPrice;

/// Block length.
pub const BLOCK_MINUTES: i64 = 15;
/// Days before the replayed day used for the load forecast.
pub const FORECAST_HISTORY_DAYS: i64 = 7;

/// One recorded state. `state` is `None` when HA recorded it as unavailable or unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryState {
    pub at: DateTime<Utc>,
    pub state: Option<String>,
    /// Present only when the history was read with attributes
    pub attributes: Option<serde_json::Value>,
}

impl HistoryState {
    /// A state from HA's raw text: `unavailable`, `unknown`, `none` and empty are gaps.
    #[must_use]
    pub fn from_raw(at: DateTime<Utc>, raw: &str, attributes: Option<serde_json::Value>) -> Self {
        let gap = matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "unavailable" | "unknown" | "none" | ""
        );
        Self {
            at,
            state: (!gap).then(|| raw.trim().to_owned()),
            attributes,
        }
    }
}

/// Why history could not be read. The messages are safe to show: no URL, path or token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// HA rejected the token (401/403)
    Unauthorized,
    /// HA answered with this HTTP status
    Status(u16),
    /// The request timed out
    Timeout,
    /// HA could not be reached
    Unreachable,
    /// HA answered with something that is not history
    InvalidResponse,
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "Home Assistant refused the history request"),
            Self::Status(status) => {
                write!(f, "Home Assistant history returned HTTP {status}")
            }
            Self::Timeout => write!(f, "Home Assistant history timed out"),
            Self::Unreachable => write!(f, "Home Assistant is not reachable"),
            Self::InvalidResponse => write!(f, "Home Assistant history response is not valid"),
        }
    }
}

impl std::error::Error for HistoryError {}

/// Read-only access to the recorder history (`/api/history/period`).
#[async_trait]
pub trait HistoryReader: Send + Sync {
    /// States of `entity_id` in `[start, end]`, oldest first, including the state valid at
    /// `start`. Without `with_attributes` the response is minimal (no attributes).
    async fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        with_attributes: bool,
    ) -> Result<Vec<HistoryState>, HistoryError>;

    /// The HDO entity the live HDO fetcher reads for `configured`: that exact entity when it
    /// exists, else the one found by content. `None` when there is none. Default: `configured`.
    async fn resolve_hdo_entity(&self, configured: &str) -> Result<Option<String>, HistoryError> {
        Ok(Some(configured.to_owned()))
    }
}

/// Entity ids the loader reads, all from the live configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BacktestEntities {
    pub soc: String,
    pub house_load: String,
    /// Master PV first, then the `pv_extra_prefixes` units; summed
    pub pv: Vec<String>,
    pub grid_import: String,
    pub grid_export: String,
    pub spot_price: String,
    pub buy_price: Option<String>,
    /// Configured HDO sensor (`strategies.winter_adaptive_v3.hdo_sensor_entity`), resolved at
    /// load time like live
    pub hdo: Option<String>,
}

/// Everything the backtest needs about the site.
#[derive(Debug, Clone)]
pub struct BacktestSite {
    pub timezone: Tz,
    pub entities: BacktestEntities,
    pub pricing: PricingConfig,
    pub control: ControlConfig,
    pub winter_reserve: fluxion_types::config::WinterReserveConfigCore,
}

/// Fields counted when they have no data for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Spot,
    BuySensor,
    Hdo,
    Load,
    Pv,
    /// PV of an extra unit (`pv_extra_prefixes`) had no data: the block's PV is the sum of the
    /// units that have it, as live
    PvPartial,
    Soc,
    Import,
    Export,
}

/// One 15-minute block as loaded. `None` = no data. Energies in kWh over the block.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LoadedBlock {
    pub start: DateTime<Utc>,
    pub spot_czk: Option<f32>,
    /// Buy sensor state (`pricing.buy_price_entity`), when configured and recorded
    pub buy_sensor_czk: Option<f32>,
    /// HDO low tariff, `None` when the schedule did not cover the block
    pub hdo_low: Option<bool>,
    pub load_kwh: Option<f32>,
    /// Master PV plus the extra units that have data; `None` when the master has none
    pub pv_kwh: Option<f32>,
    /// An extra PV unit had no data, so `pv_kwh` holds only the units that did
    pub pv_partial: bool,
    pub soc_pct: Option<f32>,
    pub import_kwh: Option<f32>,
    pub export_kwh: Option<f32>,
    /// Effective buy and sell (CZK/kWh), when the block has a spot price
    pub buy_czk: Option<f32>,
    pub sell_czk: Option<f32>,
}

impl LoadedBlock {
    /// The fields without data. The buy sensor is listed only when one is configured; a block
    /// without it is still usable (priced like live: spot scaled by the median ratio).
    #[must_use]
    pub fn missing_fields(&self, buy_sensor_configured: bool) -> Vec<Field> {
        let mut fields = Vec::new();
        let checks = [
            (Field::Spot, self.spot_czk.is_none()),
            (
                Field::BuySensor,
                buy_sensor_configured && self.buy_sensor_czk.is_none(),
            ),
            (Field::Hdo, self.hdo_low.is_none()),
            (Field::Load, self.load_kwh.is_none()),
            (Field::Pv, self.pv_kwh.is_none()),
            (Field::PvPartial, self.pv_partial),
            (Field::Soc, self.soc_pct.is_none()),
            (Field::Import, self.import_kwh.is_none()),
            (Field::Export, self.export_kwh.is_none()),
        ];
        for (field, missing) in checks {
            if missing {
                fields.push(field);
            }
        }
        fields
    }

    /// The recorded block when everything the replay needs is there.
    #[must_use]
    pub fn recorded(&self) -> Option<(RecordedBlock, f32)> {
        Some((
            RecordedBlock {
                start: self.start,
                buy_czk: self.buy_czk?,
                sell_czk: self.sell_czk?,
                load_kwh: self.load_kwh?,
                pv_kwh: self.pv_kwh?,
                import_kwh: self.import_kwh?,
                export_kwh: self.export_kwh?,
            },
            self.soc_pct?,
        ))
    }
}

/// UTC start of local midnight of `date` (DST-safe: a midnight that does not exist starts at
/// the first valid instant after it).
#[must_use]
pub fn local_midnight_utc(date: NaiveDate, tz: Tz) -> DateTime<Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).unwrap_or_default();
    match tz.from_local_datetime(&midnight).earliest() {
        Some(t) => t.with_timezone(&Utc),
        None => {
            // No midnight that day: the first quarter-hour that exists
            (1..=8)
                .find_map(|q| {
                    tz.from_local_datetime(&(midnight + Duration::minutes(BLOCK_MINUTES * q)))
                        .earliest()
                })
                .map_or_else(
                    || Utc.from_utc_datetime(&midnight),
                    |t| t.with_timezone(&Utc),
                )
        }
    }
}

/// UTC starts of the 15-minute blocks of local day `date`: 92, 96 or 100 on DST days.
#[must_use]
pub fn day_block_starts(date: NaiveDate, tz: Tz) -> Vec<DateTime<Utc>> {
    let start = local_midnight_utc(date, tz);
    let end = date.succ_opt().map_or(start + Duration::days(1), |next| {
        local_midnight_utc(next, tz)
    });
    block_starts(start, end)
}

/// Block starts in `[start, end)`, 15 minutes apart.
fn block_starts(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut starts = Vec::new();
    let mut t = start;
    while t < end {
        starts.push(t);
        t += Duration::minutes(BLOCK_MINUTES);
    }
    starts
}

/// Piecewise-constant numeric history. `None` values are gaps (unavailable or not numeric).
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    points: Vec<(DateTime<Utc>, Option<f32>)>,
}

impl Timeline {
    #[must_use]
    pub fn from_states(states: &[HistoryState]) -> Self {
        let mut points: Vec<(DateTime<Utc>, Option<f32>)> = states
            .iter()
            .map(|s| {
                (
                    s.at,
                    s.state
                        .as_deref()
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|v| v.is_finite()),
                )
            })
            .collect();
        points.sort_by_key(|(at, _)| *at);
        Self { points }
    }

    /// Index of the last point at or before `t`.
    fn index_at(&self, t: DateTime<Utc>) -> Option<usize> {
        self.points
            .partition_point(|(at, _)| *at <= t)
            .checked_sub(1)
    }

    /// Value valid at `t` (last change at or before `t`).
    #[must_use]
    pub fn value_at(&self, t: DateTime<Utc>) -> Option<f32> {
        self.index_at(t).and_then(|i| self.points[i].1)
    }

    /// Time-weighted mean over `[start, end)` of the segments with a value, or `None` when no
    /// part of the interval has one. The last state holds until `end`.
    #[must_use]
    pub fn mean(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Option<f32> {
        let first = self.index_at(start).unwrap_or(0);
        let mut weighted = 0.0f64;
        let mut covered = 0.0f64;
        for (i, (seg_start, value)) in self.points.iter().enumerate().skip(first) {
            if *seg_start >= end {
                break;
            }
            let seg_end = self.points.get(i + 1).map_or(end, |(at, _)| *at);
            let lo = (*seg_start).max(start);
            let hi = seg_end.min(end);
            if hi > lo
                && let Some(v) = value
            {
                let secs = (hi - lo).num_milliseconds() as f64 / 1000.0;
                weighted += f64::from(*v) * secs;
                covered += secs;
            }
        }
        (covered > 0.0).then(|| (weighted / covered) as f32)
    }
}

/// HDO schedules from the HDO sensor's recorded states (with attributes). For each date the
/// schedule of the latest state recorded on or before that local date wins, so a day is priced
/// with the schedule the planner had then.
#[must_use]
pub fn hdo_schedules_from_history(states: &[HistoryState], tz: Tz) -> Vec<HdoDaySchedule> {
    let mut sorted: Vec<&HistoryState> = states.iter().collect();
    sorted.sort_by_key(|s| s.at);
    let mut by_date: BTreeMap<NaiveDate, HdoDaySchedule> = BTreeMap::new();
    for state in sorted {
        let Some(attributes) = &state.attributes else {
            continue;
        };
        let raw = serde_json::json!({
            "state": state.state,
            "attributes": attributes,
        })
        .to_string();
        let recorded_on = state.at.with_timezone(&tz).date_naive();
        for schedule in parse_hdo_sensor_data(&raw) {
            if schedule.date >= recorded_on || !by_date.contains_key(&schedule.date) {
                by_date.insert(schedule.date, schedule);
            }
        }
    }
    by_date.into_values().collect()
}

/// Recorded series of every entity the loader reads.
#[derive(Debug, Clone, Default)]
pub struct SiteHistory {
    pub soc: Timeline,
    pub house_load: Timeline,
    pub pv: Vec<Timeline>,
    pub grid_import: Timeline,
    pub grid_export: Timeline,
    pub spot: Timeline,
    pub buy: Option<Timeline>,
    pub hdo_schedules: Vec<HdoDaySchedule>,
}

/// Median of buy/spot over the blocks that have both prices (the live fallback for blocks the
/// buy sensor does not cover, see `ConfigurablePriceDataSource`).
fn buy_to_spot_ratio(blocks: &[LoadedBlock]) -> Option<f32> {
    let mut ratios: Vec<f32> = blocks
        .iter()
        .filter_map(|b| match (b.spot_czk, b.buy_sensor_czk) {
            (Some(spot), Some(buy)) if spot.abs() > f32::EPSILON => Some(buy / spot),
            _ => None,
        })
        .collect();
    if ratios.is_empty() {
        return None;
    }
    ratios.sort_by(f32::total_cmp);
    let mid = ratios.len() / 2;
    Some(if ratios.len().is_multiple_of(2) {
        f32::midpoint(ratios[mid - 1], ratios[mid])
    } else {
        ratios[mid]
    })
}

/// Fill `buy_czk` / `sell_czk` (and `hdo_low`) the way the live planner prices its blocks.
pub fn apply_planner_prices(
    blocks: &mut [LoadedBlock],
    hdo_schedules: &[HdoDaySchedule],
    pricing: &PricingConfig,
    control: &ControlConfig,
    tz: Tz,
) {
    let cache = GlobalHdoCache::default();
    cache.set_timezone(tz);
    cache.update(hdo_schedules.to_vec());
    // Blocks without a schedule: the planner prices them at the high tariff (and warns once per
    // block). Price them through a cache whose days have no low-tariff window: the same high
    // tariff and local time, without the per-block warning; one summary line instead.
    let unknown_days: std::collections::BTreeSet<NaiveDate> = blocks
        .iter()
        .filter(|b| cache.is_low_tariff(b.start).is_none())
        .map(|b| b.start.with_timezone(&tz).date_naive())
        .collect();
    let high_cache = GlobalHdoCache::default();
    high_cache.set_timezone(tz);
    high_cache.update(
        unknown_days
            .iter()
            .map(|date| HdoDaySchedule {
                date: *date,
                low_tariff_ranges: Vec::new(),
            })
            .collect(),
    );
    let ratio = buy_to_spot_ratio(blocks);
    let mut unknown = 0usize;
    for block in blocks.iter_mut() {
        block.hdo_low = cache.is_low_tariff(block.start);
        let Some(spot) = block.spot_czk else {
            block.buy_czk = None;
            block.sell_czk = None;
            continue;
        };
        let buy_sensor = if pricing.buy_price_entity.is_some() {
            block.buy_sensor_czk.or_else(|| ratio.map(|r| spot * r))
        } else {
            None
        };
        let mut price = [TimeBlockPrice {
            block_start: block.start,
            duration_minutes: 15,
            price_czk_per_kwh: spot,
            effective_price_czk_per_kwh: spot,
            spot_sell_price_czk_per_kwh: pricing
                .use_spot_prices_to_sell
                .then_some(spot - pricing.spot_sell_fee_czk),
            buy_price_czk_per_kwh: buy_sensor,
        }];
        let cache_for_block = if block.hdo_low.is_some() {
            &cache
        } else {
            unknown += 1;
            &high_cache
        };
        crate::scheduling::calculate_effective_prices(&mut price, Some(cache_for_block), pricing);
        block.buy_czk = Some(price[0].effective_price_czk_per_kwh);
        // Winter Reserve sells at the spot sell price, else at the control export price
        block.sell_czk = Some(
            price[0]
                .spot_sell_price_czk_per_kwh
                .unwrap_or(control.grid_export_fee_czk_per_kwh),
        );
    }
    if unknown > 0 {
        tracing::debug!(
            "Backtest: {unknown} block(s) without HDO schedule priced at the high tariff"
        );
    }
}

/// Resample the history into blocks starting in `[start, end)`.
#[must_use]
pub fn build_blocks(
    history: &SiteHistory,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    pricing: &PricingConfig,
    control: &ControlConfig,
    tz: Tz,
) -> Vec<LoadedBlock> {
    let price_offset = Duration::minutes(1);
    let to_kwh = |mean_w: f32| mean_w * 0.25 / 1000.0;
    let mut blocks: Vec<LoadedBlock> = block_starts(start, end)
        .into_iter()
        .map(|t| {
            let block_end = t + Duration::minutes(BLOCK_MINUTES);
            let power = |timeline: &Timeline| timeline.mean(t, block_end).map(to_kwh);
            // PV, as live: the master's plus the extra units that have data (an unavailable
            // extra unit is skipped and the block flagged `pv_partial`); no master, no PV
            let master = history.pv.first().and_then(power);
            let extras: Vec<Option<f32>> = history.pv.iter().skip(1).map(power).collect();
            let pv_partial = master.is_some() && extras.iter().any(Option::is_none);
            let pv = master.map(|m| m + extras.iter().flatten().sum::<f32>());
            LoadedBlock {
                start: t,
                spot_czk: history.spot.value_at(t + price_offset),
                buy_sensor_czk: history
                    .buy
                    .as_ref()
                    .and_then(|b| b.value_at(t + price_offset)),
                hdo_low: None,
                load_kwh: power(&history.house_load),
                pv_kwh: pv,
                pv_partial,
                soc_pct: history.soc.value_at(t),
                import_kwh: power(&history.grid_import),
                export_kwh: power(&history.grid_export),
                buy_czk: None,
                sell_czk: None,
            }
        })
        .collect();
    apply_planner_prices(&mut blocks, &history.hdo_schedules, pricing, control, tz);
    blocks
}

/// Blocks of the replayed day and the days around it.
#[derive(Debug, Clone)]
pub struct LoadedDay {
    pub date: NaiveDate,
    pub day_start: DateTime<Utc>,
    pub day_end: DateTime<Utc>,
    /// From local midnight `FORECAST_HISTORY_DAYS` before `date` to the end of the next day (or
    /// now, whichever is earlier); the next day feeds the planner horizon after 13:00
    pub blocks: Vec<LoadedBlock>,
    /// The HDO sensor had a schedule for at least one block of the day
    pub hdo_from_sensor: bool,
    /// The HDO entity read (after live-style discovery), if any
    pub hdo_entity: Option<String>,
    /// The next day was loaded in full; `false` when `now` cut the planner horizon (the result
    /// can still change, so it should be cached only briefly)
    pub horizon_complete: bool,
    /// Looking up the HDO entity (`/api/states`) failed: every block is priced at the high
    /// tariff, as live without HDO data (the result can change, so cache it only briefly)
    pub hdo_lookup_error: Option<HistoryError>,
}

impl LoadedDay {
    /// The blocks of the replayed day.
    pub fn day_blocks(&self) -> impl Iterator<Item = &LoadedBlock> {
        self.blocks
            .iter()
            .filter(|b| b.start >= self.day_start && b.start < self.day_end)
    }
}

/// Why a day cannot be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The day has not ended yet (or is in the future)
    NotFinished,
    /// The date is outside the range the backtest accepts
    OutOfRange,
    History(HistoryError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinished => write!(f, "the day has not ended yet; pick a past day"),
            Self::OutOfRange => write!(f, "the date is outside the range the backtest accepts"),
            Self::History(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<HistoryError> for LoadError {
    fn from(err: HistoryError) -> Self {
        Self::History(err)
    }
}

/// Read the history of `date` (and the 7 days before, and the next day up to `now`) and build
/// its blocks. Only GET requests to the recorder history; nothing is written.
pub async fn load_day(
    reader: &dyn HistoryReader,
    site: &BacktestSite,
    date: NaiveDate,
    now: DateTime<Utc>,
) -> Result<LoadedDay, LoadError> {
    let tz = site.timezone;
    if !date_in_range(date, now) {
        return Err(LoadError::OutOfRange);
    }
    let day_start = local_midnight_utc(date, tz);
    let next = date.succ_opt().ok_or(LoadError::OutOfRange)?;
    let day_end = local_midnight_utc(next, tz);
    if day_end > now {
        return Err(LoadError::NotFinished);
    }
    let first_history_day = date
        .checked_sub_days(chrono::Days::new(FORECAST_HISTORY_DAYS.unsigned_abs()))
        .ok_or(LoadError::OutOfRange)?;
    let range_start = local_midnight_utc(first_history_day, tz);
    let horizon_end = next
        .succ_opt()
        .map_or(day_end, |after| local_midnight_utc(after, tz));
    let range_end = horizon_end.min(now);

    let entities = &site.entities;
    let mut history = SiteHistory {
        soc: read_numeric(reader, &entities.soc, range_start, range_end).await?,
        house_load: read_numeric(reader, &entities.house_load, range_start, range_end).await?,
        pv: Vec::new(),
        grid_import: read_numeric(reader, &entities.grid_import, range_start, range_end).await?,
        grid_export: read_numeric(reader, &entities.grid_export, range_start, range_end).await?,
        spot: read_numeric(reader, &entities.spot_price, range_start, range_end).await?,
        buy: None,
        hdo_schedules: Vec::new(),
    };
    for entity in &entities.pv {
        history
            .pv
            .push(read_numeric(reader, entity, range_start, range_end).await?);
    }
    if let Some(buy) = &entities.buy_price {
        history.buy = Some(read_numeric(reader, buy, range_start, range_end).await?);
    }
    // The HDO sensor as live resolves it (exact entity, else found by content). When the lookup
    // fails, price at the high tariff as live does without HDO data rather than fail the day.
    let mut hdo_lookup_error = None;
    let hdo_entity = match &entities.hdo {
        Some(configured) => match reader.resolve_hdo_entity(configured).await {
            Ok(entity) => entity,
            Err(e) => {
                tracing::warn!("Backtest {date}: HDO entity lookup failed ({e}); high tariff");
                hdo_lookup_error = Some(e);
                None
            }
        },
        None => None,
    };
    if let Some(hdo) = &hdo_entity {
        // Only the replayed day and the horizon need tariffs; attributes are large, so read less
        let states = reader.history(hdo, day_start, range_end, true).await?;
        history.hdo_schedules = hdo_schedules_from_history(&states, tz);
    }

    let blocks = build_blocks(
        &history,
        range_start,
        range_end,
        &site.pricing,
        &site.control,
        tz,
    );
    let hdo_from_sensor = blocks
        .iter()
        .any(|b| b.start >= day_start && b.start < day_end && b.hdo_low.is_some());
    Ok(LoadedDay {
        date,
        day_start,
        day_end,
        blocks,
        hdo_from_sensor,
        hdo_entity,
        horizon_complete: range_end >= horizon_end,
        hdo_lookup_error,
    })
}

/// Earliest date the backtest accepts.
#[must_use]
pub fn earliest_backtest_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2020, 1, 1).unwrap_or_default()
}

/// Dates the backtest accepts: from 2020-01-01 to tomorrow (UTC). Anything else is a 400.
#[must_use]
pub fn date_in_range(date: NaiveDate, now: DateTime<Utc>) -> bool {
    let tomorrow = now.date_naive().succ_opt().unwrap_or(now.date_naive());
    date >= earliest_backtest_date() && date <= tomorrow
}

async fn read_numeric(
    reader: &dyn HistoryReader,
    entity: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Timeline, HistoryError> {
    let states = reader.history(entity, start, end, false).await?;
    Ok(Timeline::from_states(&states))
}

/// A local day the backtest can replay, with how many of the 7 days before it have history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailableDay {
    pub date: NaiveDate,
    pub history_days: u32,
}

/// Past local days fully covered by the SOC history (the recorder keeps ~10 days by default),
/// newest first. `lookback_days` bounds the request.
pub async fn available_days(
    reader: &dyn HistoryReader,
    site: &BacktestSite,
    now: DateTime<Utc>,
    lookback_days: i64,
) -> Result<Vec<AvailableDay>, HistoryError> {
    let tz = site.timezone;
    let today = now.with_timezone(&tz).date_naive();
    let start = local_midnight_utc(today - Duration::days(lookback_days), tz);
    let states = reader
        .history(&site.entities.soc, start, now, false)
        .await?;
    let Some(first) = states
        .iter()
        .filter(|s| s.state.is_some())
        .map(|s| s.at)
        .min()
    else {
        return Ok(Vec::new());
    };
    let mut days = Vec::new();
    let mut date = today - Duration::days(1);
    while local_midnight_utc(date, tz) >= first {
        let mut history_days = 0u32;
        for back in 1..=FORECAST_HISTORY_DAYS {
            if local_midnight_utc(date - Duration::days(back), tz) >= first {
                history_days += 1;
            }
        }
        days.push(AvailableDay { date, history_days });
        date -= Duration::days(1);
    }
    Ok(days)
}
