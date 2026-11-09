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

//! Startup preconditions of direct VPP mode control (`control.vpp_direct_enabled`, default on)
//! and the choice between it and the EEPROM mode writes (the automatic fallback).

use crate::ha::client::HomeAssistantClient;
use fluxion_core::vpp::VppCommand;
use tracing::{debug, info, warn};

/// Oldest solax-modbus with the full-block FC16 direct VPP write (2026.09.2 wrote each register
/// on its own and is not usable).
pub const MIN_SOLAX_MODBUS_VERSION: [u32; 3] = [2026, 10, 1];

/// Target-set-type selects (modes 1–7, modes 8/9); FluxION's keep-alive relies on "Set" (a
/// re-sent command restarts its target or duration).
pub const TARGET_SET_TYPE_KEYS: [&str; 2] = [
    "remote_control_target_set_type_direct",
    "power_control_mode_target_set_type_direct",
];
/// The target set type FluxION needs
pub const TARGET_SET_TYPE: &str = "Set";

/// Parameter keys of `commands`, sorted, without duplicates
fn parameter_keys(commands: &[VppCommand]) -> Vec<&'static str> {
    let mut keys: Vec<&str> = commands
        .iter()
        .flat_map(|c| c.parameters().into_iter().map(|(key, _)| key))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// The entities of modes 1–7 that every direct VPP inverter needs: the mode select, the modes
/// 1–7 target-set-type select and the numbers of modes 4, 6 and 7 (control prefix), and the
/// read-back sensor (sensor prefix = inverter id).
#[must_use]
pub fn base_entities(sensor_prefix: &str, control_prefix: &str) -> Vec<String> {
    let control = control_prefix.replace('.', "_");
    let mut entities = vec![
        format!("select.{control}_modbus_power_control_direct"),
        format!("select.{control}_{}", TARGET_SET_TYPE_KEYS[0]),
    ];
    entities.extend(
        parameter_keys(&[VppCommand::ChargeOnly, VppCommand::Push { push_w: 0 }])
            .iter()
            .map(|key| format!("number.{control}_remotecontrol_{key}_direct")),
    );
    entities.push(format!(
        "sensor.{}_modbus_power_control",
        sensor_prefix.replace('.', "_")
    ));
    entities
}

/// The mode 8/9 entities (target set type, PV limit, push, duration, NextMotion). The X3-Ultra
/// (GEN5) has none of them in solax-modbus 2026.10.x (owner direction D5).
#[must_use]
pub fn mode8_entities(control_prefix: &str) -> Vec<String> {
    let control = control_prefix.replace('.', "_");
    let mut entities = vec![format!("select.{control}_{}", TARGET_SET_TYPE_KEYS[1])];
    entities.extend(
        parameter_keys(&[VppCommand::PvAndBattery {
            pv_limit_w: 0,
            push_w: 0,
        }])
        .iter()
        .map(|key| format!("number.{control}_remotecontrol_{key}_direct")),
    );
    entities
}

/// Every entity direct VPP uses on an inverter with mode 8
#[must_use]
pub fn required_entities(sensor_prefix: &str, control_prefix: &str) -> Vec<String> {
    let mut entities = base_entities(sensor_prefix, control_prefix);
    entities.extend(mode8_entities(control_prefix));
    entities
}

/// Parse "2026.10.01" / "v2026.10.1" into numeric parts; `None` when not numeric.
#[must_use]
pub fn parse_version(version: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| part.parse().ok())
        .collect();
    parts.filter(|p| !p.is_empty())
}

/// Whether `version` is at least [`MIN_SOLAX_MODBUS_VERSION`].
#[must_use]
pub fn version_supported(version: &str) -> bool {
    parse_version(version).is_some_and(|parts| {
        let mut padded = [0u32; 3];
        for (slot, part) in padded.iter_mut().zip(parts.iter()) {
            *slot = *part;
        }
        padded >= MIN_SOLAX_MODBUS_VERSION
    })
}

/// A candidate for the solax_modbus integration's HACS update entity (N1): the real
/// `update.solax_inverter_modbus_update`, `update.solax_modbus*` / `update.solaxmodbus*`, or any
/// `update.*` whose entity id or title names both "solax" and "modbus". Only a candidate with a
/// calendar version (YYYY.MM.x) is the integration (a firmware entity is not).
fn is_solax_modbus_update(entity_id: &str, title: Option<&str>) -> bool {
    if !entity_id.starts_with("update.") {
        return false;
    }
    let names = |text: &str| {
        let text = text.to_lowercase();
        text.contains("solax") && text.contains("modbus")
    };
    [
        "update.solax_inverter_modbus",
        "update.solax_modbus",
        "update.solaxmodbus",
    ]
    .iter()
    .any(|prefix| entity_id.starts_with(prefix))
        || names(entity_id)
        || title.is_some_and(names)
}

/// "2026.10.01" / "v2026.9.2": a 4-digit year, a month, then more parts.
fn is_calendar_version(version: &str) -> bool {
    let trimmed = version.trim().trim_start_matches(['v', 'V']);
    let mut parts = trimmed.split('.');
    let year = parts.next().unwrap_or_default();
    let month = parts.next().unwrap_or_default();
    year.len() == 4
        && year.starts_with("20")
        && year.chars().all(|c| c.is_ascii_digit())
        && month.parse::<u32>().is_ok_and(|m| (1..=12).contains(&m))
        && parts.next().is_some()
}

/// Precondition check result: `problems` decide the EEPROM path; `incomplete` says the read
/// cannot decide yet (states unreadable, the integration not loaded, its version unknown);
/// `warnings` are logged only; `no_mode8` are the inverters (sensor prefix) that run modes 1–7.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Preconditions {
    pub problems: Vec<String>,
    pub incomplete: Vec<String>,
    pub warnings: Vec<String>,
    pub no_mode8: Vec<String>,
    /// Update entities matched with a calendar version (by id or title)
    pub update_ids: Vec<String>,
}

fn unavailable(state: &str) -> bool {
    matches!(state, "unavailable" | "unknown" | "")
}

