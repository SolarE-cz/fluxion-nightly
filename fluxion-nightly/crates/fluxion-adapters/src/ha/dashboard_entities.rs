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

//! Dashboard entities that replace the Node-RED duties (legacy decommission, brief H10).
//!
//! Publishes `sensor.fluxion_*` entities with the attribute schema Node-RED used, so the owner's
//! dashboard cards only need their entity ids changed. Also carries the SolaX inverter lock guard
//! and the daily plan notification.
//!
//! Pure builders (plan attributes, mode labels, night usage, lock decision) are separated from the
//! ECS systems so they can be unit tested without Home Assistant.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::ha::client::HomeAssistantClient;
use crate::ha::plugin::{HaClientResource, HaConfig, HaEntityPrefix};
use crate::ha::types::HistoryDataPoint;
use fluxion_core::continuous_systems::ExportCurtailmentState;
use fluxion_core::resources::UserControlResource;
use fluxion_core::{DebugModeConfig, SystemConfig, TimezoneConfig, UserLeversResource};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::SpotPriceData;
use fluxion_types::scheduling::{CurrentMode, OperationSchedule, ScheduledMode};

pub const PLAN_ENTITY: &str = "sensor.fluxion_pv_work_mode_plan";
pub const ACTIVE_MODE_ENTITY: &str = "sensor.fluxion_aktivni_pracovni_rezim";
pub const NIGHT_USAGE_ENTITY: &str = "sensor.fluxion_prum_nocni_vyuziti_baterie";

pub const PLAN_FRIENDLY_NAME: &str = "FluxION Plán pracovních režimů";
pub const ACTIVE_MODE_FRIENDLY_NAME: &str = "FluxION Aktivní pracovní režim";
pub const NIGHT_USAGE_FRIENDLY_NAME: &str = "FluxION Průměrné noční využití baterie";

/// Suffix appended to the active-mode label while export is curtailed (Node-RED wording)
pub const CURTAILED_SUFFIX: &str = ", přetoky vypnuty";

/// Lock guard notification (title and message from the brief and Node-RED's "Set Message and Title")
pub const LOCK_NOTIFICATION_TITLE: &str = "FluxION: střídač je zamčený";
/// Daily plan notification title (Node-RED "FVE Info")
pub const DAILY_PLAN_NOTIFICATION_TITLE: &str = "FVE Info";

/// How often the plan and active-mode payloads are rebuilt (window and source can move with time)
const EVALUATE_INTERVAL: Duration = Duration::from_secs(60);

/// Publish again even when the payload is unchanged, at most this often
const FORCED_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Night usage recalculation: daily at 06:05 local
const NIGHT_USAGE_HOUR: u32 = 6;
const NIGHT_USAGE_MINUTE: u32 = 5;

/// Daily plan notification: 00:15 local
const DAILY_PLAN_HOUR: u32 = 0;
const DAILY_PLAN_MINUTE: u32 = 15;

// ============================================================================
// Mode labels
// ============================================================================

/// Home Assistant labels and display data for one FluxION operation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeLabel {
    /// SolaX charger use mode label (as sent to the inverter)
    pub mode: &'static str,
    /// SolaX manual mode label, empty when the mode has none
    pub submode: &'static str,
    /// Czech dashboard label
    pub display: &'static str,
    /// mdi icon for the active-mode sensor
    pub icon: &'static str,
}

/// Map a FluxION operation mode to its SolaX labels and Czech dashboard label.
///
/// `BackUpMode` shows as "Zákaz vybíjení" (battery reserved, no discharge) and
/// `NoChargeNoDischarge` as "Podržení baterie" (hold SOC).
#[must_use]
pub fn mode_label(mode: InverterOperationMode) -> ModeLabel {
    match mode {
        InverterOperationMode::SelfUse => ModeLabel {
            mode: "Self Use Mode",
            submode: "",
            display: "Běžný režim",
            icon: "mdi:home-battery",
        },
        InverterOperationMode::BackUpMode => ModeLabel {
            mode: "Back Up Mode",
            submode: "",
            display: "Zákaz vybíjení",
            icon: "mdi:battery-lock",
        },
        InverterOperationMode::ForceCharge => ModeLabel {
            mode: "Manual Mode",
            submode: "Force Charge",
            display: "Nabíjení baterie",
            icon: "mdi:battery-charging",
        },
        InverterOperationMode::ForceDischarge => ModeLabel {
            mode: "Manual Mode",
            submode: "Force Discharge",
            display: "Vybíjení baterie",
            icon: "mdi:battery-arrow-down",
        },
        InverterOperationMode::NoChargeNoDischarge => ModeLabel {
            mode: "Manual Mode",
            submode: "Stop Charge and Discharge",
            display: "Podržení baterie",
            icon: "mdi:battery-sync",
        },
        InverterOperationMode::FeedinPriority => ModeLabel {
            mode: "Feedin Priority",
            submode: "",
            display: "Prodej do sítě",
            icon: "mdi:transmission-tower-export",
        },
    }
}

/// Short Czech `condition` shown in the plan for a mode
#[must_use]
pub fn condition_label(mode: InverterOperationMode) -> &'static str {
    match mode {
        InverterOperationMode::SelfUse => "Vždy",
        InverterOperationMode::ForceCharge => "levná cena",
        InverterOperationMode::ForceDischarge => "špička",
        InverterOperationMode::FeedinPriority => "prodej do sítě",
        InverterOperationMode::BackUpMode => "záloha",
        InverterOperationMode::NoChargeNoDischarge => "držení SOC",
    }
}

/// Where the active mode comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSource {
    Plan,
    Vpr,
    KillSwitch,
    Debug,
}

impl ActiveSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Vpr => "vpr",
            Self::KillSwitch => "kill_switch",
            Self::Debug => "debug",
        }
    }
}

/// Pick the source label. Kill switch wins over VPR, VPR over debug, debug over plan.
#[must_use]
pub fn active_source(kill_switch: bool, vpr_active: bool, debug: bool) -> ActiveSource {
    if kill_switch {
        ActiveSource::KillSwitch
    } else if vpr_active {
        ActiveSource::Vpr
    } else if debug {
        ActiveSource::Debug
    } else {
        ActiveSource::Plan
    }
}

/// Attributes for `sensor.fluxion_aktivni_pracovni_rezim` (state = `value`).
#[must_use]
pub fn active_mode_state(
    mode: InverterOperationMode,
    source: ActiveSource,
    reason: &str,
    curtailed: bool,
) -> (String, Value) {
    let label = mode_label(mode);
    let mut value = label.display.to_string();
    if curtailed {
        value.push_str(CURTAILED_SUFFIX);
    }
    let attributes = json!({
        "source": source.as_str(),
        "reason": reason,
        "value": value,
        "icon": label.icon,
    });
    (value, attributes)
}

// ============================================================================
// Plan sensor
// ============================================================================

/// One merged run of equal-mode 15-minute blocks, in local time
#[derive(Debug, Clone, PartialEq)]
pub struct MergedBlock {
    pub day: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// "HH:MM" local start
    pub start_label: String,
    /// "HH:MM" local end, "24:00" when the run ends at local midnight
    pub end_label: String,
    pub mode: &'static str,
    pub submode: &'static str,
    pub display_mode: &'static str,
    pub condition: String,
}

/// Local midnight of `date` in `tz`, as UTC
fn local_midnight(tz: Tz, date: NaiveDate) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).expect("valid midnight");
    tz.from_local_datetime(&naive).earliest().map_or_else(
        || Utc.from_utc_datetime(&naive),
        |dt| dt.with_timezone(&Utc),
    )
}

