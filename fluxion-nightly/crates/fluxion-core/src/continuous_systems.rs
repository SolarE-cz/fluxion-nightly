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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use fluxion_types::config::ControlConfig;
use fluxion_types::{VprMode, VprPhase};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, trace, warn};

use crate::{
    components::*,
    debug::DebugModeConfig,
    execution::*,
    traits::{InverterDataSource, PriceDataSource},
};
use std::time::Instant;

/// Where the export limit to restore is persisted while curtailed (survives restarts).
pub const EXPORT_LIMIT_RESTORE_PATH: &str = "/data/export_limit_restore.json";

/// Export curtailment state. `restore_limits_w` has an entry for every curtailed inverter.
#[derive(Resource, Debug, Default)]
pub struct ExportCurtailmentState {
    /// Per inverter id: export limit (W) to restore when curtailment is no longer wanted.
    pub restore_limits_w: BTreeMap<String, u32>,
    /// Old single-value file: the master's restore limit, moved to the master on the first run.
    pub legacy_restore_limit_w: Option<u32>,
    /// Limit last written per inverter, watched for read-back drift.
    pub limit_watch: BTreeMap<String, ExportLimitWatch>,
    /// Where `restore_limits_w` is persisted. `None` keeps it in memory only.
    pub persist_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
struct ExportLimitRestoreFile {
    #[serde(default)]
    restore_limits_w: BTreeMap<String, u32>,
    /// Old single-value format (master only), read but never written
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_limit_w: Option<u32>,
}

impl ExportCurtailmentState {
    /// Load persisted restore limits (if any) so a restart while curtailed restores them.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| serde_json::from_str::<ExportLimitRestoreFile>(&json).ok())
            .unwrap_or_default();
        Self {
            restore_limits_w: file.restore_limits_w,
            legacy_restore_limit_w: file.restore_limit_w,
            persist_path: Some(path),
            ..Default::default()
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let result = if self.restore_limits_w.is_empty() {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            serde_json::to_string(&ExportLimitRestoreFile {
                restore_limits_w: self.restore_limits_w.clone(),
                restore_limit_w: None,
            })
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(path, json))
        };
        if let Err(e) = result {
            warn!("Failed to persist export limit state to {:?}: {}", path, e);
        }
    }
}

/// Wrapper resource for the price data source
#[derive(Resource)]
pub struct PriceDataSourceResource(pub Arc<dyn PriceDataSource>);

/// Wrapper resource for the inverter data source
#[derive(Resource)]
pub struct InverterDataSourceResource(pub Arc<dyn InverterDataSource>);

/// Resource to track last battery SOC history fetch time
#[derive(Resource)]
pub struct BatteryHistoryFetchTimer {
    pub last_fetch: Instant,
    pub interval_secs: u64,
}

impl Default for BatteryHistoryFetchTimer {
    fn default() -> Self {
        Self {
            last_fetch: Instant::now(),
            interval_secs: 5 * 60, // Fetch history every 5 minutes
        }
    }
}

/// Resource to track if initial mode sync has been completed for each inverter
/// This allows bypassing the debounce on the first mode change after startup
#[derive(Resource, Default)]
pub struct InitialModeSyncTracker {
    /// Set of inverter IDs that have completed initial sync
    pub synced_inverters: std::collections::HashSet<String>,
}

/// System that executes scheduled mode changes
/// Runs every update cycle to check if mode changes are needed
///
/// On initial startup, compares against ACTUAL inverter mode and applies immediately.
/// After initial sync, uses debounce interval between mode changes.
/// Map a VPR option to the FluxION mode (SolaX mapping from the legacy levers contract).
/// `None` means no override.
pub fn vpr_inverter_mode(mode: VprMode) -> Option<InverterOperationMode> {
    match mode {
        VprMode::None => None,
        VprMode::Normal => Some(InverterOperationMode::SelfUse),
        VprMode::DischargeBattery => Some(InverterOperationMode::ForceDischarge),
        VprMode::ChargeBattery | VprMode::ChargeFromGrid => {
            Some(InverterOperationMode::ForceCharge)
        }
        VprMode::NoDischarge => Some(InverterOperationMode::BackUpMode),
        VprMode::SellToGrid => Some(InverterOperationMode::FeedinPriority),
    }
}

/// SOC safety for a mode change: true when the change must be skipped.
fn soc_blocks_mode(mode: InverterOperationMode, soc: f32, control: &ControlConfig) -> bool {
    (mode == InverterOperationMode::ForceCharge && soc >= control.max_battery_soc)
        || (mode == InverterOperationMode::ForceDischarge && soc <= control.min_battery_soc)
}

/// Once-per-activation warnings of the schedule system (kept in one `Local` to stay within the
/// Bevy system parameter limit).
#[derive(Default)]
pub struct ScheduleWarnings {
    vpr_discharge: bool,
    fixed_slots: BTreeSet<String>,
}

/// Defence in depth: ForceDischarge is never sent while `control.force_discharge_enabled` is false.
/// The mode actually sent is returned (SelfUse instead of ForceDischarge).
fn permitted_mode(
    mode: InverterOperationMode,
    force_discharge_enabled: bool,
    inverter_id: &str,
) -> InverterOperationMode {
    if mode == InverterOperationMode::ForceDischarge && !force_discharge_enabled {
        error!(
            "🛑 ForceDischarge blocked for {}: control.force_discharge_enabled is false, sending SelfUse",
            inverter_id
        );
        return InverterOperationMode::SelfUse;
    }
    mode
}

/// Max wait for the first lever sync before inverter writes proceed (startup only).
const LEVERS_STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Startup state for lever-gated writes: when the first run happened, and whether the wait was logged.
#[derive(Default)]
pub struct LeversStartup {
    started: Option<Instant>,
    logged: bool,
}

/// True while the lever sync is expected but not yet available, within the startup wait.
/// Writes (modes and curtailment) are skipped then.
pub fn lever_startup_wait(
    levers: Option<&crate::resources::UserLeversResource>,
    startup: &mut LeversStartup,
    now: Instant,
) -> bool {
    let started = *startup.started.get_or_insert(now);
    let Some(levers) = levers else {
        return false;
    };
    if !levers.sync_expected
        || levers.available
        || now.saturating_duration_since(started) >= LEVERS_STARTUP_WAIT
    {
        return false;
    }
    if !startup.logged {
        info!("⏳ Waiting for the lever sync before writing inverter modes (max 120 s)");
        startup.logged = true;
    }
    true
}

/// Manual modes: the SolaX work mode "Manual Mode" with a manual sub-mode (Force Charge,
/// Force Discharge, Stop Charge and Discharge).
fn is_manual_mode(mode: InverterOperationMode) -> bool {
    matches!(
        mode,
        InverterOperationMode::ForceCharge
            | InverterOperationMode::ForceDischarge
            | InverterOperationMode::NoChargeNoDischarge
    )
}

/// Whether the hardware read-back matches the expected mode. When the manual sub-mode could not
/// be read, a ForceCharge read-back may be any Manual mode, so it matches every Manual mode.
fn readback_matches(
    state: &crate::traits::GenericInverterState,
    expected: InverterOperationMode,
) -> bool {
    if state.work_mode_ambiguous && state.work_mode == InverterOperationMode::ForceCharge {
        return is_manual_mode(expected);
    }
    state.work_mode == expected
}

/// How long a block-boundary write waits for a schedule generated inside the new block (H25 C).
const BOUNDARY_FRESH_SCHEDULE_WAIT_SECS: i64 = 60;

/// What the user asked for: levers, user control, the control config and the Back Up discharge
/// minimum. A difference from the value the block lock was taken under releases the lock for
/// the rest of the block (brief H25 B).
#[derive(Debug, Clone, PartialEq)]
pub struct UserIntent {
    levers: Option<fluxion_types::UserLevers>,
    disallow_charge: bool,
    disallow_discharge: bool,
    fixed_slot: Option<(String, InverterOperationMode)>,
    default_mode: Option<fluxion_types::user_control::DefaultMode>,
    backup_min_soc_pct: Option<u8>,
    /// Hash of the whole control config (force discharge, SOC limits, export caps, ...)
    control_hash: u64,
    /// `BackupDischargeMinSoc` from HA, as bits
    backup_discharge_min_soc: Option<u32>,
}

/// Hash of the control config's serialised form: any edit changes it.
fn control_config_hash(control: &ControlConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(control)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

impl UserIntent {
    fn of(
        levers: Option<fluxion_types::UserLevers>,
        user_control: Option<&fluxion_types::UserControlState>,
        control: &ControlConfig,
        backup_discharge_min_soc: Option<f32>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            control_hash: control_config_hash(control),
            backup_discharge_min_soc: backup_discharge_min_soc.map(f32::to_bits),
            levers,
            disallow_charge: user_control.is_some_and(|uc| uc.disallow_charge),
            disallow_discharge: user_control.is_some_and(|uc| uc.disallow_discharge),
            fixed_slot: user_control
                .and_then(|uc| uc.get_fixed_slot_at(now))
                .map(|slot| (slot.id.clone(), slot.mode)),
            default_mode: user_control.map(|uc| uc.default_mode),
            backup_min_soc_pct: user_control.map(|uc| uc.backup_min_soc_pct),
        }
    }
}

/// Block lock of one inverter: the mode of the block starting at `block_start` was applied (or
/// confirmed) from a schedule generated inside that block, under `intent`.
#[derive(Debug, Clone)]
pub struct BlockLock {
    block_start: DateTime<Utc>,
    intent: UserIntent,
    held_logged: bool,
    /// The intent changed inside the block: the lock no longer holds anything until the next
    /// block (the replan for the new intent may land after the executor saw the change).
    released: bool,
}

/// What the executor does with a planner mode change (brief H25 B and C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockGate {
    Apply,
    /// The block's mode is locked: the change waits for the next block boundary.
    HeldUntilBoundary,
    /// The schedule predates the block start: wait (max 60 s) for the fresh one.
    WaitForFreshSchedule,
    /// The schedule still predates the block start after the 60 s wait: apply it anyway.
    ApplyStale,
}

/// Mode changes happen at 15-minute block boundaries (owner ruling 2026-10-17). A change inside a
/// block that was already applied from a schedule generated inside it waits for the next
/// boundary, unless `good_reason` (initial sync, VPR, a fixed slot, a safety guard) or the user's
/// intent changed since the lock was taken (that releases the lock for the rest of the block).
/// At a boundary, a planner change from a schedule generated before the block started is applied
/// only after `BOUNDARY_FRESH_SCHEDULE_WAIT_SECS`; `good_reason` changes never wait.
fn block_gate(
    lock: Option<&BlockLock>,
    block_start: DateTime<Utc>,
    schedule_generated_at: Option<DateTime<Utc>>,
    intent: &UserIntent,
    good_reason: bool,
    now: DateTime<Utc>,
) -> BlockGate {
    if good_reason {
        return BlockGate::Apply;
    }
    if let Some(lock) = lock.filter(|l| l.block_start == block_start) {
        if !lock.released && lock.intent == *intent {
            return BlockGate::HeldUntilBoundary;
        }
        return BlockGate::Apply;
    }
    let stale = schedule_generated_at.is_some_and(|generated| generated < block_start);
    if !stale {
        return BlockGate::Apply;
    }
    if (now - block_start).num_seconds() < BOUNDARY_FRESH_SCHEDULE_WAIT_SECS {
        return BlockGate::WaitForFreshSchedule;
    }
    BlockGate::ApplyStale
}

/// Per-inverter executor state kept across runs (one `Local`, within the Bevy parameter limit).
#[derive(Default)]
pub struct ExecutorState {
    drift: BTreeMap<String, ModeDriftWatch>,
    locks: BTreeMap<String, BlockLock>,
    /// Block start whose fresh-schedule wait was already logged, per inverter
    wait_logged: BTreeMap<String, DateTime<Utc>>,
    /// Block start whose stale-schedule fallback was already warned about, per inverter
    stale_warned: BTreeMap<String, DateTime<Utc>>,
}

impl ExecutorState {
    /// Release this block's lock when the intent changed since it was taken. Called on every run
    /// before the gate, so a change seen before its replan lands still releases the lock.
    fn observe_intent(
        &mut self,
        inverter_id: &str,
        block_start: DateTime<Utc>,
        intent: &UserIntent,
    ) {
        if let Some(lock) = self.locks.get_mut(inverter_id)
            && lock.block_start == block_start
            && !lock.released
            && lock.intent != *intent
        {
            lock.released = true;
            debug!(
                "{inverter_id}: user or config change, block lock released until the next block"
            );
        }
    }

    /// Lock the block after its mode was applied or confirmed, when the schedule was generated
    /// inside the block (a projection made before the block never locks it). A lock released in
    /// this block stays released until the next block.
    fn lock_block(
        &mut self,
        inverter_id: &str,
        block_start: DateTime<Utc>,
        schedule_generated_at: Option<DateTime<Utc>>,
        intent: &UserIntent,
    ) {
        self.observe_intent(inverter_id, block_start, intent);
        if schedule_generated_at.is_some_and(|generated| generated >= block_start)
            && self
                .locks
                .get(inverter_id)
                .is_none_or(|l| l.block_start != block_start)
        {
            self.locks.insert(
                inverter_id.to_string(),
                BlockLock {
                    block_start,
                    intent: intent.clone(),
                    held_logged: false,
                    released: false,
                },
            );
        }
    }
}

/// Drift watch for one inverter's mode: confirms a hardware mismatch over two state reads at
/// least 30 s apart, then allows one re-apply per 5 minutes.
#[derive(Debug, Default, Clone)]
pub struct ModeDriftWatch {
    first_mismatch_at: Option<DateTime<Utc>>,
    mismatched_reads: u32,
    last_read_at: Option<DateTime<Utc>>,
    last_resend_at: Option<DateTime<Utc>>,
}

impl ModeDriftWatch {
    /// Feed one state read (`read_at` is the read time). Returns true when a re-apply is due.
    fn observe(&mut self, read_at: DateTime<Utc>, matches: bool, now: DateTime<Utc>) -> bool {
        if self.last_read_at == Some(read_at) {
            return false;
        }
        self.last_read_at = Some(read_at);
        if matches {
            self.mismatched_reads = 0;
            self.first_mismatch_at = None;
            return false;
        }
        let first = *self.first_mismatch_at.get_or_insert(read_at);
        self.mismatched_reads += 1;
        if self.mismatched_reads < 2 || read_at - first < chrono::Duration::seconds(30) {
            return false;
        }
        self.mismatched_reads = 0;
        self.first_mismatch_at = None;
        if self
            .last_resend_at
            .is_some_and(|t| now - t < chrono::Duration::minutes(5))
        {
            return false;
        }
        self.last_resend_at = Some(now);
        true
    }
}

