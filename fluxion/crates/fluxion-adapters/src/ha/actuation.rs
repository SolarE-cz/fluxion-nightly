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

//! Pre-actuation guards for SolaX inverters (brief H11).
//!
//! Node-RED ran these checks before every inverter command. FluxION runs them in the adapter's
//! `write_command`, the one choke point every SolaX write passes through:
//! - lock: a "Locked" inverter is unlocked and read back before the write;
//! - wake: before ForceCharge, a run mode that is not Normal gets `button.<id>_battery_awaken`;
//! - pre-wake: ten minutes before a scheduled ForceCharge the same press is made in advance, so
//!   the inverter is Normal when the charge starts;
//! - charge check: once run mode reads Normal (waited for up to 15 minutes), five minutes later
//!   FluxION warns if the battery is not charging.
//!
//! Pure decisions are separated from the HA calls so they can be unit tested.

use anyhow::{Result, anyhow, bail};
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::ha::client::HomeAssistantClient;
use crate::ha::dashboard_entities::{LOCK_NOTIFICATION_TITLE, lock_notification_id};
use crate::ha::errors::HaError;
use crate::ha::plugin::HaClientResource;
use fluxion_core::continuous_systems::{LeversStartup, lever_startup_wait};
use fluxion_core::{
    BatteryStatus, DebugModeConfig, Inverter, InverterOperationMode, OperationSchedule,
    ScheduledMode, SystemConfig, TimezoneConfig, UserControlResource, UserLeversResource,
};
use fluxion_types::config::InverterTopology;
use fluxion_types::levers::VprPhase;

/// Stable notification id for the charge-not-running warning
pub const CHARGE_NOT_RUNNING_NOTIFICATION_ID: &str = "fluxion_charge_not_running";
/// Title of the charge-not-running notification (brief H11)
pub const CHARGE_NOT_RUNNING_TITLE: &str = "FluxION: nabíjení ze sítě neběží";

/// Minimum time between two awaken presses
pub const WAKE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How far ahead of a scheduled ForceCharge the pre-wake presses the awaken button
pub const PREWAKE_LOOKAHEAD: Duration = Duration::from_secs(10 * 60);

/// Interval of the pre-wake check against the schedule
pub const PREWAKE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Settings of the pre-actuation guards
#[derive(Debug, Clone)]
pub struct ActuationGuardConfig {
    /// `control.auto_unlock_inverter`: unlock a Locked inverter, otherwise skip the write
    pub auto_unlock: bool,
    /// How long to poll the lock read-back after selecting "Unlocked"
    pub unlock_timeout: Duration,
    /// Interval between lock read-backs
    pub unlock_poll: Duration,
    /// Delay between run mode reading Normal and the charge-running check
    pub charge_check_delay: Duration,
    /// How long a ForceCharge waits for run mode Normal before the charge check gives up
    pub run_mode_wait: Duration,
    /// Interval between run mode reads while waiting for Normal
    pub run_mode_poll: Duration,
    /// `control.mode_control_enabled` (brief H37): when false only `SetExportLimit` is written;
    /// mode and Back Up commands (with their awaken and minimum SOC writes) are refused
    pub mode_control_enabled: bool,
}

impl Default for ActuationGuardConfig {
    fn default() -> Self {
        Self {
            auto_unlock: true,
            unlock_timeout: Duration::from_secs(10),
            unlock_poll: Duration::from_secs(1),
            charge_check_delay: Duration::from_secs(5 * 60),
            run_mode_wait: Duration::from_secs(15 * 60),
            run_mode_poll: Duration::from_secs(5),
            mode_control_enabled: true,
        }
    }
}

/// Mutable guard state of one adapter
#[derive(Default)]
pub struct ActuationState {
    last_wake: Mutex<Option<Instant>>,
    charge_run_active: Arc<AtomicBool>,
    charge_watch: Arc<Mutex<ChargeRunWatch>>,
    /// A mode write refused with mode control off was logged (brief H37: once, not per tick)
    mode_refusal_logged: AtomicBool,
}

/// Warns once per charge run when the battery is not charging
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChargeRunWatch {
    warned: bool,
}

impl ChargeRunWatch {
    /// A new charge run starts: the next "not charging" observation may warn again
    pub fn start_run(&mut self) {
        self.warned = false;
    }

    /// Record one check. Returns true only for the first "not charging" check of the run.
    pub fn observe(&mut self, charging: bool) -> bool {
        if charging || self.warned {
            return false;
        }
        self.warned = true;
        true
    }
}

/// Entity of the SolaX lock state (a control entity: pass the control prefix, brief H36)
#[must_use]
pub fn lock_entity(control_prefix: &str) -> String {
    format!("select.{control_prefix}_lock_state")
}

/// Entity of the SolaX battery awaken button (a control entity: pass the control prefix)
#[must_use]
pub fn awaken_button_entity(control_prefix: &str) -> String {
    format!("button.{control_prefix}_battery_awaken")
}

/// Entity of the SolaX run mode
#[must_use]
pub fn run_mode_entity(inverter_id: &str) -> String {
    format!("sensor.{inverter_id}_run_mode")
}

/// Start of the first ForceCharge block of `inverter_id` that begins between `now` and
/// `now + lookahead`. Blocks without a target list apply to every inverter.
#[must_use]
pub fn upcoming_force_charge(
    blocks: &[ScheduledMode],
    inverter_id: &str,
    now: DateTime<Utc>,
    lookahead: Duration,
) -> Option<DateTime<Utc>> {
    let end = now + chrono::Duration::from_std(lookahead).ok()?;
    blocks
        .iter()
        .filter(|b| b.mode == InverterOperationMode::ForceCharge)
        .filter(|b| {
            b.target_inverters
                .as_ref()
                .is_none_or(|ids| ids.is_empty() || ids.iter().any(|id| id == inverter_id))
        })
        .map(|b| b.block_start)
        .filter(|start| *start >= now && *start <= end)
        .min()
}