/// Target set type "Set" on both selects: written while VPP is Disabled (read-back), else a
/// problem (FluxION does not change it under a running command). Debug mode writes nothing (N7).
async fn ensure_target_set_type(
    client: &HomeAssistantClient,
    states: &[crate::ha::types::HaEntityState],
    sensor_prefix: &str,
    control_prefix: &str,
    debug: bool,
    found: &mut Preconditions,
) {
    let control = control_prefix.replace('.', "_");
    let readback_id = format!(
        "sensor.{}_modbus_power_control",
        sensor_prefix.replace('.', "_")
    );
    let readback = states
        .iter()
        .find(|s| s.entity_id == readback_id)
        .map(|s| s.state.as_str());
    for key in TARGET_SET_TYPE_KEYS {
        let entity_id = format!("select.{control}_{key}");
        let Some(current) = states
            .iter()
            .find(|s| s.entity_id == entity_id)
            .map(|s| s.state.as_str())
        else {
            continue;
        };
        if current == TARGET_SET_TYPE {
            continue;
        }
        // An unreadable select or read-back says nothing about a running command: retried, not
        // a refusal (R6-7)
        if unavailable(current) || readback.is_none_or(unavailable) {
            found.incomplete.push(format!(
                "{entity_id} reads '{current}' and {readback_id} {readback:?}: cannot tell whether a VPP command runs"
            ));
            continue;
        }
        if readback != Some("Disabled") {
            found.problems.push(format!(
                "{entity_id} is '{current}', not '{TARGET_SET_TYPE}', while VPP reads {readback:?}; set it to '{TARGET_SET_TYPE}' with the power control Disabled"
            ));
            continue;
        }
        if debug {
            info!(
                "🔧 [DEBUG] Would set target type {TARGET_SET_TYPE}: {entity_id} (reads '{current}')"
            );
            found.warnings.push(format!(
                "[DEBUG] {entity_id} is '{current}': would set it to '{TARGET_SET_TYPE}'"
            ));
            continue;
        }
        match client
            .call_service(
                "select.select_option",
                serde_json::json!({ "entity_id": entity_id, "option": TARGET_SET_TYPE }),
            )
            .await
        {
            Ok(()) => found.warnings.push(format!(
                "{entity_id} was '{current}': set to '{TARGET_SET_TYPE}' (VPP Disabled)"
            )),
            // Transient (HA or transport): retried, not a reason for the EEPROM path (R5-6)
            Err(e) => found.incomplete.push(format!(
                "cannot set {entity_id} to '{TARGET_SET_TYPE}': {e}"
            )),
        }
    }
}

/// The solax_modbus version from the states: a problem when it is below
/// [`MIN_SOLAX_MODBUS_VERSION`] or not parsable, incomplete without an update entity or version
/// (fail closed, N1).
fn check_version(states: &[crate::ha::types::HaEntityState], found: &mut Preconditions) {
    let mut seen = false;
    for update in states {
        let title = update.attributes.get("title").and_then(|v| v.as_str());
        if !is_solax_modbus_update(&update.entity_id, title) {
            continue;
        }
        let Some(version) = update
            .attributes
            .get("installed_version")
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if !is_calendar_version(version) {
            // e.g. an inverter firmware update entity
            continue;
        }
        seen = true;
        found.update_ids.push(update.entity_id.clone());
        if !version_supported(version) {
            found.problems.push(format!(
                "{} installed_version {} is not 2026.10.01 or newer (no full-block direct VPP write)",
                update.entity_id, version
            ));
        }
    }
    if !seen {
        found.incomplete.push(
            "solax-modbus version unknown (no update entity with an installed version)".to_owned(),
        );
    }
}

/// Update entity ids a full read matched (by id or title), re-read by the targeted retries
pub type UpdateIds = std::sync::Mutex<std::collections::BTreeSet<String>>;

/// The integration's HACS update entity ids read by a targeted check (R4-6)
pub const UPDATE_ENTITY_IDS: [&str; 3] = [
    "update.solax_inverter_modbus_update",
    "update.solax_modbus_update",
    "update.solaxmodbus_update",
];

/// The states of `entity_ids` one by one (a missing entity is left out); an error when Home
/// Assistant cannot answer
async fn read_entities(
    client: &HomeAssistantClient,
    entity_ids: &[String],
) -> crate::ha::errors::HaResult<Vec<crate::ha::types::HaEntityState>> {
    let mut states = Vec::new();
    for entity_id in entity_ids {
        if let Some(state) = client.get_state_optional(entity_id).await? {
            states.push(state);
        }
    }
    Ok(states)
}

/// What a check reads: every state (`/api/states`, the startup window) or only the update
/// entities and the direct entities (the 60 s retries after the window, R4-6)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    All,
    Targeted,
}

/// Check the preconditions for the given (sensor prefix, control prefix) inverters. Nothing is
/// written unless the version and every inverter's entities are in place.
pub async fn check_preconditions(
    client: &HomeAssistantClient,
    inverters: &[(String, String)],
    debug: bool,
    scope: ReadScope,
    updates: &UpdateIds,
) -> Preconditions {
    let mut found = Preconditions::default();
    let read = match scope {
        ReadScope::All => client.get_all_states().await,
        ReadScope::Targeted => {
            let remembered: Vec<String> = updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|id| !UPDATE_ENTITY_IDS.contains(&id.as_str()))
                .cloned()
                .collect();
            let ids: Vec<String> = UPDATE_ENTITY_IDS
                .iter()
                .map(|id| (*id).to_owned())
                .chain(remembered)
                .chain(
                    inverters
                        .iter()
                        .flat_map(|(sensor, control)| required_entities(sensor, control)),
                )
                .collect();
            read_entities(client, &ids).await
        }
    };
    let states = match read {
        Ok(states) => states,
        Err(e) => {
            found
                .incomplete
                .push(format!("cannot read Home Assistant states: {e}"));
            return found;
        }
    };
    let exists = |entity: &String| states.iter().any(|s| &s.entity_id == entity);
    let mut ready = Vec::new();
    for (sensor_prefix, control_prefix) in inverters {
        let sensors = format!("sensor.{}_", sensor_prefix.replace('.', "_"));
        // The integration has loaded and polls this inverter
        if !states
            .iter()
            .any(|s| s.entity_id.starts_with(&sensors) && !unavailable(&s.state))
        {
            found.incomplete.push(format!(
                "no available {sensors}* entity (integration not loaded yet)"
            ));
            continue;
        }
        let readback = format!("{sensors}modbus_power_control");
        if states
            .iter()
            .any(|s| s.entity_id == readback && unavailable(&s.state))
        {
            found.incomplete.push(format!("{readback} is unavailable"));
            continue;
        }
        let mut missing = false;
        for entity in base_entities(sensor_prefix, control_prefix) {
            if !exists(&entity) {
                found.problems.push(format!("{entity} does not exist"));
                missing = true;
            }
        }
        // Mode 8 only with all its entities; none (the Ultra) or some: modes 1–7
        let mode8_missing: Vec<String> = mode8_entities(control_prefix)
            .into_iter()
            .filter(|entity| !exists(entity))
            .collect();
        if !mode8_missing.is_empty() {
            found.no_mode8.push(sensor_prefix.clone());
            if mode8_missing.len() < mode8_entities(control_prefix).len() {
                found.warnings.push(format!(
                    "{sensor_prefix}: mode 8 entities missing ({}): modes 1-7 only",
                    mode8_missing.join(", ")
                ));
            }
        }
        if !missing {
            ready.push((sensor_prefix, control_prefix));
        }
    }
    check_version(&states, &mut found);
    updates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(found.update_ids.iter().cloned());
    // The target set type is written only on a complete, otherwise good read
    if found.problems.is_empty() && found.incomplete.is_empty() {
        for (sensor_prefix, control_prefix) in ready {
            ensure_target_set_type(
                client,
                &states,
                sensor_prefix,
                control_prefix,
                debug,
                &mut found,
            )
            .await;
        }
    }
    found
}

