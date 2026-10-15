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

//! HotStandBy guard for SolaX inverters (brief H11).
//!
//! With HotStandBy off the inverter goes offline at night and a night Force Charge does nothing.
//! The entity is resolved once at startup, then checked at startup and every 5 minutes. A
//! disabled setting is turned on at most once per hour, with a WARN and a notification.

use anyhow::Result;
use bevy_ecs::prelude::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::ha::actuation::lock_entity;
use crate::ha::client::HomeAssistantClient;
use crate::ha::errors::HaResult;
use crate::ha::plugin::{HaClientResource, HaConfig};
use crate::ha::ws::{HaWsSession, websocket_url};
use fluxion_core::DebugModeConfig;
use fluxion_core::SystemConfig;

/// Stable notification id for the HotStandBy warning
pub const HOT_STANDBY_NOTIFICATION_ID: &str = "fluxion_hot_standby";
/// Check interval (startup check runs in addition)
pub const HOT_STANDBY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// At most one enable per this interval
pub const HOT_STANDBY_ENABLE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// HA entity domain of the HotStandBy setting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotStandbyKind {
    Switch,
    Select,
}

/// Resolved HotStandBy entity
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotStandbyTarget {
    pub entity_id: String,
    pub kind: HotStandbyKind,
}

/// One entry of the entity registry (only the fields used here)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub entity_id: String,
    pub platform: String,
    pub unique_id: String,
    pub config_entry_id: Option<String>,
}

fn is_unusable(state: &str) -> bool {
    matches!(state, "unavailable" | "unknown")
}

/// Kind from the entity domain; other domains are not HotStandBy entities
#[must_use]
pub fn kind_of(entity_id: &str) -> Option<HotStandbyKind> {
    if entity_id.starts_with("switch.") {
        Some(HotStandbyKind::Switch)
    } else if entity_id.starts_with("select.") {
        Some(HotStandbyKind::Select)
    } else {
        None
    }
}

/// Whether HotStandBy must be turned on. Unavailable or unknown states are left alone.
#[must_use]
pub fn needs_enable(kind: HotStandbyKind, state: &str) -> bool {
    if is_unusable(state) {
        return false;
    }
    match kind {
        HotStandbyKind::Switch => state == "off",
        HotStandbyKind::Select => state != "Enabled",
    }
}

/// Service call that turns HotStandBy on
#[must_use]
pub fn enable_call(target: &HotStandbyTarget) -> (&'static str, Value) {
    match target.kind {
        HotStandbyKind::Switch => ("switch.turn_on", json!({ "entity_id": target.entity_id })),
        HotStandbyKind::Select => (
            "select.select_option",
            json!({ "entity_id": target.entity_id, "option": "Enabled" }),
        ),
    }
}

/// Whether an enable may happen now, given the last enable
#[must_use]
pub fn enable_due(last_enable: Option<Instant>, now: Instant) -> bool {
    last_enable.is_none_or(|t| now.duration_since(t) >= HOT_STANDBY_ENABLE_INTERVAL)
}

