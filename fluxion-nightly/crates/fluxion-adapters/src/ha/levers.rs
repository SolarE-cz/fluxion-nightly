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

//! Lever helpers on the "FVE-Automatizace" card, owned by FluxION.
//!
//! Contract: `docs/legacy-levers/CONTRACT.md`. FluxION creates the helpers in Home Assistant when
//! they are missing, copies their first values from the Node-RED entities (once, at creation),
//! polls them into [`UserLevers`] and resets an expired VPR back to "Žádný".

use crate::ha::client::HomeAssistantClient;
use crate::ha::errors::{HaError, HaResult};
use crate::ha::plugin::HaClientResource;
use crate::ha::types::HaEntityState;
use crate::ha::ws::{HaWsSession, websocket_url};
use bevy_ecs::prelude::*;
use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use chrono_tz::Tz;
use crossbeam_channel::{Receiver, Sender};
use fluxion_core::resources::UserLeversResource;
use fluxion_types::{DischargeWindowLimit, UserLevers, VprMode, VprPhase, VprWindow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Poll interval for the helper states.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Wait between ensure/migrate retries while helpers are missing or migrations are pending.
const ENSURE_RETRY_INTERVAL: Duration = Duration::from_secs(300);

/// After this long from task start, levers are published even if a safety helper was never readable.
const DEGRADED_PUBLISH_AFTER: Duration = Duration::from_secs(120);

/// How long a created helper waits for its Node-RED source to become usable before the contract
/// default is used.
const SOURCE_WAIT_LIMIT: TimeDelta = TimeDelta::hours(24);

/// Default location of the helper-id map (the add-on's persistent `/data` volume).
const DEFAULT_STATE_PATH: &str = "/data/fluxion_levers.json";

/// Where a helper's value is used in [`UserLevers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    VprMode,
    VprStart,
    VprStop,
    AutomationDisabled,
    MorningHours,
    MorningMinSoc,
    EveningHours,
    EveningMinSoc,
    ForceChargeBelowCzk,
    NoDischargeBelowCzk,
}

/// Entity id per slot.
pub type EntityIds = HashMap<Slot, String>;

/// Kind of HA helper, with its range and contract default.
#[derive(Debug, Clone, Copy)]
pub enum HelperKind {
    Select,
    Time,
    Switch,
    Number {
        min: f32,
        max: f32,
        step: f32,
        default: f32,
        slider: bool,
        unit: Option<&'static str>,
    },
}

/// One FluxION-owned helper.
#[derive(Debug, Clone, Copy)]
pub struct HelperSpec {
    pub slot: Slot,
    pub domain: &'static str,
    pub name: &'static str,
    /// Node-RED entity whose value is copied once, when the helper is created.
    pub source: Option<&'static str>,
    pub kind: HelperKind,
}

/// Helper table, in the contract's order.
pub const HELPERS: [HelperSpec; 10] = [
    HelperSpec {
        slot: Slot::VprMode,
        domain: "input_select",
        name: "FluxION Vlastní pracovní režim",
        // Never migrated: Node-RED's VPR is a one-time instruction for that day.
        source: None,
        kind: HelperKind::Select,
    },
    HelperSpec {
        slot: Slot::VprStart,
        domain: "input_datetime",
        name: "FluxION VPR Start",
        source: None,
        kind: HelperKind::Time,
    },
    HelperSpec {
        slot: Slot::VprStop,
        domain: "input_datetime",
        name: "FluxION VPR Stop",
        source: None,
        kind: HelperKind::Time,
    },
    HelperSpec {
        slot: Slot::AutomationDisabled,
        domain: "input_boolean",
        name: "FluxION Zakázat Automatizaci",
        // Never migrated: the kill switch always starts off.
        source: None,
        kind: HelperKind::Switch,
    },
    HelperSpec {
        slot: Slot::MorningHours,
        domain: "input_number",
        name: "FluxION Počet ranních hodin pro vybíjení",
        source: Some("number.pocet_rannich_hodin_pro_vybijeni"),
        kind: HelperKind::Number {
            min: 0.0,
            max: 6.0,
            step: 1.0,
            default: 1.0,
            slider: true,
            unit: None,
        },
    },
    HelperSpec {
        slot: Slot::MorningMinSoc,
        domain: "input_number",
        name: "FluxION Max ranní vybití baterie",
        source: Some("number.max_ranni_vybiti_baterie"),
        kind: HelperKind::Number {
            min: 10.0,
            max: 100.0,
            step: 1.0,
            default: 30.0,
            slider: true,
            unit: Some("%"),
        },
    },
    HelperSpec {
        slot: Slot::EveningHours,
        domain: "input_number",
        name: "FluxION Počet večerních hodin pro vybíjení",
        source: Some("number.pocet_vecernich_hodin_pro_vybijeni"),
        kind: HelperKind::Number {
            min: 0.0,
            max: 6.0,
            step: 1.0,
            default: 2.0,
            slider: true,
            unit: None,
        },
    },
    HelperSpec {
        slot: Slot::EveningMinSoc,
        domain: "input_number",
        name: "FluxION Max večerní vybití baterie",
        source: Some("number.max_vecerni_vybiti_baterie"),
        kind: HelperKind::Number {
            min: 10.0,
            max: 100.0,
            step: 1.0,
            default: 30.0,
            slider: true,
            unit: Some("%"),
        },
    },
    HelperSpec {
        slot: Slot::ForceChargeBelowCzk,
        domain: "input_number",
        name: "FluxION Vynutit nabíjení baterie při ceně",
        source: Some("text.vynutit_nabijeni_baterie_pri_cene"),
        kind: HelperKind::Number {
            min: -10.0,
            max: 50.0,
            step: 0.01,
            default: -10.0,
            slider: false,
            unit: Some("CZK/kWh"),
        },
    },
    HelperSpec {
        slot: Slot::NoDischargeBelowCzk,
        domain: "input_number",
        name: "FluxION Zákaz vybíjení při ceně",
        source: Some("text.zakaz_vybijeni_pri_cene"),
        kind: HelperKind::Number {
            min: -10.0,
            max: 50.0,
            step: 0.01,
            default: -10.0,
            slider: false,
            unit: Some("CZK/kWh"),
        },
    },
];

/// Startup settings for the sync. `enabled: false` spawns nothing.
#[derive(Resource, Debug, Clone)]
pub struct LeversSyncConfig {
    pub enabled: bool,
    /// JSON map of helper name → entity id.
    pub state_path: PathBuf,
}

impl Default for LeversSyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            state_path: PathBuf::from(DEFAULT_STATE_PATH),
        }
    }
}

/// Channel from the sync task to the ECS. Holds the sender for the task and the receiver for
/// `poll_levers_channel`.
#[derive(Resource)]
pub struct LeversChannel {
    pub sender: Sender<UserLevers>,
    pub receiver: Receiver<UserLevers>,
}

impl Default for LeversChannel {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }
}

// ============================================================================
// Pure parsing and building (unit-tested)
// ============================================================================

/// Parse an HA time state, `HH:MM:SS` (or `HH:MM`).
pub fn parse_ha_time(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    NaiveTime::parse_from_str(text, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M"))
        .ok()
}

/// Parse a numeric state. Accepts forms like `"-1."` or `"1"`; non-finite values are rejected.
pub fn parse_number(text: &str) -> Option<f32> {
    text.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Clamp to a helper range. Integer steps round first.
pub fn clamp_to_helper(value: f32, min: f32, max: f32, step: f32) -> f32 {
    let stepped = if step >= 1.0 { value.round() } else { value };
    stepped.clamp(min, max)
}

/// The state is usable: present and not `unavailable` / `unknown`.
fn usable(state: &HaEntityState) -> bool {
    !matches!(state.state.as_str(), "unavailable" | "unknown")
}

/// State of the helper in `slot`, if it is usable.
fn lever_state<'a>(
    states: &'a [HaEntityState],
    ids: &EntityIds,
    slot: Slot,
) -> Option<&'a HaEntityState> {
    let id = ids.get(&slot)?;
    states.iter().find(|s| &s.entity_id == id && usable(s))
}