/// Window of the plan: local today 00:00 up to local tomorrow's end (exclusive)
#[must_use]
pub fn plan_window(now: DateTime<Utc>, tz: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let today = now.with_timezone(&tz).date_naive();
    let start = local_midnight(tz, today);
    let end = local_midnight(tz, today + ChronoDuration::days(2));
    (start, end)
}

/// Merge scheduled 15-minute blocks inside the window into runs of equal mode.
///
/// The condition of a run is `condition_label` of its mode.
#[must_use]
pub fn merged_blocks(
    schedule: &[ScheduledMode],
    window: (DateTime<Utc>, DateTime<Utc>),
    tz: Tz,
) -> Vec<MergedBlock> {
    let mut blocks: Vec<&ScheduledMode> = schedule
        .iter()
        .filter(|b| b.block_start >= window.0 && b.block_start < window.1)
        .collect();
    blocks.sort_by_key(|b| b.block_start);

    let mut merged: Vec<MergedBlock> = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        let first = blocks[i];
        let mut last = first;
        let mut j = i + 1;
        while j < blocks.len() && blocks[j].mode == first.mode {
            last = blocks[j];
            j += 1;
        }
        let start = first.block_start;
        let end = last.block_start + ChronoDuration::minutes(i64::from(last.duration_minutes));
        let start_local = start.with_timezone(&tz);
        let end_local = end.with_timezone(&tz);
        let label = mode_label(first.mode);
        let condition = condition_label(first.mode).to_string();
        let end_label = if end_local.date_naive() != start_local.date_naive() {
            "24:00".to_string()
        } else {
            end_local.format("%H:%M").to_string()
        };
        merged.push(MergedBlock {
            day: start_local.date_naive(),
            start,
            end,
            start_label: start_local.format("%H:%M").to_string(),
            end_label,
            mode: label.mode,
            submode: label.submode,
            display_mode: label.display,
            condition,
        });
        i = j;
    }
    merged
}

/// Build the attributes of `sensor.fluxion_pv_work_mode_plan`.
///
/// Keys are exactly Node-RED's: `date`, `blocks`, `prices`, `price_stats`, `total_blocks`,
/// `display_blocks_count`. `friendly_name` is added at publish time, not here.
///
/// - `total_blocks`: number of scheduled 15-minute blocks of today (local day)
/// - `display_blocks_count`: number of merged blocks shown
/// - `prices`: one entry per 15-minute price block of today (local day), `timestamp` in ms.
///   `sell` is `spot_sell_price_czk_per_kwh`, or 0 when unavailable.
#[must_use]
pub fn build_plan_attributes(
    schedule: &[ScheduledMode],
    prices: &[fluxion_types::pricing::TimeBlockPrice],
    tz: Tz,
    now: DateTime<Utc>,
    generated_at: DateTime<Utc>,
) -> Value {
    let window = plan_window(now, tz);
    let merged = merged_blocks(schedule, window, tz);

    // Today's local blocks only: 96 on a normal day, 92/100 on DST days (Node-RED semantics)
    let today = now.with_timezone(&tz).date_naive();
    let day = (
        local_midnight(tz, today),
        local_midnight(tz, today + ChronoDuration::days(1)),
    );
    let total_blocks = schedule
        .iter()
        .filter(|b| b.block_start >= day.0 && b.block_start < day.1)
        .count();

    let mut window_prices: Vec<&fluxion_types::pricing::TimeBlockPrice> = prices
        .iter()
        .filter(|p| p.block_start >= day.0 && p.block_start < day.1)
        .collect();
    window_prices.sort_by_key(|p| p.block_start);

    let price_entries: Vec<Value> = window_prices
        .iter()
        .map(|p| {
            json!({
                "buy": round2(p.effective_price_czk_per_kwh),
                "sell": round2(p.spot_sell_price_czk_per_kwh.unwrap_or(0.0)),
                "timestamp": p.block_start.timestamp_millis(),
            })
        })
        .collect();

    let buys: Vec<f32> = window_prices
        .iter()
        .map(|p| p.effective_price_czk_per_kwh)
        .collect();
    let sells: Vec<f32> = window_prices
        .iter()
        .map(|p| p.spot_sell_price_czk_per_kwh.unwrap_or(0.0))
        .collect();
    let price_stats = price_stats(&buys, &sells);

    let block_entries: Vec<Value> = merged
        .iter()
        .map(|m| {
            json!({
                "start": m.start_label,
                "end": m.end_label,
                "mode": m.mode,
                "submode": m.submode,
                "display_mode": m.display_mode,
                "condition": m.condition,
            })
        })
        .collect();

    json!({
        "date": generated_at.with_timezone(&tz).format("%-d. %-m. %Y %H:%M:%S").to_string(),
        "blocks": block_entries,
        "prices": price_entries,
        "price_stats": price_stats,
        "total_blocks": total_blocks,
        "display_blocks_count": merged.len(),
    })
}

/// Prices are published with 2 decimals (CZK/kWh)
fn round2(value: f32) -> f64 {
    (f64::from(value) * 100.0).round() / 100.0
}

/// min/max/avg of buy and sell prices, rounded to 2 decimals; all zero when there are no prices
fn price_stats(buys: &[f32], sells: &[f32]) -> Value {
    fn stats(values: &[f32]) -> (f32, f32, f32) {
        if values.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let min = values.iter().copied().fold(f32::INFINITY, f32::min);
        let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let avg = values.iter().sum::<f32>() / values.len() as f32;
        (min, max, avg)
    }
    let (min_buy, max_buy, avg_buy) = stats(buys);
    let (min_sell, max_sell, avg_sell) = stats(sells);
    json!({
        "min_buy": round2(min_buy),
        "max_buy": round2(max_buy),
        "avg_buy": round2(avg_buy),
        "min_sell": round2(min_sell),
        "max_sell": round2(max_sell),
        "avg_sell": round2(avg_sell),
    })
}