/// Read-back watch for one inverter's export limit, active once we have written it.
#[derive(Debug, Default, Clone)]
pub struct ExportLimitWatch {
    /// Limit last written (0 while curtailed, the restore value after restoring)
    pub intended_w: u32,
    /// Watching is active from the first write until the read-back matches a restore once
    written: bool,
    /// True while curtailed (intended 0 W); a restore is watched only until it is confirmed
    curtailed: bool,
    /// A restore was written and the read-back has not confirmed it yet
    restore_pending: bool,
    /// When the last write was sent (paces restore retries)
    last_write_at: Option<DateTime<Utc>>,
    /// Restore give-ups since the last confirmed restore or curtailment (the stored limit is
    /// dropped after `MAX_RESTORE_GIVE_UPS`)
    restore_give_ups: u32,
    /// Rewrites issued since the last write (stops after `MAX_EXPORT_REWRITES`)
    rewrites: u32,
    /// Already warned that this inverter is skipped (reset when curtailment is not wanted)
    skip_warned: bool,
    mismatched_reads: u32,
    last_read_at: Option<DateTime<Utc>>,
    last_rewrite_at: Option<DateTime<Utc>>,
}

/// Rewrites of an export limit that the read-back does not confirm before watching stops.
const MAX_EXPORT_REWRITES: u32 = 5;

/// Restore give-ups before the stored pre-curtail limit is dropped (no endless restore cycle).
const MAX_RESTORE_GIVE_UPS: u32 = 3;

/// Outcome of one export limit read-back check.
#[derive(Debug, PartialEq, Eq)]
pub enum Reassert {
    Hold,
    Rewrite,
    /// The read-back confirmed a restore: the stored limit is no longer needed
    Confirmed,
    /// Not confirmed after `MAX_EXPORT_REWRITES` rewrites: watching stops (logged once)
    GiveUp,
}

impl ExportLimitWatch {
    /// Feed one state read. Two consecutive mismatched reads trigger a rewrite, at most one per
    /// minute.
    fn observe(
        &mut self,
        read_at: DateTime<Utc>,
        read_back_w: Option<u32>,
        now: DateTime<Utc>,
    ) -> Reassert {
        let Some(read_back) = read_back_w.filter(|_| self.written) else {
            return Reassert::Hold;
        };
        if self.last_read_at == Some(read_at) {
            return Reassert::Hold;
        }
        self.last_read_at = Some(read_at);
        if read_back == self.intended_w {
            self.mismatched_reads = 0;
            if !self.curtailed {
                // Restore confirmed: stop watching so a later owner change is never overwritten
                self.written = false;
                return Reassert::Confirmed;
            }
            return Reassert::Hold;
        }
        self.mismatched_reads += 1;
        if self.mismatched_reads < 2 {
            return Reassert::Hold;
        }
        if self
            .last_rewrite_at
            .is_some_and(|t| now - t < chrono::Duration::minutes(1))
        {
            return Reassert::Hold;
        }
        self.mismatched_reads = 0;
        if self.rewrites >= MAX_EXPORT_REWRITES {
            self.written = false;
            return Reassert::GiveUp;
        }
        self.rewrites += 1;
        self.last_rewrite_at = Some(now);
        Reassert::Rewrite
    }
}