fn parse_last_changed(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn discharge_limit(
    states: &[HaEntityState],
    ids: &EntityIds,
    hours: Slot,
    min_soc: Slot,
) -> Option<DischargeWindowLimit> {
    let hours = parse_number(&lever_state(states, ids, hours)?.state)?;
    let min_soc_pct = parse_number(&lever_state(states, ids, min_soc)?.state)?;
    Some(DischargeWindowLimit {
        hours: clamp_to_helper(hours, 0.0, 6.0, 1.0) as u8,
        min_soc_pct,
    })
}

fn price_lever(states: &[HaEntityState], ids: &EntityIds, slot: Slot) -> Option<f32> {
    lever_state(states, ids, slot)
        .and_then(|s| parse_number(&s.state))
        .and_then(UserLevers::price_lever)
}

/// Safety helpers: the kill switch and the VPR mode, start and stop.
const SAFETY_SLOTS: [Slot; 4] = [
    Slot::AutomationDisabled,
    Slot::VprMode,
    Slot::VprStart,
    Slot::VprStop,
];

/// Last readable state of each safety helper. A missing or unusable read keeps the last known
/// value, so a kill switch that was on stays on while HA is briefly unable to report it.
#[derive(Debug, Default, Clone)]
pub struct SafetyMemory {
    last: HashMap<Slot, HaEntityState>,
}

impl SafetyMemory {
    /// Remember the readable safety states of this poll.
    pub fn update(&mut self, states: &[HaEntityState], ids: &EntityIds) {
        for slot in SAFETY_SLOTS {
            if let Some(state) = lever_state(states, ids, slot) {
                self.last.insert(slot, state.clone());
            }
        }
    }

    /// All four safety helpers have been readable in some poll. Until then `available` stays false.
    pub fn complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// Safety helpers never readable so far.
    pub fn missing(&self) -> Vec<Slot> {
        SAFETY_SLOTS
            .into_iter()
            .filter(|slot| !self.last.contains_key(slot))
            .collect()
    }
}

/// State of a safety helper: this poll's read, else the last known one.
fn safety_state<'a>(
    states: &'a [HaEntityState],
    ids: &EntityIds,
    memory: &'a SafetyMemory,
    slot: Slot,
) -> Option<&'a HaEntityState> {
    lever_state(states, ids, slot).or_else(|| memory.last.get(&slot))
}

/// Build [`UserLevers`] from the helper states. Unavailable helpers give the lever's default, except
/// the safety helpers, which keep their last known value (see [`SafetyMemory`]).
///
/// `now` is the `set_at` fallback when no VPR helper has a usable `last_changed`.
pub fn build_user_levers(
    states: &[HaEntityState],
    ids: &EntityIds,
    now: DateTime<Utc>,
    memory: &SafetyMemory,
) -> UserLevers {
    let midnight = NaiveTime::MIN;
    let mode = safety_state(states, ids, memory, Slot::VprMode)
        .map(|s| VprMode::from_option(&s.state))
        .unwrap_or(VprMode::None);

    // A VPR helper that was never readable means no VPR: its window cannot be known.
    let vpr_readable = [Slot::VprMode, Slot::VprStart, Slot::VprStop]
        .into_iter()
        .all(|slot| safety_state(states, ids, memory, slot).is_some());
    let vpr = (vpr_readable && mode != VprMode::None).then(|| {
        let time_of =
            |slot| safety_state(states, ids, memory, slot).and_then(|s| parse_ha_time(&s.state));
        let set_at = [Slot::VprMode, Slot::VprStart, Slot::VprStop]
            .into_iter()
            .filter_map(|slot| safety_state(states, ids, memory, slot))
            .filter_map(|s| parse_last_changed(&s.last_changed))
            .max()
            .unwrap_or(now);
        VprWindow {
            mode,
            from_local: time_of(Slot::VprStart).unwrap_or(midnight),
            until_local: time_of(Slot::VprStop).unwrap_or(midnight),
            set_at,
        }
    });

    UserLevers {
        vpr,
        automation_disabled: safety_state(states, ids, memory, Slot::AutomationDisabled)
            .is_some_and(|s| s.state == "on"),
        morning_discharge: discharge_limit(states, ids, Slot::MorningHours, Slot::MorningMinSoc),
        evening_discharge: discharge_limit(states, ids, Slot::EveningHours, Slot::EveningMinSoc),
        force_charge_at_or_below_czk: price_lever(states, ids, Slot::ForceChargeBelowCzk),
        no_discharge_at_or_below_czk: price_lever(states, ids, Slot::NoDischargeBelowCzk),
    }
}

/// Helpers with no usable state this poll (missing id, or no state, or unavailable).
pub fn unavailable_helpers(states: &[HaEntityState], ids: &EntityIds) -> Vec<Slot> {
    HELPERS
        .iter()
        .map(|spec| spec.slot)
        .filter(|&slot| lever_state(states, ids, slot).is_none())
        .collect()
}

/// Helper JSON for `<domain>/create`, without the `type` field.
pub fn create_payload(spec: &HelperSpec) -> Value {
    match spec.kind {
        HelperKind::Select => json!({
            "name": spec.name,
            "options": VprMode::OPTIONS,
            "icon": "mdi:home-lightning-bolt-outline",
        }),
        HelperKind::Time => json!({
            "name": spec.name,
            "has_date": false,
            "has_time": true,
        }),
        HelperKind::Switch => json!({
            "name": spec.name,
            "icon": "mdi:home-automation",
        }),
        HelperKind::Number {
            min,
            max,
            step,
            slider,
            unit,
            ..
        } => {
            // Round away f32 noise (0.01 would otherwise go out as 0.009999999776).
            let clean = |v: f32| (f64::from(v) * 1e4).round() / 1e4;
            let mut payload = json!({
                "name": spec.name,
                "min": clean(min),
                "max": clean(max),
                "step": clean(step),
                "mode": if slider { "slider" } else { "box" },
            });
            if let Some(unit) = unit {
                payload["unit_of_measurement"] = json!(unit);
            }
            payload
        }
    }
}

/// Service call that copies a value into a freshly created helper.
///
/// `source` is the Node-RED state text, or `None` when it is missing (the contract default is used).
/// `None` means nothing is written (the kill switch).
pub fn migration_call(
    spec: &HelperSpec,
    entity_id: &str,
    source: Option<&str>,
) -> Option<(&'static str, Value)> {
    match spec.kind {
        HelperKind::Select => {
            let option = source
                .map(str::trim)
                .filter(|o| VprMode::OPTIONS.contains(o))
                .unwrap_or(VprMode::OPTIONS[0]);
            Some((
                "input_select.select_option",
                json!({"entity_id": entity_id, "option": option}),
            ))
        }
        HelperKind::Time => {
            let time = source.and_then(parse_ha_time).unwrap_or(NaiveTime::MIN);
            Some((
                "input_datetime.set_datetime",
                json!({"entity_id": entity_id, "time": time.format("%H:%M:%S").to_string()}),
            ))
        }
        HelperKind::Switch => None,
        HelperKind::Number {
            min,
            max,
            step,
            default,
            ..
        } => {
            let value = source.and_then(parse_number).unwrap_or(default);
            Some((
                "input_number.set_value",
                json!({
                    "entity_id": entity_id,
                    "value": clamp_to_helper(value, min, max, step),
                }),
            ))
        }
    }
}