/// Plain-text body of the daily plan notification: one line per merged block of today
#[must_use]
pub fn daily_plan_message(merged: &[MergedBlock], today: NaiveDate) -> String {
    merged
        .iter()
        .filter(|m| m.day == today)
        .map(|m| {
            format!(
                "{}-{}: {} ({})",
                m.start_label, m.end_label, m.display_mode, m.condition
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// Night battery usage
// ============================================================================

/// Node-RED's `calc_average_evening_consumption` inputs (7-day history of four counters)
#[derive(Debug, Clone, Default)]
pub struct NightUsageHistory {
    pub import: Vec<HistoryDataPoint>,
    pub battery_output: Vec<HistoryDataPoint>,
    pub export: Vec<HistoryDataPoint>,
    pub solar: Vec<HistoryDataPoint>,
}

/// Node-RED's `findClosestValue`: the sample whose local time-of-day (minutes) is nearest to
/// `target_minutes`. Ties keep the earlier sample.
fn closest_sample(samples: &[(f64, f32)], target_minutes: f64) -> Option<f32> {
    let mut best: Option<(f64, f32)> = None;
    for &(minutes, value) in samples {
        let dist = (minutes - target_minutes).abs();
        match best {
            Some((best_dist, _)) if dist >= best_dist => {}
            _ => best = Some((dist, value)),
        }
    }
    best.map(|(_, value)| value)
}

/// Average nightly battery usage in % of `capacity_kwh`, following Node-RED's formula.
///
/// `history` covers the eight complete local days `today-8 .. today-1`, and samples are
/// bucketed by local date. The seven nights `d` in `today-7 .. today-1` each need the day
/// before (`d-1`) too, which is why eight days are fetched. For each night whose data is
/// present, the counters nearest to these local times are read:
/// - evening (day `d-1`): 18:00 and 23:59;
/// - night (day `d`): 00:01 and 06:00.
///
/// Each window's house consumption is `battery_output - export + solar + import` (deltas).
/// The result is `(mean evening + mean night) / capacity_kwh * 100` over the valid nights.
///
/// Returns `None` when no night is valid or the capacity is not positive.
#[must_use]
pub fn night_battery_usage_pct(
    history: &NightUsageHistory,
    capacity_kwh: f32,
    tz: Tz,
    today: NaiveDate,
) -> Option<f32> {
    if capacity_kwh <= 0.0 {
        return None;
    }

    type ByDate = BTreeMap<NaiveDate, Vec<(f64, f32)>>;
    // Local date -> (minutes of day, value); only dates with samples are present
    let bucket = |series: &[HistoryDataPoint]| -> ByDate {
        let mut out: ByDate = BTreeMap::new();
        for point in series {
            if !point.value.is_finite() {
                continue;
            }
            let local = point.timestamp.with_timezone(&tz);
            let minutes = f64::from(local.hour() * 60 + local.minute());
            out.entry(local.date_naive())
                .or_default()
                .push((minutes, point.value));
        }
        out
    };
    let import = bucket(&history.import);
    let battery = bucket(&history.battery_output);
    let export = bucket(&history.export);
    let solar = bucket(&history.solar);

    // Counter value nearest to `target` on `date`; 0 when the date has no sample
    let value = |series: &ByDate, date: NaiveDate, target: f64| -> f64 {
        series
            .get(&date)
            .and_then(|samples| closest_sample(samples, target))
            .map_or(0.0, f64::from)
    };

    let mut valid_nights = 0_u32;
    let mut sum_evening = 0.0_f64;
    let mut sum_night = 0.0_f64;
    let first_day = today - ChronoDuration::days(7);

    for offset in 0..7 {
        let night = first_day + ChronoDuration::days(offset);
        let evening_day = night - ChronoDuration::days(1);
        let all_present = [&import, &battery, &export, &solar]
            .iter()
            .all(|s| s.contains_key(&night) && s.contains_key(&evening_day));
        if !all_present {
            continue;
        }

        // Night (day `night`): 00:01 and 06:00
        let (i6, b6, e6, s6) = (
            value(&import, night, 360.0),
            value(&battery, night, 360.0),
            value(&export, night, 360.0),
            value(&solar, night, 360.0),
        );
        let (i001, b001, e001, s001) = (
            value(&import, night, 0.6),
            value(&battery, night, 0.6),
            value(&export, night, 0.6),
            value(&solar, night, 0.6),
        );
        // Evening (day before): 18:00 and midnight (23:59)
        let (i18, b18, e18, s18) = (
            value(&import, evening_day, 1080.0),
            value(&battery, evening_day, 1080.0),
            value(&export, evening_day, 1080.0),
            value(&solar, evening_day, 1080.0),
        );
        let (imid, bmid, emid, smid) = (
            value(&import, evening_day, 1439.4),
            value(&battery, evening_day, 1439.4),
            value(&export, evening_day, 1439.4),
            value(&solar, evening_day, 1439.4),
        );

        sum_evening += (bmid - b18) - (emid - e18) + (smid - s18) + (imid - i18);
        sum_night += (b6 - b001) - (e6 - e001) + (s6 - s001) + (i6 - i001);
        valid_nights += 1;
    }

    if valid_nights == 0 {
        return None;
    }
    let nights = f64::from(valid_nights);
    let avg_evening = sum_evening / nights;
    let avg_night = sum_night / nights;
    Some(((avg_evening + avg_night) / f64::from(capacity_kwh) * 100.0) as f32)
}

/// Start of the night-usage history: local midnight eight days before `today`
#[must_use]
pub fn night_history_start(tz: Tz, today: NaiveDate) -> DateTime<Utc> {
    local_midnight(tz, today - ChronoDuration::days(8))
}

/// Whether the night-usage calculation should run now.
///
/// It always runs once at startup. After that it runs at or after 06:05 local, once per day.
#[must_use]
pub fn night_run_due(
    started: bool,
    local_hm: (u32, u32),
    last_run: Option<NaiveDate>,
    today: NaiveDate,
) -> bool {
    !started || (local_hm >= (NIGHT_USAGE_HOUR, NIGHT_USAGE_MINUTE) && last_run != Some(today))
}

// ============================================================================
// Lock guard
// ============================================================================

/// What the lock guard does for one reading
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockDecision {
    pub notify: bool,
    pub unlock: bool,
}

/// Per-inverter lock event state
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockGuardState {
    /// A lock event is in progress (state is "Locked")
    pub locked: bool,
    /// The notification for this event was created
    pub notified: bool,
    /// The unlock for this event was requested
    pub unlock_sent: bool,
}

/// Decide the lock guard action for one reading.
///
/// - notify once per lock event;
/// - unlock once per lock event, only when `auto_unlock` is enabled and debug is off;
/// - the event ends when the state is no longer "Locked".
pub fn lock_guard_step(
    state: &mut LockGuardState,
    is_locked: bool,
    auto_unlock: bool,
    debug: bool,
) -> LockDecision {
    if !is_locked {
        *state = LockGuardState::default();
        return LockDecision::default();
    }
    state.locked = true;
    let notify = !state.notified;
    state.notified = true;
    let unlock = auto_unlock && !debug && !state.unlock_sent;
    if unlock {
        state.unlock_sent = true;
    }
    LockDecision { notify, unlock }
}

/// Decide the lock guard action for one HA reading.
///
/// `unavailable` and `unknown` are transient and change nothing: they neither start nor end a
/// lock event, so a flap Locked -> unavailable -> Locked is one event.
pub fn lock_reading_decision(
    state: &mut LockGuardState,
    reading: &str,
    auto_unlock: bool,
    debug: bool,
) -> Option<LockDecision> {
    if matches!(reading, "unavailable" | "unknown") {
        return None;
    }
    Some(lock_guard_step(
        state,
        reading == "Locked",
        auto_unlock,
        debug,
    ))
}

/// Stable notification id, so a repeated lock event replaces rather than stacks notifications
#[must_use]
pub fn lock_notification_id(entity_id: &str) -> String {
    format!("fluxion_inverter_locked_{entity_id}")
}

/// Hash of an entity payload, used to skip republishing an unchanged state
#[must_use]
pub fn payload_hash(state: &str, attributes: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    state.hash(&mut hasher);
    attributes.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Publish when the payload changed or the last publish is older than the forced refresh
pub fn should_publish(
    last_hash: &mut Option<u64>,
    last_sent: &mut Option<Instant>,
    hash: u64,
    now: Instant,
) -> bool {
    let changed = *last_hash != Some(hash);
    let stale = last_sent.is_none_or(|t| now.duration_since(t) >= FORCED_REFRESH_INTERVAL);
    if changed || stale {
        *last_hash = Some(hash);
        *last_sent = Some(now);
        true
    } else {
        false
    }
}

// ============================================================================
// ECS wiring
// ============================================================================

/// One lock-state reading sent from the poll task to the update system
#[derive(Debug, Clone)]
pub struct LockReading {
    pub entity_id: String,
    pub state: String,
}

/// Channels from the async tasks back to the ECS systems
#[derive(Resource)]
pub struct DashboardChannels {
    pub lock_tx: crossbeam_channel::Sender<LockReading>,
    pub lock_rx: crossbeam_channel::Receiver<LockReading>,
    pub night_tx: crossbeam_channel::Sender<Option<f32>>,
    pub night_rx: crossbeam_channel::Receiver<Option<f32>>,
}

impl Default for DashboardChannels {
    fn default() -> Self {
        let (lock_tx, lock_rx) = crossbeam_channel::unbounded();
        let (night_tx, night_rx) = crossbeam_channel::unbounded();
        Self {
            lock_tx,
            lock_rx,
            night_tx,
            night_rx,
        }
    }
}

/// Mutable state of the dashboard systems
#[derive(Resource, Default)]
pub struct DashboardState {
    last_evaluate: Option<Instant>,
    plan_hash: Option<u64>,
    plan_sent: Option<Instant>,
    active_hash: Option<u64>,
    active_sent: Option<Instant>,
    lock_poll: Option<Instant>,
    lock_states: BTreeMap<String, LockGuardState>,
    night_started: bool,
    night_in_flight: bool,
    night_date: Option<NaiveDate>,
    daily_plan_date: Option<NaiveDate>,
    daily_plan_initialized: bool,
}

/// Publish `state` and `attributes` (plus `friendly_name`) on a background task
fn spawn_publish(
    client: Arc<HomeAssistantClient>,
    entity_id: &'static str,
    state: String,
    mut attributes: Value,
    friendly_name: &'static str,
) {
    if let Value::Object(map) = &mut attributes {
        map.insert("friendly_name".to_string(), json!(friendly_name));
    }
    tokio::spawn(async move {
        if let Err(e) = client.set_state(entity_id, &state, attributes).await {
            warn!("⚠️ Failed to publish {}: {}", entity_id, e);
        }
    });
}

/// Call an HA service on a background task
fn spawn_service(client: Arc<HomeAssistantClient>, service: &'static str, data: Value) {
    tokio::spawn(async move {
        if let Err(e) = client.call_service(service, data).await {
            warn!("⚠️ HA service {} failed: {}", service, e);
        }
    });
}

/// Update system: publish the plan and active-mode entities.
///
/// Payloads are rebuilt every 60 s and on schedule, price or mode changes. A payload is
/// published only when it differs from the last one, or every 15 minutes regardless.
#[allow(clippy::too_many_arguments)]
pub fn publish_dashboard_entities_system(
    mut state: ResMut<DashboardState>,
    ha_client: Option<Res<HaClientResource>>,
    timezone: Option<Res<TimezoneConfig>>,
    debug_mode: Option<Res<DebugModeConfig>>,
    user_control: Option<Res<UserControlResource>>,
    levers: Option<Res<UserLeversResource>>,
    curtailment: Option<Res<ExportCurtailmentState>>,
    schedules: Query<Ref<OperationSchedule>>,
    prices: Query<Ref<SpotPriceData>>,
    modes: Query<Ref<CurrentMode>>,
) {
    let Some(client) = ha_client else {
        return;
    };
    let now_instant = Instant::now();
    let due = state
        .last_evaluate
        .is_none_or(|t| now_instant.duration_since(t) >= EVALUATE_INTERVAL);
    let changed = schedules.iter().any(|s| s.is_changed())
        || prices.iter().any(|p| p.is_changed())
        || modes.iter().any(|m| m.is_changed());
    if !due && !changed {
        return;
    }
    state.last_evaluate = Some(now_instant);

    let tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let now = Utc::now();
    // Runtime debug state (the web UI can toggle it); SystemConfig's copy can be stale
    let debug = debug_mode.as_ref().is_some_and(|d| d.enabled);
    let kill_switch = user_control.as_ref().is_some_and(|uc| !uc.state.enabled)
        || levers
            .as_ref()
            .is_some_and(|l| l.available && l.levers.automation_disabled);
    let vpr_active = levers.as_ref().is_some_and(|l| {
        l.available
            && l.levers
                .vpr
                .as_ref()
                .is_some_and(|w| w.phase(now, &tz) == fluxion_types::levers::VprPhase::Active)
    });
    let curtailed = curtailment
        .as_ref()
        .is_some_and(|c| !c.restore_limits_w.is_empty());

    let (schedule, generated_at): (Vec<ScheduledMode>, DateTime<Utc>) = schedules
        .iter()
        .next()
        .map(|s| (s.scheduled_blocks.clone(), s.generated_at))
        .unwrap_or_else(|| (Vec::new(), now));
    let price_blocks: Vec<fluxion_types::pricing::TimeBlockPrice> = prices
        .iter()
        .next()
        .map(|p| p.time_block_prices.clone())
        .unwrap_or_default();

    let plan_attrs = build_plan_attributes(&schedule, &price_blocks, tz, now, generated_at);
    let plan_hash = payload_hash("planned", &plan_attrs);
    let tracker = &mut *state;
    if should_publish(
        &mut tracker.plan_hash,
        &mut tracker.plan_sent,
        plan_hash,
        now_instant,
    ) {
        spawn_publish(
            client.0.clone(),
            PLAN_ENTITY,
            "planned".to_string(),
            plan_attrs,
            PLAN_FRIENDLY_NAME,
        );
    }

    // Applied mode; in debug mode the mode the plan would apply now
    let planned_now = schedule.iter().find(|b| {
        now >= b.block_start
            && now < b.block_start + ChronoDuration::minutes(i64::from(b.duration_minutes))
    });
    let (mode, reason) = match (debug, modes.iter().next(), planned_now) {
        (true, _, Some(block)) => (block.mode, block.reason.clone()),
        (_, Some(current), _) => (current.mode, current.reason.clone()),
        (_, None, Some(block)) => (block.mode, block.reason.clone()),
        _ => (InverterOperationMode::SelfUse, "Initial state".to_string()),
    };
    let source = active_source(kill_switch, vpr_active, debug);
    let (value, attrs) = active_mode_state(mode, source, &reason, curtailed);
    let active_hash = payload_hash(&value, &attrs);
    if should_publish(
        &mut tracker.active_hash,
        &mut tracker.active_sent,
        active_hash,
        now_instant,
    ) {
        spawn_publish(
            client.0.clone(),
            ACTIVE_MODE_ENTITY,
            value,
            attrs,
            ACTIVE_MODE_FRIENDLY_NAME,
        );
    }
    debug!(
        "🧭 Dashboard entities evaluated (source {})",
        source.as_str()
    );
}

/// Update system: recalculate the 7-day night usage at startup and daily at 06:05 local.
pub fn night_usage_system(
    mut state: ResMut<DashboardState>,
    channels: Res<DashboardChannels>,
    ha_client: Option<Res<HaClientResource>>,
    ha_config: Res<HaConfig>,
    timezone: Option<Res<TimezoneConfig>>,
    system_config: Option<Res<SystemConfig>>,
) {
    // Drain the result of a finished calculation
    while let Ok(result) = channels.night_rx.try_recv() {
        state.night_in_flight = false;
        match result {
            Some(pct) => {
                if let Some(client) = &ha_client {
                    spawn_publish(
                        client.0.clone(),
                        NIGHT_USAGE_ENTITY,
                        format!("{:.1}", pct),
                        json!({ "unit_of_measurement": "%" }),
                        NIGHT_USAGE_FRIENDLY_NAME,
                    );
                }
                info!("🌙 Average night battery usage (7 days): {:.1} %", pct);
            }
            None => warn!("⚠️ Night battery usage: no valid history for the last 7 days"),
        }
    }

    let Some(client) = ha_client else {
        return;
    };
    let tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let now = Utc::now();
    let local = now.with_timezone(&tz);
    let today = local.date_naive();
    let local_hm = (local.hour(), local.minute());

    if !night_run_due(state.night_started, local_hm, state.night_date, today)
        || state.night_in_flight
    {
        return;
    }
    state.night_started = true;
    state.night_in_flight = true;
    // Only the scheduled slot marks today as done; a startup run before 06:05 keeps the slot
    if local_hm >= (NIGHT_USAGE_HOUR, NIGHT_USAGE_MINUTE) {
        state.night_date = Some(today);
    }

    let capacity = system_config
        .as_ref()
        .map_or(0.0, |c| c.control_config.battery_capacity_kwh);
    let prefix = ha_config.inverter_prefix.clone();
    let tx = channels.night_tx.clone();
    let client = client.0.clone();
    // Eight complete local days before today: seven nights, each with its day before
    let start = night_history_start(tz, today);
    let end = local_midnight(tz, today);
    tokio::spawn(async move {
        let fetch = |suffix: &'static str| {
            let client = client.clone();
            let entity = format!("sensor.{}_{}", prefix, suffix);
            async move { client.get_history(&entity, start, Some(end)).await }
        };
        let (import, battery_output, export, solar) = tokio::join!(
            fetch("today_s_import_energy"),
            fetch("battery_output_energy_today"),
            fetch("today_s_export_energy"),
            fetch("today_s_solar_energy"),
        );
        let history = match (import, battery_output, export, solar) {
            (Ok(import), Ok(battery_output), Ok(export), Ok(solar)) => NightUsageHistory {
                import,
                battery_output,
                export,
                solar,
            },
            _ => {
                warn!("⚠️ Night battery usage: history fetch failed");
                let _ = tx.send(None);
                return;
            }
        };
        let result = night_battery_usage_pct(&history, capacity, tz, today);
        let _ = tx.send(result);
    });
}

/// Update system: poll `select.<prefix>_lock_state` at the HA polling interval.
pub fn poll_lock_state_system(
    mut state: ResMut<DashboardState>,
    channels: Res<DashboardChannels>,
    ha_client: Option<Res<HaClientResource>>,
    ha_config: Res<HaConfig>,
    q_prefixes: Query<&HaEntityPrefix>,
) {
    let Some(client) = ha_client else {
        return;
    };
    let now = Instant::now();
    if let Some(last) = state.lock_poll
        && now.duration_since(last) < ha_config.polling_interval
    {
        return;
    }
    state.lock_poll = Some(now);

    let prefixes: Vec<String> = if q_prefixes.is_empty() {
        vec![ha_config.inverter_prefix.clone()]
    } else {
        q_prefixes.iter().map(|p| p.0.clone()).collect()
    };
    for prefix in prefixes {
        let entity_id = format!("select.{}_lock_state", prefix);
        let client = client.0.clone();
        let tx = channels.lock_tx.clone();
        tokio::spawn(async move {
            match client.get_state(&entity_id).await {
                Ok(s) => {
                    let _ = tx.send(LockReading {
                        entity_id,
                        state: s.state,
                    });
                }
                Err(e) => debug!("Lock state {} not readable: {}", entity_id, e),
            }
        });
    }
}

/// Update system: apply the lock guard to each reading (notify, optionally unlock).
pub fn lock_guard_system(
    mut state: ResMut<DashboardState>,
    channels: Res<DashboardChannels>,
    ha_client: Option<Res<HaClientResource>>,
    debug_mode: Option<Res<DebugModeConfig>>,
    system_config: Option<Res<SystemConfig>>,
) {
    let Some(client) = ha_client else {
        return;
    };
    // Runtime debug state; without it, behave as debug and never unlock
    let debug = debug_mode.as_ref().is_none_or(|d| d.enabled);
    let auto_unlock = system_config
        .as_ref()
        .is_some_and(|c| c.control_config.auto_unlock_inverter);

    while let Ok(reading) = channels.lock_rx.try_recv() {
        let entry = state
            .lock_states
            .entry(reading.entity_id.clone())
            .or_default();
        let Some(decision) = lock_reading_decision(entry, &reading.state, auto_unlock, debug)
        else {
            debug!(
                "Lock state {} is {}; no change",
                reading.entity_id, reading.state
            );
            continue;
        };
        if decision.notify {
            warn!(
                "🔒 Inverter lock state {} is Locked; notifying",
                reading.entity_id
            );
            spawn_service(
                client.0.clone(),
                "persistent_notification.create",
                json!({
                    "title": LOCK_NOTIFICATION_TITLE,
                    "message": format!(
                        "Střídač má nastaven senzor {} na Locked!",
                        reading.entity_id
                    ),
                    "notification_id": lock_notification_id(&reading.entity_id),
                }),
            );
        }
        if decision.unlock {
            info!(
                "🔓 Auto-unlocking inverter: selecting Unlocked on {}",
                reading.entity_id
            );
            spawn_service(
                client.0.clone(),
                "select.select_option",
                json!({
                    "entity_id": reading.entity_id,
                    "option": "Unlocked",
                }),
            );
        }
    }
}

/// Update system: at 00:15 local, create a persistent notification with today's merged blocks.
pub fn daily_plan_notification_system(
    mut state: ResMut<DashboardState>,
    ha_client: Option<Res<HaClientResource>>,
    timezone: Option<Res<TimezoneConfig>>,
    system_config: Option<Res<SystemConfig>>,
    schedules: Query<&OperationSchedule>,
) {
    let tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let now = Utc::now();
    let local = now.with_timezone(&tz);
    let today = local.date_naive();

    // First run: do not send for today, so a restart after 00:15 does not notify again
    if !state.daily_plan_initialized {
        state.daily_plan_initialized = true;
        state.daily_plan_date = Some(today);
        return;
    }

    let enabled = system_config
        .as_ref()
        .is_some_and(|c| c.notifications.daily_plan);
    if !enabled || state.daily_plan_date == Some(today) {
        return;
    }
    if (local.hour(), local.minute()) < (DAILY_PLAN_HOUR, DAILY_PLAN_MINUTE) {
        return;
    }
    let Some(client) = ha_client else {
        return;
    };
    let schedule: Vec<ScheduledMode> = schedules
        .iter()
        .next()
        .map(|s| s.scheduled_blocks.clone())
        .unwrap_or_default();
    let merged = merged_blocks(&schedule, plan_window(now, tz), tz);
    let message = daily_plan_message(&merged, today);
    if message.is_empty() {
        // Schedule for today not ready yet; try again on the next tick
        return;
    }
    state.daily_plan_date = Some(today);
    spawn_service(
        client.0.clone(),
        "persistent_notification.create",
        json!({
            "title": DAILY_PLAN_NOTIFICATION_TITLE,
            "message": message,
        }),
    );
    info!("📨 Daily plan notification sent");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as D;
    use fluxion_types::pricing::TimeBlockPrice;

    fn today() -> NaiveDate {
        "2026-10-13".parse().unwrap()
    }

    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    fn prague() -> Tz {
        "Europe/Prague".parse().unwrap()
    }

    fn block(start: DateTime<Utc>, mode: InverterOperationMode, reason: &str) -> ScheduledMode {
        ScheduledMode {
            block_start: start,
            duration_minutes: 15,
            target_inverters: None,
            mode,
            reason: reason.to_string(),
            decision_uid: None,
            debug_info: None,
        }
    }

    fn price(start: DateTime<Utc>, buy: f32, sell: f32) -> TimeBlockPrice {
        TimeBlockPrice {
            block_start: start,
            duration_minutes: 15,
            price_czk_per_kwh: buy,
            effective_price_czk_per_kwh: buy,
            spot_sell_price_czk_per_kwh: Some(sell),
        }
    }

    /// Local Prague midnight on `date` as UTC
    fn midnight(date: &str) -> DateTime<Utc> {
        let d: NaiveDate = date.parse().unwrap();
        local_midnight(prague(), d)
    }

    #[test]
    fn mode_labels_cover_every_mode() {
        assert_eq!(
            mode_label(InverterOperationMode::SelfUse).display,
            "Běžný režim"
        );
        assert_eq!(
            mode_label(InverterOperationMode::ForceCharge).display,
            "Nabíjení baterie"
        );
        assert_eq!(
            mode_label(InverterOperationMode::ForceDischarge).display,
            "Vybíjení baterie"
        );
        assert_eq!(
            mode_label(InverterOperationMode::BackUpMode).display,
            "Zákaz vybíjení"
        );
        assert_eq!(
            mode_label(InverterOperationMode::NoChargeNoDischarge).display,
            "Podržení baterie"
        );
        assert_eq!(
            mode_label(InverterOperationMode::FeedinPriority).display,
            "Prodej do sítě"
        );
        assert_eq!(
            mode_label(InverterOperationMode::ForceCharge).submode,
            "Force Charge"
        );
    }

    #[test]
    fn active_mode_label_for_each_source_and_curtailment() {
        let cases = [
            (false, false, false, ActiveSource::Plan, "plan"),
            (false, false, true, ActiveSource::Debug, "debug"),
            (false, true, false, ActiveSource::Vpr, "vpr"),
            (true, false, false, ActiveSource::KillSwitch, "kill_switch"),
            (true, true, true, ActiveSource::KillSwitch, "kill_switch"),
        ];
        for (kill, vpr, dbg, expected, name) in cases {
            let source = active_source(kill, vpr, dbg);
            assert_eq!(source, expected);
            assert_eq!(source.as_str(), name);
        }

        let (state, attrs) = active_mode_state(
            InverterOperationMode::FeedinPriority,
            ActiveSource::Plan,
            "cena 0.10",
            false,
        );
        assert_eq!(state, "Prodej do sítě");
        assert_eq!(attrs["value"], "Prodej do sítě");
        assert_eq!(attrs["source"], "plan");
        assert_eq!(attrs["reason"], "cena 0.10");
        assert_eq!(attrs["icon"], "mdi:transmission-tower-export");

        let (state, attrs) = active_mode_state(
            InverterOperationMode::SelfUse,
            ActiveSource::Vpr,
            "VPR",
            true,
        );
        assert_eq!(state, "Běžný režim, přetoky vypnuty");
        assert_eq!(attrs["value"], "Běžný režim, přetoky vypnuty");
    }

    #[test]
    fn plan_merges_equal_mode_runs_and_uses_local_hhmm() {
        // now: 2026-10-14 12:00 local
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let mut schedule = Vec::new();
        for i in 0..8 {
            schedule.push(block(
                start + D::minutes(15 * i),
                InverterOperationMode::SelfUse,
                "Vždy",
            ));
        }
        for i in 8..12 {
            schedule.push(block(
                start + D::minutes(15 * i),
                InverterOperationMode::ForceCharge,
                "cena 4.55 ≤ medián",
            ));
        }
        let attrs = build_plan_attributes(&schedule, &[], prague(), now, now);
        let blocks = attrs["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["start"], "00:00");
        assert_eq!(blocks[0]["end"], "02:00");
        assert_eq!(blocks[0]["display_mode"], "Běžný režim");
        assert_eq!(blocks[0]["condition"], "Vždy");
        assert_eq!(blocks[1]["start"], "02:00");
        assert_eq!(blocks[1]["end"], "03:00");
        assert_eq!(blocks[1]["mode"], "Manual Mode");
        assert_eq!(blocks[1]["submode"], "Force Charge");
        assert_eq!(blocks[1]["display_mode"], "Nabíjení baterie");
        assert_eq!(blocks[1]["condition"], "levná cena");
        // Only 12 blocks are scheduled, all inside today
        assert_eq!(attrs["total_blocks"], 12);
        assert_eq!(attrs["display_blocks_count"], 2);
    }

    #[test]
    fn plan_local_hhmm_across_dst_switch() {
        // 2026-03-29 is the Europe/Prague DST switch day (02:00 CET -> 03:00 CEST).
        // Blocks are 15 minutes apart in UTC; the local clock skips 02:00.
        let now = "2026-03-29T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-03-29");
        let mut schedule = Vec::new();
        for i in 0..12 {
            let mode = if i < 8 {
                InverterOperationMode::SelfUse
            } else {
                InverterOperationMode::ForceCharge
            };
            schedule.push(block(start + D::minutes(15 * i), mode, "x"));
        }
        let attrs = build_plan_attributes(&schedule, &[], prague(), now, now);
        let blocks = attrs["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        // Block 7 starts 01:45 CET; the run ends at 01:00Z, which is 03:00 CEST
        assert_eq!(blocks[0]["start"], "00:00");
        assert_eq!(blocks[0]["end"], "03:00");
        // Block 8 starts 01:00Z = 03:00 CEST (no 02:00 label exists)
        assert_eq!(blocks[1]["start"], "03:00");
        assert_eq!(blocks[1]["end"], "04:00");
    }

    #[test]
    fn plan_prices_use_ms_timestamps_and_stats() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let prices = vec![
            price(start, 2.0, 1.0),
            price(start + D::minutes(15), 4.0, 3.0),
            // outside the window (day after tomorrow) is excluded
            price(start + D::days(3), 9.0, 9.0),
        ];
        let attrs = build_plan_attributes(&[], &prices, prague(), now, now);
        let entries = attrs["prices"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["timestamp"], start.timestamp_millis());
        assert_eq!(entries[1]["timestamp"], start.timestamp_millis() + 900_000);
        assert_eq!(entries[0]["buy"].as_f64().unwrap(), 2.0);
        assert_eq!(entries[0]["sell"].as_f64().unwrap(), 1.0);
        let stats = &attrs["price_stats"];
        assert_eq!(stats["min_buy"].as_f64().unwrap(), 2.0);
        assert_eq!(stats["max_buy"].as_f64().unwrap(), 4.0);
        assert_eq!(stats["avg_buy"].as_f64().unwrap(), 3.0);
        assert_eq!(stats["min_sell"].as_f64().unwrap(), 1.0);
        assert_eq!(stats["max_sell"].as_f64().unwrap(), 3.0);
        assert_eq!(stats["avg_sell"].as_f64().unwrap(), 2.0);
    }

    #[test]
    fn plan_attribute_keys_match_node_red_snapshot() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let schedule = vec![block(start, InverterOperationMode::SelfUse, "Vždy")];
        let prices = vec![price(start, 2.0, 1.0)];
        let attrs = build_plan_attributes(&schedule, &prices, prague(), now, now);

        assert_eq!(
            sorted_keys(&attrs),
            [
                "blocks",
                "date",
                "display_blocks_count",
                "price_stats",
                "prices",
                "total_blocks"
            ]
        );

        assert_eq!(
            sorted_keys(&attrs["blocks"][0]),
            [
                "condition",
                "display_mode",
                "end",
                "mode",
                "start",
                "submode"
            ]
        );
        assert_eq!(
            sorted_keys(&attrs["prices"][0]),
            ["buy", "sell", "timestamp"]
        );
        assert_eq!(
            sorted_keys(&attrs["price_stats"]),
            [
                "avg_buy", "avg_sell", "max_buy", "max_sell", "min_buy", "min_sell"
            ]
        );
        // Node-RED date format: "14. 10. 2026 12:00:00" in local time
        assert_eq!(attrs["date"], "14. 10. 2026 12:00:00");
    }

    fn series(points: &[(&str, f32)]) -> Vec<HistoryDataPoint> {
        points
            .iter()
            .map(|(ts, v)| HistoryDataPoint {
                timestamp: ts.parse().unwrap(),
                value: *v,
            })
            .collect()
    }

    #[test]
    fn night_usage_matches_node_red_formula_on_synthetic_history() {
        // Oct 2026 (CEST, UTC+2). One weekday bucket pair: Monday 2026-10-12 (day 1) and
        // Sunday 2026-10-11 (day 0, previous weekday of Monday).
        // Counters are daily cumulative values in kWh.
        // Sunday 18:00 local = 16:00Z; Sunday 23:59 local = 21:59Z
        // Monday 00:01 local = 2026-10-11T22:01Z; Monday 06:00 local = 04:00Z
        let import = series(&[
            ("2026-10-11T16:00:00Z", 10.0), // Sun 18:00
            ("2026-10-11T21:59:00Z", 14.0), // Sun 23:59
            ("2026-10-11T22:01:00Z", 0.0),  // Mon 00:01
            ("2026-10-12T04:00:00Z", 3.0),  // Mon 06:00
        ]);
        let battery = series(&[
            ("2026-10-11T16:00:00Z", 20.0),
            ("2026-10-11T21:59:00Z", 24.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 2.0),
        ]);
        let export = series(&[
            ("2026-10-11T16:00:00Z", 0.0),
            ("2026-10-11T21:59:00Z", 1.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 0.0),
        ]);
        let solar = series(&[
            ("2026-10-11T16:00:00Z", 5.0),
            ("2026-10-11T21:59:00Z", 5.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 1.0),
        ]);
        let history = NightUsageHistory {
            import,
            battery_output: battery,
            export,
            solar,
        };
        // evening: battery (24-20)=4 - export (1-0)=1 + solar (5-5)=0 + import (14-10)=4 = 7
        // night:   battery (2-0)=2 - export 0 + solar (1-0)=1 + import (3-0)=3 = 6
        // pct = (7 + 6) / 20 * 100 = 65
        let pct = night_battery_usage_pct(&history, 20.0, prague(), today()).unwrap();
        assert!((pct - 65.0).abs() < 1e-3, "got {pct}");
    }

    #[test]
    fn night_usage_none_without_valid_days_or_capacity() {
        let history = NightUsageHistory::default();
        assert!(night_battery_usage_pct(&history, 20.0, prague(), today()).is_none());
        assert!(night_battery_usage_pct(&history, 0.0, prague(), today()).is_none());
    }

    #[test]
    fn closest_sample_picks_nearest_time_of_day() {
        let samples = [(300.0, 1.0_f32), (355.0, 2.0), (420.0, 3.0)];
        assert_eq!(closest_sample(&samples, 360.0), Some(2.0));
        assert_eq!(closest_sample(&samples, 0.6), Some(1.0));
        assert_eq!(closest_sample(&[], 360.0), None);
    }

    #[test]
    fn lock_guard_notifies_once_per_event() {
        let mut state = LockGuardState::default();
        let first = lock_guard_step(&mut state, true, false, false);
        assert_eq!(
            first,
            LockDecision {
                notify: true,
                unlock: false
            }
        );
        let second = lock_guard_step(&mut state, true, false, false);
        assert_eq!(second, LockDecision::default());
        // Event ends, next lock is a new event
        lock_guard_step(&mut state, false, false, false);
        let third = lock_guard_step(&mut state, true, false, false);
        assert!(third.notify);
    }

    #[test]
    fn lock_guard_unlocks_only_when_enabled_and_not_debug() {
        let mut state = LockGuardState::default();
        // Auto-unlock disabled: notify only
        assert_eq!(
            lock_guard_step(&mut state, true, false, false),
            LockDecision {
                notify: true,
                unlock: false
            }
        );
        lock_guard_step(&mut state, false, false, false);

        // Enabled but debug: notify, never unlock
        let debug = lock_guard_step(&mut state, true, true, true);
        assert_eq!(
            debug,
            LockDecision {
                notify: true,
                unlock: false
            }
        );
        lock_guard_step(&mut state, false, true, true);

        // Enabled and not debug: unlock once per event
        let unlock = lock_guard_step(&mut state, true, true, false);
        assert_eq!(
            unlock,
            LockDecision {
                notify: true,
                unlock: true
            }
        );
        let again = lock_guard_step(&mut state, true, true, false);
        assert_eq!(again, LockDecision::default());
    }

    #[test]
    fn lock_guard_unlock_waits_for_debug_off_within_event() {
        let mut state = LockGuardState::default();
        assert_eq!(
            lock_guard_step(&mut state, true, true, true),
            LockDecision {
                notify: true,
                unlock: false
            }
        );
        // Debug turned off while still locked: unlock now, once
        assert_eq!(
            lock_guard_step(&mut state, true, true, false),
            LockDecision {
                notify: false,
                unlock: true
            }
        );
    }

    #[test]
    fn daily_plan_message_lists_today_only() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let schedule = vec![
            block(start, InverterOperationMode::SelfUse, "Vždy"),
            block(
                start + D::days(1),
                InverterOperationMode::FeedinPriority,
                "zítra",
            ),
        ];
        let merged = merged_blocks(&schedule, plan_window(now, prague()), prague());
        let message = daily_plan_message(&merged, "2026-10-14".parse().unwrap());
        assert_eq!(message, "00:00-00:15: Běžný režim (Vždy)");
    }

    /// Two local days with `count` merged runs of alternating modes (15-minute blocks)
    fn two_day_schedule(start: DateTime<Utc>, runs: usize) -> Vec<ScheduledMode> {
        let blocks = 192;
        (0..blocks)
            .map(|i| {
                let run = i * runs / blocks;
                let mode = if run.is_multiple_of(2) {
                    InverterOperationMode::SelfUse
                } else {
                    InverterOperationMode::ForceCharge
                };
                block(start + D::minutes(15 * i as i64), mode, "x")
            })
            .collect()
    }

    #[test]
    fn plan_payload_for_two_days_is_under_12_kib() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let schedule = two_day_schedule(start, 20);
        let prices: Vec<TimeBlockPrice> = (0..192)
            .map(|i| {
                price(
                    start + D::minutes(15 * i),
                    2.0 + (i % 40) as f32 * 0.137,
                    1.0 + (i % 25) as f32 * 0.113,
                )
            })
            .collect();
        let attrs = build_plan_attributes(&schedule, &prices, prague(), now, now);
        assert_eq!(attrs["display_blocks_count"], 20);
        assert_eq!(attrs["total_blocks"], 96);
        let size = serde_json::to_string(&attrs).unwrap().len();
        assert!(size < 12 * 1024, "plan payload is {size} bytes");
    }

    #[test]
    fn plan_prices_are_rounded_to_two_decimals() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let prices = vec![price(start, 2.345_678, 1.006)];
        let attrs = build_plan_attributes(&[], &prices, prague(), now, now);
        assert_eq!(attrs["prices"][0]["buy"], 2.35);
        assert_eq!(attrs["prices"][0]["sell"], 1.01);
        assert_eq!(attrs["price_stats"]["avg_buy"], 2.35);
    }

    #[test]
    fn plan_date_is_schedule_generation_time() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let generated = "2026-10-14T09:15:00Z".parse::<DateTime<Utc>>().unwrap();
        let attrs = build_plan_attributes(&[], &[], prague(), now, generated);
        assert_eq!(attrs["date"], "14. 10. 2026 11:15:00");
    }

    #[test]
    fn unchanged_schedule_does_not_republish() {
        let now = "2026-10-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start = midnight("2026-10-14");
        let schedule = two_day_schedule(start, 20);
        let first = build_plan_attributes(&schedule, &[], prague(), now, now);
        let later = build_plan_attributes(&schedule, &[], prague(), now + D::seconds(60), now);
        let hash = payload_hash("planned", &first);
        assert_eq!(hash, payload_hash("planned", &later));

        let t0 = Instant::now();
        let mut last_hash = None;
        let mut last_sent = None;
        assert!(should_publish(&mut last_hash, &mut last_sent, hash, t0));
        assert!(!should_publish(
            &mut last_hash,
            &mut last_sent,
            hash,
            t0 + Duration::from_secs(60)
        ));
        // Changed payload publishes at once
        assert!(should_publish(
            &mut last_hash,
            &mut last_sent,
            hash.wrapping_add(1),
            t0 + Duration::from_secs(120)
        ));
        // Unchanged payload refreshes after 15 minutes
        let refreshed = t0 + Duration::from_secs(120) + FORCED_REFRESH_INTERVAL;
        assert!(should_publish(
            &mut last_hash,
            &mut last_sent,
            hash.wrapping_add(1),
            refreshed
        ));
    }

    #[test]
    fn active_payload_publishes_only_on_change() {
        let (value, attrs) = active_mode_state(
            InverterOperationMode::SelfUse,
            ActiveSource::Plan,
            "Initial state",
            false,
        );
        let hash = payload_hash(&value, &attrs);
        let (value2, attrs2) = active_mode_state(
            InverterOperationMode::ForceCharge,
            ActiveSource::Plan,
            "Initial state",
            false,
        );
        assert_ne!(hash, payload_hash(&value2, &attrs2));
        let t0 = Instant::now();
        let mut last_hash = None;
        let mut last_sent = None;
        assert!(should_publish(&mut last_hash, &mut last_sent, hash, t0));
        assert!(!should_publish(&mut last_hash, &mut last_sent, hash, t0));
        assert!(should_publish(
            &mut last_hash,
            &mut last_sent,
            hash_of_other(),
            t0
        ));
    }

    fn hash_of_other() -> u64 {
        payload_hash("x", &json!({}))
    }

    #[test]
    fn condition_labels_are_short_czech() {
        for mode in [
            InverterOperationMode::SelfUse,
            InverterOperationMode::ForceCharge,
            InverterOperationMode::ForceDischarge,
            InverterOperationMode::BackUpMode,
            InverterOperationMode::NoChargeNoDischarge,
            InverterOperationMode::FeedinPriority,
        ] {
            let label = condition_label(mode);
            assert!(label.chars().count() <= 40, "{label}");
        }
        assert_eq!(condition_label(InverterOperationMode::SelfUse), "Vždy");
    }

    #[test]
    fn lock_flap_through_unavailable_notifies_and_unlocks_once() {
        let mut state = LockGuardState::default();
        let readings = ["Locked", "unavailable", "Locked", "unknown", "Locked"];
        let mut notifies = 0;
        let mut unlocks = 0;
        for reading in readings {
            if let Some(decision) = lock_reading_decision(&mut state, reading, true, false) {
                notifies += usize::from(decision.notify);
                unlocks += usize::from(decision.unlock);
            }
        }
        assert_eq!((notifies, unlocks), (1, 1));
    }

    #[test]
    fn lock_unlocked_reading_ends_event() {
        let mut state = LockGuardState::default();
        lock_reading_decision(&mut state, "Locked", true, false);
        lock_reading_decision(&mut state, "Unlocked", true, false);
        let again = lock_reading_decision(&mut state, "Locked", true, false).unwrap();
        assert_eq!(
            again,
            LockDecision {
                notify: true,
                unlock: true
            }
        );
    }

    #[test]
    fn lock_notification_id_is_stable_per_entity() {
        assert_eq!(
            lock_notification_id("select.solax_lock_state"),
            "fluxion_inverter_locked_select.solax_lock_state"
        );
    }

    #[test]
    fn night_history_fetches_eight_local_days() {
        let start = night_history_start(prague(), today());
        assert_eq!(start, midnight("2026-10-05"));
    }

    #[test]
    fn night_run_due_at_startup_and_at_0605() {
        let today: NaiveDate = "2026-10-13".parse().unwrap();
        // Startup always computes, before or after 06:05
        assert!(night_run_due(false, (5, 0), None, today));
        assert!(night_run_due(false, (14, 0), None, today));
        // Startup before 06:05 does not consume today's slot: 06:05 still runs
        assert!(!night_run_due(true, (5, 59), None, today));
        assert!(night_run_due(true, (6, 5), None, today));
        // Once today's slot ran, no more runs today
        assert!(!night_run_due(true, (9, 0), Some(today), today));
        // Next day at 06:05 runs again
        let tomorrow = today + ChronoDuration::days(1);
        assert!(night_run_due(true, (6, 5), Some(today), tomorrow));
    }

    #[test]
    fn night_usage_uses_local_dates_not_weekdays() {
        // Same counters as the weekday example, but the history is in a window with an extra
        // earlier Sunday (2026-10-04). It is outside the seven nights ending yesterday
        // (today is 2026-10-13, so nights run 10-06 .. 10-12), so it must not count.
        let import = series(&[
            ("2026-10-04T16:00:00Z", 1.0),
            ("2026-10-04T21:59:00Z", 2.0),
            ("2026-10-04T22:01:00Z", 0.0),
            ("2026-10-05T04:00:00Z", 0.5),
            ("2026-10-11T16:00:00Z", 10.0),
            ("2026-10-11T21:59:00Z", 14.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 3.0),
        ]);
        let battery = series(&[
            ("2026-10-04T16:00:00Z", 1.0),
            ("2026-10-04T21:59:00Z", 2.0),
            ("2026-10-04T22:01:00Z", 0.0),
            ("2026-10-05T04:00:00Z", 0.0),
            ("2026-10-11T16:00:00Z", 20.0),
            ("2026-10-11T21:59:00Z", 24.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 2.0),
        ]);
        let zero = series(&[
            ("2026-10-04T16:00:00Z", 0.0),
            ("2026-10-04T21:59:00Z", 0.0),
            ("2026-10-04T22:01:00Z", 0.0),
            ("2026-10-05T04:00:00Z", 0.0),
            ("2026-10-11T16:00:00Z", 0.0),
            ("2026-10-11T21:59:00Z", 1.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 0.0),
        ]);
        let solar = series(&[
            ("2026-10-04T16:00:00Z", 5.0),
            ("2026-10-04T21:59:00Z", 5.0),
            ("2026-10-04T22:01:00Z", 0.0),
            ("2026-10-05T04:00:00Z", 1.0),
            ("2026-10-11T16:00:00Z", 5.0),
            ("2026-10-11T21:59:00Z", 5.0),
            ("2026-10-11T22:01:00Z", 0.0),
            ("2026-10-12T04:00:00Z", 1.0),
        ]);
        let history = NightUsageHistory {
            import,
            battery_output: battery,
            export: zero,
            solar,
        };
        // Only the 2026-10-12 night is valid (both 10-11 and 10-12 present): 65 %
        let pct = night_battery_usage_pct(&history, 20.0, prague(), today()).unwrap();
        assert!((pct - 65.0).abs() < 1e-3, "got {pct}");
    }
}