/// The pre-wake system acts only outside debug mode (a missing debug config counts as debug)
#[must_use]
pub fn prewake_allowed(debug_enabled: Option<bool>) -> bool {
    debug_enabled == Some(false)
}

/// The executor's gates that also stop a pre-wake: the user's enable switch, the kill-switch
/// lever and an active VPR window
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrewakeGate {
    pub user_enabled: bool,
    pub automation_disabled: bool,
    pub vpr_active: bool,
}

/// Whether a pre-wake press is blocked: automation off, VPR active, or SOC already at the
/// configured maximum (charging would not add anything). An unknown SOC does not block.
#[must_use]
pub fn prewake_blocked(gate: PrewakeGate, soc_percent: Option<f32>, max_battery_soc: f32) -> bool {
    !gate.user_enabled
        || gate.automation_disabled
        || gate.vpr_active
        || soc_percent.is_some_and(|soc| soc >= max_battery_soc)
}

/// The lock is engaged only for the exact state "Locked"; "Unlocked - Advanced" is unlocked
#[must_use]
pub fn is_locked(state: &str) -> bool {
    state == "Locked"
}

/// Run mode is Normal when it starts with "Normal" ("Normal Mode", "Normal (R)")
#[must_use]
pub fn run_mode_is_normal(state: &str) -> bool {
    state.starts_with("Normal")
}

/// Charging when the BDC status is "Charge" or the battery power is positive
#[must_use]
pub fn is_charging(bdc_status: Option<&str>, battery_power_w: Option<f32>) -> bool {
    bdc_status == Some("Charge") || battery_power_w.is_some_and(|p| p > 0.0)
}

/// Shared actuation state of the inverter adapter, so the pre-wake shares the wake limit
#[derive(Resource, Clone)]
pub struct ActuationStateResource(pub Arc<ActuationState>);

/// Pre-wake bookkeeping: the last schedule check, and per inverter the block start of the charge
/// run already handled
#[derive(Resource, Default)]
pub struct PreWakeState {
    last_check: Option<Instant>,
    done: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    /// The "pre-wake off: mode control disabled" line was logged (brief H37)
    mode_control_off_logged: bool,
}