/// Remembers which VPR window has already been reset, so the reset goes out once per window.
#[derive(Debug, Default)]
pub struct VprResetGuard {
    reset_for: Option<DateTime<Utc>>,
}

impl VprResetGuard {
    /// The VPR window has ended and its reset has not been sent yet.
    pub fn reset_due(&self, levers: &UserLevers, tz: &Tz, now: DateTime<Utc>) -> bool {
        levers.vpr.is_some_and(|window| {
            window.phase(now, tz) == VprPhase::Expired && self.reset_for != Some(window.set_at)
        })
    }

    /// Record that the reset for this window was sent.
    pub fn mark_reset(&mut self, levers: &UserLevers) {
        if let Some(window) = levers.vpr {
            self.reset_for = Some(window.set_at);
        }
    }
}

/// The active VPR window as last anchored: its key, and the `set_at` kept while the key is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VprAnchor {
    pub mode: VprMode,
    pub from_local: NaiveTime,
    pub until_local: NaiveTime,
    pub set_at: DateTime<Utc>,
}

/// Polls in a row that must miss a helper before it is forgotten. HA's REST API can answer 404 for
/// `input_*` helpers while their domains are still loading at startup.
const ABSENT_POLLS_BEFORE_FORGET: u32 = 3;

/// Consecutive not-found reads per entity id, kept by the poll task.
#[derive(Debug, Default)]
pub struct AbsenceCounter {
    misses: HashMap<String, u32>,
}

impl AbsenceCounter {
    /// Record one poll: `found` were read, `absent` were not found. A successful read resets the
    /// count. Returns the ids that have now missed [`ABSENT_POLLS_BEFORE_FORGET`] polls in a row.
    pub fn observe(&mut self, found: &[String], absent: &[String]) -> Vec<String> {
        for id in found {
            self.misses.remove(id);
        }
        let mut confirmed = Vec::new();
        for id in absent {
            let misses = self.misses.entry(id.clone()).or_insert(0);
            *misses += 1;
            if *misses >= ABSENT_POLLS_BEFORE_FORGET {
                confirmed.push(id.clone());
            }
        }
        confirmed
    }
}

/// Persisted setup state: helper entity ids, and helpers created by FluxION whose Node-RED value
/// has not been copied yet. Saved after every change, so a restart resumes the retries.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperSetup {
    /// Helper name → entity id.
    #[serde(default)]
    helpers: BTreeMap<String, String>,
    /// Names of created helpers still waiting for their one-time migration write.
    #[serde(default)]
    unmigrated: BTreeSet<String>,
    /// When each helper was created (name → time). Bounds the wait for its Node-RED source.
    #[serde(default)]
    created_at: BTreeMap<String, DateTime<Utc>>,
    /// The active VPR window. A restart resets `last_changed`, which would move a window that
    /// crosses midnight to the next night; the stored `set_at` keeps it in place.
    #[serde(default)]
    vpr_anchor: Option<VprAnchor>,
}

impl HelperSetup {
    /// Load the saved state. A missing or unreadable file is an empty setup.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
            });
        if let Err(e) = result {
            warn!("⚠️ Could not save lever setup to {}: {}", path.display(), e);
        }
    }

    /// Entity id per slot, for the helpers known so far.
    pub fn ids(&self) -> EntityIds {
        HELPERS
            .iter()
            .filter_map(|spec| Some((spec.slot, self.helpers.get(spec.name)?.clone())))
            .collect()
    }

    /// Every helper in the table has an entity id.
    pub fn all_resolved(&self) -> bool {
        HELPERS
            .iter()
            .all(|spec| self.helpers.contains_key(spec.name))
    }

    /// Record a helper that already exists in HA.
    pub fn record_existing(&mut self, spec: &HelperSpec, entity_id: String) {
        self.helpers.insert(spec.name.to_string(), entity_id);
    }

    /// Record a helper FluxION just created: it still needs its one-time migration.
    pub fn record_created(&mut self, spec: &HelperSpec, entity_id: String, now: DateTime<Utc>) {
        self.helpers.insert(spec.name.to_string(), entity_id);
        self.unmigrated.insert(spec.name.to_string());
        self.created_at.insert(spec.name.to_string(), now);
    }

    /// Creation time of a pending helper. A helper saved without one (older file) starts its
    /// wait now, and that time is persisted.
    pub fn created_at_or_insert(&mut self, name: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        *self.created_at.entry(name.to_string()).or_insert(now)
    }

    /// Created helpers whose migration write is still pending, in table order.
    pub fn pending_migration(&self) -> Vec<HelperSpec> {
        HELPERS
            .iter()
            .filter(|spec| self.unmigrated.contains(spec.name))
            .copied()
            .collect()
    }

    pub fn mark_migrated(&mut self, name: &str) {
        self.unmigrated.remove(name);
    }

    /// Drop the ids of helpers HA has confirmed as deleted. They are recreated by the next ensure,
    /// which runs because the setup is no longer resolved. A pending migration is kept, so a helper
    /// re-resolved to the same entity id still gets its one-time copy. Returns the names dropped.
    pub fn forget_absent(&mut self, absent: &[String]) -> Vec<&'static str> {
        let mut dropped = Vec::new();
        for spec in HELPERS {
            let gone = self
                .helpers
                .get(spec.name)
                .is_some_and(|id| absent.contains(id));
            if gone {
                self.helpers.remove(spec.name);
                dropped.push(spec.name);
            }
        }
        dropped
    }

    /// Anchor the active VPR window. The same (mode, from, until) as the stored record keeps its
    /// stored `set_at`; a different window keeps the `set_at` it was given and is stored. No window
    /// clears the record. Returns true when the record changed.
    pub fn anchor_vpr(&mut self, vpr: &mut Option<VprWindow>) -> bool {
        let before = self.vpr_anchor;
        match vpr {
            None => self.vpr_anchor = None,
            Some(window) => match self.vpr_anchor {
                Some(anchor)
                    if anchor.mode == window.mode
                        && anchor.from_local == window.from_local
                        && anchor.until_local == window.until_local =>
                {
                    window.set_at = anchor.set_at;
                }
                _ => {
                    self.vpr_anchor = Some(VprAnchor {
                        mode: window.mode,
                        from_local: window.from_local,
                        until_local: window.until_local,
                        set_at: window.set_at,
                    });
                }
            },
        }
        self.vpr_anchor != before
    }

    /// The websocket ensure should run: it is possible here and some helper is still missing.
    pub fn ensure_wanted(&self, ws_available: bool) -> bool {
        ws_available && !self.all_resolved()
    }

    /// Some retry work exists (an ensure or a pending migration).
    pub fn needs_work(&self, ws_available: bool) -> bool {
        self.ensure_wanted(ws_available) || !self.unmigrated.is_empty()
    }
}

/// What a poll publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// Not yet: a safety helper has not been readable and the grace period runs.
    Wait,
    /// Every safety helper has been readable once.
    Normal,
    /// The grace period passed with a safety helper never readable: publish without it.
    Degraded,
}

pub fn publish_decision(safety_complete: bool, elapsed: Duration) -> Publish {
    if safety_complete {
        Publish::Normal
    } else if elapsed < DEGRADED_PUBLISH_AFTER {
        Publish::Wait
    } else {
        Publish::Degraded
    }
}

/// Whether the ensure/migrate retry should run now. The first attempt is immediate; later attempts
/// wait [`ENSURE_RETRY_INTERVAL`] after the previous one.
pub fn retry_due(
    setup: &HelperSetup,
    ws_available: bool,
    last_attempt: Option<Instant>,
    now: Instant,
) -> bool {
    setup.needs_work(ws_available)
        && last_attempt.is_none_or(|last| now.duration_since(last) >= ENSURE_RETRY_INTERVAL)
}