/// Parse `config/entity_registry/list` results
#[must_use]
pub fn parse_registry(value: &Value) -> Vec<RegistryEntry> {
    value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    Some(RegistryEntry {
                        entity_id: e["entity_id"].as_str()?.to_string(),
                        platform: e["platform"].as_str().unwrap_or_default().to_string(),
                        unique_id: e["unique_id"].as_str().unwrap_or_default().to_string(),
                        config_entry_id: e["config_entry_id"].as_str().map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Entity ids of SolaX HotStandBy registry entries (platform `solax_modbus`, unique_id `*_hotstandby`)
/// in the same config entry as the lock entity. Without a registry entry for the lock entity there
/// are none, so an entry of another config entry (another inverter) is never picked.
#[must_use]
pub fn registry_candidates(entries: &[RegistryEntry], lock_entity: &str) -> Vec<String> {
    let Some(config_entry) = entries
        .iter()
        .find(|e| e.entity_id == lock_entity)
        .and_then(|e| e.config_entry_id.as_deref())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|e| {
            e.platform == "solax_modbus"
                && e.unique_id.ends_with("_hotstandby")
                && e.config_entry_id.as_deref() == Some(config_entry)
        })
        .map(|e| e.entity_id.clone())
        .collect()
}

/// Pick the registry candidate from `(entity_id, state)` pairs: a usable switch is preferred,
/// otherwise a usable select.
#[must_use]
pub fn choose_registry_candidate(candidates: &[(String, String)]) -> Option<HotStandbyTarget> {
    let usable = |(_, state): &&(String, String)| !is_unusable(state);
    let pick = |kind: HotStandbyKind| {
        candidates
            .iter()
            .filter(usable)
            .find(|(entity, _)| kind_of(entity) == Some(kind))
            .map(|(entity, _)| HotStandbyTarget {
                entity_id: entity.clone(),
                kind,
            })
    };
    pick(HotStandbyKind::Switch).or_else(|| pick(HotStandbyKind::Select))
}

/// Read the entity registry over the websocket (reuses the levers' session code)
pub async fn fetch_registry(url: &str, token: &str) -> HaResult<Vec<RegistryEntry>> {
    let mut ws = HaWsSession::connect(url, token).await?;
    let result = ws
        .call(json!({ "type": "config/entity_registry/list" }))
        .await?;
    Ok(parse_registry(&result))
}

/// Current state of an entity, or `None` when it is missing or unreadable
async fn state_of(client: &HomeAssistantClient, entity_id: &str) -> Option<String> {
    match client.get_state_optional(entity_id).await {
        Ok(state) => state.map(|s| s.state),
        Err(e) => {
            debug!("HotStandBy candidate {} unreadable: {}", entity_id, e);
            None
        }
    }
}

/// Resolve the HotStandBy entity of one inverter. Order: the configured entity, the usable
/// `switch.<prefix>_hotstandby`, the usable `select.<prefix>_hotstandby`, then the registry
/// restricted to the lock entity's config entry. `lookup` returns the state of an entity that
/// exists; `registry` is `None` when the websocket is unavailable.
pub async fn resolve_hot_standby_with<F, Fut>(
    lookup: F,
    prefix: &str,
    configured: Option<&str>,
    registry: Option<Vec<RegistryEntry>>,
) -> Option<HotStandbyTarget>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    if let Some(entity) = configured.map(str::trim).filter(|e| !e.is_empty()) {
        if let Some(kind) = kind_of(entity)
            && lookup(entity.to_string()).await.is_some()
        {
            return Some(HotStandbyTarget {
                entity_id: entity.to_string(),
                kind,
            });
        }
        warn!(
            "⚠️ Configured control.hot_standby_entity {} not found",
            entity
        );
    }

    for (entity, kind) in [
        (
            format!("switch.{prefix}_hotstandby"),
            HotStandbyKind::Switch,
        ),
        (
            format!("select.{prefix}_hotstandby"),
            HotStandbyKind::Select,
        ),
    ] {
        if let Some(state) = lookup(entity.clone()).await
            && !is_unusable(&state)
        {
            return Some(HotStandbyTarget {
                entity_id: entity,
                kind,
            });
        }
    }

    let entries = registry?;
    let mut with_state = Vec::new();
    for entity in registry_candidates(&entries, &lock_entity(prefix)) {
        if let Some(state) = lookup(entity.clone()).await {
            with_state.push((entity, state));
        }
    }
    choose_registry_candidate(&with_state)
}

/// Resolve the HotStandBy entity of one inverter, reading the registry over the websocket
pub async fn resolve_hot_standby(
    client: &HomeAssistantClient,
    prefix: &str,
    configured: Option<&str>,
) -> Option<HotStandbyTarget> {
    let registry = match websocket_url(client.base_url()) {
        Some(url) => match fetch_registry(&url, client.token()).await {
            Ok(entries) => Some(entries),
            Err(e) => {
                debug!("entity registry unavailable: {}", e);
                None
            }
        },
        None => None,
    };
    let lookup = move |entity: String| async move { state_of(client, &entity).await };
    resolve_hot_standby_with(lookup, prefix, configured, registry).await
}

/// Enable HotStandBy when it is off. Debug mode logs only.
async fn check_once(
    client: &HomeAssistantClient,
    target: &HotStandbyTarget,
    last_enable: &Mutex<Option<Instant>>,
    debug: bool,
) -> Result<()> {
    let state = client.get_state(&target.entity_id).await?.state;
    if !needs_enable(target.kind, &state) {
        return Ok(());
    }
    if debug {
        info!(
            "would enable HotStandBy {} (state '{}'); debug mode, no change",
            target.entity_id, state
        );
        return Ok(());
    }
    {
        let mut last = last_enable.lock();
        if !enable_due(*last, Instant::now()) {
            debug!(
                "HotStandBy {} still off; enabled within the hour",
                target.entity_id
            );
            return Ok(());
        }
        *last = Some(Instant::now());
    }

    warn!(
        "⚠️ HotStandBy {} is '{}'; turning it on so the inverter stays online at night",
        target.entity_id, state
    );
    let (service, data) = enable_call(target);
    client.call_service(service, data).await?;
    client
        .call_service(
            "persistent_notification.create",
            json!({
                "title": "FluxION: HotStandBy střídače byl zapnut",
                "message": format!("{} byl vypnutý; FluxION jej zapnul.", target.entity_id),
                "notification_id": HOT_STANDBY_NOTIFICATION_ID,
            }),
        )
        .await?;
    Ok(())
}

/// Channel that carries each inverter's startup resolution to the update system
#[derive(Resource)]
pub struct HotStandbyChannel {
    pub tx: crossbeam_channel::Sender<(String, Option<HotStandbyTarget>)>,
    pub rx: crossbeam_channel::Receiver<(String, Option<HotStandbyTarget>)>,
}

impl Default for HotStandbyChannel {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx }
    }
}