impl ActuationState {
    /// Take the wake slot if the last press is older than `WAKE_INTERVAL`
    pub fn try_begin_wake(&self, now: Instant) -> bool {
        let mut last = self.last_wake.lock();
        if let Some(previous) = *last
            && now.duration_since(previous) < WAKE_INTERVAL
        {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// Whether `command` may be written: with mode control off (brief H37) only the export limit.
#[must_use]
pub fn command_allowed(
    cfg: &ActuationGuardConfig,
    command: &fluxion_core::InverterCommand,
) -> bool {
    cfg.mode_control_enabled || matches!(command, fluxion_core::InverterCommand::SetExportLimit(_))
}

impl ActuationState {
    /// True only the first time a write is refused because mode control is off
    pub fn first_mode_refusal(&self) -> bool {
        !self.mode_refusal_logged.swap(true, Ordering::SeqCst)
    }
}

/// Record a written mode. Returns true when a ForceCharge starts a new charge run.
pub fn note_mode_written(state: &ActuationState, mode: InverterOperationMode) -> bool {
    if mode == InverterOperationMode::ForceCharge {
        !state.charge_run_active.swap(true, Ordering::SeqCst)
    } else {
        state.charge_run_active.store(false, Ordering::SeqCst);
        false
    }
}

async fn notify(client: &HomeAssistantClient, title: &str, message: String, id: String) {
    let data = json!({
        "title": title,
        "message": message,
        "notification_id": id,
    });
    if let Err(e) = client
        .call_service("persistent_notification.create", data)
        .await
    {
        warn!("⚠️ Could not create notification '{}': {}", title, e);
    }
}

/// A missing lock entity is a WARN when a separate control prefix is set (it may be mistyped),
/// else DEBUG as before (brief H36)
#[must_use]
pub fn missing_lock_warns(inverter_id: &str, control_prefix: &str) -> bool {
    inverter_id != control_prefix
}

/// Unlock guard. Reads the lock state; a "Locked" inverter is unlocked and read back.
///
/// Returns `Ok` when the write may proceed. A missing lock entity proceeds (DEBUG log; WARN
/// when a separate control prefix is set, as it may be wrong). A read error other than "not
/// found" fails the write, so the usual retry applies. The lock entity uses `control_prefix`,
/// the inverter's control prefix (brief H36).
pub async fn ensure_unlocked(
    client: &HomeAssistantClient,
    inverter_id: &str,
    control_prefix: &str,
    cfg: &ActuationGuardConfig,
) -> Result<()> {
    let entity = lock_entity(control_prefix);
    let state = match client.get_state(&entity).await {
        Ok(s) => s.state,
        Err(HaError::EntityNotFound(_)) => {
            if missing_lock_warns(inverter_id, control_prefix) {
                warn!(
                    "⚠️ {} not found (control prefix {} of {}); proceeding without a lock check",
                    entity, control_prefix, inverter_id
                );
            } else {
                debug!("🔓 {} not found; proceeding without a lock check", entity);
            }
            return Ok(());
        }
        Err(e) => return Err(anyhow!("cannot read lock state {}: {}", entity, e)),
    };
    if !is_locked(&state) {
        debug!("🔓 {} is '{}'; proceeding", entity, state);
        return Ok(());
    }

    notify(
        client,
        LOCK_NOTIFICATION_TITLE,
        format!("Střídač má nastaven senzor {entity} na Locked!"),
        lock_notification_id(&entity),
    )
    .await;

    if !cfg.auto_unlock {
        error!(
            "🛑 {} is Locked and control.auto_unlock_inverter is off; write skipped",
            entity
        );
        bail!("{} is Locked; write skipped", entity);
    }

    warn!("🔓 {} is Locked; selecting Unlocked before writing", entity);
    client
        .call_service(
            "select.select_option",
            json!({ "entity_id": entity, "option": "Unlocked" }),
        )
        .await
        .map_err(|e| anyhow!("failed to unlock {}: {}", entity, e))?;

    let deadline = Instant::now() + cfg.unlock_timeout;
    loop {
        tokio::time::sleep(cfg.unlock_poll).await;
        if let Ok(read) = client.get_state(&entity).await
            && !is_locked(&read.state)
        {
            info!("🔓 {} read back '{}'; writing", entity, read.state);
            return Ok(());
        }
        if Instant::now() >= deadline {
            error!(
                "🛑 {} still Locked after {:?}; write skipped",
                entity, cfg.unlock_timeout
            );
            bail!("{} still Locked after unlock; write skipped", entity);
        }
    }
}

/// Wake guard, called before ForceCharge. Presses the awaken button when the run mode is not
/// Normal, at most once per `WAKE_INTERVAL`. Failures are logged and do not block the write.
/// The run mode sensor uses `inverter_id`, the button `control_prefix` (brief H36).
pub async fn ensure_awake(
    client: &HomeAssistantClient,
    inverter_id: &str,
    control_prefix: &str,
    state: &ActuationState,
) {
    let run_mode_entity = format!("sensor.{inverter_id}_run_mode");
    let run_mode = match client.get_state(&run_mode_entity).await {
        Ok(s) => s.state,
        Err(e) => {
            debug!("run mode {} unreadable, not waking: {}", run_mode_entity, e);
            return;
        }
    };
    if matches!(run_mode.as_str(), "unavailable" | "unknown") || run_mode_is_normal(&run_mode) {
        return;
    }
    if !state.try_begin_wake(Instant::now()) {
        debug!("run mode '{}' but awaken pressed within 10 min", run_mode);
        return;
    }
    let button = awaken_button_entity(control_prefix);
    info!(
        "⏰ {} is '{}'; pressing {} before charging",
        run_mode_entity, run_mode, button
    );
    if let Err(e) = client
        .call_service("button.press", json!({ "entity_id": button }))
        .await
    {
        warn!("⚠️ Awaken press failed: {}", e);
    }
}

/// Wait until run mode reads Normal. Returns false when `cfg.run_mode_wait` passes first.
async fn wait_for_normal(
    client: &HomeAssistantClient,
    run_mode_entity: &str,
    cfg: &ActuationGuardConfig,
) -> bool {
    let deadline = Instant::now() + cfg.run_mode_wait;
    loop {
        if let Ok(read) = client.get_state(run_mode_entity).await
            && run_mode_is_normal(&read.state)
        {
            info!(
                "✅ {} is '{}'; charge check window starts",
                run_mode_entity, read.state
            );
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(cfg.run_mode_poll).await;
    }
}

/// Start a charge run: reset the watch, wait for run mode Normal, then check the charge status
/// after `charge_check_delay`. If Normal is not reached within `run_mode_wait`, warn once and stop.
pub fn start_charge_check(
    client: Arc<HomeAssistantClient>,
    state: &ActuationState,
    inverter_id: &str,
    battery_power_entity: String,
    cfg: &ActuationGuardConfig,
) -> tokio::task::JoinHandle<()> {
    state.charge_watch.lock().start_run();
    let watch = state.charge_watch.clone();
    let run_active = state.charge_run_active.clone();
    let bdc_entity = format!("sensor.{inverter_id}_inverter_bdc_status");
    let run_mode = run_mode_entity(inverter_id);
    let cfg = cfg.clone();
    tokio::spawn(async move {
        if !wait_for_normal(&client, &run_mode, &cfg).await {
            warn!(
                "⚠️ {} did not reach Normal within {:?} after ForceCharge; charge check given up",
                run_mode, cfg.run_mode_wait
            );
            // No notice once the charge run has ended (mode changed away from ForceCharge)
            if run_active.load(Ordering::SeqCst) && watch.lock().observe(false) {
                notify(
                    &client,
                    CHARGE_NOT_RUNNING_TITLE,
                    format!(
                        "Střídač nepřešel do režimu Normal do {} minut. Nabíjení ze sítě pravděpodobně neběží.",
                        cfg.run_mode_wait.as_secs() / 60
                    ),
                    CHARGE_NOT_RUNNING_NOTIFICATION_ID.to_string(),
                )
                .await;
            }
            return;
        }
        tokio::time::sleep(cfg.charge_check_delay).await;
        // The SolaX Ultra has no BDC status sensor: a missing one is not an error, power decides
        let bdc = client
            .get_state_optional(&bdc_entity)
            .await
            .ok()
            .flatten()
            .map(|s| s.state);
        let power = client
            .get_state(&battery_power_entity)
            .await
            .ok()
            .and_then(|s| s.state.parse::<f32>().ok());
        if bdc.is_none() && power.is_none() {
            debug!("charge check skipped: no BDC status or battery power entity");
            return;
        }
        if is_charging(bdc.as_deref(), power) {
            info!("✅ Battery is charging from the grid");
            return;
        }
        if !run_active.load(Ordering::SeqCst) {
            debug!("charge run ended before the check; no charge notice");
            return;
        }
        if watch.lock().observe(false) {
            warn!(
                "⚠️ ForceCharge sent but the battery is not charging (BDC '{:?}', power {:?} W)",
                bdc, power
            );
            notify(
                &client,
                CHARGE_NOT_RUNNING_TITLE,
                "Nabíjení ze sítě se nespustilo. Zkontrolujte režim střídače.".to_string(),
                CHARGE_NOT_RUNNING_NOTIFICATION_ID.to_string(),
            )
            .await;
        }
    })
}

/// Pre-wake one inverter ahead of the charge run starting at `run_start`: press the awaken
/// button when run mode is not Normal and no gate blocks it (`blocked`, see `prewake_blocked`).
///
/// Each charge run is keyed by its block start, so a run is pre-woken at most once. Across runs,
/// presses are limited by the shared `WAKE_INTERVAL` (10 minutes). A read error, a blocked gate
/// or a wake limit hit leaves the run open for the next check. The run mode sensor uses
/// `inverter_id`, the awaken button `control_prefix` (brief H36).
pub async fn prewake_before_run(
    client: &HomeAssistantClient,
    inverter_id: &str,
    control_prefix: &str,
    run_start: DateTime<Utc>,
    state: &ActuationState,
    done: &Mutex<HashMap<String, DateTime<Utc>>>,
    blocked: bool,
) {
    if blocked {
        debug!("pre-wake for {inverter_id} blocked by automation gates");
        return;
    }
    if done.lock().get(inverter_id) == Some(&run_start) {
        return;
    }
    let run_mode_entity = run_mode_entity(inverter_id);
    let run_mode = match client.get_state(&run_mode_entity).await {
        Ok(s) => s.state,
        Err(e) => {
            debug!("pre-wake: run mode {} unreadable: {}", run_mode_entity, e);
            return;
        }
    };
    if matches!(run_mode.as_str(), "unavailable" | "unknown") {
        return;
    }
    if run_mode_is_normal(&run_mode) {
        debug!(
            "pre-wake: {} already Normal for {}",
            run_mode_entity, run_start
        );
        done.lock().insert(inverter_id.to_string(), run_start);
        return;
    }
    if !state.try_begin_wake(Instant::now()) {
        debug!("pre-wake: awaken pressed within 10 min; retried at the next check");
        return;
    }
    let button = awaken_button_entity(control_prefix);
    info!(
        "⏰ Pre-wake: {} is '{}', charge run at {}; pressing {}",
        run_mode_entity, run_mode, run_start, button
    );
    if let Err(e) = client
        .call_service("button.press", json!({ "entity_id": button }))
        .await
    {
        warn!("⚠️ Pre-wake press failed: {}", e);
    }
    done.lock().insert(inverter_id.to_string(), run_start);
}

/// Update system: ten minutes before a scheduled ForceCharge, pre-wake the master or independent
/// inverters whose run mode is not Normal. Checked every `PREWAKE_CHECK_INTERVAL`. Never in debug,
/// and never while the executor would hold writes: user disabled, the kill-switch lever, an active
/// VPR window, the lever startup wait, or SOC at the maximum. Never with mode control off (H37).
#[expect(clippy::too_many_arguments)]
pub fn prewake_system(
    mut state: ResMut<PreWakeState>,
    ha_client: Option<Res<HaClientResource>>,
    wake_state: Option<Res<ActuationStateResource>>,
    system_config: Option<Res<SystemConfig>>,
    debug_mode: Option<Res<DebugModeConfig>>,
    user_control: Option<Res<UserControlResource>>,
    levers: Option<Res<UserLeversResource>>,
    timezone: Option<Res<TimezoneConfig>>,
    mut lever_startup: Local<LeversStartup>,
    schedules: Query<&OperationSchedule>,
    batteries: Query<(&Inverter, &BatteryStatus)>,
) {
    let (Some(client), Some(wake), Some(config)) = (ha_client, wake_state, system_config) else {
        return;
    };
    if !prewake_allowed(debug_mode.as_ref().map(|d| d.enabled)) {
        return;
    }
    // Mode control off (brief H37): no awaken press ahead of a planned charge
    if !config.control_config.mode_control_enabled {
        if !state.mode_control_off_logged {
            info!("Mode control disabled: no battery pre-wake (awaken) presses");
            state.mode_control_off_logged = true;
        }
        return;
    }
    state.mode_control_off_logged = false;
    let now_instant = Instant::now();
    if state
        .last_check
        .is_some_and(|t| now_instant.duration_since(t) < PREWAKE_CHECK_INTERVAL)
    {
        return;
    }
    state.last_check = Some(now_instant);
    // The executor waits for the first lever sync before writing any mode; so does the pre-wake
    if lever_startup_wait(levers.as_deref(), &mut lever_startup, now_instant) {
        return;
    }

    // Same derivation as the executor: a lever set is used only when available
    let now = Utc::now();
    let tz: chrono_tz::Tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let available_levers = levers.as_deref().filter(|l| l.available).map(|l| &l.levers);
    let gate = PrewakeGate {
        user_enabled: user_control.as_ref().is_none_or(|uc| uc.state.enabled),
        automation_disabled: available_levers.is_some_and(|l| l.automation_disabled),
        vpr_active: available_levers
            .and_then(|l| l.vpr)
            .is_some_and(|w| w.phase(now, &tz) == VprPhase::Active),
    };

    for inverter in config
        .inverters
        .iter()
        .filter(|inv| !matches!(inv.topology, InverterTopology::Slave { .. }))
    {
        let run_start = schedules
            .iter()
            .filter_map(|s| {
                upcoming_force_charge(&s.scheduled_blocks, &inverter.id, now, PREWAKE_LOOKAHEAD)
            })
            .min();
        let Some(run_start) = run_start else {
            continue;
        };
        let soc = batteries
            .iter()
            .find(|(inv, _)| inv.id == inverter.id)
            .map(|(_, battery)| f32::from(battery.soc_percent));
        let blocked = prewake_blocked(gate, soc, config.control_config.max_battery_soc);
        let client = client.0.clone();
        let wake = wake.0.clone();
        let done = state.done.clone();
        let inverter_id = inverter.id.clone();
        let control_prefix = inverter
            .control_prefix_override()
            .unwrap_or(&inverter.id)
            .to_owned();
        tokio::spawn(async move {
            prewake_before_run(
                &client,
                &inverter_id,
                &control_prefix,
                run_start,
                &wake,
                &done,
                blocked,
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_locked_counts_as_locked() {
        assert!(is_locked("Locked"));
        assert!(!is_locked("Unlocked"));
        assert!(!is_locked("Unlocked - Advanced"));
        assert!(!is_locked("unavailable"));
    }

    #[test]
    fn run_mode_normal_prefix() {
        assert!(run_mode_is_normal("Normal Mode"));
        assert!(run_mode_is_normal("Normal (R)"));
        assert!(!run_mode_is_normal("Idle Mode"));
        assert!(!run_mode_is_normal("Checking"));
    }

    #[test]
    fn charging_from_bdc_or_power() {
        assert!(is_charging(Some("Charge"), None));
        assert!(is_charging(Some("Stop"), Some(350.0)));
        assert!(!is_charging(Some("Stop"), Some(0.0)));
        assert!(!is_charging(Some("Discharge"), Some(-100.0)));
        assert!(!is_charging(None, None));
    }

    #[test]
    fn ultra_without_bdc_status_uses_battery_power() {
        // SolaX Ultra has no inverter_bdc_status: charging is judged by battery power alone
        assert!(is_charging(None, Some(350.0)));
        assert!(!is_charging(None, Some(0.0)));
        assert!(!is_charging(None, Some(-120.0)));
    }

    #[test]
    fn charge_warning_fires_once_per_run() {
        let mut watch = ChargeRunWatch::default();
        watch.start_run();
        assert!(watch.observe(false));
        assert!(!watch.observe(false));
        assert!(!watch.observe(true));
        // A new charge run warns again
        watch.start_run();
        assert!(watch.observe(false));
    }

    #[test]
    fn wake_at_most_once_per_ten_minutes() {
        let state = ActuationState::default();
        let t0 = Instant::now();
        assert!(state.try_begin_wake(t0));
        assert!(!state.try_begin_wake(t0 + Duration::from_secs(5 * 60)));
        assert!(state.try_begin_wake(t0 + WAKE_INTERVAL));
    }

    #[test]
    fn new_charge_run_only_on_first_force_charge() {
        let state = ActuationState::default();
        assert!(note_mode_written(
            &state,
            InverterOperationMode::ForceCharge
        ));
        assert!(!note_mode_written(
            &state,
            InverterOperationMode::ForceCharge
        ));
        assert!(!note_mode_written(&state, InverterOperationMode::SelfUse));
        assert!(note_mode_written(
            &state,
            InverterOperationMode::ForceCharge
        ));
    }

    #[test]
    fn lock_entity_name() {
        assert_eq!(lock_entity("solax"), "select.solax_lock_state");
    }

    fn block(
        start: DateTime<Utc>,
        mode: InverterOperationMode,
        targets: Option<Vec<&str>>,
    ) -> ScheduledMode {
        ScheduledMode {
            block_start: start,
            duration_minutes: 15,
            target_inverters: targets.map(|ids| ids.into_iter().map(str::to_string).collect()),
            mode,
            reason: String::new(),
            decision_uid: None,
            debug_info: None,
        }
    }

    #[test]
    fn prewake_finds_force_charge_ten_minutes_ahead_only() {
        let now: DateTime<Utc> = "2026-10-15T17:40:00Z".parse().unwrap();
        let lookahead = Duration::from_secs(10 * 60);
        let force = InverterOperationMode::ForceCharge;
        let at = |min: i64| now + chrono::Duration::minutes(min);

        // Starts in 9 minutes: due
        let blocks = [block(at(9), force, None)];
        assert_eq!(
            upcoming_force_charge(&blocks, "solax", now, lookahead),
            Some(at(9))
        );
        // Starts in 11 minutes: not yet
        let blocks = [block(at(11), force, None)];
        assert_eq!(
            upcoming_force_charge(&blocks, "solax", now, lookahead),
            None
        );
        // Other modes and past charge blocks do not count
        let blocks = [
            block(at(5), InverterOperationMode::SelfUse, None),
            block(at(-5), force, None),
        ];
        assert_eq!(
            upcoming_force_charge(&blocks, "solax", now, lookahead),
            None
        );
        // A charge block of another inverter does not count; an empty target list is all inverters
        let blocks = [block(at(5), force, Some(vec!["other"]))];
        assert_eq!(
            upcoming_force_charge(&blocks, "solax", now, lookahead),
            None
        );
        let blocks = [block(at(5), force, Some(vec![]))];
        assert_eq!(
            upcoming_force_charge(&blocks, "solax", now, lookahead),
            Some(at(5))
        );
    }

    #[test]
    fn prewake_never_runs_in_debug_mode() {
        assert!(!prewake_allowed(None));
        assert!(!prewake_allowed(Some(true)));
        assert!(prewake_allowed(Some(false)));
    }
}

#[cfg(test)]
mod prewake_and_charge_tests {
    use super::*;
    use mockito::{Matcher, Server};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;

    fn state_body(entity: &str, state: &str) -> String {
        json!({
            "entity_id": entity,
            "state": state,
            "attributes": {},
            "last_changed": "2026-10-15T19:00:00Z",
            "last_updated": "2026-10-15T19:00:00Z",
        })
        .to_string()
    }

    fn fast_cfg(delay: Duration, wait: Duration) -> ActuationGuardConfig {
        ActuationGuardConfig {
            charge_check_delay: delay,
            run_mode_wait: wait,
            run_mode_poll: Duration::from_millis(5),
            ..ActuationGuardConfig::default()
        }
    }

    #[tokio::test]
    async fn prewake_presses_once_per_charge_run_when_idle() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Idle Mode"))
            .expect_at_least(1)
            .create_async()
            .await;
        let press = server
            .mock("POST", "/api/services/button/press")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "button.solax_battery_awaken"}),
            ))
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let state = ActuationState::default();
        let done = Mutex::new(HashMap::new());
        let run: DateTime<Utc> = "2026-10-15T19:45:00Z".parse().unwrap();

        prewake_before_run(&client, "solax", "solax", run, &state, &done, false).await;
        // The same charge run is not pre-woken twice
        prewake_before_run(&client, "solax", "solax", run, &state, &done, false).await;
        // A later run within the wake limit is held back as well
        let later = run + chrono::Duration::minutes(15);
        prewake_before_run(&client, "solax", "solax", later, &state, &done, false).await;

        press.assert_async().await;
    }

    /// Review nit 2: a missing lock entity is a WARN only with a separate control prefix
    #[tokio::test]
    async fn missing_lock_entity_proceeds_and_warns_only_with_a_control_prefix() {
        assert!(!missing_lock_warns("solax", "solax"));
        assert!(missing_lock_warns("solax", "solax_inverter"));

        let mut server = Server::new_async().await;
        let _missing = server
            .mock("GET", "/api/states/select.solax_inverter_lock_state")
            .with_status(404)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        ensure_unlocked(
            &client,
            "solax",
            "solax_inverter",
            &ActuationGuardConfig::default(),
        )
        .await
        .unwrap();
    }

    /// Brief H36: the run mode is a sensor (entity prefix), the awaken button a control
    #[tokio::test]
    async fn prewake_presses_the_button_of_the_control_prefix() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Idle Mode"))
            .expect_at_least(1)
            .create_async()
            .await;
        let press = server
            .mock("POST", "/api/services/button/press")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "button.solax_inverter_battery_awaken"}),
            ))
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let state = ActuationState::default();
        let done = Mutex::new(HashMap::new());
        let run: DateTime<Utc> = "2026-10-15T19:45:00Z".parse().unwrap();

        prewake_before_run(
            &client,
            "solax",
            "solax_inverter",
            run,
            &state,
            &done,
            false,
        )
        .await;

        press.assert_async().await;
        assert_eq!(awaken_button_entity("solax"), "button.solax_battery_awaken");
        assert_eq!(
            lock_entity("solax_inverter"),
            "select.solax_inverter_lock_state"
        );
    }

    #[tokio::test]
    async fn prewake_does_not_press_when_already_normal() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Normal Mode"))
            .expect_at_least(1)
            .create_async()
            .await;
        let press = server
            .mock("POST", "/api/services/button/press")
            .with_body("[]")
            .expect(0)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let state = ActuationState::default();
        let done = Mutex::new(HashMap::new());
        let run: DateTime<Utc> = "2026-10-15T19:45:00Z".parse().unwrap();

        prewake_before_run(&client, "solax", "solax", run, &state, &done, false).await;

        press.assert_async().await;
    }

    #[tokio::test]
    async fn charge_check_waits_for_normal_before_its_window() {
        let mut server = Server::new_async().await;
        let log: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));
        let reads = Arc::new(AtomicUsize::new(0));
        let (run_log, run_reads) = (log.clone(), reads.clone());
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body_from_request(move |_| {
                // Idle for the first two reads, then Normal
                let n = run_reads.fetch_add(1, Ordering::SeqCst);
                let (mode, tag) = if n < 2 {
                    ("Idle Mode", "run:Idle")
                } else {
                    ("Normal Mode", "run:Normal")
                };
                run_log.lock().unwrap().push(tag);
                state_body("sensor.solax_run_mode", mode).into_bytes()
            })
            .create_async()
            .await;
        let power_log = log.clone();
        let _power = server
            .mock("GET", "/api/states/sensor.solax_battery_power_charge")
            .with_body_from_request(move |_| {
                power_log.lock().unwrap().push("power");
                state_body("sensor.solax_battery_power_charge", "800").into_bytes()
            })
            .expect_at_least(1)
            .create_async()
            .await;
        let _bdc = server
            .mock("GET", "/api/states/sensor.solax_inverter_bdc_status")
            .with_status(404)
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let state = ActuationState::default();
        let cfg = fast_cfg(Duration::from_millis(20), Duration::from_secs(5));

        let handle = start_charge_check(
            client,
            &state,
            "solax",
            "sensor.solax_battery_power_charge".to_string(),
            &cfg,
        );
        handle.await.unwrap();

        let log = log.lock().unwrap().clone();
        let first_normal = log
            .iter()
            .position(|t| *t == "run:Normal")
            .expect("Normal read");
        assert!(log[..first_normal].iter().all(|t| *t == "run:Idle"));
        let first_power = log.iter().position(|t| *t == "power").expect("power read");
        assert!(
            first_power > first_normal,
            "charge read before Normal: {log:?}"
        );
    }

    #[tokio::test]
    async fn charge_window_starts_at_once_when_already_normal() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Normal Mode"))
            .expect_at_least(1)
            .create_async()
            .await;
        let _power = server
            .mock("GET", "/api/states/sensor.solax_battery_power_charge")
            .with_body(state_body("sensor.solax_battery_power_charge", "0"))
            .expect_at_least(1)
            .create_async()
            .await;
        let _bdc = server
            .mock("GET", "/api/states/sensor.solax_inverter_bdc_status")
            .with_status(404)
            .create_async()
            .await;
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let state = ActuationState::default();
        note_mode_written(&state, InverterOperationMode::ForceCharge);
        let cfg = fast_cfg(Duration::from_millis(10), Duration::from_secs(5));

        start_charge_check(
            client,
            &state,
            "solax",
            "sensor.solax_battery_power_charge".to_string(),
            &cfg,
        )
        .await
        .unwrap();

        // Not charging after the window: one notification
        notify.assert_async().await;
    }

    #[tokio::test]
    async fn charge_check_gives_up_without_normal_and_warns_once() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Idle Mode"))
            .expect_at_least(2)
            .create_async()
            .await;
        let power = server
            .mock("GET", "/api/states/sensor.solax_battery_power_charge")
            .with_body(state_body("sensor.solax_battery_power_charge", "0"))
            .expect(0)
            .create_async()
            .await;
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let state = ActuationState::default();
        note_mode_written(&state, InverterOperationMode::ForceCharge);
        let cfg = fast_cfg(Duration::from_millis(10), Duration::from_millis(60));

        start_charge_check(
            client,
            &state,
            "solax",
            "sensor.solax_battery_power_charge".to_string(),
            &cfg,
        )
        .await
        .unwrap();

        // Gave up waiting for Normal: no charge reading, one notification
        power.assert_async().await;
        notify.assert_async().await;
    }

    fn gate(user_enabled: bool, automation_disabled: bool, vpr_active: bool) -> PrewakeGate {
        PrewakeGate {
            user_enabled,
            automation_disabled,
            vpr_active,
        }
    }

    #[test]
    fn prewake_gate_blocks_like_the_executor() {
        let open = gate(true, false, false);
        assert!(!prewake_blocked(open, Some(40.0), 100.0));
        assert!(
            !prewake_blocked(open, None, 100.0),
            "unknown SOC does not block"
        );
        // Kill switch lever, VPR window, user disabled
        assert!(prewake_blocked(gate(true, true, false), Some(40.0), 100.0));
        assert!(prewake_blocked(gate(true, false, true), Some(40.0), 100.0));
        assert!(prewake_blocked(
            gate(false, false, false),
            Some(40.0),
            100.0
        ));
        // SOC at or above the configured maximum
        assert!(prewake_blocked(open, Some(100.0), 100.0));
        assert!(prewake_blocked(open, Some(95.0), 90.0));
        assert!(!prewake_blocked(open, Some(89.0), 90.0));
    }

    #[tokio::test]
    async fn prewake_never_presses_while_a_gate_blocks() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Idle Mode"))
            .expect_at_least(0)
            .create_async()
            .await;
        let press = server
            .mock("POST", "/api/services/button/press")
            .with_body("[]")
            .expect(0)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        let run: DateTime<Utc> = "2026-10-15T19:45:00Z".parse().unwrap();
        let blocked_cases = [
            // kill switch lever
            (gate(true, true, false), Some(40.0)),
            // VPR window active
            (gate(true, false, true), Some(40.0)),
            // user control disabled
            (gate(false, false, false), Some(40.0)),
            // SOC at the maximum (100 %)
            (gate(true, false, false), Some(100.0)),
        ];
        for (gate, soc) in blocked_cases {
            let state = ActuationState::default();
            let done = Mutex::new(HashMap::new());
            let blocked = prewake_blocked(gate, soc, 100.0);
            assert!(blocked, "case should be blocked: {gate:?} {soc:?}");
            prewake_before_run(&client, "solax", "solax", run, &state, &done, blocked).await;
            // Not marked done either: the run is still open once the gate lifts
            assert!(done.lock().is_empty());
        }
        press.assert_async().await;
    }

    #[tokio::test]
    async fn charge_check_gives_no_notice_after_the_run_ended() {
        let mut server = Server::new_async().await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body(state_body("sensor.solax_run_mode", "Normal Mode"))
            .expect_at_least(1)
            .create_async()
            .await;
        let _power = server
            .mock("GET", "/api/states/sensor.solax_battery_power_charge")
            .with_body(state_body("sensor.solax_battery_power_charge", "0"))
            .expect_at_least(0)
            .create_async()
            .await;
        let _bdc = server
            .mock("GET", "/api/states/sensor.solax_inverter_bdc_status")
            .with_status(404)
            .create_async()
            .await;
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body("[]")
            .expect(0)
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let state = ActuationState::default();
        note_mode_written(&state, InverterOperationMode::ForceCharge);
        let cfg = fast_cfg(Duration::from_millis(50), Duration::from_secs(5));

        let handle = start_charge_check(
            client,
            &state,
            "solax",
            "sensor.solax_battery_power_charge".to_string(),
            &cfg,
        );
        // The mode changes away from ForceCharge before the charge check warns
        note_mode_written(&state, InverterOperationMode::SelfUse);
        handle.await.unwrap();

        notify.assert_async().await;
    }
}