// ============================================================================
// Home Assistant I/O
// ============================================================================

/// Make sure every helper exists, recording each one as it is found or created. Stops at the first
/// error; the helpers recorded so far are kept, so the next retry resumes from there.
async fn ensure_helpers(ws: &mut HaWsSession, setup: &mut HelperSetup) -> HaResult<()> {
    let mut listed: HashMap<&'static str, Vec<Value>> = HashMap::new();

    for spec in HELPERS {
        if !listed.contains_key(spec.domain) {
            let list = ws
                .call(json!({"type": format!("{}/list", spec.domain)}))
                .await?;
            listed.insert(spec.domain, list.as_array().cloned().unwrap_or_default());
        }
        let existing = listed
            .get(spec.domain)
            .into_iter()
            .flatten()
            .find(|item| item["name"].as_str() == Some(spec.name))
            .and_then(|item| item["id"].as_str())
            .map(str::to_string);

        match existing {
            Some(id) => setup.record_existing(&spec, format!("{}.{}", spec.domain, id)),
            None => {
                let mut msg = create_payload(&spec);
                msg["type"] = json!(format!("{}/create", spec.domain));
                let result = ws.call(msg).await?;
                let id = result["id"].as_str().ok_or_else(|| {
                    HaError::InvalidResponse(format!("{}/create returned no id", spec.domain))
                })?;
                let entity_id = format!("{}.{}", spec.domain, id);
                info!("🎛️ Created helper {} → {}", spec.name, entity_id);
                if spec.slot == Slot::VprMode {
                    info!("🎛️ VPR starts as Žádný 00:00–00:00; Node-RED VPR is not migrated");
                }
                setup.record_created(&spec, entity_id, Utc::now());
            }
        }
    }
    Ok(())
}

/// What a created helper's migration does, given its Node-RED source.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceDecision<'a> {
    /// Copy this Node-RED state text.
    Value(&'a str),
    /// The source entity does not exist (fresh site): write the contract default.
    AbsentDefault,
    /// The source exists but is unavailable and the wait is still running: keep it pending.
    Wait,
    /// Still unavailable after the wait limit: write the contract default.
    TimedOutDefault,
}

/// Decide the migration source. An entity that is absent from HA is a fresh site without
/// Node-RED. A present but unavailable/unknown entity (Node-RED still starting) is waited for, up
/// to [`SOURCE_WAIT_LIMIT`] after the helper was created.
pub fn decide_source<'a>(
    source: Option<&'a HaEntityState>,
    created_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> SourceDecision<'a> {
    match source {
        None => SourceDecision::AbsentDefault,
        Some(state) if usable(state) => SourceDecision::Value(&state.state),
        Some(_) if now - created_at >= SOURCE_WAIT_LIMIT => SourceDecision::TimedOutDefault,
        Some(_) => SourceDecision::Wait,
    }
}

/// Copy the Node-RED values into created helpers that still need it. A helper stays pending until
/// its write succeeds, so the next retry tries again. Kill switch: nothing to write, done at once.
async fn migrate_pending(client: &HomeAssistantClient, setup: &mut HelperSetup) {
    let pending = setup.pending_migration();
    if pending.is_empty() {
        return;
    }
    // All states: Node-RED entities may be absent (fresh site) or unavailable (still starting).
    let states = match client.get_all_states().await {
        Ok(states) => states,
        Err(e) => {
            warn!(
                "⚠️ Could not read HA states for migration, will retry: {}",
                e
            );
            return;
        }
    };
    let now = Utc::now();

    for spec in pending {
        let Some(entity_id) = setup.ids().get(&spec.slot).cloned() else {
            continue;
        };
        let created_at = setup.created_at_or_insert(spec.name, now);
        let source = spec
            .source
            .and_then(|src| states.iter().find(|s| s.entity_id == src));
        let source_state = match decide_source(source, created_at, now) {
            SourceDecision::Value(text) => Some(text),
            SourceDecision::AbsentDefault => None,
            SourceDecision::TimedOutDefault => {
                warn!(
                    "⚠️ {} source {} still unavailable after 24 h; using the default",
                    spec.name,
                    spec.source.unwrap_or("-")
                );
                None
            }
            SourceDecision::Wait => {
                debug!(
                    "🎛️ {} waiting for {} to become available",
                    spec.name,
                    spec.source.unwrap_or("-")
                );
                continue;
            }
        };
        let Some((service, data)) = migration_call(&spec, &entity_id, source_state) else {
            setup.mark_migrated(spec.name);
            continue;
        };
        let origin = match (source_state, spec.source) {
            (Some(_), Some(src)) => src,
            _ => "default",
        };
        match client.call_service(service, data.clone()).await {
            Ok(()) => {
                if matches!(spec.slot, Slot::VprMode | Slot::VprStart | Slot::VprStop) {
                    debug!("🎛️ {} set to its default: {}", spec.name, data);
                } else {
                    info!("🎛️ Migrated {} from {}: {}", spec.name, origin, data);
                }
                setup.mark_migrated(spec.name);
            }
            Err(e) => warn!("⚠️ Migration of {} failed, will retry: {}", spec.name, e),
        }
    }
}

/// Read the helper states. Also returns the entity ids HA reports as deleted (not found). Other read
/// errors only leave that helper out of this poll.
async fn read_helper_states(
    client: &HomeAssistantClient,
    entity_ids: &[String],
) -> (Vec<HaEntityState>, Vec<String>) {
    let mut states = Vec::new();
    let mut absent = Vec::new();
    for entity_id in entity_ids {
        match client.get_state(entity_id).await {
            Ok(state) => states.push(state),
            Err(HaError::EntityNotFound(_)) => absent.push(entity_id.clone()),
            Err(e) => debug!("🎛️ Could not read {}: {}", entity_id, e),
        }
    }
    (states, absent)
}

