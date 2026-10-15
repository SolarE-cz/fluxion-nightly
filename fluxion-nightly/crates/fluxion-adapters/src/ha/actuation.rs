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
//! - charge check: a few minutes after ForceCharge, FluxION warns if the battery is not charging.
//!
//! Pure decisions are separated from the HA calls so they can be unit tested.

use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::ha::client::HomeAssistantClient;
use crate::ha::dashboard_entities::{LOCK_NOTIFICATION_TITLE, lock_notification_id};
use crate::ha::errors::HaError;
use fluxion_core::InverterOperationMode;

/// Stable notification id for the charge-not-running warning
pub const CHARGE_NOT_RUNNING_NOTIFICATION_ID: &str = "fluxion_charge_not_running";
/// Title of the charge-not-running notification (brief H11)
pub const CHARGE_NOT_RUNNING_TITLE: &str = "FluxION: nabíjení ze sítě neběží";

/// Minimum time between two awaken presses
pub const WAKE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Settings of the pre-actuation guards
#[derive(Debug, Clone)]
pub struct ActuationGuardConfig {
    /// `control.auto_unlock_inverter`: unlock a Locked inverter, otherwise skip the write
    pub auto_unlock: bool,
    /// How long to poll the lock read-back after selecting "Unlocked"
    pub unlock_timeout: Duration,
    /// Interval between lock read-backs
    pub unlock_poll: Duration,
    /// Delay between a ForceCharge and the charge-running check
    pub charge_check_delay: Duration,
}

impl Default for ActuationGuardConfig {
    fn default() -> Self {
        Self {
            auto_unlock: true,
            unlock_timeout: Duration::from_secs(10),
            unlock_poll: Duration::from_secs(1),
            charge_check_delay: Duration::from_secs(5 * 60),
        }
    }
}

/// Mutable guard state of one adapter
#[derive(Default)]
pub struct ActuationState {
    last_wake: Mutex<Option<Instant>>,
    charge_run_active: AtomicBool,
    charge_watch: Arc<Mutex<ChargeRunWatch>>,
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

/// Entity of the SolaX lock state
#[must_use]
pub fn lock_entity(inverter_id: &str) -> String {
    format!("select.{inverter_id}_lock_state")
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

/// Unlock guard. Reads the lock state; a "Locked" inverter is unlocked and read back.
///
/// Returns `Ok` when the write may proceed. A missing lock entity proceeds (DEBUG log). A
/// read error other than "not found" fails the write, so the usual retry applies.
pub async fn ensure_unlocked(
    client: &HomeAssistantClient,
    inverter_id: &str,
    cfg: &ActuationGuardConfig,
) -> Result<()> {
    let entity = lock_entity(inverter_id);
    let state = match client.get_state(&entity).await {
        Ok(s) => s.state,
        Err(HaError::EntityNotFound(_)) => {
            debug!("🔓 {} not found; proceeding without a lock check", entity);
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
pub async fn ensure_awake(client: &HomeAssistantClient, inverter_id: &str, state: &ActuationState) {
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
    let button = format!("button.{inverter_id}_battery_awaken");
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

/// Start a charge run: reset the watch and check the charge status after the configured delay.
pub fn start_charge_check(
    client: Arc<HomeAssistantClient>,
    state: &ActuationState,
    inverter_id: &str,
    battery_power_entity: String,
    delay: Duration,
) {
    state.charge_watch.lock().start_run();
    let watch = state.charge_watch.clone();
    let bdc_entity = format!("sensor.{inverter_id}_inverter_bdc_status");
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let bdc = client.get_state(&bdc_entity).await.ok().map(|s| s.state);
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
    });
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
}