/// Largest export limit FluxION may write: the lowest of the hard cap, `maximum_export_power_w`,
/// the inverter's own `max` and the value itself. Logs a warning when it lowers the value.
pub fn clamp_export_limit(value_w: u32, control: &ControlConfig, entity_max_w: Option<u32>) -> u32 {
    let clamped = [
        Some(value_w),
        control.export_limit_hard_cap_w,
        Some(control.maximum_export_power_w),
        entity_max_w,
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or(value_w);
    if clamped < value_w {
        warn!(
            "Export limit {} W clamped to {} W (hard cap {:?}, maximum_export_power_w {} W, entity max {:?} W)",
            value_w,
            clamped,
            control.export_limit_hard_cap_w,
            control.maximum_export_power_w,
            entity_max_w
        );
    }
    clamped
}

/// Send `SetExportLimit` with the value clamped by `clamp_export_limit`, or log it in debug mode.
/// Returns the value written (or that would have been written).
fn write_export_limit(
    async_writer: &crate::resources::AsyncInverterWriter,
    control: &ControlConfig,
    entity_max_w: Option<u32>,
    debug: bool,
    inverter_id: &str,
    limit_w: u32,
) -> u32 {
    let limit_w = clamp_export_limit(limit_w, control, entity_max_w);
    if debug {
        info!(
            "🔧 [DEBUG] Would set export limit of {} to {} W",
            inverter_id, limit_w
        );
    } else {
        async_writer.write_command_async(
            inverter_id.to_string(),
            InverterCommand::SetExportLimit(limit_w),
        );
    }
    limit_w
}

/// System that executes scheduled mode changes
/// Runs every update cycle to check if mode changes are needed
///
/// Priority: VPR (one-time window, applies even with the kill switch on) > kill switch
/// (Self-Use once, then nothing) > fixed slots > schedule.
///
/// On initial startup, compares against ACTUAL inverter mode and applies immediately.
/// After initial sync, uses debounce interval between mode changes.
#[expect(clippy::too_many_arguments)]
pub fn schedule_execution_system(
    schedule_query: Query<&OperationSchedule>,
    async_writer: Res<crate::resources::AsyncInverterWriter>,
    mut current_mode_query: Query<(
        &mut CurrentMode,
        &Inverter,
        Option<&BatteryStatus>,
        Option<&RawInverterState>,
    )>,
    config: Res<ExecutionConfig>,
    debug: Res<DebugModeConfig>,
    system_config: Res<crate::resources::SystemConfig>,
    mut sync_tracker: ResMut<InitialModeSyncTracker>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
    (levers, backup_soc): (
        Option<Res<crate::resources::UserLeversResource>>,
        Option<Res<crate::async_systems::BackupDischargeMinSoc>>,
    ),
    timezone: Option<Res<crate::resources::TimezoneConfig>>,
    mut debug_was_enabled: Local<Option<bool>>,
    mut vpr_was_active: Local<bool>,
    mut kill_was_active: Local<bool>,
    mut lever_startup: Local<LeversStartup>,
    mut executor: Local<ExecutorState>,
    mut warned: Local<ScheduleWarnings>,
) {
    let now = Utc::now();
    let force_discharge_enabled = system_config.control_config.force_discharge_enabled;

    // Debug mode switched off at runtime: CurrentMode already equals the plan, so the
    // sync tracker is cleared to compare against the real hardware mode and send.
    if *debug_was_enabled == Some(true) && !debug.enabled {
        info!("🔓 Debug mode turned off — resyncing inverter modes");
        sync_tracker.synced_inverters.clear();
    }
    *debug_was_enabled = Some(debug.enabled);

    // Startup: wait (max 120 s) for the first lever sync before writing any mode
    if lever_startup_wait(levers.as_deref(), &mut lever_startup, Instant::now()) {
        return;
    }

    let tz: chrono_tz::Tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let levers = levers.filter(|l| l.available).map(|l| l.levers);

    // VPR: highest priority, applied only while its window is active
    let vpr_override: Option<(InverterOperationMode, String)> = levers
        .and_then(|l| l.vpr)
        .filter(|w| w.phase(now, &tz) == VprPhase::Active)
        .and_then(|w| {
            vpr_inverter_mode(w.mode).map(|mode| (mode, format!("VPR: {}", w.mode.option())))
        })
        .map(|(mode, reason)| {
            // VPR "Vybíjení baterie" is a Force Discharge: blocked while it is disabled
            if mode == InverterOperationMode::ForceDischarge && !force_discharge_enabled {
                if !warned.vpr_discharge {
                    warn!(
                        "VPR Vybíjení baterie blocked: force discharge disabled (control.force_discharge_enabled)"
                    );
                    warned.vpr_discharge = true;
                }
                (InverterOperationMode::SelfUse, reason)
            } else {
                (mode, reason)
            }
        });
    if vpr_override.is_none() {
        warned.vpr_discharge = false;
    }
    let vpr_exited = *vpr_was_active && vpr_override.is_none();
    *vpr_was_active = vpr_override.is_some();

    // Kill switch: user disabled FluxION, or the "Zakázat Automatizaci" lever
    let kill_switch = user_control.as_ref().is_some_and(|uc| !uc.state.enabled)
        || levers.is_some_and(|l| l.automation_disabled);

    // Kill switch turned off: the owner may have set modes by hand, so resync against hardware
    if *kill_was_active && !kill_switch {
        info!("▶️ Automation re-enabled — resyncing inverter modes");
        sync_tracker.synced_inverters.clear();
    }
    *kill_was_active = kill_switch;

    // Kill switch: VPR while active, otherwise SelfUse once, then no more mode commands
    if kill_switch {
        for (mut current_mode, inverter, battery_status, raw_state) in current_mode_query.iter_mut()
        {
            // No state read yet: nothing to compare with, so skip without marking synced
            let Some(raw) = raw_state else {
                continue;
            };
            let (target, reason) = match &vpr_override {
                Some((mode, reason)) => (*mode, reason.clone()),
                None => (
                    InverterOperationMode::SelfUse,
                    "FluxION disabled by user".to_string(),
                ),
            };
            let target = permitted_mode(target, force_discharge_enabled, &inverter.id);
            // Not synced yet (startup or after a resync): compare with the hardware mode
            let is_initial_sync = !sync_tracker.synced_inverters.contains(&inverter.id);
            let compared = if is_initial_sync {
                raw.state.work_mode
            } else {
                current_mode.mode
            };
            // Initial sync against an ambiguous Manual read-back: re-apply a Manual target (idempotent)
            let ambiguous_manual_resync = is_initial_sync
                && is_manual_mode(target)
                && raw.state.work_mode_ambiguous
                && raw.state.work_mode == InverterOperationMode::ForceCharge;
            if compared == target && !ambiguous_manual_resync {
                sync_tracker.synced_inverters.insert(inverter.id.clone());
                continue;
            }
            if vpr_override.is_some() {
                let Some(battery) = battery_status else {
                    warn!(
                        "Skipping VPR for {}: no battery status available (safety constraint)",
                        inverter.id
                    );
                    continue;
                };
                if soc_blocks_mode(
                    target,
                    battery.soc_percent as f32,
                    &system_config.control_config,
                ) {
                    debug!("Skipping VPR {:?} for {}: SOC safety", target, inverter.id);
                    continue;
                }
            }
            if debug.enabled {
                info!(
                    "🔧 [DEBUG] {} - would set {} to {:?} (currently {:?})",
                    reason, inverter.id, target, current_mode.mode
                );
            } else {
                info!("⏸️ {} - setting {} to {:?}", reason, inverter.id, target);
                let command = InverterCommand::SetMode(target);
                async_writer.write_command_async(inverter.id.clone(), command);
            }
            current_mode.mode = target;
            current_mode.set_at = now;
            current_mode.reason = reason;
            sync_tracker.synced_inverters.insert(inverter.id.clone());
        }
        return; // Stop - no more mode commands while disabled
    }

    // The VPR window replaces the schedule; otherwise the current schedule block applies
    let schedule = schedule_query.single().ok();
    if vpr_override.is_none() && schedule.is_none() {
        trace!("No schedule available yet");
        return;
    }
    let planned: Option<ScheduledMode> = match &vpr_override {
        Some((mode, reason)) => Some(ScheduledMode {
            block_start: now,
            duration_minutes: 0,
            target_inverters: None,
            mode: *mode,
            reason: reason.clone(),
            decision_uid: None,
            debug_info: None,
        }),
        None => schedule.and_then(|s| s.get_current_mode(now)).cloned(),
    };

    // Block lock inputs (brief H25): when the schedule was generated, and what the user asked for
    let schedule_generated_at = schedule.map(|s| s.generated_at);
    let intent = UserIntent::of(
        levers,
        user_control.as_ref().map(|uc| &uc.state),
        &system_config.control_config,
        backup_soc.as_ref().map(|b| b.value),
        now,
    );

    // Default mode Back Up (brief H19): planner Self-Use runs as Back Up Mode with the user's
    // minimum SOC. Explicit overrides (VPR, a fixed slot active now) keep their own mode.
    let backup_default = user_control
        .as_ref()
        .filter(|uc| uc.state.default_mode == fluxion_types::user_control::DefaultMode::Backup);
    let explicit_override = vpr_override.is_some()
        || user_control
            .as_ref()
            .is_some_and(|uc| uc.state.get_fixed_slot_at(now).is_some());

    // Process each inverter
    for (mut current_mode, inverter, battery_status, raw_state) in current_mode_query.iter_mut() {
        // Find inverter config
        let inverter_config = system_config.inverters.iter().find(|i| i.id == inverter.id);

        if let Some(inv_cfg) = inverter_config {
            // Check if this inverter should receive commands based on topology
            match &inv_cfg.topology {
                crate::resources::InverterTopology::Slave { master_id } => {
                    trace!(
                        "Skipping slave inverter {} (controlled by {})",
                        inverter.id, master_id
                    );
                    continue;
                }
                crate::resources::InverterTopology::Master { slave_ids } => {
                    trace!(
                        "Processing master inverter {} (controls {} slaves)",
                        inverter.id,
                        slave_ids.len()
                    );
                }
                crate::resources::InverterTopology::Independent => {
                    trace!("Processing independent inverter {}", inverter.id);
                }
            }

            // Get current scheduled mode (or VPR)
            if let Some(scheduled_mode) = planned.clone() {
                // Owned copy so the fixed slot override can modify it
                let mut effective_mode = scheduled_mode;

                // Check if user has a fixed slot override active RIGHT NOW
                // This ensures newly created slots take effect immediately (never over VPR)
                if vpr_override.is_none()
                    && let Some(ref uc) = user_control
                    && let Some(fixed_slot) = uc.state.get_fixed_slot_at(now)
                {
                    // Force discharge is gated here too: a ForceDischarge slot runs as Self-Use
                    let slot_mode = if fixed_slot.mode == InverterOperationMode::ForceDischarge
                        && !force_discharge_enabled
                    {
                        if warned.fixed_slots.insert(fixed_slot.id.clone()) {
                            warn!(
                                "Fixed slot {} ForceDischarge blocked: force discharge disabled (control.force_discharge_enabled), running Self-Use",
                                fixed_slot.id
                            );
                        }
                        InverterOperationMode::SelfUse
                    } else {
                        fixed_slot.mode
                    };
                    if effective_mode.mode != slot_mode {
                        info!(
                            "🔒 Fixed slot override active: {} -> {:?} (slot: {})",
                            effective_mode.mode, slot_mode, fixed_slot.id
                        );
                        effective_mode.mode = slot_mode;
                        effective_mode.reason = format!(
                            "Fixed slot: {}",
                            fixed_slot.note.as_deref().unwrap_or("User override")
                        );
                    }
                }

                // Check if this scheduled mode applies to this inverter
                if !should_execute_for_inverter(&effective_mode, &inverter.id) {
                    continue;
                }

                // Planner Self-Use in Back Up default: Back Up Mode with the user's minimum SOC
                let backup_min_soc = match backup_default {
                    Some(uc)
                        if !explicit_override
                            && effective_mode.mode == InverterOperationMode::SelfUse =>
                    {
                        effective_mode.mode = InverterOperationMode::BackUpMode;
                        Some(uc.state.backup_min_soc_pct)
                    }
                    _ => None,
                };

                // Use effective_mode (with potential fixed slot override) for the rest
                let scheduled_mode = &effective_mode;

                // Check if this is the initial sync for this inverter
                let is_initial_sync = !sync_tracker.synced_inverters.contains(&inverter.id);

                // VPR entry and the first schedule block after VPR bypass the debounce
                let bypass_debounce = is_initial_sync || vpr_override.is_some() || vpr_exited;

                // Get actual inverter mode from hardware (if available)
                let actual_mode = raw_state.map(|r| r.state.work_mode);

                // Determine which mode to compare against:
                // - On initial sync: use actual inverter mode (if available) to detect mismatch
                // - After initial sync: use CurrentMode (our internal tracking)
                let effective_current_mode = if is_initial_sync {
                    actual_mode.unwrap_or(current_mode.mode)
                } else {
                    current_mode.mode
                };

                // Drift: the hardware disagrees with the tracked mode although the plan agrees with
                // it (e.g. another controller rewrote it). Never in debug mode.
                if !is_initial_sync
                    && !debug.enabled
                    && scheduled_mode.mode == current_mode.mode
                    && let Some(raw) = raw_state
                    && executor
                        .drift
                        .entry(inverter.id.clone())
                        .or_default()
                        .observe(
                            raw.last_updated,
                            readback_matches(&raw.state, current_mode.mode),
                            now,
                        )
                {
                    let soc_ok = battery_status.is_some_and(|b| {
                        !soc_blocks_mode(
                            current_mode.mode,
                            b.soc_percent as f32,
                            &system_config.control_config,
                        )
                    });
                    if soc_ok {
                        warn!(
                            "⚠️ Inverter mode drifted to {:?} (expected {:?}) — re-applying",
                            raw.state.work_mode, current_mode.mode
                        );
                        // Back Up re-apply goes through SetBackupMode so the minimum SOC is synced
                        let drift_command = match backup_min_soc {
                            Some(min_soc_pct) => InverterCommand::SetBackupMode { min_soc_pct },
                            None => InverterCommand::SetMode(permitted_mode(
                                current_mode.mode,
                                force_discharge_enabled,
                                &inverter.id,
                            )),
                        };
                        async_writer.write_command_async(inverter.id.clone(), drift_command);
                        current_mode.set_at = now;
                    } else {
                        debug!("Skipping drift re-apply for {}: SOC safety", inverter.id);
                    }
                }

                // Check if mode change is needed
                let ambiguous_manual_resync = is_initial_sync
                    && is_manual_mode(scheduled_mode.mode)
                    && raw_state.is_some_and(|r| {
                        r.state.work_mode_ambiguous
                            && r.state.work_mode == InverterOperationMode::ForceCharge
                    });
                // Back Up default: the adapter must confirm the minimum SOC, not just the mode
                let backup_pending = !debug.enabled
                    && backup_min_soc.is_some_and(|target| {
                        async_writer.confirmed_backup_min_soc(&inverter.id) != Some(target)
                    });
                if scheduled_mode.mode != effective_current_mode
                    || ambiguous_manual_resync
                    || backup_pending
                {
                    // Block boundaries (brief H25): a planner change inside a locked block waits for
                    // the next boundary, and a boundary waits briefly for the fresh schedule.
                    if scheduled_mode.mode != effective_current_mode {
                        let soc = battery_status.map(|b| b.soc_percent as f32);
                        let control = &system_config.control_config;
                        let floor = backup_default.map_or(control.min_battery_soc, |uc| {
                            uc.state.planner_min_soc_pct(
                                control.min_battery_soc,
                                control.max_battery_soc,
                            )
                        });
                        let safety_stop = soc.is_some_and(|soc| {
                            (current_mode.mode == InverterOperationMode::ForceCharge
                                && soc >= control.max_battery_soc)
                                || (current_mode.mode == InverterOperationMode::ForceDischarge
                                    && soc <= floor)
                        });
                        // Not planner-originated: VPR, a fixed slot, initial sync, guards
                        let good_reason = bypass_debounce
                            || explicit_override
                            || scheduled_mode.reason.contains("guard:")
                            || safety_stop;
                        executor.observe_intent(&inverter.id, scheduled_mode.block_start, &intent);
                        match block_gate(
                            executor.locks.get(&inverter.id),
                            scheduled_mode.block_start,
                            schedule_generated_at,
                            &intent,
                            good_reason,
                            now,
                        ) {
                            BlockGate::Apply => {}
                            BlockGate::ApplyStale => {
                                if executor.stale_warned.get(&inverter.id)
                                    != Some(&scheduled_mode.block_start)
                                {
                                    executor
                                        .stale_warned
                                        .insert(inverter.id.clone(), scheduled_mode.block_start);
                                    warn!(
                                        "{}: no schedule generated inside the block at {} after {} s, applying the earlier one",
                                        inverter.id,
                                        scheduled_mode.block_start,
                                        BOUNDARY_FRESH_SCHEDULE_WAIT_SECS
                                    );
                                }
                            }
                            BlockGate::HeldUntilBoundary => {
                                if let Some(lock) = executor.locks.get_mut(&inverter.id)
                                    && !lock.held_logged
                                {
                                    lock.held_logged = true;
                                    debug!(
                                        "{} {:?} -> {:?} held until next block boundary: {}",
                                        inverter.id,
                                        effective_current_mode,
                                        scheduled_mode.mode,
                                        scheduled_mode.reason
                                    );
                                }
                                continue;
                            }
                            BlockGate::WaitForFreshSchedule => {
                                if executor.wait_logged.get(&inverter.id)
                                    != Some(&scheduled_mode.block_start)
                                {
                                    executor
                                        .wait_logged
                                        .insert(inverter.id.clone(), scheduled_mode.block_start);
                                    debug!(
                                        "{}: schedule predates the block at {}, waiting for the fresh one (max {} s)",
                                        inverter.id,
                                        scheduled_mode.block_start,
                                        BOUNDARY_FRESH_SCHEDULE_WAIT_SECS
                                    );
                                }
                                continue;
                            }
                        }
                    }

                    // On initial sync: bypass debounce to apply plan immediately
                    // After initial sync: check minimum interval
                    if !bypass_debounce && !can_change_mode(&current_mode, &config, now) {
                        debug!(
                            "Skipping mode change for {}: too soon since last change",
                            inverter.id
                        );
                        continue;
                    }

                    // Check battery SOC constraints for charge/discharge operations
                    // SAFETY: Require battery status to prevent charging at 100% or discharging at min SOC
                    let Some(battery) = battery_status else {
                        warn!(
                            "Skipping mode change for {}: no battery status available (safety constraint)",
                            inverter.id
                        );
                        continue;
                    };

                    let soc = battery.soc_percent as f32;

                    // Skip force-charge if battery is already at max SOC
                    if scheduled_mode.mode == InverterOperationMode::ForceCharge
                        && soc >= system_config.control_config.max_battery_soc
                    {
                        debug!(
                            "Skipping force-charge for {}: SOC ({:.1}%) >= max ({:.1}%)",
                            inverter.id, soc, system_config.control_config.max_battery_soc
                        );
                        // Mark as synced even if skipped due to SOC (schedule is correct, just not needed)
                        if is_initial_sync {
                            sync_tracker.synced_inverters.insert(inverter.id.clone());
                            info!(
                                "🔄 Initial sync for {}: skipped (SOC at max), mode already aligned",
                                inverter.id
                            );
                        }
                        continue;
                    }

                    // Skip force-discharge at or below the effective floor: in Back Up default the
                    // inverter keeps the Back Up minimum, so that is the floor (brief H19 item 6)
                    let discharge_floor =
                        backup_default.map_or(system_config.control_config.min_battery_soc, |uc| {
                            uc.state.planner_min_soc_pct(
                                system_config.control_config.min_battery_soc,
                                system_config.control_config.max_battery_soc,
                            )
                        });
                    if scheduled_mode.mode == InverterOperationMode::ForceDischarge
                        && soc <= discharge_floor
                    {
                        debug!(
                            "Skipping force-discharge for {}: SOC ({:.1}%) <= min ({:.1}%)",
                            inverter.id, soc, discharge_floor
                        );
                        // Mark as synced even if skipped due to SOC
                        if is_initial_sync {
                            sync_tracker.synced_inverters.insert(inverter.id.clone());
                            info!(
                                "🔄 Initial sync for {}: skipped (SOC at min), mode already aligned",
                                inverter.id
                            );
                        }
                        continue;
                    }

                    // Execute mode change
                    if debug.enabled {
                        info!(
                            "🔧 [DEBUG] Would change {} from {:?} to {:?}: {}{}",
                            inverter.id,
                            effective_current_mode,
                            scheduled_mode.mode,
                            scheduled_mode.reason,
                            if is_initial_sync {
                                " (initial sync)"
                            } else {
                                ""
                            }
                        );
                        // In debug mode, update the mode immediately for testing
                        current_mode.mode = scheduled_mode.mode;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    } else {
                        // Send command using direct async writer (fire-and-forget)
                        let mode_sent = permitted_mode(
                            scheduled_mode.mode,
                            force_discharge_enabled,
                            &inverter.id,
                        );
                        let command = match backup_min_soc {
                            Some(min_soc_pct) => InverterCommand::SetBackupMode { min_soc_pct },
                            None => InverterCommand::SetMode(mode_sent),
                        };

                        info!(
                            "📤 Sending command to change {} to {:?}: {}{}",
                            inverter.id,
                            scheduled_mode.mode,
                            scheduled_mode.reason,
                            if is_initial_sync {
                                " (initial sync - bypassing debounce)"
                            } else {
                                ""
                            }
                        );

                        // Use async fire-and-forget for mode changes (don't block the ECS system)
                        async_writer.write_command_async(inverter.id.clone(), command);

                        // Update current mode immediately (optimistic)
                        current_mode.mode = mode_sent;
                        current_mode.set_at = now;
                        current_mode.reason = scheduled_mode.reason.clone();
                    }

                    // Mark inverter as synced after first mode change
                    if is_initial_sync {
                        sync_tracker.synced_inverters.insert(inverter.id.clone());
                        info!("🔄 Initial sync completed for {}", inverter.id);
                    }
                    if vpr_override.is_none() {
                        executor.lock_block(
                            &inverter.id,
                            scheduled_mode.block_start,
                            schedule_generated_at,
                            &intent,
                        );
                    }
                } else {
                    if vpr_override.is_none() {
                        executor.lock_block(
                            &inverter.id,
                            scheduled_mode.block_start,
                            schedule_generated_at,
                            &intent,
                        );
                    }
                    // Mode matches schedule - mark as synced
                    if is_initial_sync {
                        sync_tracker.synced_inverters.insert(inverter.id.clone());
                        info!(
                            "🔄 Initial sync for {}: mode already matches schedule ({:?})",
                            inverter.id, scheduled_mode.mode
                        );
                    }
                }
            }
        }
    }
}

/// Curtail export (SolaX export limit 0 W) in blocks whose sell price is at or below the
/// threshold, and restore each curtailed inverter's own limit once curtailment is not wanted.
/// Not wanted while the kill switch is on, while an active VPR sells to the grid, without a sell
/// price, or during the lever sync startup wait.
#[expect(clippy::too_many_arguments)]
pub fn export_curtailment_system(
    async_writer: Res<crate::resources::AsyncInverterWriter>,
    inverter_query: Query<(&Inverter, Option<&RawInverterState>)>,
    price_query: Query<&SpotPriceData>,
    system_config: Res<crate::resources::SystemConfig>,
    debug: Res<DebugModeConfig>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
    levers: Option<Res<crate::resources::UserLeversResource>>,
    timezone: Option<Res<crate::resources::TimezoneConfig>>,
    mut state: ResMut<ExportCurtailmentState>,
    mut debug_state: Local<ExportCurtailmentState>,
    mut startup: Local<LeversStartup>,
) {
    let now = Utc::now();
    if lever_startup_wait(levers.as_deref(), &mut startup, Instant::now()) {
        return;
    }
    let control: &ControlConfig = &system_config.control_config;
    let threshold = control.export_curtail_at_or_below_czk;
    let tz: chrono_tz::Tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let levers = levers.filter(|l| l.available).map(|l| l.levers);

    let kill_switch = user_control.as_ref().is_some_and(|uc| !uc.state.enabled)
        || levers.as_ref().is_some_and(|l| l.automation_disabled);
    let sells_to_grid = levers
        .as_ref()
        .and_then(|l| l.vpr)
        .filter(|w| w.phase(now, &tz) == VprPhase::Active)
        .is_some_and(|w| w.mode == VprMode::SellToGrid);

    let sell_price = price_query.single().ok().and_then(|prices| {
        prices
            .time_block_prices
            .iter()
            .find(|block| {
                let block_end =
                    block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
                now >= block.block_start && now < block_end
            })
            .and_then(|block| block.spot_sell_price_czk_per_kwh)
    });

    let curtail_wanted = control.export_curtailment_enabled
        && !kill_switch
        && !sells_to_grid
        && sell_price.is_some_and(|price| price <= threshold);

    // Debug mode keeps its own in-memory would-be state and never touches the real one
    let export: &mut ExportCurtailmentState = if debug.enabled {
        &mut debug_state
    } else {
        &mut state
    };
    let mut changed = false;

    // Old single-value file: the master's restore limit
    if let Some(limit_w) = export.legacy_restore_limit_w.take() {
        let targets: Vec<String> = system_config
            .inverters
            .iter()
            .filter(|i| !matches!(i.topology, crate::resources::InverterTopology::Slave { .. }))
            .map(|i| i.id.clone())
            .collect();
        if targets.is_empty() {
            warn!(
                "Export limit restore file has no inverter to restore, dropping {} W",
                limit_w
            );
        }
        for id in targets {
            export.restore_limits_w.entry(id).or_insert(limit_w);
        }
        changed = true;
    }

    for (inverter, raw_state) in inverter_query.iter() {
        let topology = system_config
            .inverters
            .iter()
            .find(|i| i.id == inverter.id)
            .map(|i| &i.topology);
        if matches!(
            topology,
            Some(crate::resources::InverterTopology::Slave { .. })
        ) {
            continue;
        }
        let read_back = raw_state.and_then(|raw| raw.state.export_limit_w);
        let entity_max_w = raw_state.and_then(|raw| raw.state.export_limit_max_w);
        let watch = export.limit_watch.entry(inverter.id.clone()).or_default();
        if !curtail_wanted {
            watch.skip_warned = false;
        }

        // Restore: the stored limit stays until the read-back confirms it, so a refused write is
        // retried and never lost. A restore is retried at most once a minute after a give-up.
        let retry_due = watch
            .last_write_at
            .is_none_or(|t| now - t >= chrono::Duration::minutes(1));
        if !curtail_wanted
            && let Some(restore_w) = export.restore_limits_w.get(&inverter.id).copied()
            && !watch.restore_pending
            && (watch.curtailed || retry_due)
        {
            // Restore to the exact limit read back before curtailing (clamped on write)
            let written_w = write_export_limit(
                &async_writer,
                control,
                entity_max_w,
                debug.enabled,
                &inverter.id,
                restore_w,
            );
            info!("✅ Export restore sent: {} W ({})", written_w, inverter.id);
            watch.intended_w = written_w;
            watch.written = true;
            watch.curtailed = false;
            watch.restore_pending = true;
            watch.last_write_at = Some(now);
            watch.rewrites = 0;
            watch.mismatched_reads = 0;
        } else if curtail_wanted && !watch.curtailed {
            // A stored limit (restore not yet confirmed) is the pre-curtail value to keep
            let curtail_from = match export.restore_limits_w.get(&inverter.id).copied() {
                Some(stored_w) => Some(stored_w),
                None => match read_back {
                    // Curtail only from a known limit that is above the curtail target
                    Some(limit_w) if limit_w > 0 && control.export_limit_min_w < limit_w => {
                        Some(limit_w)
                    }
                    Some(limit_w) => {
                        if !watch.skip_warned {
                            warn!(
                                "Export limit of {} is {} W, not above the curtail target {} W; not curtailing",
                                inverter.id, limit_w, control.export_limit_min_w
                            );
                            watch.skip_warned = true;
                        }
                        None
                    }
                    None => {
                        if !watch.skip_warned {
                            warn!(
                                "Export limit of {} is unknown, not curtailing it (no restore value)",
                                inverter.id
                            );
                            watch.skip_warned = true;
                        }
                        None
                    }
                },
            };
            if let Some(restore_w) = curtail_from {
                info!(
                    "🚫 Export curtailed (sell {:.3} ≤ {:.3}) for {}",
                    sell_price.unwrap_or_default(),
                    threshold,
                    inverter.id
                );
                export
                    .restore_limits_w
                    .insert(inverter.id.clone(), restore_w);
                let written_w = write_export_limit(
                    &async_writer,
                    control,
                    entity_max_w,
                    debug.enabled,
                    &inverter.id,
                    control.export_limit_min_w,
                );
                watch.intended_w = written_w;
                watch.written = true;
                watch.curtailed = true;
                watch.restore_pending = false;
                watch.restore_give_ups = 0;
                watch.rewrites = 0;
                watch.mismatched_reads = 0;
                changed = true;
            }
        }

        // Re-assert: the read-back differs from what we wrote (never in debug mode)
        if !debug.enabled
            && let Some(raw) = raw_state
        {
            // A restore read back between the curtail target and the restore target is the
            // owner's own limit: stop watching and drop the stored value
            let owner_choice = read_back.filter(|&w| {
                watch.written
                    && !watch.curtailed
                    && w > control.export_limit_min_w
                    && w < watch.intended_w
            });
            let reassert = if let Some(owner_w) = owner_choice {
                info!(
                    "Export limit of {} reads {} W, below the restore target {} W: treating it as the owner's choice, not restoring",
                    inverter.id, owner_w, watch.intended_w
                );
                watch.written = false;
                watch.restore_pending = false;
                watch.restore_give_ups = 0;
                export.restore_limits_w.remove(&inverter.id);
                changed = true;
                Reassert::Hold
            } else {
                watch.observe(raw.last_updated, read_back, now)
            };
            match reassert {
                Reassert::Hold => {}
                Reassert::Confirmed => {
                    if export.restore_limits_w.remove(&inverter.id).is_some() {
                        info!(
                            "✅ Export limit of {} confirmed at {:?} W",
                            inverter.id, read_back
                        );
                        changed = true;
                    }
                    watch.restore_pending = false;
                    watch.restore_give_ups = 0;
                }
                Reassert::Rewrite => {
                    warn!(
                        "⚠️ Export limit of {} reads {:?} W, expected {} W — rewriting",
                        inverter.id, read_back, watch.intended_w
                    );
                    write_export_limit(
                        &async_writer,
                        control,
                        entity_max_w,
                        false,
                        &inverter.id,
                        watch.intended_w,
                    );
                }
                Reassert::GiveUp => {
                    if watch.restore_pending {
                        watch.restore_give_ups += 1;
                        watch.restore_pending = false;
                        watch.curtailed = false;
                        if watch.restore_give_ups >= MAX_RESTORE_GIVE_UPS {
                            // Stop the restore cycle: the stored value is dropped, the limit is left as read
                            error!(
                                "❌ Export restore of {} not confirmed after {} give-ups (reads {:?} W, expected {} W), dropping the stored limit {:?} W; the limit is left as the inverter reports it",
                                inverter.id,
                                watch.restore_give_ups,
                                read_back,
                                watch.intended_w,
                                export.restore_limits_w.get(&inverter.id)
                            );
                            export.restore_limits_w.remove(&inverter.id);
                            watch.restore_give_ups = 0;
                            changed = true;
                        } else {
                            // The stored limit is kept; the restore is retried after a minute
                            watch.last_write_at = Some(now);
                            warn!(
                                "⚠️ Export restore of {} not confirmed after {} rewrites (reads {:?} W, expected {} W), keeping it for retry",
                                inverter.id, MAX_EXPORT_REWRITES, read_back, watch.intended_w
                            );
                        }
                    } else {
                        warn!(
                            "⚠️ Export limit of {} still reads {:?} W after {} rewrites (expected {} W), stopped watching",
                            inverter.id, read_back, MAX_EXPORT_REWRITES, watch.intended_w
                        );
                    }
                }
            }
        }
    }

    if changed {
        export.persist();
    }
}

/// System for initializing inverter entities on startup
pub fn initialize_inverters_system(
    mut commands: Commands,
    system_config: Res<crate::resources::SystemConfig>,
    query: Query<&Inverter>,
) {
    // Only run once at startup
    if !query.is_empty() {
        return;
    }

    info!("Initializing inverter entities...");

    for inv_config in &system_config.inverters {
        info!(
            "Creating entity for inverter: {} ({})",
            inv_config.id, inv_config.inverter_type
        );

        commands.spawn((
            Inverter {
                id: inv_config.id.clone(),
                inverter_type: inv_config.inverter_type,
            },
            CurrentMode::default(),
            // Add other required components
            PowerGeneration::default(),
            GridPower::default(),
            BatteryStatus::default(),
            InverterStatus::default(),
        ));
    }
}

/// Marker to track if we've fetched initial battery history from HA
#[derive(Resource, Default)]
pub struct BatteryHistoryInitialized(pub bool);

// TODO: Implement system to sync ConsumptionHistory data to WinterAdaptiveStrategy config
// The strategy config is boxed inside AdaptiveSeasonalOptimizer, making direct updates complex.
// Possible approaches:
// 1. Pass ConsumptionHistory as a parameter to strategy evaluation (requires trait changes)
// 2. Store strategy configs in a separate ECS component that can be mutated
// 3. Rebuild the optimizer when consumption history updates (expensive)
//
// For now, the strategy uses fallback consumption estimates (per-block × 96).

/// System to fetch initial battery history from Home Assistant on startup
/// This populates the BatteryHistory with the last 48 hours of data from HA
pub fn fetch_initial_battery_history_system(
    mut initialized: ResMut<BatteryHistoryInitialized>,
    system_config: Res<crate::resources::SystemConfig>,
    _battery_history: Res<BatteryHistory>,
) {
    // Only run once
    if initialized.0 {
        return;
    }

    initialized.0 = true;

    // Get the battery entity from first inverter config
    let battery_entity = system_config.inverters.first().map(|inv| {
        format!(
            "sensor.{}_battery_capacity",
            inv.entity_prefix.replace(".", "_")
        )
    });

    if let Some(entity) = battery_entity {
        info!(
            "📊 Fetching initial battery history from HA for entity: {}",
            entity
        );

        // Future: Implement initial history fetch from Home Assistant
        // Would require:
        // 1. Pass HA client to this system via resource
        // 2. Call HA history API to fetch last 48h of battery SOC data
        // 3. Populate BatteryHistory with the fetched data
        // For now, history is populated as new data arrives (every 15 min)

        debug!("Note: Initial history fetch not yet implemented");
        debug!("History will be populated as new inverter data arrives (every 15 min)");
    }
}

/// System to trigger battery SOC history fetch from HA
/// Runs periodically (every 5 minutes) to fetch and update historical battery SOC data
pub fn trigger_battery_history_fetch_system(
    mut timer: ResMut<BatteryHistoryFetchTimer>,
    system_config: Res<crate::resources::SystemConfig>,
) {
    let now = Instant::now();
    let elapsed = now.duration_since(timer.last_fetch).as_secs();

    // Only fetch if interval has elapsed
    if elapsed < timer.interval_secs {
        return;
    }

    // Update timer
    timer.last_fetch = now;

    // Get the battery entity from first inverter config
    let battery_entity = system_config.inverters.first().map(|inv| {
        format!(
            "sensor.{}_battery_capacity",
            inv.entity_prefix.replace(".", "_")
        )
    });

    if let Some(entity) = battery_entity {
        debug!(
            "Battery history fetch interval elapsed for entity: {}",
            entity
        );
        // Actual fetch will be done via web query or separate channel
    } else {
        debug!("No inverter configured for battery history fetch");
    }
}

/// Keep the scheduler's HDO cache in sync with the HA HDO sensor and timezone.
/// Runs before `update_prices_system`, so prices are computed with the current tariff.
pub fn sync_hdo_cache_system(
    hdo_data: Option<Res<crate::async_systems::HdoScheduleData>>,
    hdo_cache: Option<Res<crate::resources::GlobalHdoCache>>,
    timezone: Option<Res<crate::resources::TimezoneConfig>>,
) {
    let (Some(hdo), Some(cache)) = (hdo_data, hdo_cache) else {
        return;
    };
    let timezone_changed = timezone.as_ref().is_some_and(|tz| tz.is_changed());
    if !hdo.is_changed() && !timezone_changed {
        return;
    }

    if let Some(tz) = timezone.as_ref().and_then(|tz| tz.tz) {
        cache.set_timezone(tz);
    }

    if hdo.is_changed()
        && let Some(raw) = hdo.raw_data.as_deref()
    {
        let schedules = crate::strategy::pricing::parse_hdo_sensor_data(raw);
        if !schedules.is_empty() {
            debug!("HDO cache updated with {} day schedules", schedules.len());
            cache.update(schedules);
        }
    }
}

/// Plugin to register all continuous operation systems
pub struct ContinuousSystemsPlugin;

impl Plugin for ContinuousSystemsPlugin {
    fn build(&self, app: &mut App) {
        app
            // Initialize battery history resources
            .init_resource::<BatteryHistory>()
            .init_resource::<BatteryHistoryFetchTimer>()
            .init_resource::<BatteryHistoryInitialized>()
            // Initialize PV generation history resources
            .init_resource::<PvHistory>()
            // Initialize consumption history for winter adaptive strategy
            .init_resource::<crate::components::ConsumptionHistory>()
            // Initialize mode sync tracker for initial sync bypass
            .init_resource::<InitialModeSyncTracker>()
            // Persisted export limit to restore (a restart while curtailed restores it)
            .insert_resource(ExportCurtailmentState::load(EXPORT_LIMIT_RESTORE_PATH))
            .add_systems(
                Startup,
                (
                    initialize_inverters_system,
                    crate::async_systems::setup_async_workers,
                    fetch_initial_battery_history_system,
                )
                    .chain(), // Ensure inverters are created before async workers
            )
            .add_systems(Startup, crate::async_systems::setup_price_cache)
            .add_systems(
                Update,
                (
                    crate::async_systems::poll_consumption_history_channel,
                    crate::async_systems::config_event_handler,
                    crate::async_systems::user_control_event_handler,
                    crate::async_systems::check_health_system,
                    // poll_command_results removed - using direct AsyncInverterWriter
                    crate::async_systems::read_inverter_states_system,
                    // Decompose RawInverterState into individual components
                    crate::async_systems::decompose_inverter_state,
                    // Keep schedule execution but update to use channels
                    schedule_execution_system,
                    export_curtailment_system,
                    // Trigger battery history fetch periodically
                    trigger_battery_history_fetch_system,
                    // Process web queries via message passing (ECS -> Web)
                    crate::web_bridge::web_query_system,
                    // New channel-based systems (non-blocking)
                    sync_hdo_cache_system.before(crate::async_systems::update_prices_system),
                    crate::async_systems::update_prices_system,
                ),
            );
    }
}

#[cfg(test)]
mod hdo_cache_sync_tests {
    use super::*;
    use crate::async_systems::HdoScheduleData;
    use crate::resources::{GlobalHdoCache, TimezoneConfig};
    use crate::scheduling::calculate_effective_prices;
    use chrono::{TimeZone, Utc};
    use fluxion_types::config::{PriceSchedule, PricingConfig};
    use fluxion_types::pricing::TimeBlockPrice;

    fn pricing() -> PricingConfig {
        PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            buy_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: PriceSchedule::Flat(0.0),
            fixed_sell_price_czk: PriceSchedule::Flat(0.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        }
    }

    fn block(utc_hour: u32, spot: f32) -> TimeBlockPrice {
        TimeBlockPrice {
            block_start: Utc.with_ymd_and_hms(2026, 1, 14, utc_hour, 0, 0).unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: spot,
            effective_price_czk_per_kwh: spot,
            spot_sell_price_czk_per_kwh: None,
            buy_price_czk_per_kwh: None,
        }
    }

    #[test]
    fn test_scheduler_effective_price_uses_hdo_schedule_data() {
        let raw = r#"{"data": {"signals": [{"signal": "EVV1", "datum": "14.01.2026",
            "casy": "00:00-06:00; 07:00-09:00"}]}}"#;

        let mut world = World::new();
        world.insert_resource(HdoScheduleData {
            raw_data: Some(raw.to_string()),
            ..Default::default()
        });
        world.insert_resource(GlobalHdoCache::default());
        world.insert_resource(TimezoneConfig::new(Some("Europe/Prague".to_string())));

        let mut schedule = Schedule::default();
        schedule.add_systems(sync_hdo_cache_system);
        schedule.run(&mut world);

        let cache = world.resource::<GlobalHdoCache>();
        // 02:00 UTC = 03:00 Prague (low window), 09:00 UTC = 10:00 Prague (high)
        let mut blocks = vec![block(2, 2.0), block(9, 2.0)];
        calculate_effective_prices(&mut blocks, Some(cache), &pricing());

        // spot + low/high HDO fee + operator buy fee
        assert!((blocks[0].effective_price_czk_per_kwh - 3.00).abs() < 0.001);
        assert!((blocks[1].effective_price_czk_per_kwh - 4.30).abs() < 0.001);
    }
}