/// Sync task: ensure helpers and migrate them (retried every few minutes until done), then poll
/// and publish with whatever ids are known.
async fn run_levers_sync(
    client: Arc<HomeAssistantClient>,
    sender: Sender<UserLevers>,
    state_path: PathBuf,
    fallback_tz: Option<Tz>,
) {
    let tz = client
        .get_timezone()
        .await
        .ok()
        .and_then(|name| name.parse::<Tz>().ok())
        .or(fallback_tz)
        .unwrap_or(chrono_tz::UTC);

    let mut setup = HelperSetup::load(&state_path);
    let ws_url = websocket_url(client.base_url());
    if ws_url.is_none() {
        warn!(
            "⚠️ HA base URL {} is not http: lever helpers cannot be created, using existing ones only",
            client.base_url()
        );
    }

    let mut last_attempt: Option<Instant> = None;
    let mut guard = VprResetGuard::default();
    let mut safety = SafetyMemory::default();
    let mut warned: HashSet<Slot> = HashSet::new();
    let mut unreadable_warned: HashSet<Slot> = HashSet::new();
    let mut absence = AbsenceCounter::default();
    let started = Instant::now();
    let mut tick = tokio::time::interval(POLL_INTERVAL);

    loop {
        tick.tick().await;
        let attempt_at = Instant::now();
        if retry_due(&setup, ws_url.is_some(), last_attempt, attempt_at) {
            last_attempt = Some(attempt_at);
            if let Some(url) = &ws_url
                && !setup.all_resolved()
            {
                match HaWsSession::connect(url, client.token()).await {
                    Ok(mut ws) => {
                        if let Err(e) = ensure_helpers(&mut ws, &mut setup).await {
                            warn!("⚠️ Lever helper setup incomplete, retrying: {}", e);
                        }
                    }
                    Err(e) => warn!("⚠️ Lever helper websocket unavailable, retrying: {}", e),
                }
            }
            migrate_pending(&client, &mut setup).await;
            setup.save(&state_path);
        }

        let ids = setup.ids();
        let all_ids: Vec<String> = ids.values().cloned().collect();
        if all_ids.is_empty() {
            continue;
        }
        let (states, absent) = read_helper_states(&client, &all_ids).await;
        let found: Vec<String> = states.iter().map(|s| s.entity_id.clone()).collect();
        let confirmed = absence.observe(&found, &absent);
        let dropped = setup.forget_absent(&confirmed);
        if !dropped.is_empty() {
            for name in &dropped {
                if ws_url.is_some() {
                    warn!(
                        "⚠️ Lever helper {} was deleted in HA; it will be recreated",
                        name
                    );
                } else {
                    warn!(
                        "⚠️ Lever helper {} was deleted in HA and cannot be recreated over https",
                        name
                    );
                }
            }
            setup.save(&state_path);
        }
        let ids = setup.ids();
        if states.is_empty() {
            // Nothing came back (HA unreachable): keep the last levers rather than defaults.
            warn!("⚠️ No lever helper states returned; keeping last values");
            continue;
        }

        let now = Utc::now();
        for spec in HELPERS {
            let missing = unavailable_helpers(&states, &ids).contains(&spec.slot);
            if missing && warned.insert(spec.slot) {
                warn!(
                    "⚠️ Lever helper {} is unavailable; using its default",
                    spec.name
                );
            } else if !missing {
                warned.remove(&spec.slot);
            }
        }

        safety.update(&states, &ids);
        let decision = publish_decision(safety.complete(), started.elapsed());
        if decision == Publish::Wait {
            // Execution waits for `available`, which needs one poll with all safety helpers readable.
            debug!("🎛️ Safety helpers not all readable yet; levers not published");
            continue;
        }
        if decision == Publish::Degraded {
            for slot in safety.missing() {
                if unreadable_warned.insert(slot) {
                    let spec = HELPERS.iter().find(|s| s.slot == slot);
                    warn!(
                        "⚠️ Safety helper {} was never readable; publishing without it ({})",
                        spec.map_or("?", |s| s.name),
                        match slot {
                            Slot::AutomationDisabled => "kill switch off",
                            _ => "no VPR",
                        }
                    );
                }
            }
        }
        let mut levers = build_user_levers(&states, &ids, now, &safety);
        // The anchor is not touched while a safety helper is missing: its absence would clear it.
        if decision == Publish::Normal && setup.anchor_vpr(&mut levers.vpr) {
            setup.save(&state_path);
        }
        if guard.reset_due(&levers, &tz, now)
            && let (Some(window), Some(select)) = (levers.vpr, ids.get(&Slot::VprMode))
        {
            let reset = client
                .call_service(
                    "input_select.select_option",
                    json!({"entity_id": select, "option": VprMode::None.option()}),
                )
                .await;
            match reset {
                Ok(()) => {
                    info!(
                        "🎛️ VPR {} window ended — reset to Žádný",
                        window.mode.option()
                    );
                    guard.mark_reset(&levers);
                }
                Err(e) => warn!("⚠️ VPR reset failed, will retry: {}", e),
            }
        }

        debug!("🎛️ Levers: {:?}", levers);
        if sender.send(levers).is_err() {
            return;
        }
    }
}

/// Startup system: spawn the sync task unless disabled or HA is not configured.
pub fn spawn_levers_sync(
    ha_client: Option<Res<HaClientResource>>,
    channel: Res<LeversChannel>,
    config: Res<LeversSyncConfig>,
    timezone: Option<Res<fluxion_core::TimezoneConfig>>,
    mut levers_res: ResMut<UserLeversResource>,
) {
    if !config.enabled {
        info!("ℹ️ Levers sync disabled in config; lever helpers not managed");
        return;
    }
    let Some(client) = ha_client else {
        warn!("⚠️ HaClientResource not available, cannot sync levers");
        return;
    };
    let client = client.0.clone();
    let sender = channel.sender.clone();
    let state_path = config.state_path.clone();
    let fallback_tz = timezone.and_then(|t| t.tz);

    // Set before the first poll, so execution waits for the levers instead of writing defaults.
    levers_res.sync_expected = true;
    tokio::spawn(run_levers_sync(client, sender, state_path, fallback_tz));
}