/// A resolved HotStandBy entity with its own enable throttle
struct Tracked {
    target: HotStandbyTarget,
    last_enable: Arc<Mutex<Option<Instant>>>,
}

/// Resolved HotStandBy entities keyed by inverter id
#[derive(Resource, Default)]
pub struct HotStandbyState {
    targets: HashMap<String, Tracked>,
    last_check: Option<Instant>,
}

/// PostStartup: resolve the HotStandBy entity of every configured inverter in the background and
/// log once per inverter if it is missing
pub fn spawn_hot_standby_resolver(
    channel: Res<HotStandbyChannel>,
    ha_client: Option<Res<HaClientResource>>,
    ha_config: Res<HaConfig>,
    system_config: Option<Res<SystemConfig>>,
) {
    let Some(client) = ha_client else {
        return;
    };
    let Some(config) = system_config else {
        return;
    };
    if !config.control_config.ensure_hot_standby {
        info!("HotStandBy guard disabled (control.ensure_hot_standby: false)");
        return;
    }
    let tx = channel.tx.clone();
    // The configured entity is one entity, so it applies to the first inverter only
    let configured = config.control_config.hot_standby_entity.clone();
    let mut inverters: Vec<String> = config.inverters.iter().map(|inv| inv.id.clone()).collect();
    if inverters.is_empty() {
        inverters.push(ha_config.inverter_prefix.clone());
    }
    let client = client.0.clone();
    tokio::spawn(async move {
        for (index, inverter) in inverters.into_iter().enumerate() {
            let configured = if index == 0 {
                configured.as_deref()
            } else {
                None
            };
            let target = resolve_hot_standby(&client, &inverter, configured).await;
            match &target {
                Some(t) => info!(
                    "HotStandBy entity resolved for {}: {}",
                    inverter, t.entity_id
                ),
                None => warn!(
                    "⚠️ HotStandBy entity not found for {} (set control.hot_standby_entity); guard inactive",
                    inverter
                ),
            }
            let _ = tx.send((inverter, target));
        }
    });
}