/// `EnsureVppTargetSetType` (after debug mode, R4-3): read the target-set-type selects and the
/// read-back, set "Set" where needed while the power control reads Disabled; an error when a
/// select is not "Set" afterwards
pub async fn ensure_target_set_type_now(
    client: &HomeAssistantClient,
    sensor_prefix: &str,
    control_prefix: &str,
) -> anyhow::Result<()> {
    let control = control_prefix.replace('.', "_");
    let ids: Vec<String> = TARGET_SET_TYPE_KEYS
        .iter()
        .map(|key| format!("select.{control}_{key}"))
        .chain([format!(
            "sensor.{}_modbus_power_control",
            sensor_prefix.replace('.', "_")
        )])
        .collect();
    let states = read_entities(client, &ids)
        .await
        .map_err(|e| anyhow::anyhow!("cannot read the target set type: {e}"))?;
    let mut found = Preconditions::default();
    ensure_target_set_type(
        client,
        &states,
        sensor_prefix,
        control_prefix,
        false,
        &mut found,
    )
    .await;
    for warning in &found.warnings {
        warn!("⚠️ Direct VPP: {}", warning);
    }
    // Not "Set" under a running command: definite (EEPROM path); a failed write: retried (R5-6)
    if !found.problems.is_empty() {
        return Err(fluxion_core::vpp::TargetTypeRefused(found.problems.join("; ")).into());
    }
    if !found.incomplete.is_empty() {
        anyhow::bail!("{}", found.incomplete.join("; "));
    }
    Ok(())
}

/// One attempt of the startup check as [`decide_path`] runs it: `full` reads all states (the
/// window), else only the needed entities; the debug flag is read live from `state` for each
/// attempt (R5-2), and a direct path decided outside debug marks the target set type verified
/// (else it is checked before the first VPP command).
pub async fn live_attempt(
    client: &HomeAssistantClient,
    enabled: bool,
    mode_control_enabled: bool,
    inverters: &[VppInverter],
    state: &fluxion_core::vpp::VppPathState,
    full: bool,
    updates: &UpdateIds,
) -> Attempt {
    let debug = state.debug();
    let scope = if full {
        ReadScope::All
    } else {
        ReadScope::Targeted
    };
    let attempt = resolve_path(
        client,
        enabled,
        mode_control_enabled,
        inverters,
        debug,
        scope,
        updates,
    )
    .await;
    if let Attempt::Decided(path, _) = &attempt
        && path.is_direct()
        && !debug
    {
        state.set_target_verified(true);
    }
    attempt
}

/// One controlled inverter for the direct VPP path decision: a master of a parallel site or an
/// independent unit, never a slave (a slave follows its master's VPP command, D5)
#[derive(Debug, Clone)]
pub struct VppInverter {
    pub id: String,
    pub control_prefix: String,
    pub solax: bool,
}

/// Which mode path runs (owner directions D1, D5, N2): see [`ModePath`]
pub use fluxion_core::vpp::ModePath as VppPath;

/// One startup check (owner direction D1): `Decided` by a complete read (or without a read:
/// the option off, mode control off, a non-SolaX inverter), `Incomplete` when this read cannot
/// decide (N2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Decided(VppPath, Vec<String>),
    Incomplete(String),
}

/// One check of the mode path. Direct VPP is the default, parallel sites included (commands to
/// the master only); the EEPROM mode writes run when `enabled` is false, with mode control off,
/// on a non-SolaX inverter, or when a complete read finds a failed precondition. An inverter
/// without the mode 8 entities stays on direct VPP with modes 1–7.
pub async fn resolve_path(
    client: &HomeAssistantClient,
    enabled: bool,
    mode_control_enabled: bool,
    inverters: &[VppInverter],
    debug: bool,
    scope: ReadScope,
    updates: &UpdateIds,
) -> Attempt {
    if !enabled {
        return Attempt::Decided(
            VppPath::Eeprom("control.vpp_direct_enabled = false".to_owned()),
            Vec::new(),
        );
    }
    if !mode_control_enabled {
        return Attempt::Decided(
            VppPath::Eeprom("mode control off: no mode writes".to_owned()),
            Vec::new(),
        );
    }
    let reasons: Vec<String> = inverters
        .iter()
        .filter(|inv| !inv.solax)
        .map(|inv| format!("inverter {} is not a SolaX", inv.id))
        .collect();
    if !reasons.is_empty() {
        return Attempt::Decided(VppPath::Eeprom(reasons.join("; ")), Vec::new());
    }
    let prefixes: Vec<(String, String)> = inverters
        .iter()
        .map(|inv| (inv.id.clone(), inv.control_prefix.clone()))
        .collect();
    let checked = check_preconditions(client, &prefixes, debug, scope, updates).await;
    if !checked.problems.is_empty() {
        Attempt::Decided(
            VppPath::Eeprom(checked.problems.join("; ")),
            checked.warnings,
        )
    } else if !checked.incomplete.is_empty() {
        Attempt::Incomplete(checked.incomplete.join("; "))
    } else {
        Attempt::Decided(
            VppPath::Direct {
                no_mode8: checked.no_mode8,
            },
            checked.warnings,
        )
    }
}

fn log_decision(path: &VppPath, warnings: &[String]) {
    for warning in warnings {
        warn!("⚠️ Direct VPP precondition: {}", warning);
    }
    match path {
        VppPath::Direct { no_mode8 } => {
            info!(
                "Direct VPP mode control: on (block modes and curtailment as volatile VPP commands; EEPROM only for the basic work mode)"
            );
            for id in no_mode8 {
                info!(
                    "Direct VPP on {id}: no mode 8 on this inverter, modes 1-7 (force modes as mode 4); export curtailment uses the export limit when mode 8 would be needed"
                );
            }
        }
        VppPath::Eeprom(reason) => {
            warn!("⚠️ Direct VPP unavailable: {reason}, using EEPROM mode writes");
        }
        VppPath::Pending => {}
    }
}