/// Update system: publish the newest levers into `UserLeversResource`.
pub fn poll_levers_channel(channel: Res<LeversChannel>, mut resource: ResMut<UserLeversResource>) {
    while let Ok(levers) = channel.receiver.try_recv() {
        resource.levers = levers;
        resource.available = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(entity_id: &str, value: &str, last_changed: &str) -> HaEntityState {
        HaEntityState {
            entity_id: entity_id.to_string(),
            state: value.to_string(),
            attributes: json!({}),
            last_changed: last_changed.to_string(),
            last_updated: last_changed.to_string(),
        }
    }

    /// Entity ids and states for all ten helpers, in table order.
    fn fixture(values: &[(Slot, &str, &str)]) -> (Vec<HaEntityState>, EntityIds) {
        let mut ids = EntityIds::new();
        let mut states = Vec::new();
        for (i, spec) in HELPERS.iter().enumerate() {
            let entity = format!("{}.fluxion_{}", spec.domain, i);
            ids.insert(spec.slot, entity.clone());
            let (value, at) = values
                .iter()
                .find(|(slot, _, _)| *slot == spec.slot)
                .map(|(_, v, t)| (*v, *t))
                .unwrap_or(("unavailable", "2026-10-14T10:00:00+00:00"));
            states.push(state(&entity, value, at));
        }
        (states, ids)
    }

    fn all_present() -> Vec<(Slot, &'static str, &'static str)> {
        let t = "2026-10-14T10:00:00+00:00";
        vec![
            (Slot::VprMode, "Vybíjení baterie", t),
            (Slot::VprStart, "15:00:00", t),
            (Slot::VprStop, "17:30:00", t),
            (Slot::AutomationDisabled, "on", t),
            (Slot::MorningHours, "2.0", t),
            (Slot::MorningMinSoc, "40.0", t),
            (Slot::EveningHours, "3.0", t),
            (Slot::EveningMinSoc, "50.0", t),
            (Slot::ForceChargeBelowCzk, "-2.5", t),
            (Slot::NoDischargeBelowCzk, "1.5", t),
        ]
    }

    fn now() -> DateTime<Utc> {
        "2026-10-14T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn levers_from_states_all_present() {
        let (states, ids) = fixture(&all_present());
        let levers = build_user_levers(&states, &ids, now(), &SafetyMemory::default());
        let vpr = levers.vpr.expect("vpr set");
        assert_eq!(vpr.mode, VprMode::DischargeBattery);
        assert_eq!(vpr.from_local, NaiveTime::from_hms_opt(15, 0, 0).unwrap());
        assert_eq!(vpr.until_local, NaiveTime::from_hms_opt(17, 30, 0).unwrap());
        assert!(levers.automation_disabled);
        assert_eq!(
            levers.morning_discharge,
            Some(DischargeWindowLimit {
                hours: 2,
                min_soc_pct: 40.0
            })
        );
        assert_eq!(
            levers.evening_discharge,
            Some(DischargeWindowLimit {
                hours: 3,
                min_soc_pct: 50.0
            })
        );
        assert_eq!(levers.force_charge_at_or_below_czk, Some(-2.5));
        assert_eq!(levers.no_discharge_at_or_below_czk, Some(1.5));
    }

    #[test]
    fn vpr_none_gives_no_window() {
        let mut values = all_present();
        values[0] = (Slot::VprMode, "Žádný", "2026-10-14T10:00:00+00:00");
        let (states, ids) = fixture(&values);
        assert_eq!(
            build_user_levers(&states, &ids, now(), &SafetyMemory::default()).vpr,
            None
        );
    }

    #[test]
    fn price_off_sentinel_is_none() {
        let mut values = all_present();
        values[8] = (
            Slot::ForceChargeBelowCzk,
            "-10.0",
            "2026-10-14T10:00:00+00:00",
        );
        let (states, ids) = fixture(&values);
        assert_eq!(
            build_user_levers(&states, &ids, now(), &SafetyMemory::default())
                .force_charge_at_or_below_czk,
            None
        );
    }

    #[test]
    fn unavailable_hours_give_no_discharge_limit() {
        let mut values = all_present();
        values[4] = (
            Slot::MorningHours,
            "unavailable",
            "2026-10-14T10:00:00+00:00",
        );
        let (states, ids) = fixture(&values);
        let levers = build_user_levers(&states, &ids, now(), &SafetyMemory::default());
        assert_eq!(levers.morning_discharge, None);
        assert!(levers.evening_discharge.is_some());
        assert_eq!(unavailable_helpers(&states, &ids), vec![Slot::MorningHours]);
    }

    #[test]
    fn kill_switch_off_when_not_on() {
        let mut values = all_present();
        values[3] = (Slot::AutomationDisabled, "off", "2026-10-14T10:00:00+00:00");
        let (states, ids) = fixture(&values);
        assert!(
            !build_user_levers(&states, &ids, now(), &SafetyMemory::default()).automation_disabled
        );
    }

    #[test]
    fn set_at_is_latest_last_changed() {
        let mut values = all_present();
        values[0] = (
            Slot::VprMode,
            "Vybíjení baterie",
            "2026-10-14T08:00:00+00:00",
        );
        values[1] = (Slot::VprStart, "15:00:00", "2026-10-14T09:30:00+00:00");
        values[2] = (Slot::VprStop, "17:30:00", "2026-10-14T09:00:00+00:00");
        let (states, ids) = fixture(&values);
        let vpr = build_user_levers(&states, &ids, now(), &SafetyMemory::default())
            .vpr
            .unwrap();
        assert_eq!(
            vpr.set_at,
            "2026-10-14T09:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn text_price_with_trailing_dot_parses() {
        assert_eq!(parse_number("-1."), Some(-1.0));
        assert_eq!(parse_number(" 1 "), Some(1.0));
        assert_eq!(parse_number("unknown"), None);
        assert_eq!(parse_number("NaN"), None);
    }

    #[test]
    fn migration_clamps_hours_to_helper_range() {
        assert_eq!(clamp_to_helper(9.0, 0.0, 6.0, 1.0), 6.0);
        assert_eq!(clamp_to_helper(-3.0, 0.0, 6.0, 1.0), 0.0);
        let spec = HELPERS[4];
        let (service, data) = migration_call(&spec, "input_number.fluxion_4", Some("9.0")).unwrap();
        assert_eq!(service, "input_number.set_value");
        assert_eq!(data["value"], json!(6.0));
    }

    #[test]
    fn migration_uses_default_when_source_missing() {
        let spec = HELPERS[6];
        let (_, data) = migration_call(&spec, "input_number.x", None).unwrap();
        assert_eq!(data["value"], json!(2.0));
        let spec = HELPERS[8];
        let (_, data) = migration_call(&spec, "input_number.x", Some("unavailable")).unwrap();
        assert_eq!(data["value"], json!(-10.0));
    }

    #[test]
    fn migration_never_writes_kill_switch() {
        assert!(migration_call(&HELPERS[3], "input_boolean.x", Some("on")).is_none());
        assert_eq!(HELPERS[3].source, None);
    }

    #[test]
    fn migration_time_and_select_values() {
        let (service, data) =
            migration_call(&HELPERS[1], "input_datetime.x", Some("15:00")).unwrap();
        assert_eq!(service, "input_datetime.set_datetime");
        assert_eq!(data["time"], json!("15:00:00"));
        let (service, data) =
            migration_call(&HELPERS[1], "input_datetime.x", Some("bogus")).unwrap();
        assert_eq!(service, "input_datetime.set_datetime");
        assert_eq!(data["time"], json!("00:00:00"));
        let (_, data) = migration_call(&HELPERS[0], "input_select.x", Some("bogus")).unwrap();
        assert_eq!(data["option"], json!("Žádný"));
    }

    #[test]
    fn vpr_helpers_are_not_migrated_from_node_red() {
        // Node-RED's VPR is a one-time instruction: the VPR helpers start as Žádný 00:00/00:00.
        for slot in [Slot::VprMode, Slot::VprStart, Slot::VprStop] {
            let spec = HELPERS.iter().find(|s| s.slot == slot).unwrap();
            assert_eq!(spec.source, None, "{:?}", slot);
        }
        let (service, data) = migration_call(&HELPERS[0], "input_select.x", None).unwrap();
        assert_eq!(service, "input_select.select_option");
        assert_eq!(data["option"], json!("Žádný"));
        let (service, data) = migration_call(&HELPERS[2], "input_datetime.x", None).unwrap();
        assert_eq!(service, "input_datetime.set_datetime");
        assert_eq!(data["time"], json!("00:00:00"));
    }

    #[test]
    fn kill_switch_kept_when_helper_absent() {
        let mut memory = SafetyMemory::default();
        let (states, ids) = fixture(&all_present());
        memory.update(&states, &ids);
        assert!(build_user_levers(&states, &ids, now(), &memory).automation_disabled);

        // Next poll: the kill switch is missing (no id, unavailable) and everything else is too.
        let mut values = all_present();
        values[3] = (
            Slot::AutomationDisabled,
            "unavailable",
            "2026-10-14T10:00:00+00:00",
        );
        let (states, ids) = fixture(&values);
        memory.update(&states, &ids);
        assert!(memory.complete());
        assert!(build_user_levers(&states, &ids, now(), &memory).automation_disabled);
        // Ordinary levers keep the current rule: unavailable hours give no limit.
        let mut values = all_present();
        values[4] = (
            Slot::MorningHours,
            "unavailable",
            "2026-10-14T10:00:00+00:00",
        );
        let (states, ids) = fixture(&values);
        assert_eq!(
            build_user_levers(&states, &ids, now(), &memory).morning_discharge,
            None
        );
    }

    #[test]
    fn safety_never_readable_is_not_complete() {
        let mut memory = SafetyMemory::default();
        let mut values = all_present();
        values[3] = (
            Slot::AutomationDisabled,
            "unavailable",
            "2026-10-14T10:00:00+00:00",
        );
        let (states, ids) = fixture(&values);
        memory.update(&states, &ids);
        assert!(!memory.complete());
        // Readable once, then unavailable: the state is kept and the poll counts as complete.
        let (states, ids) = fixture(&all_present());
        memory.update(&states, &ids);
        assert!(memory.complete());
    }

    #[test]
    fn vpr_anchor_survives_restart() {
        let tz = chrono_tz::Europe::Prague;
        let window = |set_at: &str| VprWindow {
            mode: VprMode::DischargeBattery,
            from_local: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            until_local: NaiveTime::from_hms_opt(2, 0, 0).unwrap(),
            set_at: set_at.parse().unwrap(),
        };
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let mut setup = HelperSetup::default();

        // Set at 15:00 CEST on 14 Oct.
        let mut vpr = Some(window("2026-10-14T13:00:00Z"));
        assert!(setup.anchor_vpr(&mut vpr));
        assert_eq!(vpr.unwrap().set_at, at("2026-10-14T13:00:00Z"));

        // Restart at 01:00 CEST on 15 Oct: HA reports last_changed 01:00, which is re-anchored
        // without the stored record (Pending at 01:00); with it the window is still Active.
        let mut vpr = Some(window("2026-10-14T23:00:00Z"));
        assert!(!setup.anchor_vpr(&mut vpr));
        let anchored = vpr.unwrap();
        assert_eq!(anchored.set_at, at("2026-10-14T13:00:00Z"));
        assert_eq!(
            window("2026-10-14T23:00:00Z").phase(at("2026-10-14T23:00:00Z"), &tz),
            VprPhase::Pending
        );
        assert_eq!(
            anchored.phase(at("2026-10-14T23:00:00Z"), &tz),
            VprPhase::Active
        );
        assert_eq!(
            anchored.phase(at("2026-10-15T00:00:00Z"), &tz),
            VprPhase::Expired
        );

        // The record survives a save and load.
        let dir = std::env::temp_dir().join(format!("fluxion-anchor-test-{}", std::process::id()));
        let path = dir.join("fluxion_levers.json");
        setup.save(&path);
        let mut setup = HelperSetup::load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        let mut vpr = Some(window("2026-10-15T23:00:00Z"));
        assert!(!setup.anchor_vpr(&mut vpr));
        assert_eq!(vpr.unwrap().set_at, at("2026-10-14T13:00:00Z"));

        // A different window takes its own set_at and replaces the record.
        let mut vpr = Some(VprWindow {
            mode: VprMode::NoDischarge,
            ..window("2026-10-15T12:00:00Z")
        });
        assert!(setup.anchor_vpr(&mut vpr));
        assert_eq!(vpr.unwrap().set_at, at("2026-10-15T12:00:00Z"));

        // "Žádný" clears the record.
        let mut none = None;
        assert!(setup.anchor_vpr(&mut none));
        assert!(!setup.anchor_vpr(&mut none));
    }

    #[test]
    fn create_payload_shapes() {
        let select = create_payload(&HELPERS[0]);
        assert_eq!(select["options"], json!(VprMode::OPTIONS));
        let price = create_payload(&HELPERS[8]);
        assert_eq!(price["mode"], json!("box"));
        assert_eq!(price["unit_of_measurement"], json!("CZK/kWh"));
        assert_eq!(price["step"], json!(0.01));
        let time = create_payload(&HELPERS[1]);
        assert_eq!(time["has_time"], json!(true));
        assert_eq!(time["has_date"], json!(false));
    }

    #[test]
    fn vpr_reset_sent_once_per_window() {
        let tz = chrono_tz::Europe::Prague;
        let (states, ids) = fixture(&[
            (
                Slot::VprMode,
                "Vybíjení baterie",
                "2026-10-14T12:59:00+00:00",
            ),
            (Slot::VprStart, "15:00:00", "2026-10-14T12:59:00+00:00"),
            (Slot::VprStop, "17:30:00", "2026-10-14T12:59:00+00:00"),
        ]);
        let after_end: DateTime<Utc> = "2026-10-14T15:31:00Z".parse().unwrap();
        let levers = build_user_levers(&states, &ids, after_end, &SafetyMemory::default());
        let mut guard = VprResetGuard::default();
        assert!(guard.reset_due(&levers, &tz, after_end));
        guard.mark_reset(&levers);
        assert!(!guard.reset_due(&levers, &tz, after_end));

        // A new window (different set_at) is reset again when it ends.
        let (states, ids) = fixture(&[
            (
                Slot::VprMode,
                "Vybíjení baterie",
                "2026-10-15T12:59:00+00:00",
            ),
            (Slot::VprStart, "15:00:00", "2026-10-15T12:59:00+00:00"),
            (Slot::VprStop, "17:30:00", "2026-10-15T12:59:00+00:00"),
        ]);
        let next_end: DateTime<Utc> = "2026-10-15T15:31:00Z".parse().unwrap();
        let levers = build_user_levers(&states, &ids, next_end, &SafetyMemory::default());
        assert!(guard.reset_due(&levers, &tz, next_end));
    }

    #[test]
    fn vpr_reset_not_due_while_active_or_none() {
        let tz = chrono_tz::Europe::Prague;
        let (states, ids) = fixture(&[
            (
                Slot::VprMode,
                "Vybíjení baterie",
                "2026-10-14T12:59:00+00:00",
            ),
            (Slot::VprStart, "15:00:00", "2026-10-14T12:59:00+00:00"),
            (Slot::VprStop, "17:30:00", "2026-10-14T12:59:00+00:00"),
        ]);
        let during: DateTime<Utc> = "2026-10-14T13:30:00Z".parse().unwrap();
        let levers = build_user_levers(&states, &ids, during, &SafetyMemory::default());
        assert!(!VprResetGuard::default().reset_due(&levers, &tz, during));
        assert!(!VprResetGuard::default().reset_due(&UserLevers::default(), &tz, during));
    }

    #[test]
    fn setup_round_trip_keeps_pending_migrations() {
        let dir = std::env::temp_dir().join(format!("fluxion-levers-test-{}", std::process::id()));
        let path = dir.join("fluxion_levers.json");
        let mut setup = HelperSetup::default();
        setup.record_existing(&HELPERS[0], "input_select.a".to_string());
        setup.record_created(&HELPERS[1], "input_datetime.b".to_string(), Utc::now());
        setup.save(&path);
        let loaded = HelperSetup::load(&path);
        assert_eq!(loaded, setup);
        assert_eq!(loaded.pending_migration().len(), 1);
        assert_eq!(
            loaded.ids().get(&Slot::VprStart).map(String::as_str),
            Some("input_datetime.b")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_resolves_only_when_all_ten_known() {
        let mut setup = HelperSetup::default();
        for (i, spec) in HELPERS.iter().enumerate().take(9) {
            setup.record_existing(spec, format!("x.{i}"));
        }
        assert!(!setup.all_resolved());
        assert!(setup.ensure_wanted(true));
        setup.record_existing(&HELPERS[9], "x.last".to_string());
        assert!(setup.all_resolved());
        assert!(!setup.ensure_wanted(true));
    }

    #[test]
    fn migration_done_clears_pending() {
        let mut setup = HelperSetup::default();
        setup.record_created(&HELPERS[3], "input_boolean.kill".to_string(), Utc::now());
        setup.record_created(&HELPERS[4], "input_number.h".to_string(), Utc::now());
        assert_eq!(setup.pending_migration().len(), 2);
        setup.mark_migrated(HELPERS[3].name);
        assert_eq!(setup.pending_migration().len(), 1);
        setup.mark_migrated(HELPERS[4].name);
        assert!(setup.pending_migration().is_empty());
        assert!(!setup.needs_work(false));
    }

    #[test]
    fn retry_schedule_and_offline_behaviour() {
        let base = Instant::now();
        let fresh = HelperSetup::default();
        let mut partial = HelperSetup::default();
        partial.record_existing(&HELPERS[0], "input_select.a".to_string());
        let mut pending = HelperSetup::default();
        pending.record_created(&HELPERS[4], "input_number.h".to_string(), Utc::now());

        // First attempt is immediate when there is work.
        assert!(retry_due(&partial, true, None, base));
        // Nothing to do (all resolved, nothing pending): never due.
        let mut complete = HelperSetup::default();
        for (i, spec) in HELPERS.iter().enumerate() {
            complete.record_existing(spec, format!("x.{i}"));
        }
        assert!(!retry_due(&complete, true, None, base));
        // Missing helpers but no websocket (https): ensure is not possible, so not due.
        assert!(!retry_due(&partial, false, None, base));
        // Pending migration is retried even without a websocket.
        assert!(retry_due(&pending, false, None, base));
        assert!(!retry_due(&fresh, false, None, base));

        // Five minutes between attempts.
        assert!(!retry_due(
            &partial,
            true,
            Some(base),
            base + Duration::from_secs(60)
        ));
        assert!(!retry_due(
            &partial,
            true,
            Some(base),
            base + Duration::from_secs(299)
        ));
        assert!(retry_due(
            &partial,
            true,
            Some(base),
            base + ENSURE_RETRY_INTERVAL
        ));
        assert!(retry_due(
            &pending,
            false,
            Some(base),
            base + ENSURE_RETRY_INTERVAL
        ));
    }

    #[test]
    fn source_absent_means_fresh_site_default() {
        let now: DateTime<Utc> = "2026-10-14T10:00:00Z".parse().unwrap();
        assert_eq!(decide_source(None, now, now), SourceDecision::AbsentDefault);
    }

    #[test]
    fn source_usable_is_copied() {
        let now: DateTime<Utc> = "2026-10-14T10:00:00Z".parse().unwrap();
        let src = state("number.x", "60.0", "2026-10-14T09:00:00+00:00");
        assert_eq!(
            decide_source(Some(&src), now, now),
            SourceDecision::Value("60.0")
        );
    }

    #[test]
    fn source_unavailable_waits_then_defaults_after_24h() {
        let created: DateTime<Utc> = "2026-10-14T10:00:00Z".parse().unwrap();
        let unavailable = state("number.x", "unavailable", "2026-10-14T09:00:00+00:00");
        let unknown = state("number.x", "unknown", "2026-10-14T09:00:00+00:00");
        let within = created + TimeDelta::hours(23) + TimeDelta::minutes(59);
        let at_limit = created + TimeDelta::hours(24);
        for src in [&unavailable, &unknown] {
            assert_eq!(
                decide_source(Some(src), created, within),
                SourceDecision::Wait
            );
            assert_eq!(
                decide_source(Some(src), created, at_limit),
                SourceDecision::TimedOutDefault
            );
        }
    }

    #[test]
    fn deleted_helper_id_is_dropped_for_recreate() {
        let mut setup = HelperSetup::default();
        for (i, spec) in HELPERS.iter().enumerate() {
            setup.record_existing(spec, format!("{}.h{}", spec.domain, i));
        }
        assert!(setup.all_resolved());
        // HA reports the kill switch helper as deleted; the others are still there.
        let absent = vec![format!("{}.h{}", HELPERS[3].domain, 3)];
        assert_eq!(setup.forget_absent(&absent), vec![HELPERS[3].name]);
        assert!(!setup.all_resolved());
        assert!(!setup.ids().contains_key(&Slot::AutomationDisabled));
        // The next ensure runs (websocket available) and recreates it.
        assert!(setup.ensure_wanted(true));
        assert!(retry_due(&setup, true, None, Instant::now()));
        // Nothing dropped when nothing is absent.
        assert!(setup.forget_absent(&[]).is_empty());
    }

    #[test]
    fn absence_confirmed_only_after_three_misses_in_a_row() {
        let id = "input_number.h".to_string();
        let mut counter = AbsenceCounter::default();
        assert!(counter.observe(&[], std::slice::from_ref(&id)).is_empty());
        assert!(counter.observe(&[], std::slice::from_ref(&id)).is_empty());
        assert_eq!(
            counter.observe(&[], std::slice::from_ref(&id)),
            vec![id.clone()]
        );

        // A successful read in between resets the count.
        let mut counter = AbsenceCounter::default();
        counter.observe(&[], std::slice::from_ref(&id));
        counter.observe(&[], std::slice::from_ref(&id));
        assert!(counter.observe(std::slice::from_ref(&id), &[]).is_empty());
        assert!(counter.observe(&[], std::slice::from_ref(&id)).is_empty());
        assert!(counter.observe(&[], std::slice::from_ref(&id)).is_empty());
        assert_eq!(counter.observe(&[], std::slice::from_ref(&id)), vec![id]);
    }

    #[test]
    fn startup_404s_do_not_drop_helpers_or_pending_migration() {
        // HA 404s every helper for two polls while its domains load, then answers again.
        let mut setup = HelperSetup::default();
        setup.record_created(&HELPERS[4], "input_number.h".to_string(), Utc::now());
        let mut counter = AbsenceCounter::default();
        let absent = vec!["input_number.h".to_string()];
        for _ in 0..2 {
            let confirmed = counter.observe(&[], &absent);
            assert!(setup.forget_absent(&confirmed).is_empty());
        }
        let confirmed = counter.observe(&["input_number.h".to_string()], &[]);
        assert!(setup.forget_absent(&confirmed).is_empty());
        assert_eq!(
            setup.ids().get(&Slot::MorningHours).map(String::as_str),
            Some("input_number.h")
        );
        assert_eq!(setup.pending_migration().len(), 1);
    }

    #[test]
    fn forgotten_helper_keeps_pending_migration_when_re_resolved_to_same_id() {
        let mut setup = HelperSetup::default();
        setup.record_created(&HELPERS[4], "input_number.h".to_string(), Utc::now());
        assert_eq!(
            setup.forget_absent(&["input_number.h".to_string()]),
            vec![HELPERS[4].name]
        );
        assert!(!setup.ids().contains_key(&Slot::MorningHours));
        assert_eq!(setup.pending_migration().len(), 1);
        // Re-resolved to the same entity id: the one-time copy is still owed.
        setup.record_existing(&HELPERS[4], "input_number.h".to_string());
        assert_eq!(setup.pending_migration().len(), 1);
        assert_eq!(
            setup.ids().get(&Slot::MorningHours).map(String::as_str),
            Some("input_number.h")
        );
    }

    #[test]
    fn publish_waits_then_degrades_after_grace() {
        assert_eq!(publish_decision(true, Duration::ZERO), Publish::Normal);
        assert_eq!(
            publish_decision(true, Duration::from_secs(500)),
            Publish::Normal
        );
        assert_eq!(
            publish_decision(false, Duration::from_secs(119)),
            Publish::Wait
        );
        assert_eq!(
            publish_decision(false, Duration::from_secs(120)),
            Publish::Degraded
        );
    }

    #[test]
    fn never_readable_kill_switch_is_off_and_vpr_start_gives_no_vpr() {
        let mut values = all_present();
        values[3] = (
            Slot::AutomationDisabled,
            "unavailable",
            "2026-10-14T10:00:00+00:00",
        );
        values[1] = (Slot::VprStart, "unavailable", "2026-10-14T10:00:00+00:00");
        let (states, ids) = fixture(&values);
        let mut memory = SafetyMemory::default();
        memory.update(&states, &ids);
        assert_eq!(
            memory.missing(),
            vec![Slot::AutomationDisabled, Slot::VprStart]
        );
        assert!(!memory.complete());
        let levers = build_user_levers(&states, &ids, now(), &memory);
        assert!(!levers.automation_disabled);
        assert_eq!(levers.vpr, None);
        // Other levers are unaffected.
        assert!(levers.evening_discharge.is_some());
    }

    #[test]
    fn readable_before_keeps_last_value_when_missing() {
        // Kill switch and VPR were readable earlier; now their helpers are gone.
        let mut memory = SafetyMemory::default();
        let (states, ids) = fixture(&all_present());
        memory.update(&states, &ids);
        let mut values = all_present();
        for slot in [Slot::AutomationDisabled, Slot::VprStart] {
            let i = values.iter().position(|(s, _, _)| *s == slot).unwrap();
            values[i] = (slot, "unavailable", "2026-10-14T10:00:00+00:00");
        }
        let (states, ids) = fixture(&values);
        memory.update(&states, &ids);
        assert!(memory.missing().is_empty());
        let levers = build_user_levers(&states, &ids, now(), &memory);
        assert!(levers.automation_disabled);
        assert!(levers.vpr.is_some());
    }
}