/// Brief H37: with `control.mode_control_enabled: false` the pre-wake, the HotStandBy guard and
/// the lock watcher write nothing to the inverter
#[cfg(test)]
mod mode_control_off_tests {
    use super::*;
    use crate::ha::dashboard_entities::{
        DashboardChannels, DashboardState, LockReading, lock_guard_system,
    };
    use crate::ha::hot_standby::{
        HotStandbyChannel, HotStandbyKind, HotStandbyState, HotStandbyTarget, hot_standby_system,
        spawn_hot_standby_resolver,
    };
    use crate::ha::plugin::HaConfig;
    use bevy_ecs::schedule::Schedule;
    use fluxion_types::config::{
        Currency, InverterConfig, PriceSchedule, PricingConfig, SystemSettingsConfig,
    };
    use mockito::{Matcher, Server, ServerGuard};
    use std::sync::atomic::AtomicUsize;

    /// Counter of the requests one mock served
    fn counter() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (count.clone(), count)
    }

    fn system_config(mode_control_enabled: bool) -> SystemConfig {
        SystemConfig {
            inverters: vec![InverterConfig {
                id: "solax".to_owned(),
                inverter_type: fluxion_types::InverterType::Solax,
                entity_prefix: "solax".to_owned(),
                control_entity_prefix: String::new(),
                topology: InverterTopology::Independent,
                pv_extra_prefixes: Vec::new(),
            }],
            pricing_config: PricingConfig {
                spot_price_entity: "sensor.spot_price".to_owned(),
                tomorrow_price_entity: None,
                buy_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_price_czk: PriceSchedule::Flat(4.0),
                fixed_sell_price_czk: PriceSchedule::Flat(2.0),
                spot_buy_fee_czk: 0.5,
                spot_sell_fee_czk: 0.5,
                hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_owned(),
                hdo_low_tariff_czk: 0.5,
                hdo_high_tariff_czk: 1.8,
            },
            control_config: fluxion_types::ControlConfig {
                maximum_export_power_w: 8000,
                mode_control_enabled,
                ..Default::default()
            },
            system_config: SystemSettingsConfig {
                update_interval_secs: 60,
                debug_mode: false,
                display_currency: Currency::CZK,
                language: Default::default(),
                timezone: None,
                display_timezone: None,
            },
            strategies_config: Default::default(),
            history: Default::default(),
            solar_forecast: Default::default(),
            remote_access: Default::default(),
            notifications: Default::default(),
        }
    }

    /// World with the HA client of `server`, debug off and the given mode control
    fn world(server: &ServerGuard, mode_control_enabled: bool) -> World {
        let client = Arc::new(HomeAssistantClient::new(server.url(), "test_token").unwrap());
        let mut world = World::new();
        world.insert_resource(HaClientResource(client));
        world.insert_resource(system_config(mode_control_enabled));
        world.insert_resource(DebugModeConfig {
            enabled: false,
            ..Default::default()
        });
        world
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Pre-wake: a ForceCharge 5 minutes ahead and run mode Standby. With mode control on the run
    /// mode is read and the button pressed; with it off nothing is read or pressed.
    async fn prewake_requests(mode_control_enabled: bool) -> (usize, usize) {
        let mut server = Server::new_async().await;
        let (reads, read_count) = counter();
        server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body_from_request(move |_| {
                read_count.fetch_add(1, Ordering::SeqCst);
                br#"{"entity_id":"sensor.solax_run_mode","state":"Standby Mode","attributes":{},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}"#.to_vec()
            })
            .create_async()
            .await;
        let (presses, press_count) = counter();
        server
            .mock("POST", "/api/services/button/press")
            .with_body_from_request(move |_| {
                press_count.fetch_add(1, Ordering::SeqCst);
                b"[]".to_vec()
            })
            .create_async()
            .await;
        let mut world = world(&server, mode_control_enabled);
        world.insert_resource(PreWakeState::default());
        world.insert_resource(ActuationStateResource(Arc::new(ActuationState::default())));
        let now = Utc::now();
        world.spawn(OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now + chrono::Duration::minutes(5),
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::ForceCharge,
                reason: "schedule".to_owned(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        });
        let mut schedule = Schedule::default();
        schedule.add_systems(prewake_system);
        schedule.run(&mut world);
        settle().await;
        (reads.load(Ordering::SeqCst), presses.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn prewake_presses_nothing_with_mode_control_off() {
        assert_eq!(prewake_requests(false).await, (0, 0));
        // The same setup with mode control on does press, so the test can see a press
        assert_eq!(prewake_requests(true).await, (1, 1));
    }

    #[tokio::test]
    async fn hot_standby_guard_writes_nothing_with_mode_control_off() {
        let mut server = Server::new_async().await;
        let any_read = server
            .mock("GET", Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let any_write = server
            .mock("POST", Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let mut world = world(&server, false);
        world.insert_resource(HaConfig::default());
        world.insert_resource(HotStandbyChannel::default());
        world.insert_resource(HotStandbyState::default());
        // A target that reads "off" would be switched on with mode control on
        let tx = world.resource::<HotStandbyChannel>().tx.clone();
        tx.send((
            "solax".to_owned(),
            Some(HotStandbyTarget {
                entity_id: "switch.solax_hotstandby".to_owned(),
                kind: HotStandbyKind::Switch,
            }),
        ))
        .unwrap();
        let mut schedule = Schedule::default();
        schedule.add_systems((spawn_hot_standby_resolver, hot_standby_system).chain());
        schedule.run(&mut world);
        schedule.run(&mut world);
        settle().await;
        any_read.assert_async().await;
        any_write.assert_async().await;
    }

    async fn lock_event_requests(mode_control_enabled: bool) -> usize {
        let mut server = Server::new_async().await;
        let _notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body("[]")
            .create_async()
            .await;
        let (unlocks, unlock_count) = counter();
        server
            .mock("POST", "/api/services/select/select_option")
            .with_body_from_request(move |_| {
                unlock_count.fetch_add(1, Ordering::SeqCst);
                b"[]".to_vec()
            })
            .create_async()
            .await;
        let mut world = world(&server, mode_control_enabled);
        world.insert_resource(DashboardState::default());
        world.insert_resource(DashboardChannels::default());
        world
            .resource::<DashboardChannels>()
            .lock_tx
            .send(LockReading {
                entity_id: "select.solax_lock_state".to_owned(),
                state: "Locked".to_owned(),
            })
            .unwrap();
        let mut schedule = Schedule::default();
        schedule.add_systems(lock_guard_system);
        schedule.run(&mut world);
        settle().await;
        unlocks.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn lock_event_is_not_unlocked_with_mode_control_off() {
        assert_eq!(lock_event_requests(false).await, 0);
        assert_eq!(lock_event_requests(true).await, 1);
    }
}