/// Decide the run's mode path into `state` (owner ruling N2, 2026-10-28). `state` stays
/// `Pending` (no mode writes on either path) until a complete read decides; the check is
/// retried every `STARTUP_RETRY_SECS` for `STARTUP_READ_WINDOW_SECS`. Without a complete read
/// in that window the EEPROM path runs (WARN) and the check is retried every
/// `PATH_RETRY_SECS`; a later complete read switches to direct VPP (one way, logged once) or
/// confirms the EEPROM path. A decided path is never checked again. `attempt(true)` reads all
/// states (the window), `attempt(false)` only the entities the check needs (R4-6). The window
/// is wall clock: an attempt is cut at its end (R4-5), and each later one after 60 s.
pub async fn decide_path<F, Fut>(state: fluxion_core::vpp::VppPathState, mut attempt: F)
where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = Attempt>,
{
    use fluxion_core::vpp::{PATH_RETRY_SECS, STARTUP_READ_WINDOW_SECS, STARTUP_RETRY_SECS};
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(STARTUP_READ_WINDOW_SECS);
    let timed_out = |secs: u64| Attempt::Incomplete(format!("no answer within {secs} s"));
    let mut last = String::from("no check finished");
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let result = tokio::time::timeout(left, attempt(true))
            .await
            .unwrap_or_else(|_| timed_out(STARTUP_READ_WINDOW_SECS));
        match result {
            Attempt::Decided(path, warnings) => {
                log_decision(&path, &warnings);
                state.set(path);
                return;
            }
            Attempt::Incomplete(reason) => {
                debug!("Direct VPP startup check incomplete: {}", reason);
                last = reason;
            }
        }
        let next = tokio::time::Instant::now() + std::time::Duration::from_secs(STARTUP_RETRY_SECS);
        tokio::time::sleep_until(next.min(deadline)).await;
    }
    warn!(
        "⚠️ Direct VPP unavailable: no complete read within {} s ({}), using EEPROM mode writes; retrying every {} s",
        STARTUP_READ_WINDOW_SECS, last, PATH_RETRY_SECS
    );
    state.set(VppPath::Eeprom(format!(
        "no complete read within {STARTUP_READ_WINDOW_SECS} s: {last}"
    )));
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(PATH_RETRY_SECS)).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(PATH_RETRY_SECS),
            attempt(false),
        )
        .await
        .unwrap_or_else(|_| timed_out(PATH_RETRY_SECS));
        match result {
            Attempt::Decided(path, warnings) => {
                if path.is_direct() {
                    info!(
                        "✅ Direct VPP confirmed: switching from the EEPROM mode writes (one way)"
                    );
                }
                log_decision(&path, &warnings);
                state.set(path);
                return;
            }
            Attempt::Incomplete(reason) => {
                debug!("Direct VPP check still incomplete: {}", reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[test]
    fn entities_cover_every_command_parameter() {
        assert_eq!(
            required_entities("solax", "solax_inverter"),
            vec![
                "select.solax_inverter_modbus_power_control_direct",
                "select.solax_inverter_remote_control_target_set_type_direct",
                "number.solax_inverter_remotecontrol_push_mode_power_direct",
                "number.solax_inverter_remotecontrol_timeout_direct",
                "sensor.solax_modbus_power_control",
                "select.solax_inverter_power_control_mode_target_set_type_direct",
                "number.solax_inverter_remotecontrol_duration_8_direct",
                "number.solax_inverter_remotecontrol_push_mode_power_8_9_direct",
                "number.solax_inverter_remotecontrol_pv_power_limit_direct",
                "number.solax_inverter_remotecontrol_timeout_8_9_direct",
            ]
        );
        // Every parameter of every command FluxION sends has its entity
        let all = required_entities("solax", "solax_inverter");
        for command in [
            VppCommand::Push { push_w: 0 },
            VppCommand::SelfConsume,
            VppCommand::ChargeOnly,
            VppCommand::PvAndBattery {
                pv_limit_w: 0,
                push_w: 0,
            },
        ] {
            for (key, _) in command.parameters() {
                let entity = format!("number.solax_inverter_remotecontrol_{key}_direct");
                assert!(all.contains(&entity), "{entity}");
            }
        }
    }

    #[test]
    fn version_must_be_at_least_2026_10_01() {
        assert!(version_supported("2026.10.01"));
        assert!(version_supported("2026.10.1"));
        assert!(version_supported("v2026.11.0"));
        assert!(version_supported("2027.1"));
        assert!(!version_supported("2026.09.2"));
        assert!(!version_supported("2026.10.0"));
        assert!(!version_supported("2026.10.01b1"));
        assert!(!version_supported("unknown"));
        assert!(!version_supported(""));
    }

    fn state(entity_id: &str, attributes: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "entity_id": entity_id, "state": "on", "attributes": attributes,
            "last_changed": "2026-10-27T10:00:00Z", "last_updated": "2026-10-27T10:00:00Z"
        })
    }

    /// The integration's real HACS update entity (repo fixture) at `version`
    fn update(version: &str) -> serde_json::Value {
        state(
            "update.solax_inverter_modbus_update",
            serde_json::json!({"installed_version": version, "title": "SolaX Inverter Modbus"}),
        )
    }

    /// `required_entities` with target set type "Set" and VPP Disabled
    fn entity_states(entities: &[String], drop: Option<&str>) -> Vec<serde_json::Value> {
        entities
            .iter()
            .filter(|e| Some(e.as_str()) != drop)
            .map(|e| {
                let mut s = state(e, serde_json::json!({}));
                if e.contains("target_set_type") {
                    s["state"] = "Set".into();
                } else if e.ends_with("_modbus_power_control") {
                    s["state"] = "Disabled".into();
                }
                s
            })
            .collect()
    }

    async fn problems(extra: Vec<serde_json::Value>, drop: Option<&str>) -> Preconditions {
        let mut states = entity_states(&required_entities("solax", "solax"), drop);
        states.extend(extra);
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/states")
            .with_body(serde_json::Value::Array(states).to_string())
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        check_preconditions(
            &client,
            &[("solax".to_owned(), "solax".to_owned())],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await
    }

    /// N1: the integration is found by its real entity id, `update.solax_modbus*`, or a title
    /// or id naming solax and modbus; a firmware entity is not it
    #[test]
    fn the_update_entity_is_found_by_its_real_id_or_title() {
        assert!(is_solax_modbus_update(
            "update.solax_inverter_modbus_update",
            None
        ));
        assert!(is_solax_modbus_update("update.solax_modbus_update", None));
        assert!(is_solax_modbus_update(
            "update.hacs_integration_1",
            Some("SolaX Inverter Modbus")
        ));
        assert!(!is_solax_modbus_update("update.solax_firmware", None));
        assert!(!is_solax_modbus_update(
            "sensor.solax_inverter_modbus_update",
            None
        ));
    }

    #[tokio::test]
    async fn preconditions_pass_with_entities_and_a_new_integration() {
        let matched = |id: &str| Preconditions {
            update_ids: vec![id.to_owned()],
            ..Preconditions::default()
        };
        assert_eq!(
            problems(vec![update("2026.10.01")], None).await,
            matched("update.solax_inverter_modbus_update")
        );
        // Found by its title too (remembered for the targeted retries)
        let titled = state(
            "update.hacs_repo_42",
            serde_json::json!({"installed_version": "2026.10.02", "title": "SolaX Modbus"}),
        );
        assert_eq!(
            problems(vec![titled], None).await,
            matched("update.hacs_repo_42")
        );
    }

    /// N1, fail closed: no update entity, or only a firmware one, leaves the version unknown
    /// (no decision); an unparsable or old version is a problem
    #[tokio::test]
    async fn an_unknown_or_old_version_never_passes() {
        let found = problems(vec![], None).await;
        assert!(found.problems.is_empty(), "{found:?}");
        assert_eq!(found.incomplete.len(), 1, "{found:?}");
        let firmware = state(
            "update.solax_modbus_firmware",
            serde_json::json!({"installed_version": "3.005.02"}),
        );
        let found = problems(vec![firmware], None).await;
        assert_eq!(found.incomplete.len(), 1, "{found:?}");
        for version in ["2026.09.2", "2026.10.01b1", "2025.1.0"] {
            let found = problems(vec![update(version)], None).await;
            assert_eq!(found.problems.len(), 1, "{version}: {found:?}");
            assert!(found.problems[0].contains(version));
        }
    }

    #[tokio::test]
    async fn preconditions_fail_on_a_missing_entity() {
        let found = problems(
            vec![update("2026.10.01")],
            Some("number.solax_remotecontrol_push_mode_power_direct"),
        )
        .await
        .problems;
        assert_eq!(
            found,
            vec!["number.solax_remotecontrol_push_mode_power_direct does not exist"]
        );
    }

    /// D5: a mode 8 entity missing is no problem: that inverter runs modes 1–7 (warned when
    /// only some are missing)
    #[tokio::test]
    async fn a_missing_mode_8_entity_means_modes_1_to_7() {
        let found = problems(
            vec![update("2026.10.01")],
            Some("number.solax_remotecontrol_pv_power_limit_direct"),
        )
        .await;
        assert!(found.problems.is_empty(), "{found:?}");
        assert_eq!(found.no_mode8, vec!["solax"]);
        assert!(
            found.warnings.iter().any(|w| w.contains("pv_power_limit")),
            "{found:?}"
        );
    }

    fn solax(id: &str) -> VppInverter {
        VppInverter {
            id: id.to_owned(),
            control_prefix: id.to_owned(),
            solax: true,
        }
    }

    /// D1: the option off, mode control off or a non-SolaX inverter pick the EEPROM path
    /// without reading Home Assistant
    #[tokio::test]
    async fn eeprom_path_without_a_home_assistant_read() {
        let mut server = Server::new_async().await;
        let states = server
            .mock("GET", "/api/states")
            .expect(0)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let one = [solax("solax")];
        assert_eq!(
            resolve_path(
                &client,
                false,
                true,
                &one,
                false,
                ReadScope::All,
                &UpdateIds::default()
            )
            .await,
            Attempt::Decided(
                VppPath::Eeprom("control.vpp_direct_enabled = false".to_owned()),
                vec![]
            )
        );
        let Attempt::Decided(path, _) = resolve_path(
            &client,
            true,
            false,
            &one,
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await
        else {
            panic!("decided");
        };
        assert!(!path.is_direct());
        let other = [VppInverter {
            solax: false,
            ..solax("goodwe")
        }];
        let Attempt::Decided(path, _) = resolve_path(
            &client,
            true,
            true,
            &other,
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await
        else {
            panic!("decided");
        };
        assert!(path.describe().contains("not a SolaX"), "{path:?}");
        states.assert_async().await;
    }

    /// The default (option on): direct VPP when the preconditions hold; the EEPROM path with the
    /// reason when they fail. D5: an inverter without the mode 8 entities (the Ultra master
    /// `ultra` of a parallel pair) is direct VPP with modes 1–7
    #[tokio::test]
    async fn direct_path_follows_the_preconditions() {
        let mut server = Server::new_async().await;
        let entities: Vec<String> = required_entities("solax", "solax")
            .into_iter()
            .chain(base_entities("ultra", "ultra"))
            .chain(["sensor.other_pv_power".to_owned()])
            .collect();
        let mut states = entity_states(&entities, None);
        states.push(update("2026.10.01"));
        let _m = server
            .mock("GET", "/api/states")
            .with_body(serde_json::Value::Array(states).to_string())
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("solax")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        assert_eq!(
            attempt,
            Attempt::Decided(VppPath::Direct { no_mode8: vec![] }, vec![])
        );
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("ultra")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        let Attempt::Decided(path, warnings) = attempt else {
            panic!("decided");
        };
        assert_eq!(
            path,
            VppPath::Direct {
                no_mode8: vec!["ultra".to_owned()]
            }
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(path.describe(), "direct VPP (modes 1-7, no mode 8: ultra)");
        let Attempt::Decided(path, _) = resolve_path(
            &client,
            true,
            true,
            &[solax("other")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await
        else {
            panic!("decided");
        };
        assert!(path.describe().contains("does not exist"), "{path:?}");
        // An inverter whose integration has not loaded yet: no decision
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("missing")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        assert!(matches!(attempt, Attempt::Incomplete(_)), "{attempt:?}");
    }

    /// The repo's real SolaX hybrid entity set (`tests/fixtures/solax_hybrid_entity_ids.txt`):
    /// every direct entity present, target set type "Set", VPP Disabled, and the real HACS
    /// update entity at `version` (`None`: no update entity)
    async fn fixture_site(
        server: &mut Server,
        version: Option<&str>,
        target: &str,
    ) -> mockito::Mock {
        let ids = include_str!("../../tests/fixtures/solax_hybrid_entity_ids.txt");
        let states: Vec<serde_json::Value> = ids
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter_map(|id| {
                if id == "update.solax_inverter_modbus_update" {
                    return version.map(update);
                }
                let value = if id.contains("target_set_type") {
                    target
                } else if id == "sensor.solax_modbus_power_control" {
                    "Disabled"
                } else {
                    "0"
                };
                let mut s = state(id, serde_json::json!({}));
                s["state"] = value.into();
                Some(s)
            })
            .collect();
        server
            .mock("GET", "/api/states")
            .with_body(serde_json::Value::Array(states).to_string())
            .create_async()
            .await
    }

    /// N1 (D3 rebuilt): the real entity set on 2026.09.2 has every direct entity, and still
    /// picks EEPROM; no write while checking, even with a target set type that is not "Set" and
    /// VPP Disabled (R4-6). On 2026.10.01 the same set is direct VPP
    #[tokio::test]
    async fn real_entity_set_on_2026_09_2_picks_eeprom() {
        let mut server = Server::new_async().await;
        let _states = fixture_site(&mut server, Some("2026.09.2"), "Update").await;
        let writes = server
            .mock("POST", mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("solax")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        let Attempt::Decided(VppPath::Eeprom(reason), _) = attempt else {
            panic!("{attempt:?}");
        };
        assert!(
            reason.contains("update.solax_inverter_modbus_update"),
            "{reason}"
        );
        assert!(reason.contains("2026.09.2"), "{reason}");
        writes.assert_async().await;

        let mut server = Server::new_async().await;
        let _states = fixture_site(&mut server, Some("2026.10.01"), "Set").await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        assert_eq!(
            resolve_path(
                &client,
                true,
                true,
                &[solax("solax")],
                false,
                ReadScope::All,
                &UpdateIds::default(),
            )
            .await,
            Attempt::Decided(VppPath::Direct { no_mode8: vec![] }, vec![])
        );
    }

    /// N1: the real entity set without the update entity never decides direct VPP; after the
    /// 120 s window it runs the EEPROM path
    #[tokio::test]
    async fn real_entity_set_without_the_update_entity_picks_eeprom() {
        let mut server = Server::new_async().await;
        let _states = fixture_site(&mut server, None, "Set").await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("solax")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        assert!(
            matches!(&attempt, Attempt::Incomplete(r) if r.contains("version unknown")),
            "{attempt:?}"
        );
        let state = fluxion_core::vpp::VppPathState::default();
        decide_on_paused_time(state.clone(), move |_| attempt.clone(), 130).await;
        assert!(
            matches!(state.get(), VppPath::Eeprom(r) if r.contains("version unknown")),
            "{:?}",
            state.get()
        );
    }

    /// Run [`decide_path`] on paused time for `secs` seconds with `attempt(elapsed s)`; returns
    /// the attempt times and the path seen at every 5 s step
    async fn decide_on_paused_time(
        state: fluxion_core::vpp::VppPathState,
        attempt: impl Fn(u64) -> Attempt + Send + 'static,
        secs: u64,
    ) -> (Vec<u64>, Vec<(u64, VppPath)>) {
        let (calls, seen) = decide_stalling(state, attempt, 0, secs).await;
        (calls.into_iter().map(|(at, _)| at).collect(), seen)
    }

    /// [`decide_on_paused_time`] whose every attempt takes `stall` s; the calls carry whether
    /// they read all states
    async fn decide_stalling(
        state: fluxion_core::vpp::VppPathState,
        attempt: impl Fn(u64) -> Attempt + Send + 'static,
        stall: u64,
        secs: u64,
    ) -> (Vec<(u64, bool)>, Vec<(u64, VppPath)>) {
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = {
            let calls = calls.clone();
            let state = state.clone();
            tokio::spawn(decide_path(state, move |full| {
                let at = start.elapsed().as_secs();
                calls.lock().unwrap().push((at, full));
                let result = attempt(at);
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(stall)).await;
                    result
                }
            }))
        };
        let mut seen = Vec::new();
        for at in (0..=secs).step_by(5) {
            // Half a second after each 5 s step: a check due at that step has run (timers fire
            // up to a millisecond late)
            tokio::time::sleep_until(start + std::time::Duration::from_millis(at * 1000 + 500))
                .await;
            seen.push((at, state.get()));
        }
        task.abort();
        tokio::time::resume();
        let calls = calls.lock().unwrap().clone();
        (calls, seen)
    }

    fn incomplete() -> Attempt {
        Attempt::Incomplete("cannot read Home Assistant states: timeout".to_owned())
    }

    fn direct() -> Attempt {
        Attempt::Decided(VppPath::Direct { no_mode8: vec![] }, vec![])
    }

    /// N2: reads fail for 110 s, then succeed: direct VPP, and the path is Pending (no mode
    /// writes on either path) until then; retries every 10 s
    #[tokio::test]
    async fn a_good_read_within_the_window_decides_direct_vpp() {
        let state = fluxion_core::vpp::VppPathState::default();
        let (calls, seen) = decide_on_paused_time(
            state.clone(),
            |at| if at < 110 { incomplete() } else { direct() },
            300,
        )
        .await;
        assert_eq!(calls, (0..=110).step_by(10).collect::<Vec<u64>>());
        for (at, path) in &seen {
            if *at < 110 {
                assert_eq!(*path, VppPath::Pending, "{at} s");
            } else {
                assert!(path.is_direct(), "{at} s: {path:?}");
            }
        }
    }

    /// N2: reads fail for 130 s: the EEPROM path from 120 s, then a good read at the 60 s retry
    /// switches to direct VPP for good
    #[tokio::test]
    async fn no_read_in_the_window_runs_eeprom_then_switches_once() {
        let state = fluxion_core::vpp::VppPathState::default();
        let (calls, seen) = decide_on_paused_time(
            state.clone(),
            |at| if at < 130 { incomplete() } else { direct() },
            600,
        )
        .await;
        assert_eq!(
            calls,
            (0..=110).step_by(10).chain([180]).collect::<Vec<u64>>()
        );
        for (at, path) in &seen {
            match at {
                0..120 => assert_eq!(*path, VppPath::Pending, "{at} s"),
                120..180 => assert!(
                    matches!(path, VppPath::Eeprom(r) if r.contains("120 s")),
                    "{at} s: {path:?}"
                ),
                _ => assert!(path.is_direct(), "{at} s: {path:?}"),
            }
        }
    }

    /// N2: a good read of the real 2026.09.2 entity set decides EEPROM at once and is never
    /// checked again
    #[tokio::test]
    async fn real_2026_09_2_set_stays_on_eeprom() {
        let mut server = Server::new_async().await;
        let _states = fixture_site(&mut server, Some("2026.09.2"), "Set").await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let attempt = resolve_path(
            &client,
            true,
            true,
            &[solax("solax")],
            false,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        assert!(matches!(&attempt, Attempt::Decided(VppPath::Eeprom(_), _)));
        let state = fluxion_core::vpp::VppPathState::default();
        let (calls, seen) =
            decide_on_paused_time(state.clone(), move |_| attempt.clone(), 900).await;
        assert_eq!(calls, vec![0]);
        assert!(
            seen.iter()
                .all(|(_, path)| matches!(path, VppPath::Eeprom(r) if r.contains("2026.09.2")))
        );
    }

    /// R4-5: the window is wall clock: attempts that stall 50 s or 70 s still give the EEPROM
    /// path at 120 s (a stalled attempt is cut at the end of the window)
    #[tokio::test]
    async fn stalled_reads_still_end_the_window_at_120_s() {
        for stall in [50, 70] {
            let state = fluxion_core::vpp::VppPathState::default();
            let (_, seen) = decide_stalling(state, |_| incomplete(), stall, 200).await;
            for (at, path) in &seen {
                if *at < 120 {
                    assert_eq!(*path, VppPath::Pending, "stall {stall}: {at} s");
                } else {
                    assert!(
                        matches!(path, VppPath::Eeprom(_)),
                        "stall {stall}: {at} s: {path:?}"
                    );
                }
            }
        }
    }

    /// R4-6: inside the window the check reads all states, after it only the needed entities
    #[tokio::test]
    async fn retries_after_the_window_read_only_the_needed_entities() {
        let state = fluxion_core::vpp::VppPathState::default();
        let (calls, _) = decide_stalling(state, |_| incomplete(), 0, 300).await;
        assert!(
            calls.iter().all(|(at, full)| *full == (*at < 120)),
            "{calls:?}"
        );
        assert!(calls.iter().any(|(_, full)| !full));
    }

    /// R4-6: a targeted check reads the update entities and the direct entities one by one,
    /// never `/api/states`, and decides like a full one
    #[tokio::test]
    async fn targeted_check_never_reads_all_states() {
        let mut server = Server::new_async().await;
        let all = server
            .mock("GET", "/api/states")
            .expect(0)
            .create_async()
            .await;
        let mut mocks = Vec::new();
        for (id, body) in entity_states(&required_entities("solax", "solax"), None)
            .into_iter()
            .map(|s| (s["entity_id"].as_str().unwrap().to_owned(), s))
            .chain([(
                "update.solax_inverter_modbus_update".to_owned(),
                update("2026.10.01"),
            )])
        {
            mocks.push(
                server
                    .mock("GET", format!("/api/states/{id}").as_str())
                    .with_body(body.to_string())
                    .expect(1)
                    .create_async()
                    .await,
            );
        }
        for id in &UPDATE_ENTITY_IDS[1..] {
            mocks.push(
                server
                    .mock("GET", format!("/api/states/{id}").as_str())
                    .with_status(404)
                    .create_async()
                    .await,
            );
        }
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        assert_eq!(
            resolve_path(
                &client,
                true,
                true,
                &[solax("solax")],
                false,
                ReadScope::Targeted,
                &UpdateIds::default(),
            )
            .await,
            Attempt::Decided(VppPath::Direct { no_mode8: vec![] }, vec![])
        );
        all.assert_async().await;
        for mock in &mocks[..mocks.len() - 2] {
            mock.assert_async().await;
        }
    }

    /// Nit (R5): an update entity matched by its title in a full read is re-read by the
    /// targeted retries (its id is remembered)
    #[tokio::test]
    async fn a_title_matched_update_entity_is_re_read_on_targeted_retries() {
        let titled = serde_json::json!({
            "entity_id": "update.hacs_repo_42", "state": "off",
            "attributes": {"installed_version": "2026.10.02", "title": "SolaX Modbus"},
            "last_changed": "2026-10-27T10:00:00Z", "last_updated": "2026-10-27T10:00:00Z"
        });
        let updates = UpdateIds::default();
        // Full read, incomplete (read-back unavailable): the matched id is remembered
        let mut server = Server::new_async().await;
        let mut states = entity_states(&required_entities("solax", "solax"), None);
        for state in &mut states {
            if state["entity_id"] == "sensor.solax_modbus_power_control" {
                state["state"] = "unavailable".into();
            }
        }
        states.push(titled.clone());
        let _all = server
            .mock("GET", "/api/states")
            .with_body(serde_json::Value::Array(states).to_string())
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let first = resolve_path(
            &client,
            true,
            true,
            &[solax("solax")],
            false,
            ReadScope::All,
            &updates,
        )
        .await;
        assert!(matches!(first, Attempt::Incomplete(_)), "{first:?}");

        // Targeted retry: reads it by id and decides
        let mut server = Server::new_async().await;
        let mut mocks = Vec::new();
        for state in entity_states(&required_entities("solax", "solax"), None)
            .into_iter()
            .chain([titled])
        {
            let id = state["entity_id"].as_str().unwrap().to_owned();
            mocks.push(
                server
                    .mock("GET", format!("/api/states/{id}").as_str())
                    .with_body(state.to_string())
                    .expect(1)
                    .create_async()
                    .await,
            );
        }
        for id in UPDATE_ENTITY_IDS {
            server
                .mock("GET", format!("/api/states/{id}").as_str())
                .with_status(404)
                .create_async()
                .await;
        }
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        assert_eq!(
            resolve_path(
                &client,
                true,
                true,
                &[solax("solax")],
                false,
                ReadScope::Targeted,
                &updates,
            )
            .await,
            Attempt::Decided(VppPath::Direct { no_mode8: vec![] }, vec![])
        );
        for mock in &mocks {
            mock.assert_async().await;
        }
    }

    /// R5-2: the debug flag is read live for each attempt: debug on before a retry writes no
    /// target set type (and leaves it unverified); debug off writes it once and verifies
    #[tokio::test]
    async fn each_attempt_reads_the_live_debug_flag() {
        for debug in [true, false] {
            let mut server = Server::new_async().await;
            let mut states = entity_states(&required_entities("solax", "solax"), None);
            for state in &mut states {
                if state["entity_id"] == "select.solax_power_control_mode_target_set_type_direct" {
                    state["state"] = "Update".into();
                }
            }
            states.push(update("2026.10.01"));
            let _all = server
                .mock("GET", "/api/states")
                .with_body(serde_json::Value::Array(states).to_string())
                .create_async()
                .await;
            let write = server
                .mock("POST", "/api/services/select/select_option")
                .with_body("[]")
                .expect(usize::from(!debug))
                .create_async()
                .await;
            let client = HomeAssistantClient::new(server.url(), "t").unwrap();
            let state = fluxion_core::vpp::VppPathState::default();
            state.set_debug(debug);
            let attempt = live_attempt(
                &client,
                true,
                true,
                &[solax("solax")],
                &state,
                true,
                &UpdateIds::default(),
            )
            .await;
            assert!(
                matches!(&attempt, Attempt::Decided(path, _) if path.is_direct()),
                "debug {debug}: {attempt:?}"
            );
            assert_eq!(state.target_verified("solax"), !debug, "debug {debug}");
            write.assert_async().await;
        }
    }
}

#[cfg(test)]
mod target_set_type_tests {
    use super::*;
    use mockito::Server;

    fn state(entity_id: &str, value: &str) -> serde_json::Value {
        serde_json::json!({
            "entity_id": entity_id, "state": value, "attributes": {},
            "last_changed": "2026-10-27T10:00:00Z", "last_updated": "2026-10-27T10:00:00Z"
        })
    }

    /// Precondition check with the mode 8/9 target set type `target` and the read-back `vpp`;
    /// `writes` = expected "Set" writes
    async fn check(target: &str, vpp: &str, writes: usize, debug: bool) -> Preconditions {
        let mut server = Server::new_async().await;
        let mut states: Vec<serde_json::Value> = required_entities("solax", "solax")
            .iter()
            .map(|e| {
                let value = if e == "select.solax_power_control_mode_target_set_type_direct" {
                    target
                } else if e.contains("target_set_type") {
                    "Set"
                } else if e.ends_with("_modbus_power_control") {
                    vpp
                } else {
                    "0"
                };
                state(e, value)
            })
            .collect();
        states.push(serde_json::json!({
            "entity_id": "update.solax_inverter_modbus_update", "state": "off",
            "attributes": {"installed_version": "2026.10.01"},
            "last_changed": "2026-10-27T10:00:00Z", "last_updated": "2026-10-27T10:00:00Z"
        }));
        let _states = server
            .mock("GET", "/api/states")
            .with_body(serde_json::Value::Array(states).to_string())
            .create_async()
            .await;
        let write = server
            .mock("POST", mockito::Matcher::Any)
            .with_body("[]")
            .expect(writes)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let found = check_preconditions(
            &client,
            &[("solax".to_owned(), "solax".to_owned())],
            debug,
            ReadScope::All,
            &UpdateIds::default(),
        )
        .await;
        write.assert_async().await;
        found
    }

    /// Not "Set" with VPP Disabled: written to "Set" (WARN); with VPP active: a problem, no write
    #[tokio::test]
    async fn target_set_type_is_set_while_vpp_is_disabled() {
        let found = check("Set", "Disabled", 0, false).await;
        assert!(found.problems.is_empty(), "{found:?}");

        let found = check("Adjust", "Disabled", 1, false).await;
        assert!(found.problems.is_empty(), "{found:?}");
        assert!(
            found.warnings.iter().any(|w| w.contains("set to 'Set'")),
            "{found:?}"
        );

        let found = check("Adjust", "Push Power - P/N Mode", 0, false).await;
        assert_eq!(found.problems.len(), 1, "{found:?}");
        assert!(found.problems[0].contains("target_set_type"));
    }

    /// N7: debug mode writes no target set type, it only says so
    #[tokio::test]
    async fn debug_mode_never_writes_the_target_set_type() {
        let found = check("Adjust", "Disabled", 0, true).await;
        assert!(found.problems.is_empty(), "{found:?}");
        assert!(
            found
                .warnings
                .iter()
                .any(|w| w.contains("[DEBUG]") && w.contains("would set it to 'Set'")),
            "{found:?}"
        );
    }
}

#[cfg(test)]
mod target_check_tests {
    use super::*;
    use mockito::Server;

    async fn site(server: &mut Server, target: &str, vpp: &str) -> Vec<mockito::Mock> {
        let mut mocks = Vec::new();
        for (id, value) in [
            ("select.solax_remote_control_target_set_type_direct", "Set"),
            (
                "select.solax_power_control_mode_target_set_type_direct",
                target,
            ),
            ("sensor.solax_modbus_power_control", vpp),
        ] {
            mocks.push(
                server
                    .mock("GET", format!("/api/states/{id}").as_str())
                    .with_body(
                        serde_json::json!({
                            "entity_id": id, "state": value, "attributes": {},
                            "last_changed": "2026-10-27T10:00:00Z",
                            "last_updated": "2026-10-27T10:00:00Z"
                        })
                        .to_string(),
                    )
                    .create_async()
                    .await,
            );
        }
        mocks
    }

    /// R4-3: after debug mode the check writes "Set" exactly once while VPP is Disabled; under
    /// a running command it writes nothing and fails (the EEPROM path then runs)
    #[tokio::test]
    async fn the_check_after_debug_sets_the_type_once_or_fails() {
        let mut server = Server::new_async().await;
        let _states = site(&mut server, "Update", "Disabled").await;
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "entity_id": "select.solax_power_control_mode_target_set_type_direct",
                "option": "Set"
            })))
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        ensure_target_set_type_now(&client, "solax", "solax")
            .await
            .expect("set");
        write.assert_async().await;

        let mut server = Server::new_async().await;
        let _states = site(&mut server, "Update", "Push Power - P/N Mode").await;
        let write = server
            .mock("POST", mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let err = ensure_target_set_type_now(&client, "solax", "solax")
            .await
            .expect_err("not settable");
        assert!(err.to_string().contains("target_set_type"), "{err}");
        // Definite: the EEPROM path (R5-6)
        assert!(err.is::<fluxion_core::vpp::TargetTypeRefused>(), "{err:#}");
        write.assert_async().await;
    }

    /// R5-6: a failed write or read is transient (retried by the VPP system), not a refusal
    #[tokio::test]
    async fn a_failed_write_or_read_is_not_a_refusal() {
        let mut server = Server::new_async().await;
        let _states = site(&mut server, "Update", "Disabled").await;
        let _write = server
            .mock("POST", "/api/services/select/select_option")
            .with_status(500)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let err = ensure_target_set_type_now(&client, "solax", "solax")
            .await
            .expect_err("write failed");
        assert!(!err.is::<fluxion_core::vpp::TargetTypeRefused>(), "{err:#}");

        let mut server = Server::new_async().await;
        let _read = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let err = ensure_target_set_type_now(&client, "solax", "solax")
            .await
            .expect_err("read failed");
        assert!(!err.is::<fluxion_core::vpp::TargetTypeRefused>(), "{err:#}");
    }

    /// R6-7: a read-back that is unavailable, unknown or missing during the target check is
    /// a transient failure (retried), never a refusal; a known non-Disabled read-back refuses
    #[tokio::test]
    async fn an_unreadable_readback_is_not_a_refusal() {
        for vpp in [
            "unavailable",
            "unknown",
            "Individual Setting - Duration Mode",
        ] {
            let mut server = Server::new_async().await;
            let _states = site(&mut server, "Update", vpp).await;
            let client = HomeAssistantClient::new(server.url(), "t").unwrap();
            let err = ensure_target_set_type_now(&client, "solax", "solax")
                .await
                .expect_err("not Set");
            let refused = err.is::<fluxion_core::vpp::TargetTypeRefused>();
            assert_eq!(refused, !vpp.starts_with('u'), "{vpp}: {err:#}");
        }
        // The read-back entity missing (404)
        let mut server = Server::new_async().await;
        let _select = server
            .mock(
                "GET",
                "/api/states/select.solax_power_control_mode_target_set_type_direct",
            )
            .with_body(
                serde_json::json!({
                    "entity_id": "select.solax_power_control_mode_target_set_type_direct",
                    "state": "Update", "attributes": {},
                    "last_changed": "2026-10-27T10:00:00Z",
                    "last_updated": "2026-10-27T10:00:00Z"
                })
                .to_string(),
            )
            .create_async()
            .await;
        let _missing = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let err = ensure_target_set_type_now(&client, "solax", "solax")
            .await
            .expect_err("not Set");
        assert!(!err.is::<fluxion_core::vpp::TargetTypeRefused>(), "{err:#}");
    }
}