#[cfg(test)]
mod levers_execution_tests {
    use super::*;
    use crate::execution::ExecutionConfig;
    use crate::resources::{
        AsyncInverterWriter, InverterConfig, InverterTopology, PriceSchedule, PricingConfig,
        SystemConfig, SystemSettingsConfig, UserControlResource, UserLeversResource,
    };
    use crate::traits::{GenericInverterState, InverterDataSource};
    use async_trait::async_trait;
    use chrono::{DateTime, Duration};
    use fluxion_i18n::Language;
    use fluxion_types::levers::VprWindow;
    use fluxion_types::pricing::TimeBlockPrice;
    use fluxion_types::{UserControlState, UserLevers};
    use std::sync::Mutex;

    /// Records every command the scheduler and curtailment systems send.
    #[derive(Default)]
    struct RecordingSource {
        commands: Arc<Mutex<Vec<(String, InverterCommand)>>>,
    }

    #[async_trait]
    impl InverterDataSource for RecordingSource {
        async fn read_state(&self, _inverter_id: &str) -> anyhow::Result<GenericInverterState> {
            Ok(GenericInverterState::default())
        }

        async fn write_command(
            &self,
            inverter_id: &str,
            command: &InverterCommand,
        ) -> anyhow::Result<()> {
            self.commands
                .lock()
                .unwrap()
                .push((inverter_id.to_string(), command.clone()));
            Ok(())
        }