/// Update: store the resolved targets and check them at startup and every 5 minutes
pub fn hot_standby_system(
    mut state: ResMut<HotStandbyState>,
    channel: Res<HotStandbyChannel>,
    ha_client: Option<Res<HaClientResource>>,
    system_config: Option<Res<SystemConfig>>,
    debug_mode: Option<Res<DebugModeConfig>>,
) {
    while let Ok((inverter, target)) = channel.rx.try_recv() {
        match target {
            Some(target) => {
                state.targets.insert(
                    inverter,
                    Tracked {
                        target,
                        last_enable: Arc::new(Mutex::new(None)),
                    },
                );
            }
            None => {
                state.targets.remove(&inverter);
            }
        }
    }
    if state.targets.is_empty() {
        return;
    }
    let Some(client) = ha_client else {
        return;
    };
    if !system_config
        .as_ref()
        .is_some_and(|c| c.control_config.ensure_hot_standby)
    {
        return;
    }
    let now = Instant::now();
    if state
        .last_check
        .is_some_and(|t| now.duration_since(t) < HOT_STANDBY_CHECK_INTERVAL)
    {
        return;
    }
    state.last_check = Some(now);
    // Runtime debug state; without it, behave as debug and change nothing
    let debug = debug_mode.as_ref().is_none_or(|d| d.enabled);
    let checks: Vec<(HotStandbyTarget, Arc<Mutex<Option<Instant>>>)> = state
        .targets
        .values()
        .map(|t| (t.target.clone(), t.last_enable.clone()))
        .collect();
    let client = client.0.clone();
    tokio::spawn(async move {
        for (target, last_enable) in checks {
            if let Err(e) = check_once(&client, &target, &last_enable, debug).await {
                warn!("⚠️ HotStandBy check failed for {}: {}", target.entity_id, e);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    fn target(entity: &str) -> HotStandbyTarget {
        HotStandbyTarget {
            entity_id: entity.to_string(),
            kind: kind_of(entity).unwrap(),
        }
    }

    #[test]
    fn switch_off_needs_enable_select_not_enabled_needs_enable() {
        assert!(needs_enable(HotStandbyKind::Switch, "off"));
        assert!(!needs_enable(HotStandbyKind::Switch, "on"));
        assert!(needs_enable(HotStandbyKind::Select, "Disabled"));
        assert!(!needs_enable(HotStandbyKind::Select, "Enabled"));
    }

    #[test]
    fn unavailable_is_left_alone() {
        assert!(!needs_enable(HotStandbyKind::Switch, "unavailable"));
        assert!(!needs_enable(HotStandbyKind::Select, "unavailable"));
        assert!(!needs_enable(HotStandbyKind::Select, "unknown"));
    }

    #[test]
    fn enable_calls_match_entity_kind() {
        let (service, data) = enable_call(&target("switch.solax_inverter_hotstandby"));
        assert_eq!(service, "switch.turn_on");
        assert_eq!(data["entity_id"], "switch.solax_inverter_hotstandby");
        let (service, data) = enable_call(&target("select.solax_hotstandby"));
        assert_eq!(service, "select.select_option");
        assert_eq!(data["option"], "Enabled");
    }

    #[test]
    fn enable_at_most_once_per_hour() {
        let t0 = Instant::now();
        assert!(enable_due(None, t0));
        assert!(!enable_due(Some(t0), t0 + Duration::from_secs(30 * 60)));
        assert!(enable_due(Some(t0), t0 + HOT_STANDBY_ENABLE_INTERVAL));
    }

    const HYBRID: &str = include_str!("../../tests/fixtures/solax_hybrid_entity_ids.txt");
    const ULTRA: &str = include_str!("../../tests/fixtures/solax_ultra_parallel_entity_ids.txt");

    /// Lookup over a fixed set of entities (state per id); other ids do not exist
    fn lookup_in(
        states: &[(&str, &str)],
    ) -> impl Fn(String) -> std::future::Ready<Option<String>> + use<> {
        let map: HashMap<String, String> = states
            .iter()
            .map(|(entity, state)| (entity.to_string(), state.to_string()))
            .collect();
        move |entity| std::future::ready(map.get(&entity).cloned())
    }

    /// Lookup over every entity of a fixture file (state `on`), with per-entity overrides
    fn fixture_lookup(
        text: &str,
        overrides: &[(&str, &str)],
    ) -> impl Fn(String) -> std::future::Ready<Option<String>> + use<> {
        let mut states: Vec<(&str, &str)> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|entity| (entity, "on"))
            .collect();
        states.extend_from_slice(overrides);
        lookup_in(&states)
    }

    #[test]
    fn registry_candidates_stay_in_lock_config_entry() {
        let value = json!([
            {"entity_id": "select.solax_lock_state", "platform": "solax_modbus", "unique_id": "SolaX_lock", "config_entry_id": "A"},
            {"entity_id": "switch.solax_inverter_hotstandby", "platform": "solax_modbus", "unique_id": "SolaX_hotstandby", "config_entry_id": "A"},
            {"entity_id": "select.solax_hotstandby", "platform": "solax_modbus", "unique_id": "SolaX_hotstandby", "config_entry_id": "A"},
            {"entity_id": "select.other_hotstandby", "platform": "solax_modbus", "unique_id": "Other_hotstandby", "config_entry_id": "B"},
            {"entity_id": "switch.foreign_hotstandby", "platform": "other", "unique_id": "x_hotstandby", "config_entry_id": "A"},
        ]);
        let entries = parse_registry(&value);
        assert_eq!(entries.len(), 5);
        assert_eq!(
            registry_candidates(&entries, "select.solax_lock_state"),
            vec![
                "switch.solax_inverter_hotstandby",
                "select.solax_hotstandby"
            ]
        );
    }

    #[test]
    fn registry_step_skipped_without_lock_registry_entry() {
        let value = json!([
            {"entity_id": "switch.solax_inverter_hotstandby", "platform": "solax_modbus", "unique_id": "SolaX_hotstandby", "config_entry_id": "A"},
        ]);
        let entries = parse_registry(&value);
        assert!(registry_candidates(&entries, "select.solax_lock_state").is_empty());
    }

    #[test]
    fn registry_prefers_switch_over_unavailable_select() {
        let candidates = vec![
            (
                "select.solax_hotstandby".to_string(),
                "unavailable".to_string(),
            ),
            (
                "switch.solax_inverter_hotstandby".to_string(),
                "on".to_string(),
            ),
        ];
        assert_eq!(
            choose_registry_candidate(&candidates),
            Some(target("switch.solax_inverter_hotstandby"))
        );
        let only_select = vec![
            (
                "select.solax_hotstandby".to_string(),
                "unavailable".to_string(),
            ),
            ("select.other_hotstandby".to_string(), "Enabled".to_string()),
        ];
        assert_eq!(
            choose_registry_candidate(&only_select),
            Some(target("select.other_hotstandby"))
        );
        assert_eq!(
            choose_registry_candidate(&[(
                "select.solax_hotstandby".to_string(),
                "unavailable".to_string()
            )]),
            None
        );
    }

    #[tokio::test]
    async fn resolution_uses_configured_entity_first() {
        let lookup = lookup_in(&[
            ("switch.custom_hot", "on"),
            ("switch.solax_hotstandby", "off"),
        ]);
        let found =
            resolve_hot_standby_with(lookup, "solax", Some("switch.custom_hot"), None).await;
        assert_eq!(found, Some(target("switch.custom_hot")));
    }

    #[tokio::test]
    async fn resolution_falls_back_to_prefix_switch() {
        let lookup = lookup_in(&[("switch.solax_hotstandby", "off")]);
        let found =
            resolve_hot_standby_with(lookup, "solax", Some("switch.custom_hot"), None).await;
        assert_eq!(found, Some(target("switch.solax_hotstandby")));
    }

    #[tokio::test]
    async fn resolution_skips_unavailable_switch_for_usable_select() {
        let lookup = lookup_in(&[
            ("switch.solax_80_master_hotstandby", "unavailable"),
            ("select.solax_80_master_hotstandby", "Disabled"),
        ]);
        let found = resolve_hot_standby_with(lookup, "solax_80_master", None, None).await;
        assert_eq!(found, Some(target("select.solax_80_master_hotstandby")));
    }

    #[tokio::test]
    async fn ultra_parallel_each_inverter_resolves_to_its_own_select() {
        for prefix in ["solax_80_master", "solax_81_slave", "solax_53_other"] {
            let lookup = fixture_lookup(ULTRA, &[]);
            let found = resolve_hot_standby_with(lookup, prefix, None, None).await;
            assert_eq!(
                found,
                Some(target(&format!("select.{prefix}_hotstandby"))),
                "prefix {prefix}"
            );
        }
    }

    #[tokio::test]
    async fn x3_hybrid_resolves_through_registry_to_renamed_switch() {
        // Fixture: select.solax_hotstandby is unavailable, switch.solax_hotstandby absent
        let lookup = fixture_lookup(HYBRID, &[("select.solax_hotstandby", "unavailable")]);
        let registry = parse_registry(&json!([
            {"entity_id": "select.solax_lock_state", "platform": "solax_modbus", "unique_id": "SolaX_lock", "config_entry_id": "X3"},
            {"entity_id": "select.solax_hotstandby", "platform": "solax_modbus", "unique_id": "SolaX_hotstandby", "config_entry_id": "X3"},
            {"entity_id": "switch.solax_inverter_hotstandby", "platform": "solax_modbus", "unique_id": "SolaX_hotstandby", "config_entry_id": "X3"},
        ]));
        let found = resolve_hot_standby_with(lookup, "solax", None, Some(registry)).await;
        assert_eq!(found, Some(target("switch.solax_inverter_hotstandby")));
    }

    #[tokio::test]
    async fn registry_picks_each_inverter_from_its_own_config_entry() {
        // No prefix-named entities exist, so only the registry can resolve them
        let registry = parse_registry(&json!([
            {"entity_id": "select.solax_80_master_lock_state", "platform": "solax_modbus", "unique_id": "m_lock", "config_entry_id": "A"},
            {"entity_id": "select.solax_81_slave_lock_state", "platform": "solax_modbus", "unique_id": "s_lock", "config_entry_id": "B"},
            {"entity_id": "select.solax_53_other_lock_state", "platform": "solax_modbus", "unique_id": "o_lock", "config_entry_id": "C"},
            {"entity_id": "select.garage_hot_a", "platform": "solax_modbus", "unique_id": "A_hotstandby", "config_entry_id": "A"},
            {"entity_id": "select.garage_hot_b", "platform": "solax_modbus", "unique_id": "B_hotstandby", "config_entry_id": "B"},
            {"entity_id": "select.garage_hot_c", "platform": "solax_modbus", "unique_id": "C_hotstandby", "config_entry_id": "C"},
        ]));
        let states = [
            ("select.garage_hot_a", "Enabled"),
            ("select.garage_hot_b", "Disabled"),
            ("select.garage_hot_c", "Enabled"),
        ];
        let master = resolve_hot_standby_with(
            lookup_in(&states),
            "solax_80_master",
            None,
            Some(registry.clone()),
        )
        .await;
        assert_eq!(master, Some(target("select.garage_hot_a")));
        let slave = resolve_hot_standby_with(
            lookup_in(&states),
            "solax_81_slave",
            None,
            Some(registry.clone()),
        )
        .await;
        assert_eq!(slave, Some(target("select.garage_hot_b")));
        let other =
            resolve_hot_standby_with(lookup_in(&states), "solax_53_other", None, Some(registry))
                .await;
        assert_eq!(other, Some(target("select.garage_hot_c")));
    }

    #[tokio::test]
    async fn registry_step_skipped_when_lock_has_no_registry_entry() {
        let registry = parse_registry(&json!([
            {"entity_id": "select.garage_hot_a", "platform": "solax_modbus", "unique_id": "A_hotstandby", "config_entry_id": "A"},
        ]));
        let lookup = lookup_in(&[("select.garage_hot_a", "Enabled")]);
        let found = resolve_hot_standby_with(lookup, "solax_80_master", None, Some(registry)).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn resolution_none_when_nothing_found() {
        let lookup = lookup_in(&[]);
        assert_eq!(
            resolve_hot_standby_with(lookup, "solax", None, None).await,
            None
        );
    }

    #[tokio::test]
    async fn check_turns_switch_on_once_and_not_in_debug() {
        let mut server = Server::new_async().await;
        let _state = server
            .mock("GET", "/api/states/switch.solax_hotstandby")
            .with_status(200)
            .with_body(r#"{"entity_id":"switch.solax_hotstandby","state":"off","attributes":{},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}"#)
            .expect_at_least(2)
            .create_async()
            .await;
        let turn_on = server
            .mock("POST", "/api/services/switch/turn_on")
            .expect(1)
            .with_status(200)
            .create_async()
            .await;
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .expect(1)
            .with_status(200)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let t = target("switch.solax_hotstandby");
        let last = Mutex::new(None);

        // Debug: nothing is called
        check_once(&client, &t, &last, true).await.unwrap();
        assert!(last.lock().is_none());

        // Not debug: enables, then the second check within the hour does nothing
        check_once(&client, &t, &last, false).await.unwrap();
        check_once(&client, &t, &last, false).await.unwrap();
        turn_on.assert_async().await;
        notify.assert_async().await;
    }
}