        async fn health_check(&self) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    fn system_config(max_export_w: u32) -> SystemConfig {
        SystemConfig {
            inverters: vec![InverterConfig {
                id: "inv1".to_string(),
                inverter_type: fluxion_types::InverterType::Solax,
                entity_prefix: "solax".to_string(),
                topology: InverterTopology::Independent,
                pv_extra_prefixes: Vec::new(),
            }],
            pricing_config: PricingConfig {
                spot_price_entity: "sensor.spot_price".to_string(),
                tomorrow_price_entity: None,
                buy_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_price_czk: PriceSchedule::Flat(4.0),
                fixed_sell_price_czk: PriceSchedule::Flat(2.0),
                spot_buy_fee_czk: 0.5,
                spot_sell_fee_czk: 0.5,
                hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
                hdo_low_tariff_czk: 0.50,
                hdo_high_tariff_czk: 1.80,
            },
            control_config: fluxion_types::ControlConfig {
                maximum_export_power_w: max_export_w,
                ..Default::default()
            },
            system_config: SystemSettingsConfig {
                update_interval_secs: 60,
                debug_mode: false,
                display_currency: fluxion_types::config::Currency::CZK,
                language: Language::English,
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

    struct Harness {
        world: World,
        schedule: Schedule,
        commands: Arc<Mutex<Vec<(String, InverterCommand)>>>,
    }

    /// One inverter in SelfUse on hardware, a battery at 50 % and the given schedule blocks.
    fn harness(blocks: Vec<ScheduledMode>, debug: bool) -> Harness {
        let source = RecordingSource::default();
        let commands = source.commands.clone();
        let now = Utc::now();

        let mut world = World::new();
        world.insert_resource(AsyncInverterWriter::new(Arc::new(source)));
        world.insert_resource(ExecutionConfig {
            min_mode_change_interval_secs: 300,
        });
        world.insert_resource(DebugModeConfig {
            enabled: debug,
            ..Default::default()
        });
        world.insert_resource(system_config(8000));
        world.insert_resource(InitialModeSyncTracker::default());
        world.insert_resource(UserControlResource::new(UserControlState::default()));
        world.insert_resource(UserLeversResource {
            levers: UserLevers::default(),
            available: true,
            sync_expected: false,
        });
        world.spawn((
            Inverter {
                id: "inv1".to_string(),
                inverter_type: fluxion_types::InverterType::Solax,
            },
            CurrentMode::default(),
            BatteryStatus {
                soc_percent: 50,
                ..Default::default()
            },
            RawInverterState {
                state: GenericInverterState {
                    work_mode: InverterOperationMode::SelfUse,
                    ..Default::default()
                },
                last_updated: now,
            },
        ));
        world.spawn(OperationSchedule {
            scheduled_blocks: blocks,
            generated_at: now,
            based_on_price_version: now,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(schedule_execution_system);
        Harness {
            world,
            schedule,
            commands,
        }
    }

    impl Harness {
        fn run(&mut self) {
            self.schedule.run(&mut self.world);
        }

        /// Fire-and-forget writes run on the tokio runtime; let them finish first.
        async fn sent_modes(&self) -> Vec<InverterOperationMode> {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, c)| match c {
                    InverterCommand::SetMode(mode) => Some(*mode),
                    InverterCommand::SetBackupMode { .. } => {
                        Some(InverterOperationMode::BackUpMode)
                    }
                    _ => None,
                })
                .collect()
        }

        fn set_levers(&mut self, levers: UserLevers) {
            self.world.resource_mut::<UserLeversResource>().levers = levers;
        }

        /// Hardware read-back (the RawInverterState of the inverter), taken at `read_at`.
        fn set_hardware(&mut self, mode: InverterOperationMode, read_at: DateTime<Utc>) {
            let mut query = self.world.query::<&mut RawInverterState>();
            for mut raw in query.iter_mut(&mut self.world) {
                raw.state.work_mode = mode;
                raw.last_updated = read_at;
            }
        }

        /// Hardware read-back as SolaX reports Manual Mode with an unreadable sub-mode.
        fn set_hardware_ambiguous(&mut self, read_at: DateTime<Utc>) {
            let mut query = self.world.query::<&mut RawInverterState>();
            for mut raw in query.iter_mut(&mut self.world) {
                raw.state.work_mode = InverterOperationMode::ForceCharge;
                raw.state.work_mode_ambiguous = true;
                raw.last_updated = read_at;
            }
        }

        fn set_force_discharge(&mut self, enabled: bool) {
            self.world
                .resource_mut::<SystemConfig>()
                .control_config
                .force_discharge_enabled = enabled;
        }

        fn set_current(&mut self, mode: InverterOperationMode) {
            let mut query = self.world.query::<&mut CurrentMode>();
            for mut current in query.iter_mut(&mut self.world) {
                current.mode = mode;
            }
        }

        fn mark_synced(&mut self) {
            self.world
                .resource_mut::<InitialModeSyncTracker>()
                .synced_inverters
                .insert("inv1".to_string());
        }

        fn current_mode(&mut self) -> CurrentMode {
            let mut query = self.world.query::<&CurrentMode>();
            query.single(&self.world).unwrap().clone()
        }
    }

    fn block(start: DateTime<Utc>, mode: InverterOperationMode) -> ScheduledMode {
        ScheduledMode {
            block_start: start,
            duration_minutes: 60,
            target_inverters: None,
            mode,
            reason: "schedule".to_string(),
            decision_uid: None,
            debug_info: None,
        }
    }

    /// A VPR window covering the whole UTC day, so the tests do not depend on the time of day.
    fn vpr_all_day(mode: VprMode) -> VprWindow {
        VprWindow {
            mode,
            from_local: chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            until_local: chrono::NaiveTime::from_hms_opt(23, 59, 0).unwrap(),
            set_at: Utc::now(),
        }
    }

    #[test]
    fn vpr_mode_mapping_follows_contract() {
        assert_eq!(vpr_inverter_mode(VprMode::None), None);
        assert_eq!(
            vpr_inverter_mode(VprMode::Normal),
            Some(InverterOperationMode::SelfUse)
        );
        assert_eq!(
            vpr_inverter_mode(VprMode::NoDischarge),
            Some(InverterOperationMode::BackUpMode)
        );
        assert_eq!(
            vpr_inverter_mode(VprMode::ChargeFromGrid),
            Some(InverterOperationMode::ForceCharge)
        );
        assert_eq!(
            vpr_inverter_mode(VprMode::SellToGrid),
            Some(InverterOperationMode::FeedinPriority)
        );
    }

    #[tokio::test]
    async fn vpr_active_overrides_schedule_and_bypasses_debounce() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        // Already synced and changed just now: only the VPR bypass can send a change.
        h.world
            .resource_mut::<InitialModeSyncTracker>()
            .synced_inverters
            .insert("inv1".to_string());
        {
            let mut query = h.world.query::<&mut CurrentMode>();
            for mut mode in query.iter_mut(&mut h.world) {
                mode.set_at = now;
            }
        }
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::ChargeBattery)),
            ..Default::default()
        });

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
        assert_eq!(h.current_mode().reason, "VPR: Nabíjení baterie");
    }

    #[tokio::test]
    async fn vpr_with_kill_switch_writes_once_then_selfuse_after_expiry() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::ChargeBattery)),
            automation_disabled: true,
            ..Default::default()
        });

        h.run();
        h.run();
        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );

        // The window has passed (set two days ago, one-minute window): SelfUse once, then nothing.
        h.set_levers(UserLevers {
            vpr: Some(VprWindow {
                mode: VprMode::ChargeBattery,
                from_local: chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                until_local: chrono::NaiveTime::from_hms_opt(0, 1, 0).unwrap(),
                set_at: now - Duration::days(2),
            }),
            automation_disabled: true,
            ..Default::default()
        });
        h.run();
        h.run();
        assert_eq!(
            h.sent_modes().await,
            vec![
                InverterOperationMode::ForceCharge,
                InverterOperationMode::SelfUse
            ]
        );
    }

    #[tokio::test]
    async fn kill_switch_without_vpr_sets_selfuse_once() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            false,
        );
        {
            h.set_hardware(InverterOperationMode::ForceCharge, now);
        }
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });

        h.run();
        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn debug_mode_sends_nothing() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            true,
        );

        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn debug_off_at_runtime_resyncs_and_sends_scheduled_mode() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            true,
        );

        // Debug on: the plan is applied to CurrentMode only, nothing is sent.
        h.run();
        assert!(h.sent_modes().await.is_empty());

        // Debug off: CurrentMode already matches the plan, so the resync must compare
        // against the hardware (SelfUse) and send.
        h.world.resource_mut::<DebugModeConfig>().enabled = false;
        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn kill_switch_restart_with_hardware_discharge_sends_selfuse_once() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            false,
        );
        h.set_hardware(InverterOperationMode::ForceDischarge, now);
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });

        h.run();
        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn kill_switch_debug_then_live_sends_selfuse() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            true,
        );
        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });

        h.run();
        assert!(h.sent_modes().await.is_empty());

        h.world.resource_mut::<DebugModeConfig>().enabled = false;
        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn kill_switch_off_resyncs_against_hardware() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });
        h.run();
        assert!(h.sent_modes().await.is_empty());

        // The owner sets ForceCharge by hand while the kill switch is on
        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.set_levers(UserLevers::default());
        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn startup_waits_for_lever_sync_then_proceeds() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            false,
        );
        h.world.resource_mut::<UserLeversResource>().sync_expected = true;
        h.world.resource_mut::<UserLeversResource>().available = false;

        h.run();
        assert!(h.sent_modes().await.is_empty());

        h.world.resource_mut::<UserLeversResource>().available = true;
        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn drift_confirmed_by_two_reads_resends_once() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::SelfUse);

        // Node-RED wrote Self Use over our ForceCharge: first read, then a read 40 s later
        h.set_hardware(
            InverterOperationMode::ForceCharge,
            now - Duration::seconds(40),
        );
        h.run();
        assert!(h.sent_modes().await.is_empty());

        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.run();
        h.run();
        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn drift_single_transient_read_does_nothing() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::SelfUse);

        h.set_hardware(
            InverterOperationMode::ForceCharge,
            now - Duration::seconds(40),
        );
        h.run();
        h.set_hardware(InverterOperationMode::SelfUse, now);
        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn drift_is_ignored_while_kill_switch_is_on() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::SelfUse);
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });

        h.set_hardware(
            InverterOperationMode::ForceCharge,
            now - Duration::seconds(40),
        );
        h.run();
        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[test]
    fn readback_is_exact_unless_sub_mode_is_unreadable() {
        let exact = |mode| crate::traits::GenericInverterState {
            work_mode: mode,
            ..Default::default()
        };
        let ambiguous = crate::traits::GenericInverterState {
            work_mode: InverterOperationMode::ForceCharge,
            work_mode_ambiguous: true,
            ..Default::default()
        };
        // Exact read: no Manual mode matches another
        assert!(!readback_matches(
            &exact(InverterOperationMode::ForceCharge),
            InverterOperationMode::ForceDischarge
        ));
        assert!(!readback_matches(
            &exact(InverterOperationMode::NoChargeNoDischarge),
            InverterOperationMode::ForceCharge
        ));
        assert!(readback_matches(
            &exact(InverterOperationMode::ForceDischarge),
            InverterOperationMode::ForceDischarge
        ));
        // Unreadable sub-mode: ForceCharge matches every Manual mode, never SelfUse
        assert!(readback_matches(
            &ambiguous,
            InverterOperationMode::NoChargeNoDischarge
        ));
        assert!(readback_matches(
            &ambiguous,
            InverterOperationMode::ForceDischarge
        ));
        assert!(!readback_matches(
            &ambiguous,
            InverterOperationMode::SelfUse
        ));
    }

    #[test]
    fn mode_drift_watch_needs_two_reads_30s_apart_and_rate_limits() {
        let t0 = Utc::now();
        let mut watch = ModeDriftWatch::default();
        // Two mismatched reads 10 s apart: not yet
        assert!(!watch.observe(t0, false, t0));
        assert!(!watch.observe(t0 + Duration::seconds(10), false, t0));
        // Third read 30 s after the first: confirmed
        assert!(watch.observe(
            t0 + Duration::seconds(30),
            false,
            t0 + Duration::seconds(30)
        ));
        // Confirmed again soon after: rate-limited to one per 5 minutes
        assert!(!watch.observe(
            t0 + Duration::seconds(35),
            false,
            t0 + Duration::seconds(35)
        ));
        assert!(!watch.observe(
            t0 + Duration::seconds(65),
            false,
            t0 + Duration::seconds(65)
        ));
        // Five minutes after the last re-apply, two fresh mismatched reads 30 s apart: re-apply
        let later = t0 + Duration::minutes(5) + Duration::seconds(30);
        assert!(!watch.observe(
            later - Duration::seconds(30),
            false,
            later - Duration::seconds(30)
        ));
        assert!(watch.observe(later, false, later));
    }

    #[test]
    fn export_limit_watch_stops_after_restore_is_confirmed() {
        let t0 = Utc::now();
        let mut watch = ExportLimitWatch {
            intended_w: 8000,
            written: true,
            curtailed: false,
            ..Default::default()
        };
        // The first read already matches the restore value: confirmed once
        assert_eq!(watch.observe(t0, Some(8000), t0), Reassert::Confirmed);
        assert!(!watch.written);
        // No further watching, so an owner's later change is left alone
        assert_eq!(
            watch.observe(t0 + Duration::seconds(5), Some(5000), t0),
            Reassert::Hold
        );
        assert_eq!(
            watch.observe(t0 + Duration::seconds(10), Some(8000), t0),
            Reassert::Hold
        );
        assert_eq!(
            watch.observe(t0 + Duration::seconds(15), Some(5000), t0),
            Reassert::Hold
        );
    }

    #[test]
    fn export_limit_watch_rewrites_after_two_mismatches_once_a_minute() {
        let t0 = Utc::now();
        let mut watch = ExportLimitWatch {
            intended_w: 0,
            written: true,
            curtailed: true,
            ..Default::default()
        };
        assert_eq!(watch.observe(t0, Some(8000), t0), Reassert::Hold);
        assert_eq!(
            watch.observe(
                t0 + Duration::seconds(5),
                Some(8000),
                t0 + Duration::seconds(5)
            ),
            Reassert::Rewrite
        );
        assert_eq!(
            watch.observe(
                t0 + Duration::seconds(10),
                Some(8000),
                t0 + Duration::seconds(10)
            ),
            Reassert::Hold
        );
        assert_eq!(
            watch.observe(
                t0 + Duration::seconds(15),
                Some(8000),
                t0 + Duration::seconds(15)
            ),
            Reassert::Hold
        );
        // A minute after the last rewrite the next mismatch rewrites again, not sooner
        assert_eq!(
            watch.observe(
                t0 + Duration::seconds(65),
                Some(8000),
                t0 + Duration::seconds(65)
            ),
            Reassert::Rewrite
        );
        assert_eq!(
            watch.observe(
                t0 + Duration::seconds(70),
                Some(8000),
                t0 + Duration::seconds(70)
            ),
            Reassert::Hold
        );
    }

    #[test]
    fn export_limit_watch_gives_up_after_five_unconfirmed_rewrites() {
        let t0 = Utc::now();
        let mut watch = ExportLimitWatch {
            intended_w: 0,
            written: true,
            curtailed: true,
            ..Default::default()
        };
        let mut outcomes = Vec::new();
        for minute in 0..6 {
            let base = t0 + Duration::minutes(minute);
            outcomes.push(watch.observe(base, Some(8000), base));
            outcomes.push(watch.observe(
                base + Duration::seconds(5),
                Some(8000),
                base + Duration::seconds(5),
            ));
        }
        let rewrites = outcomes.iter().filter(|o| **o == Reassert::Rewrite).count();
        assert_eq!(rewrites, 5);
        assert_eq!(
            outcomes.iter().filter(|o| **o == Reassert::GiveUp).count(),
            1
        );
        assert!(!watch.written);
        // Stopped: no further rewrites or warnings
        assert_eq!(
            watch.observe(
                t0 + Duration::minutes(9),
                Some(8000),
                t0 + Duration::minutes(9)
            ),
            Reassert::Hold
        );
    }

    #[test]
    fn startup_wait_is_bounded_to_120_seconds() {
        let now = Instant::now();
        let mut startup = LeversStartup::default();
        let levers = UserLeversResource {
            sync_expected: true,
            available: false,
            ..Default::default()
        };
        assert!(lever_startup_wait(Some(&levers), &mut startup, now));
        assert!(lever_startup_wait(
            Some(&levers),
            &mut startup,
            now + std::time::Duration::from_secs(119)
        ));
        assert!(!lever_startup_wait(
            Some(&levers),
            &mut startup,
            now + std::time::Duration::from_secs(121)
        ));
        assert!(!lever_startup_wait(None, &mut startup, now));
    }

    #[tokio::test]
    async fn hold_plan_with_exact_hold_readback_does_not_resend() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::NoChargeNoDischarge,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::NoChargeNoDischarge);

        h.set_hardware(
            InverterOperationMode::NoChargeNoDischarge,
            now - Duration::seconds(40),
        );
        h.run();
        h.set_hardware(InverterOperationMode::NoChargeNoDischarge, now);
        h.run();
        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn hold_plan_with_selfuse_readback_resends_once() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::NoChargeNoDischarge,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::NoChargeNoDischarge);

        h.set_hardware(InverterOperationMode::SelfUse, now - Duration::seconds(40));
        h.run();
        h.set_hardware(InverterOperationMode::SelfUse, now);
        h.run();
        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::NoChargeNoDischarge]
        );
    }

    #[tokio::test]
    async fn hold_plan_with_ambiguous_forcecharge_readback_does_not_resend() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::NoChargeNoDischarge,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::NoChargeNoDischarge);

        h.set_hardware_ambiguous(now - Duration::seconds(40));
        h.run();
        h.set_hardware_ambiguous(now);
        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn initial_sync_with_ambiguous_manual_readback_resends_manual_target() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            false,
        );
        h.set_current(InverterOperationMode::ForceCharge);
        h.set_hardware_ambiguous(now);

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn kill_switch_off_resync_sends_forcecharge_over_exact_forcedischarge() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceCharge,
            )],
            false,
        );
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });
        h.run();
        assert!(h.sent_modes().await.is_empty());

        // The owner sets Force Discharge by hand while the kill switch is on
        h.set_hardware(InverterOperationMode::ForceDischarge, now);
        h.set_levers(UserLevers::default());
        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn kill_switch_vpr_resends_manual_target_over_ambiguous_readback_once() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::ChargeBattery)),
            automation_disabled: true,
            ..Default::default()
        });
        // Hardware reads Manual Mode with an unreadable sub-mode, so it may already be Force Charge
        h.set_hardware_ambiguous(now);

        h.run();
        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn kill_switch_skips_inverter_without_state_and_does_not_sync_it() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        let inverter = h
            .world
            .query_filtered::<Entity, With<RawInverterState>>()
            .single(&h.world)
            .unwrap();
        h.world.entity_mut(inverter).remove::<RawInverterState>();
        h.set_levers(UserLevers {
            automation_disabled: true,
            ..Default::default()
        });

        h.run();
        assert!(h.sent_modes().await.is_empty());
        assert!(
            h.world
                .resource::<InitialModeSyncTracker>()
                .synced_inverters
                .is_empty()
        );

        // State arrives: hardware ForceCharge is corrected to SelfUse
        h.world.entity_mut(inverter).insert(RawInverterState {
            state: GenericInverterState {
                work_mode: InverterOperationMode::ForceCharge,
                ..Default::default()
            },
            last_updated: now,
        });
        h.run();
        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[test]
    fn permitted_mode_never_passes_force_discharge_when_disabled() {
        assert_eq!(
            permitted_mode(InverterOperationMode::ForceDischarge, false, "inv1"),
            InverterOperationMode::SelfUse
        );
        assert_eq!(
            permitted_mode(InverterOperationMode::ForceDischarge, true, "inv1"),
            InverterOperationMode::ForceDischarge
        );
        assert_eq!(
            permitted_mode(InverterOperationMode::ForceCharge, false, "inv1"),
            InverterOperationMode::ForceCharge
        );
    }

    #[tokio::test]
    async fn planned_force_discharge_is_sent_as_selfuse_when_disabled() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceDischarge,
            )],
            false,
        );

        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn planned_force_discharge_is_sent_when_enabled() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceDischarge,
            )],
            false,
        );
        h.set_force_discharge(true);

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceDischarge]
        );
    }

    /// Harness with the default mode set to Back Up (brief H19).
    fn backup_harness(blocks: Vec<ScheduledMode>) -> Harness {
        let mut h = harness(blocks, false);
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .default_mode = fluxion_types::user_control::DefaultMode::Backup;
        h
    }

    #[tokio::test]
    async fn backup_default_sends_planner_selfuse_as_backup_mode() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::SelfUse,
        )]);

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::BackUpMode]
        );
    }

    #[tokio::test]
    async fn backup_default_keeps_forcecharge_and_hold_unchanged() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::ForceCharge,
        )]);

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );

        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::NoChargeNoDischarge,
        )]);

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::NoChargeNoDischarge]
        );
    }

    #[tokio::test]
    async fn backup_default_keeps_active_fixed_slot_selfuse() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::SelfUse,
        )]);
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .fixed_time_slots
            .push(fluxion_types::user_control::FixedTimeSlot::new(
                now - Duration::minutes(5),
                now + Duration::hours(1),
                InverterOperationMode::SelfUse,
                None,
            ));
        // Hardware is in Back Up, so the explicit SelfUse slot must be written as SelfUse
        {
            let mut query = h.world.query::<&mut RawInverterState>();
            for mut raw in query.iter_mut(&mut h.world) {
                raw.state.work_mode = InverterOperationMode::BackUpMode;
            }
        }

        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    /// Commands recorded so far (writes run on the tokio runtime; let them finish first).
    async fn sent_commands(h: &Harness) -> Vec<InverterCommand> {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        h.commands
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c)| c.clone())
            .collect()
    }

    #[tokio::test]
    async fn backup_default_hardware_backup_with_different_min_soc_sends_one_setbackup() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::SelfUse,
        )]);
        h.set_hardware(InverterOperationMode::BackUpMode, now);
        // The adapter last confirmed 10 %, the target is the default 25 %
        h.world
            .resource::<AsyncInverterWriter>()
            .set_confirmed_backup_min_soc("inv1", 10);

        h.run();

        let commands = sent_commands(&h).await;
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(matches!(
            commands[0],
            InverterCommand::SetBackupMode { min_soc_pct: 25 }
        ));
    }

    #[tokio::test]
    async fn backup_default_hardware_backup_with_same_min_soc_sends_nothing() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::SelfUse,
        )]);
        h.set_hardware(InverterOperationMode::BackUpMode, now);
        h.world
            .resource::<AsyncInverterWriter>()
            .set_confirmed_backup_min_soc("inv1", 25);

        h.run();

        assert!(sent_commands(&h).await.is_empty());
    }

    #[tokio::test]
    async fn backup_default_drift_reapply_uses_setbackupmode_not_plain_backup_mode() {
        let now = Utc::now();
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::SelfUse,
        )]);
        h.mark_synced();
        h.set_current(InverterOperationMode::BackUpMode);
        h.world
            .resource::<AsyncInverterWriter>()
            .set_confirmed_backup_min_soc("inv1", 25);

        // Another controller moved the inverter to Self Use; confirmed by two reads 40 s apart
        h.set_hardware(InverterOperationMode::SelfUse, now - Duration::seconds(40));
        h.run();
        h.set_hardware(InverterOperationMode::SelfUse, now);
        h.run();
        h.run();

        let commands = sent_commands(&h).await;
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(matches!(
            commands[0],
            InverterCommand::SetBackupMode { min_soc_pct: 25 }
        ));
    }

    #[tokio::test]
    async fn backup_default_floor_blocks_planned_force_discharge_below_backup_target() {
        let now = Utc::now();
        // Battery is at 50 %, the Back Up target is 60 %: discharge stops at the higher floor
        let mut h = backup_harness(vec![block(
            now - Duration::minutes(1),
            InverterOperationMode::ForceDischarge,
        )]);
        h.set_force_discharge(true);
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .backup_min_soc_pct = 60;

        h.run();

        assert!(sent_commands(&h).await.is_empty());
    }

    #[tokio::test]
    async fn selfuse_floor_still_allows_planned_force_discharge_at_same_soc() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::ForceDischarge,
            )],
            false,
        );
        h.set_force_discharge(true);
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .backup_min_soc_pct = 60;

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceDischarge]
        );
    }

    #[tokio::test]
    async fn backup_default_debug_mode_sends_nothing() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            true,
        );
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .default_mode = fluxion_types::user_control::DefaultMode::Backup;

        h.run();

        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn vpr_discharge_battery_is_blocked_when_disabled() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::DischargeBattery)),
            ..Default::default()
        });

        h.run();

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    #[tokio::test]
    async fn vpr_discharge_battery_is_sent_when_enabled() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.set_force_discharge(true);
        h.set_hardware(InverterOperationMode::ForceCharge, now);
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::DischargeBattery)),
            ..Default::default()
        });

        h.run();

        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceDischarge]
        );
    }

    #[tokio::test]
    async fn fixed_slot_force_discharge_runs_as_selfuse_once_when_disabled() {
        let now = Utc::now();
        let mut h = harness(
            vec![block(
                now - Duration::minutes(1),
                InverterOperationMode::SelfUse,
            )],
            false,
        );
        h.mark_synced();
        h.set_current(InverterOperationMode::ForceCharge);
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .fixed_time_slots
            .push(fluxion_types::FixedTimeSlot {
                id: "slot1".to_string(),
                from: now - Duration::hours(1),
                to: now + Duration::hours(1),
                mode: InverterOperationMode::ForceDischarge,
                note: None,
                created_at: now,
            });

        // Six 5-minute steps (30 simulated minutes): each step is past the mode-change debounce
        for _ in 0..6 {
            let past = Utc::now() - Duration::minutes(6);
            let mut query = h.world.query::<&mut CurrentMode>();
            for mut current in query.iter_mut(&mut h.world) {
                current.set_at = past;
            }
            h.run();
        }

        assert_eq!(h.sent_modes().await, vec![InverterOperationMode::SelfUse]);
    }

    // ---- Block boundaries (brief H25) ----

    /// One 15-minute schedule block.
    fn qblock(start: DateTime<Utc>, mode: InverterOperationMode, reason: &str) -> ScheduledMode {
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

    impl Harness {
        /// Replace the schedule, generated at `generated_at`, with one block.
        fn set_schedule(&mut self, block: ScheduledMode, generated_at: DateTime<Utc>) {
            let mut query = self.world.query::<&mut OperationSchedule>();
            for mut schedule in query.iter_mut(&mut self.world) {
                schedule.scheduled_blocks = vec![block.clone()];
                schedule.generated_at = generated_at;
            }
        }

        /// The next run is 5+ minutes after the last change: the 300 s debounce has passed.
        fn age_last_change(&mut self) {
            let past = Utc::now() - Duration::minutes(6);
            let mut query = self.world.query::<&mut CurrentMode>();
            for mut current in query.iter_mut(&mut self.world) {
                current.set_at = past;
            }
        }
    }

    const HOLD: InverterOperationMode = InverterOperationMode::NoChargeNoDischarge;
    const SELF_USE: InverterOperationMode = InverterOperationMode::SelfUse;

    /// Synced, Hold on the inverter, and a block that started `ago` before now.
    fn boundary_harness(ago: Duration) -> (Harness, DateTime<Utc>) {
        let block_start = Utc::now() - ago;
        let mut h = harness(vec![qblock(block_start, HOLD, "WinterReserve HOLD")], false);
        h.mark_synced();
        h.set_current(HOLD);
        h.set_hardware(HOLD, Utc::now());
        h.age_last_change();
        (h, block_start)
    }

    /// A block locked in Hold: the fresh in-block schedule confirmed it.
    fn locked_hold_harness() -> (Harness, DateTime<Utc>) {
        let (mut h, start) = boundary_harness(Duration::minutes(2));
        h.set_schedule(qblock(start, HOLD, "WinterReserve HOLD"), Utc::now());
        h.run();
        h.age_last_change();
        (h, start)
    }

    #[tokio::test]
    async fn boundary_flap_from_the_evidence_sends_nothing() {
        // 2026-10-17 04:00Z: the boundary ran the projection made before the block (SELF-USE),
        // the head replan 0.3 s later kept HOLD, and the debounce let HOLD through at 04:05.
        // Before H25 that was two writes: [SelfUse, Hold]. Hold is what the head keeps: no write.
        let (mut h, start) = boundary_harness(Duration::seconds(1));
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE (projection)"),
            start - Duration::seconds(3),
        );
        h.run();
        h.set_schedule(qblock(start, HOLD, "WinterReserve HOLD"), Utc::now());
        h.run();
        // 04:05 and 04:10: the head replans keep HOLD
        for _ in 0..2 {
            h.age_last_change();
            h.set_schedule(qblock(start, HOLD, "WinterReserve HOLD"), Utc::now());
            h.run();
        }
        assert!(h.sent_modes().await.is_empty());
    }

    #[tokio::test]
    async fn boundary_change_writes_once_and_planner_change_waits_for_next_block() {
        let (mut h, start) = boundary_harness(Duration::seconds(1));
        // Fresh in-block schedule switches to SelfUse at the boundary: one write
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            Utc::now(),
        );
        h.run();
        // 04:05 and 04:10: forecast noise flips the head back to HOLD; held until 04:15
        for _ in 0..2 {
            h.age_last_change();
            h.set_schedule(qblock(start, HOLD, "WinterReserve HOLD"), Utc::now());
            h.run();
        }
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
        assert_eq!(h.current_mode().mode, SELF_USE);
    }

    #[tokio::test]
    async fn next_block_boundary_applies_the_held_change() {
        let (mut h, start) = locked_hold_harness();
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            Utc::now(),
        );
        h.run();
        assert!(h.sent_modes().await.is_empty());
        // The next block (a new block start, fresh schedule) is not locked
        let next = Utc::now() - Duration::seconds(1);
        h.set_schedule(qblock(next, SELF_USE, "WinterReserve SELF-USE"), Utc::now());
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn stale_schedule_at_boundary_waits_then_applies_after_60_s() {
        // 10 s into the block, schedule from before the block: wait
        let (mut h, start) = boundary_harness(Duration::seconds(10));
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            start - Duration::seconds(5),
        );
        h.run();
        assert!(h.sent_modes().await.is_empty());

        // 61 s into the block and still no fresh schedule: apply what is there
        let (mut h, start) = boundary_harness(Duration::seconds(61));
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            start - Duration::seconds(5),
        );
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn lever_change_mid_block_applies_at_once() {
        let (mut h, start) = locked_hold_harness();
        h.set_levers(UserLevers {
            force_charge_at_or_below_czk: Some(3.0),
            ..Default::default()
        });
        h.set_schedule(
            qblock(
                start,
                InverterOperationMode::ForceCharge,
                "lever: force charge",
            ),
            Utc::now(),
        );
        h.run();
        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn vpr_mid_block_applies_at_once() {
        let (mut h, _) = locked_hold_harness();
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::Normal)),
            ..Default::default()
        });
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn kill_switch_mid_block_applies_at_once() {
        let (mut h, _) = locked_hold_harness();
        h.world.resource_mut::<UserControlResource>().state.enabled = false;
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn fixed_slot_start_and_end_mid_block_apply_at_once() {
        let (mut h, _) = locked_hold_harness();
        let now = Utc::now();
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .fixed_time_slots
            .push(fluxion_types::FixedTimeSlot {
                id: "slot1".to_string(),
                from: now - Duration::minutes(1),
                to: now + Duration::hours(1),
                mode: SELF_USE,
                note: None,
                created_at: now,
            });
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
        // The slot is removed: back to the planned Hold at once
        h.age_last_change();
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .fixed_time_slots
            .clear();
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE, HOLD]);
    }

    #[tokio::test]
    async fn guard_change_mid_block_applies_at_once() {
        let (mut h, start) = boundary_harness(Duration::minutes(2));
        h.set_force_discharge(true);
        h.set_current(InverterOperationMode::ForceDischarge);
        h.set_hardware(InverterOperationMode::ForceDischarge, Utc::now());
        h.set_schedule(
            qblock(
                start,
                InverterOperationMode::ForceDischarge,
                "WinterReserve EXPORT",
            ),
            Utc::now(),
        );
        h.run();
        h.age_last_change();
        h.set_schedule(
            qblock(
                start,
                SELF_USE,
                "WinterReserve SELF-USE | guard: export held at night floor 40.0 kWh, SOC 50%",
            ),
            Utc::now(),
        );
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn force_charge_stop_at_max_soc_mid_block_applies_at_once() {
        let (mut h, start) = boundary_harness(Duration::minutes(2));
        h.set_current(InverterOperationMode::ForceCharge);
        h.set_hardware(InverterOperationMode::ForceCharge, Utc::now());
        h.set_schedule(
            qblock(
                start,
                InverterOperationMode::ForceCharge,
                "WinterReserve CHARGE",
            ),
            Utc::now(),
        );
        h.run();
        // The battery (50 %) reaches the maximum SOC; the replan stops the charge
        h.world
            .resource_mut::<SystemConfig>()
            .control_config
            .max_battery_soc = 50.0;
        h.age_last_change();
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            Utc::now(),
        );
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn lever_change_seen_before_its_replan_still_applies_mid_block() {
        // Review H25 #1: the executor sees the new lever with the old schedule (same mode), and
        // only a later run sees the replan. The lever change must still apply in this block.
        let (mut h, start) = locked_hold_harness();
        h.set_levers(UserLevers {
            force_charge_at_or_below_czk: Some(3.0),
            ..Default::default()
        });
        h.run();
        assert!(h.sent_modes().await.is_empty());
        h.set_schedule(
            qblock(
                start,
                InverterOperationMode::ForceCharge,
                "lever: force charge",
            ),
            Utc::now(),
        );
        h.run();
        assert_eq!(
            h.sent_modes().await,
            vec![InverterOperationMode::ForceCharge]
        );
    }

    #[tokio::test]
    async fn config_change_seen_before_its_replan_still_applies_mid_block() {
        // Review H25 #2: locked in ForceDischarge, the owner disables force discharge in the web
        // config; the replan then says SelfUse. Not held until the next block.
        let (mut h, start) = boundary_harness(Duration::minutes(2));
        h.set_force_discharge(true);
        h.set_current(InverterOperationMode::ForceDischarge);
        h.set_hardware(InverterOperationMode::ForceDischarge, Utc::now());
        h.set_schedule(
            qblock(
                start,
                InverterOperationMode::ForceDischarge,
                "WinterReserve EXPORT",
            ),
            Utc::now(),
        );
        h.run();
        h.age_last_change();
        h.set_force_discharge(false);
        h.run();
        h.set_schedule(
            qblock(start, SELF_USE, "WinterReserve SELF-USE"),
            Utc::now(),
        );
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn fixed_slot_at_boundary_does_not_wait_for_fresh_schedule() {
        // Review H25 #3: only planner changes wait for the in-block schedule
        let (mut h, start) = boundary_harness(Duration::seconds(1));
        h.set_schedule(
            qblock(start, HOLD, "WinterReserve HOLD (projection)"),
            start - Duration::seconds(3),
        );
        let now = Utc::now();
        h.world
            .resource_mut::<UserControlResource>()
            .state
            .fixed_time_slots
            .push(fluxion_types::FixedTimeSlot {
                id: "slot1".to_string(),
                from: start,
                to: now + Duration::hours(1),
                mode: SELF_USE,
                note: None,
                created_at: now,
            });
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[tokio::test]
    async fn vpr_at_boundary_does_not_wait_for_fresh_schedule() {
        let (mut h, start) = boundary_harness(Duration::seconds(1));
        h.set_schedule(
            qblock(start, HOLD, "WinterReserve HOLD (projection)"),
            start - Duration::seconds(3),
        );
        h.set_levers(UserLevers {
            vpr: Some(vpr_all_day(VprMode::Normal)),
            ..Default::default()
        });
        h.run();
        assert_eq!(h.sent_modes().await, vec![SELF_USE]);
    }

    #[test]
    fn block_gate_rules() {
        let start = Utc::now() - Duration::minutes(5);
        let intent = UserIntent::of(None, None, &ControlConfig::default(), None, start);
        let lock = BlockLock {
            block_start: start,
            intent: intent.clone(),
            held_logged: false,
            released: false,
        };
        let fresh = Some(start + Duration::seconds(1));
        let now = start + Duration::minutes(5);
        assert_eq!(
            block_gate(Some(&lock), start, fresh, &intent, false, now),
            BlockGate::HeldUntilBoundary
        );
        // Good reason, or the user's intent changed since the lock
        assert_eq!(
            block_gate(Some(&lock), start, fresh, &intent, true, now),
            BlockGate::Apply
        );
        let changed = UserIntent {
            disallow_charge: true,
            ..intent.clone()
        };
        assert_eq!(
            block_gate(Some(&lock), start, fresh, &changed, false, now),
            BlockGate::Apply
        );
        // A lock of an earlier block does not hold the next one
        let next = start + Duration::minutes(15);
        assert_eq!(
            block_gate(Some(&lock), next, Some(next), &intent, false, next),
            BlockGate::Apply
        );
        // Stale schedule: wait under 60 s, apply from 60 s
        assert_eq!(
            block_gate(
                None,
                next,
                fresh,
                &intent,
                false,
                next + Duration::seconds(59)
            ),
            BlockGate::WaitForFreshSchedule
        );
        assert_eq!(
            block_gate(
                None,
                next,
                fresh,
                &intent,
                false,
                next + Duration::seconds(60)
            ),
            BlockGate::ApplyStale
        );
        // Good reasons (VPR, fixed slot, guards) never wait for the fresh schedule
        assert_eq!(
            block_gate(
                None,
                next,
                fresh,
                &intent,
                true,
                next + Duration::seconds(1)
            ),
            BlockGate::Apply
        );
        // A released lock holds nothing for the rest of its block
        let released = BlockLock {
            released: true,
            ..lock.clone()
        };
        assert_eq!(
            block_gate(Some(&released), start, fresh, &intent, false, now),
            BlockGate::Apply
        );
        // A control config edit or a new Back Up discharge minimum is an intent change
        let edited = ControlConfig {
            force_discharge_enabled: !ControlConfig::default().force_discharge_enabled,
            ..ControlConfig::default()
        };
        assert_ne!(UserIntent::of(None, None, &edited, None, start), intent);
        assert_ne!(
            UserIntent::of(None, None, &ControlConfig::default(), Some(30.0), start),
            intent
        );
    }

    /// Export curtailment world: one inverter reading `read_back_w` (master or independent) and a
    /// sell price of `sell` in the current block.
    struct ExportHarness {
        world: World,
        schedule: Schedule,
        commands: Arc<Mutex<Vec<(String, InverterCommand)>>>,
    }

    fn sell_price_data(now: DateTime<Utc>, sell: Option<f32>) -> SpotPriceData {
        SpotPriceData {
            time_block_prices: vec![TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: now - Duration::minutes(1),
                duration_minutes: 15,
                price_czk_per_kwh: 1.0,
                effective_price_czk_per_kwh: 1.5,
                spot_sell_price_czk_per_kwh: sell,
            }],
            block_duration_minutes: 15,
            fetched_at: now,
            ha_last_updated: now,
        }
    }

    fn export_harness(read_back_w: Option<u32>, master: bool, sell: Option<f32>) -> ExportHarness {
        let source = RecordingSource::default();
        let commands = source.commands.clone();
        let now = Utc::now();
        let mut world = World::new();
        world.insert_resource(AsyncInverterWriter::new(Arc::new(source)));
        world.insert_resource(DebugModeConfig {
            enabled: false,
            ..Default::default()
        });
        let mut config = system_config(8000);
        config.control_config.export_curtailment_enabled = true;
        if master {
            config.inverters[0].topology = InverterTopology::Master {
                slave_ids: Vec::new(),
            };
        }
        world.insert_resource(config);
        world.insert_resource(ExportCurtailmentState::default());
        world.spawn((
            Inverter {
                id: "inv1".to_string(),
                inverter_type: fluxion_types::InverterType::Solax,
            },
            RawInverterState {
                state: GenericInverterState {
                    export_limit_w: read_back_w,
                    ..Default::default()
                },
                last_updated: now,
            },
        ));
        world.spawn(sell_price_data(now, sell));

        let mut schedule = Schedule::default();
        schedule.add_systems(export_curtailment_system);
        ExportHarness {
            world,
            schedule,
            commands,
        }
    }

    impl ExportHarness {
        fn run(&mut self) {
            self.schedule.run(&mut self.world);
        }

        /// (inverter id, limit) of every SetExportLimit sent so far.
        async fn sent(&self) -> Vec<(String, u32)> {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(id, c)| match c {
                    InverterCommand::SetExportLimit(w) => Some((id.clone(), *w)),
                    _ => None,
                })
                .collect()
        }

        fn set_sell(&mut self, sell: Option<f32>) {
            let now = Utc::now();
            let mut query = self.world.query::<&mut SpotPriceData>();
            for mut data in query.iter_mut(&mut self.world) {
                *data = sell_price_data(now, sell);
            }
        }

        fn set_read_back(&mut self, read_back_w: Option<u32>, read_at: DateTime<Utc>) {
            let mut query = self.world.query::<&mut RawInverterState>();
            for mut raw in query.iter_mut(&mut self.world) {
                raw.state.export_limit_w = read_back_w;
                raw.last_updated = read_at;
            }
        }

        /// The inverter's own maximum for the export limit (its `max` attribute).
        fn set_entity_max(&mut self, max_w: Option<u32>, read_at: DateTime<Utc>) {
            let mut query = self.world.query::<&mut RawInverterState>();
            for mut raw in query.iter_mut(&mut self.world) {
                raw.state.export_limit_max_w = max_w;
                raw.last_updated = read_at;
            }
        }

        /// Simulates a minute passing between two rewrites of the export limit.
        fn reset_rewrite_pacing(&mut self) {
            if let Some(watch) = self
                .world
                .resource_mut::<ExportCurtailmentState>()
                .limit_watch
                .get_mut("inv1")
            {
                watch.last_rewrite_at = None;
            }
        }

        fn set_last_write_at(&mut self, at: DateTime<Utc>) {
            if let Some(watch) = self
                .world
                .resource_mut::<ExportCurtailmentState>()
                .limit_watch
                .get_mut("inv1")
            {
                watch.last_write_at = Some(at);
            }
        }

        fn set_control(&mut self, edit: impl FnOnce(&mut fluxion_types::ControlConfig)) {
            edit(&mut self.world.resource_mut::<SystemConfig>().control_config);
        }
    }

    fn restore_entry(h: &ExportHarness) -> Option<u32> {
        h.world
            .resource::<ExportCurtailmentState>()
            .restore_limits_w
            .get("inv1")
            .copied()
    }

    #[tokio::test]
    async fn export_curtailed_at_negative_price_and_restored_once() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));

        h.run();
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        h.set_sell(Some(1.0));
        h.run();
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
        // Kept until a fresh read-back confirms the restore
        assert_eq!(restore_entry(&h), Some(8000));
        h.set_read_back(Some(8000), Utc::now() + Duration::seconds(1));
        h.run();
        assert_eq!(restore_entry(&h), None);
    }

    #[tokio::test]
    async fn export_debug_mode_never_changes_state_or_sends() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.world.resource_mut::<DebugModeConfig>().enabled = true;

        h.run();
        assert!(h.sent().await.is_empty());
        assert_eq!(restore_entry(&h), None);

        h.world.resource_mut::<DebugModeConfig>().enabled = false;
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);
    }

    #[tokio::test]
    async fn export_missing_sell_price_restores() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        h.set_sell(None);
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_kill_switch_restores_once_and_blocks_curtailment() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        h.world.insert_resource(UserLeversResource {
            levers: UserLevers {
                automation_disabled: true,
                ..Default::default()
            },
            available: true,
            sync_expected: false,
        });
        h.run();
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_vpr_sell_to_grid_blocks_curtailment() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.world.insert_resource(UserLeversResource {
            levers: UserLevers {
                vpr: Some(vpr_all_day(VprMode::SellToGrid)),
                ..Default::default()
            },
            available: true,
            sync_expected: false,
        });

        h.run();

        assert!(h.sent().await.is_empty());
    }

    #[tokio::test]
    async fn export_legacy_restore_file_goes_to_master() {
        let mut h = export_harness(Some(0), true, Some(1.0));
        h.world.insert_resource(ExportCurtailmentState {
            legacy_restore_limit_w: Some(8000),
            ..Default::default()
        });

        h.run();

        assert_eq!(h.sent().await, vec![("inv1".to_string(), 8000)]);
    }

    #[tokio::test]
    async fn export_legacy_restore_file_goes_to_independent_inverter() {
        let mut h = export_harness(Some(0), false, Some(1.0));
        h.world.insert_resource(ExportCurtailmentState {
            legacy_restore_limit_w: Some(8000),
            ..Default::default()
        });

        h.run();

        assert_eq!(h.sent().await, vec![("inv1".to_string(), 8000)]);
    }

    #[tokio::test]
    async fn export_unknown_limit_does_not_curtail() {
        let mut h = export_harness(None, false, Some(-0.1));

        h.run();
        h.run();

        assert!(h.sent().await.is_empty());
        assert_eq!(restore_entry(&h), None);
    }

    #[tokio::test]
    async fn export_restore_is_clamped_to_entity_max() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // The inverter's own maximum is now 6000 W: the exact pre-curtail 8000 W is clamped
        h.set_entity_max(Some(6000), now + Duration::seconds(1));
        h.set_read_back(Some(6000), now + Duration::seconds(1));
        h.set_sell(Some(1.0));
        h.run();

        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 6000)]
        );
    }

    #[tokio::test]
    async fn export_restore_is_clamped_to_hard_cap() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.set_control(|c| c.export_limit_hard_cap_w = Some(5000));
        h.run();
        h.set_sell(Some(1.0));
        h.run();

        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 5000)]
        );
    }

    #[tokio::test]
    async fn export_restore_is_clamped_to_maximum_export_power() {
        // Pre-curtail limit above maximum_export_power_w (8000 W in the harness)
        let mut h = export_harness(Some(9000), false, Some(-0.1));
        h.run();
        h.set_sell(Some(1.0));
        h.run();

        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_curtail_uses_export_limit_min_w() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.set_control(|c| c.export_limit_min_w = 10);
        h.run();
        h.set_sell(Some(1.0));
        h.run();

        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 10), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_curtail_target_is_clamped_too() {
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.set_control(|c| {
            c.export_limit_min_w = 10;
            c.export_limit_hard_cap_w = Some(5);
        });
        h.run();

        assert_eq!(h.sent().await, vec![("inv1".to_string(), 5)]);
    }

    #[tokio::test]
    async fn export_curtail_skipped_when_target_is_not_below_current_limit() {
        let mut h = export_harness(Some(5), false, Some(-0.1));
        h.set_control(|c| c.export_limit_min_w = 10);
        h.run();

        assert!(h.sent().await.is_empty());
        assert_eq!(restore_entry(&h), None);
    }

    #[tokio::test]
    async fn export_zero_read_back_at_startup_still_restores_entry() {
        let mut h = export_harness(Some(0), false, Some(1.0));
        h.world
            .resource_mut::<ExportCurtailmentState>()
            .restore_limits_w
            .insert("inv1".to_string(), 8000);

        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 8000)]);
        // Kept until the read-back confirms the restore
        assert_eq!(restore_entry(&h), Some(8000));

        h.set_read_back(Some(8000), Utc::now() + Duration::seconds(1));
        h.run();
        assert_eq!(restore_entry(&h), None);
    }

    #[tokio::test]
    async fn export_inverter_already_at_zero_is_skipped() {
        let mut h = export_harness(Some(0), false, Some(-0.1));

        h.run();
        h.run();

        assert!(h.sent().await.is_empty());
        assert_eq!(restore_entry(&h), None);
    }

    #[tokio::test]
    async fn export_restore_is_kept_until_confirmed_and_retried_after_give_up() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // Price positive again: the restore is sent, but the read-back stays at 0 W
        h.set_sell(Some(1.0));
        h.set_read_back(Some(0), now + Duration::seconds(1));
        h.run();
        assert_eq!(restore_entry(&h), Some(8000));

        // Five rewrites, then the sixth unconfirmed pair gives up
        for i in 0..6 {
            h.reset_rewrite_pacing();
            h.set_read_back(Some(0), now + Duration::seconds(2 + 2 * i));
            h.run();
            h.set_read_back(Some(0), now + Duration::seconds(3 + 2 * i));
            h.run();
        }
        // Given up: the stored limit is kept and nothing is re-sent right away
        assert_eq!(restore_entry(&h), Some(8000));
        h.run();

        // A minute later the restore is retried, still clamped and kept
        h.set_last_write_at(now - Duration::minutes(2));
        h.run();
        assert_eq!(restore_entry(&h), Some(8000));

        // 1 curtail + 1 restore + 5 rewrites + 1 retry
        let mut expected = vec![("inv1".to_string(), 0)];
        expected.extend(std::iter::repeat_n(("inv1".to_string(), 8000), 7));
        assert_eq!(h.sent().await, expected);
    }

    #[tokio::test]
    async fn export_restore_is_dropped_after_three_give_ups() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // Price positive again: the restore is sent, but the read-back stays at 0 W
        h.set_sell(Some(1.0));
        let mut t = 1;
        h.set_read_back(Some(0), now + Duration::seconds(t));
        t += 1;
        h.run();

        for cycle in 1..=3 {
            // Five rewrites, then the sixth unconfirmed pair gives up
            for _ in 0..6 {
                h.reset_rewrite_pacing();
                h.set_read_back(Some(0), now + Duration::seconds(t));
                t += 1;
                h.run();
                h.set_read_back(Some(0), now + Duration::seconds(t));
                t += 1;
                h.run();
            }
            if cycle < 3 {
                // Kept for retry after a give-up, then restored again
                assert_eq!(restore_entry(&h), Some(8000));
                h.set_last_write_at(Utc::now() - Duration::minutes(2));
                h.run();
            }
        }

        // Third give-up: the stored value is dropped and nothing is re-sent
        assert_eq!(restore_entry(&h), None);
        h.run();
        h.run();

        // 1 curtail + 3 restores + 15 rewrites
        let mut expected = vec![("inv1".to_string(), 0)];
        expected.extend(std::iter::repeat_n(("inv1".to_string(), 8000), 18));
        assert_eq!(h.sent().await, expected);
    }

    #[tokio::test]
    async fn export_write_resets_mismatch_count_single_mismatch_does_not_rewrite() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        // Curtail write: the first read (8000 W) mismatches the 0 W target once
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // Restore write: the single mismatching read (0 W against 8000 W) must not rewrite
        h.set_sell(Some(1.0));
        h.set_read_back(Some(0), now + Duration::seconds(1));
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
        assert_eq!(restore_entry(&h), Some(8000));
    }

    #[tokio::test]
    async fn export_read_back_below_restore_target_is_owner_choice() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        // Curtailed limit confirmed, so no stale mismatch carries into the restore
        h.set_sell(Some(1.0));
        h.set_read_back(Some(0), now + Duration::seconds(2));
        h.run();
        assert_eq!(restore_entry(&h), Some(8000));

        // The owner sets 6000 W: between the curtail target (0 W) and the restore target (8000 W)
        h.set_read_back(Some(6000), now + Duration::seconds(3));
        h.run();
        assert_eq!(restore_entry(&h), None);

        // Stays put: no rewrite toward 8000 W
        h.set_read_back(Some(6000), now + Duration::seconds(4));
        h.run();
        h.set_read_back(Some(6000), now + Duration::seconds(5));
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_owner_change_after_confirmed_restore_is_not_overwritten() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        h.set_read_back(Some(0), now + Duration::seconds(1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // Price positive again: restore to 8000 W, then the read-back confirms it
        h.set_sell(Some(1.0));
        h.run();
        h.set_read_back(Some(8000), now + Duration::seconds(2));
        h.run();
        assert_eq!(restore_entry(&h), None);
        // The owner sets 5000 W by hand: FluxION must not write
        h.set_read_back(Some(5000), now + Duration::seconds(3));
        h.run();
        h.set_read_back(Some(5000), now + Duration::seconds(4));
        h.run();

        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 8000)]
        );
    }

    #[tokio::test]
    async fn export_read_back_drift_is_rewritten() {
        let now = Utc::now();
        let mut h = export_harness(Some(8000), false, Some(-0.1));
        h.run();
        assert_eq!(h.sent().await, vec![("inv1".to_string(), 0)]);

        // The inverter still reads 8000 W on two consecutive reads
        h.set_read_back(Some(8000), now + Duration::seconds(1));
        h.run();
        assert_eq!(
            h.sent().await,
            vec![("inv1".to_string(), 0), ("inv1".to_string(), 0)]
        );
    }

    #[test]
    fn export_curtailment_is_opt_in() {
        assert!(!fluxion_types::ControlConfig::default().export_curtailment_enabled);
    }

    #[test]
    fn export_limit_clamp_applies_every_cap() {
        let mut control = fluxion_types::ControlConfig {
            maximum_export_power_w: 8000,
            ..Default::default()
        };
        assert_eq!(clamp_export_limit(9000, &control, None), 8000);
        assert_eq!(clamp_export_limit(9000, &control, Some(6000)), 6000);
        control.export_limit_hard_cap_w = Some(5000);
        assert_eq!(clamp_export_limit(9000, &control, Some(6000)), 5000);
        assert_eq!(clamp_export_limit(3000, &control, Some(6000)), 3000);
        assert_eq!(clamp_export_limit(0, &control, Some(6000)), 0);
    }

    #[test]
    fn export_limit_restore_survives_restart() {
        let path = std::env::temp_dir().join(format!(
            "fluxion-export-limit-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut state = ExportCurtailmentState {
            persist_path: Some(path.clone()),
            ..Default::default()
        };
        state.restore_limits_w.insert("inv1".to_string(), 8000);
        state.persist();

        let loaded = ExportCurtailmentState::load(path.clone());
        assert_eq!(loaded.restore_limits_w.get("inv1"), Some(&8000));
        assert_eq!(loaded.legacy_restore_limit_w, None);

        state.restore_limits_w.clear();
        state.persist();
        assert!(!path.exists());
    }

    #[test]
    fn export_limit_old_single_value_file_is_read_as_legacy() {
        let path = std::env::temp_dir().join(format!(
            "fluxion-export-limit-legacy-test-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{"restore_limit_w": 8000}"#).unwrap();

        let loaded = ExportCurtailmentState::load(path.clone());
        assert!(loaded.restore_limits_w.is_empty());
        assert_eq!(loaded.legacy_restore_limit_w, Some(8000));
        let _ = std::fs::remove_file(&path);
    }
}
