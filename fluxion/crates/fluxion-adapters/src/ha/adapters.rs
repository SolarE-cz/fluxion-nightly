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

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::ha::actuation::{self, ActuationGuardConfig, ActuationState};
use crate::ha::client::HomeAssistantClient;
use fluxion_core::continuous_systems::STATE_STALE_SECS;
use fluxion_core::pricing::parse_spot_price_response;
use fluxion_core::{
    GenericInverterState, InverterCommand, InverterDataSource, InverterOperationMode,
    PriceDataSource, SpotPriceData, VendorEntityMapper,
};
use fluxion_types::pricing::TimeBlockPrice;

/// Home Assistant adapter implementing InverterDataSource
/// Uses VendorEntityMapper to map between generic and vendor-specific entities
pub struct HomeAssistantInverterAdapter {
    client: Arc<HomeAssistantClient>,
    mapper: Arc<dyn VendorEntityMapper>,
    /// Pre-actuation guards (lock, wake, charge check), run before every write
    guard: ActuationGuardConfig,
    actuation: Arc<ActuationState>,
    /// Read-only PV extras per inverter id: entity prefixes of parallel units whose PV power and
    /// solar energy are added to that inverter's. Only `read_state` uses them; no write path does.
    pv_extra_prefixes: HashMap<String, Vec<String>>,
    /// Extra PV entities already warned about as unavailable (WARN once each)
    pv_extra_warned: Mutex<std::collections::HashSet<String>>,
    /// Capacity-weighted battery SOC per inverter id: its own battery plus read-only parallel
    /// units' batteries. Only `read_state` uses them; no write path does.
    battery_soc_weights: HashMap<String, BatterySocWeights>,
    /// Extra battery SOC entities already warned about as unreadable (WARN once each)
    battery_soc_warned: Mutex<std::collections::HashSet<String>>,
    /// Last combined SOC per inverter id and when it was read, held while a parallel unit's SOC
    /// is unreadable for up to `PAIR_SOC_HOLD_SECS`
    battery_soc_last_good: Mutex<HashMap<String, (f32, DateTime<Utc>)>>,
    /// Heartbeat entities per entity prefix, found once from `/api/states` (see `HEARTBEAT_SUFFIXES`)
    heartbeats: Mutex<HashMap<String, Vec<String>>>,
    /// Prefixes whose heartbeat discovery failed and was warned about (WARN once until it works)
    heartbeat_discovery_warned: Mutex<std::collections::HashSet<String>>,
    /// Parallel units whose SOC is taken in standby (INFO once per standby period)
    standby_units: Mutex<std::collections::HashSet<String>>,
}

/// Liveness of an entity prefix from its heartbeat entities
#[derive(Debug, Clone, Copy, PartialEq)]
enum Liveness {
    /// No heartbeat entity exists: not age-checked
    Unchecked,
    /// The newest heartbeat `last_updated`, within `STATE_STALE_SECS`
    Live(DateTime<Utc>),
    /// No heartbeat updated within `STATE_STALE_SECS`: the newest one's age, if any was readable
    Stale(Option<i64>),
}

/// A parallel unit whose battery and PV power both read within this of 0 W is in standby: its
/// SOC cannot move, so a flat SOC with flat heartbeats is still right (PR #647 R5-1)
const STANDBY_POWER_W: f32 = 50.0;

/// A unit's standby is believed only while its master shows at most this much PV (W) and is not
/// charging (battery at most `STANDBY_POWER_W`): with sun or a charge the unit would be moving
/// too, so a unit frozen at 0 W then is dead, not idle (PR #647 R6-1). In all 145 standby
/// windows on Jakub's site the master had PV 0 W and battery at most 1 W.
const STANDBY_MASTER_PV_MAX_W: f32 = 200.0;

/// Sensors of an inverter prefix whose `last_updated` moves on (almost) every poll: grid voltage
/// and frequency change nearly always, the powers often. An inverter is live while any of them
/// was updated within `STATE_STALE_SECS`. HA's REST API does not advance `last_reported` for a
/// flat value, and a flat SOC keeps its `last_updated`, so neither can tell a stall (PR #647 R4-1).
const HEARTBEAT_SUFFIXES: &[&str] = &[
    "grid_voltage",
    "grid_frequency",
    "measured_power",
    "inverter_power",
    "house_load",
    "pv_power_total",
    "battery_power_charge",
];

/// Whether `entity_id` is a heartbeat sensor of `prefix` (`grid_voltage*` / `grid_frequency*`
/// match per-phase variants too; the others match exactly)
fn is_heartbeat_entity(prefix: &str, entity_id: &str) -> bool {
    let Some(suffix) = entity_id.strip_prefix(&format!("sensor.{prefix}_")) else {
        return false;
    };
    HEARTBEAT_SUFFIXES.iter().any(|h| match *h {
        "grid_voltage" | "grid_frequency" => suffix.starts_with(h),
        _ => suffix == *h,
    })
}

/// While a parallel unit's SOC is unreadable, the last combined SOC is held at most this long
/// (seconds)
const PAIR_SOC_HOLD_SECS: i64 = 600;

/// The newest heartbeat time of a live `prefix` (None when not age-checked), else an error
fn require_live(prefix: &str, liveness: Liveness) -> Result<Option<DateTime<Utc>>> {
    match liveness {
        Liveness::Unchecked => Ok(None),
        Liveness::Live(at) => Ok(Some(at)),
        Liveness::Stale(newest) => anyhow::bail!(
            "{prefix} not live: no heartbeat entity updated in the last {STATE_STALE_SECS} s (newest {})",
            newest.map_or_else(|| "unreadable".to_owned(), |age| format!("{age} s ago"))
        ),
    }
}

/// Usable capacities that weight the SOC an inverter reports
#[derive(Debug, Clone, PartialEq)]
pub struct BatterySocWeights {
    /// Usable capacity (kWh) of the inverter's own battery
    pub own_kwh: f32,
    /// Entity prefix and usable capacity (kWh) of each read-only parallel unit's battery
    pub extras: Vec<(String, f32)>,
}

/// PV values read from a parallel unit's entities
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct PvExtras {
    power_w: f32,
    today_kwh: f32,
    total_kwh: f32,
    /// Battery power (W, positive = charge) summed over the extra units that report one
    battery_power_w: Option<f32>,
}

impl HomeAssistantInverterAdapter {
    /// Create a new HA inverter adapter
    pub fn new(client: Arc<HomeAssistantClient>, mapper: Arc<dyn VendorEntityMapper>) -> Self {
        Self {
            client,
            mapper,
            guard: ActuationGuardConfig::default(),
            actuation: Arc::new(ActuationState::default()),
            pv_extra_prefixes: HashMap::new(),
            pv_extra_warned: Mutex::new(std::collections::HashSet::new()),
            battery_soc_weights: HashMap::new(),
            battery_soc_warned: Mutex::new(std::collections::HashSet::new()),
            battery_soc_last_good: Mutex::new(HashMap::new()),
            heartbeats: Mutex::new(HashMap::new()),
            heartbeat_discovery_warned: Mutex::new(std::collections::HashSet::new()),
            standby_units: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Read-only PV extras: for each inverter id, the entity prefixes of parallel units whose
    /// `pv_power_total`, `today_s_solar_energy` and `total_solar_energy` are added to that
    /// inverter's PV readings. These prefixes are never written to.
    #[must_use]
    pub fn with_pv_extra_prefixes(mut self, extras: HashMap<String, Vec<String>>) -> Self {
        self.pv_extra_prefixes = extras
            .into_iter()
            .filter(|(_, prefixes)| !prefixes.is_empty())
            .collect();
        self
    }

    /// Capacity-weighted battery SOC per inverter id (see `BatterySocWeights`). The parallel
    /// units' batteries are only read, never written to.
    #[must_use]
    pub fn with_battery_soc_weights(mut self, weights: HashMap<String, BatterySocWeights>) -> Self {
        self.battery_soc_weights = weights;
        self
    }

    /// The SOC an inverter reports: its own when it has no battery weights, otherwise the
    /// capacity-weighted mean with each parallel unit's SOC. A unit's SOC is unreadable when the
    /// read fails, is not a finite number, or the unit is not live (no heartbeat entity updated
    /// within `STATE_STALE_SECS`; WARN once per entity). Then the last combined SOC is held for up to `PAIR_SOC_HOLD_SECS`;
    /// after that this fails, so `read_state` fails and no new state is taken: the master's own
    /// SOC alone is several kWh off the pair's. Returns the SOC and the (prefix, SOC) of each unit.
    async fn read_battery_soc(
        &self,
        inverter_id: &str,
        own_soc: f32,
        master_quiet: bool,
    ) -> Result<(f32, Vec<(String, f32)>)> {
        let Some(weights) = self.battery_soc_weights.get(inverter_id) else {
            return Ok((own_soc, Vec::new()));
        };
        let now = Utc::now();
        let mut readings = Vec::with_capacity(weights.extras.len());
        let mut unreadable = Vec::new();
        for (prefix, _) in &weights.extras {
            let entity = self.mapper.get_battery_soc_entity(prefix);
            match self.read_extra_soc(prefix, now, master_quiet).await {
                Ok(soc) => {
                    self.battery_soc_warned.lock().remove(&entity);
                    readings.push((prefix.clone(), soc));
                }
                Err(e) => {
                    if self.battery_soc_warned.lock().insert(entity.clone()) {
                        warn!(
                            "⚠️ [ADAPTER] Extra battery SOC {} for {} unreadable, holding the last combined SOC for up to {} min: {:#}",
                            entity,
                            inverter_id,
                            PAIR_SOC_HOLD_SECS / 60,
                            e
                        );
                    }
                    unreadable.push(entity);
                }
            }
        }
        let extras_kwh: f32 = weights.extras.iter().map(|(_, kwh)| kwh).sum();
        let total_kwh = weights.own_kwh + extras_kwh;
        if unreadable.is_empty() && total_kwh.is_finite() && total_kwh > 0.0 {
            let weighted_sum: f32 = own_soc * weights.own_kwh
                + readings
                    .iter()
                    .zip(&weights.extras)
                    .map(|((_, soc), (_, kwh))| soc * kwh)
                    .sum::<f32>();
            let combined = weighted_sum / total_kwh;
            self.battery_soc_last_good
                .lock()
                .insert(inverter_id.to_owned(), (combined, now));
            return Ok((combined, readings));
        }
        let held = self.battery_soc_last_good.lock().get(inverter_id).copied();
        match held {
            Some((soc, at)) if (now - at).num_seconds() <= PAIR_SOC_HOLD_SECS => {
                Ok((soc, Vec::new()))
            }
            _ => anyhow::bail!(
                "combined battery SOC of {} unknown: {} unreadable and no combined SOC in the last {} min",
                inverter_id,
                unreadable.join(", "),
                PAIR_SOC_HOLD_SECS / 60
            ),
        }
    }

    /// A parallel unit's SOC (`prefix`): a finite number while the unit is live (see
    /// `ensure_live`), or while it is in standby with every heartbeat flat: battery and PV power
    /// both within `STANDBY_POWER_W` of 0, so the SOC cannot move (Jakub's slave at its 10 % floor
    /// for hours each night). Standby counts only while the master is quiet (`master_quiet`: PV
    /// at most `STANDBY_MASTER_PV_MAX_W`, not charging), so a dead unit is found by morning. A
    /// frozen non-zero power still fails. A flat SOC keeps its `last_updated`, so its own age says
    /// nothing.
    async fn read_extra_soc(
        &self,
        prefix: &str,
        now: DateTime<Utc>,
        master_quiet: bool,
    ) -> Result<f32> {
        let entity_id = self.mapper.get_battery_soc_entity(prefix);
        let state = self
            .client
            .get_state(&entity_id)
            .await
            .with_context(|| format!("Failed to read entity: {entity_id}"))?;
        let soc = state
            .state
            .parse::<f32>()
            .with_context(|| format!("Failed to parse '{}' as float", state.state))?;
        anyhow::ensure!(soc.is_finite(), "SOC {soc} is not finite");
        let liveness = self.liveness(prefix, now).await?;
        if let Liveness::Stale(_) = liveness
            && master_quiet
            && let Some((battery_w, pv_w)) = self.standby_powers(prefix).await
        {
            if self.standby_units.lock().insert(prefix.to_owned()) {
                info!(
                    "[ADAPTER] {prefix} in standby (battery {battery_w} W, PV {pv_w} W, no heartbeat for over {STATE_STALE_SECS} s): its SOC {soc} % is used as read"
                );
            }
            return Ok(soc);
        }
        require_live(prefix, liveness)?;
        self.standby_units.lock().remove(prefix);
        Ok(soc)
    }

    /// Battery and PV power of `prefix` when both read within `STANDBY_POWER_W` of 0
    async fn standby_powers(&self, prefix: &str) -> Option<(f32, f32)> {
        let battery_w = self
            .read_sensor_float(&self.mapper.get_battery_power_entity(prefix))
            .await
            .ok()?;
        let pv_w = self
            .read_sensor_float(&self.mapper.get_pv_power_entity(prefix))
            .await
            .ok()?;
        (battery_w.abs() <= STANDBY_POWER_W && pv_w.abs() <= STANDBY_POWER_W)
            .then_some((battery_w, pv_w))
    }

    /// The heartbeat entities of `prefix`, found once from `/api/states` and logged. None found:
    /// one WARN, and the prefix is not age-checked. A failed listing fails this read (WARN once
    /// until it works) and is retried on the next read: the age is never skipped silently
    /// (PR #647 R5-4).
    async fn heartbeat_entities(&self, prefix: &str) -> Result<Vec<String>> {
        if let Some(found) = self.heartbeats.lock().get(prefix) {
            return Ok(found.clone());
        }
        let states = match self.client.get_all_states().await {
            Ok(states) => states,
            Err(e) => {
                if self
                    .heartbeat_discovery_warned
                    .lock()
                    .insert(prefix.to_owned())
                {
                    warn!(
                        "⚠️ [ADAPTER] Heartbeat discovery for {prefix} failed (/api/states), its state reads fail until it works: {e:#}"
                    );
                }
                return Err(anyhow::Error::from(e)
                    .context(format!("heartbeat discovery for {prefix} failed")));
            }
        };
        self.heartbeat_discovery_warned.lock().remove(prefix);
        let mut found: Vec<String> = states
            .into_iter()
            .map(|s| s.entity_id)
            .filter(|id| is_heartbeat_entity(prefix, id))
            .collect();
        // Voltage and frequency first: they change on almost every poll
        found.sort_by_key(|id| {
            HEARTBEAT_SUFFIXES
                .iter()
                .position(|h| id.starts_with(&format!("sensor.{prefix}_{h}")))
        });
        if found.is_empty() {
            warn!(
                "⚠️ [ADAPTER] No heartbeat entity for {prefix} (sensor.{prefix}_{{{}}}): its state age is not checked",
                HEARTBEAT_SUFFIXES.join(",")
            );
        } else {
            info!(
                "[ADAPTER] Heartbeat entities of {prefix}: {}",
                found.join(", ")
            );
        }
        self.heartbeats
            .lock()
            .insert(prefix.to_owned(), found.clone());
        Ok(found)
    }

    /// Liveness of `prefix` at `now` from the newest `last_updated` of its heartbeat entities.
    /// Fails only when the heartbeat entities cannot be listed.
    async fn liveness(&self, prefix: &str, now: DateTime<Utc>) -> Result<Liveness> {
        let heartbeats = self.heartbeat_entities(prefix).await?;
        if heartbeats.is_empty() {
            return Ok(Liveness::Unchecked);
        }
        let mut newest: Option<DateTime<Utc>> = None;
        for entity_id in &heartbeats {
            let Ok(state) = self.client.get_state(entity_id).await else {
                continue;
            };
            let Ok(at) = DateTime::parse_from_rfc3339(&state.last_updated) else {
                continue;
            };
            let at = at.with_timezone(&Utc);
            newest = Some(newest.map_or(at, |n| n.max(at)));
        }
        Ok(match newest {
            Some(at) if (now - at).num_seconds() <= STATE_STALE_SECS => Liveness::Live(at),
            newest => Liveness::Stale(newest.map(|at| (now - at).num_seconds())),
        })
    }

    /// Fail unless `prefix` is live (see `liveness`). Returns the newest heartbeat time: when the
    /// data was last new, so the state's age counts from there (PR #647 R5-2). None without
    /// heartbeat entities.
    async fn ensure_live(&self, prefix: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        require_live(prefix, self.liveness(prefix, now).await?)
    }

    /// Sum the PV power and solar energy of the inverter's read-only extra prefixes.
    /// An unavailable entity is skipped (WARN once per entity), so the master's value stays.
    async fn read_pv_extras(&self, inverter_id: &str) -> PvExtras {
        let mut extras = PvExtras::default();
        let Some(prefixes) = self.pv_extra_prefixes.get(inverter_id) else {
            return extras;
        };
        for prefix in prefixes {
            let entities = [
                Some(self.mapper.get_pv_power_entity(prefix)),
                self.mapper.get_today_solar_energy_entity(prefix),
                self.mapper.get_total_solar_energy_entity(prefix),
            ];
            for (slot, entity) in entities.into_iter().enumerate() {
                let Some(entity) = entity else { continue };
                match self.read_sensor_float(&entity).await {
                    Ok(value) => {
                        self.pv_extra_warned.lock().remove(&entity);
                        match slot {
                            0 => extras.power_w += value,
                            1 => extras.today_kwh += value,
                            _ => extras.total_kwh += value,
                        }
                    }
                    Err(e) => {
                        if self.pv_extra_warned.lock().insert(entity.clone()) {
                            warn!(
                                "⚠️ [ADAPTER] Extra PV entity {} for {} unavailable, using the rest: {:#}",
                                entity, inverter_id, e
                            );
                        }
                    }
                }
            }
            // Read-only battery power of a parallel unit with its own battery (brief H33): each
            // unit reports only its own share. A unit without the entity is skipped.
            let entity = self.mapper.get_battery_power_entity(prefix);
            match self.read_sensor_float(&entity).await {
                Ok(value) => *extras.battery_power_w.get_or_insert(0.0) += value,
                Err(e) => debug!(
                    "Extra battery power {} unreadable, skipped: {:#}",
                    entity, e
                ),
            }
        }
        extras
    }

    /// Use the given pre-actuation guard settings (e.g. `control.auto_unlock_inverter`)
    #[must_use]
    pub fn with_guard(mut self, guard: ActuationGuardConfig) -> Self {
        self.guard = guard;
        self
    }

    /// Get reference to the underlying HA client (for history queries, etc.)
    pub fn client(&self) -> &Arc<HomeAssistantClient> {
        &self.client
    }

    /// Shared actuation state, so the pre-wake system uses the same wake limit
    pub fn actuation_state(&self) -> Arc<ActuationState> {
        self.actuation.clone()
    }

    /// Set `number.<control prefix>_backup_discharge_min_soc` to `target` only when it differs
    /// (brief H19). A failed read fails the write, so Back Up Mode never starts with an unknown
    /// minimum.
    async fn sync_backup_min_soc(&self, control_prefix: &str, target: u8) -> Result<()> {
        let entity_id = format!(
            "number.{}_backup_discharge_min_soc",
            control_prefix.replace('.', "_")
        );
        let state = self
            .client
            .get_state(&entity_id)
            .await
            .with_context(|| format!("Cannot read {entity_id}, Back Up Mode not set"))?;
        if state
            .state
            .parse::<f32>()
            .is_ok_and(|value| (value - f32::from(target)).abs() < 0.5)
        {
            debug!("   {} already {}%", entity_id, target);
            return Ok(());
        }
        self.client
            .call_service(
                "number.set_value",
                serde_json::json!({
                    "entity_id": entity_id,
                    "value": target,
                }),
            )
            .await
            .with_context(|| format!("Failed to set {entity_id} to {target}%"))?;
        info!("✅ [ADAPTER] {} set to {}%", entity_id, target);
        Ok(())
    }

    /// Read backup_discharge_min_soc sensor from HA
    /// Returns the minimum SOC for battery discharge configured in HA
    /// Entity: number.<control prefix of `inverter_id`>_backup_discharge_min_soc (brief H36)
    pub async fn read_backup_discharge_min_soc(&self, inverter_id: &str) -> Result<f32> {
        let entity_id = format!(
            "number.{}_backup_discharge_min_soc",
            self.mapper.control_prefix(inverter_id).replace('.', "_")
        );
        self.read_sensor_float(&entity_id).await
    }

    /// Helper to read a sensor entity value as f32
    async fn read_sensor_float(&self, entity_id: &str) -> Result<f32> {
        debug!("📊 [ADAPTER] Reading float sensor: {}", entity_id);
        let state = self
            .client
            .get_state(entity_id)
            .await
            .with_context(|| format!("Failed to read entity: {}", entity_id))?;

        let value = state.state.parse::<f32>().with_context(|| {
            format!(
                "Failed to parse '{}' as float from entity {}",
                state.state, entity_id
            )
        })?;

        debug!("✅ [ADAPTER] {} = {}", entity_id, value);
        Ok(value)
    }

    /// Helper to read an optional sensor entity
    async fn read_optional_sensor<F>(&self, inverter_id: &str, getter: F) -> Option<f32>
    where
        F: Fn(&str) -> Option<String>,
    {
        let entity = getter(inverter_id)?;
        self.read_sensor_float(&entity).await.ok()
    }

    /// Helper to read the vendor's manual sub-mode; `None` when unavailable or not recognised
    async fn read_manual_sub_mode(&self, entity_id: &str) -> Option<InverterOperationMode> {
        let state = self.client.get_state(entity_id).await.ok()?;
        let mode = self.mapper.map_manual_sub_mode(&state.state);
        if mode.is_none() {
            debug!(
                "⚠️ [ADAPTER] Manual sub-mode '{}' not readable from {}",
                state.state, entity_id
            );
        }
        mode
    }

    /// Helper to read work mode entity and map to generic mode
    async fn read_work_mode(&self, entity_id: &str) -> Result<InverterOperationMode> {
        debug!("🔧 [ADAPTER] Reading work mode: {}", entity_id);
        let state = self
            .client
            .get_state(entity_id)
            .await
            .with_context(|| format!("Failed to read work mode entity: {}", entity_id))?;

        debug!("   Raw state: '{}'", state.state);
        debug!("   Attributes: {:?}", state.attributes);

        // For Solax, the state is the string value (e.g., "Self Use Mode")
        // But we need to extract the numeric value from attributes if available
        // Try to get numeric value from attributes first
        if let Some(options) = state.attributes.get("options").and_then(|v| v.as_array()) {
            // Find index of current state in options
            let current_index = options
                .iter()
                .position(|v| v.as_str() == Some(&state.state));

            if let Some(idx) = current_index
                && let Some(mode) = self.mapper.map_mode_from_vendor(idx as i32)
            {
                debug!(
                    "✅ [ADAPTER] Work mode mapped from options[{}] = {:?}",
                    idx, mode
                );
                return Ok(mode);
            }
        }

        // Fallback: Try to parse state directly as number
        if let Ok(mode_num) = state.state.parse::<i32>()
            && let Some(mode) = self.mapper.map_mode_from_vendor(mode_num)
        {
            debug!(
                "✅ [ADAPTER] Work mode mapped from numeric value {} = {:?}",
                mode_num, mode
            );
            return Ok(mode);
        }

        // Default to SelfUse if we can't determine the mode
        warn!("⚠️ [ADAPTER] Could not determine work mode, defaulting to SelfUse");
        Ok(InverterOperationMode::SelfUse)
    }
}

/// The `max` attribute of a number entity (the largest value the inverter accepts), if present.
pub fn number_max_attribute(attributes: &serde_json::Value) -> Option<u32> {
    attributes
        .get("max")?
        .as_f64()
        .map(|max| max.max(0.0) as u32)
}

/// Whether an export limit may be written: only when the entity's `max` is known and the limit
/// does not exceed it (fail closed).
pub fn export_limit_allowed(limit_w: u32, max_w: Option<u32>) -> bool {
    max_w.is_some_and(|max| limit_w <= max)
}

#[async_trait]
impl InverterDataSource for HomeAssistantInverterAdapter {
    async fn read_state(&self, inverter_id: &str) -> Result<GenericInverterState> {
        debug!("🔋 [ADAPTER] Reading inverter state for: {}", inverter_id);

        // Read all required sensor entities. The inverter must be live (a heartbeat entity updated
        // within STATE_STALE_SECS), or HA may be serving the values of a stalled integration.
        let now = Utc::now();
        let data_at = self.ensure_live(inverter_id, now).await?;
        let own_soc = self
            .read_sensor_float(&self.mapper.get_battery_soc_entity(inverter_id))
            .await?;
        let grid_power = self
            .read_sensor_float(&self.mapper.get_grid_power_entity(inverter_id))
            .await?;

        let battery_power = self
            .read_sensor_float(&self.mapper.get_battery_power_entity(inverter_id))
            .await?;

        let pv_power = self
            .read_sensor_float(&self.mapper.get_pv_power_entity(inverter_id))
            .await?;

        // Capacity-weighted with read-only parallel units' batteries (battery_extra_prefixes). A
        // unit's standby is believed only while this master shows no sun and no charge (R6-1).
        let master_quiet = pv_power <= STANDBY_MASTER_PV_MAX_W && battery_power <= STANDBY_POWER_W;
        let (battery_soc, battery_units) = self
            .read_battery_soc(inverter_id, own_soc, master_quiet)
            .await?;

        // Read current work mode. ForceCharge may be a Manual sub-mode: read the sub-mode exactly,
        // and mark the value ambiguous when that entity cannot be read.
        let work_mode_entity = self.mapper.get_work_mode_entity(inverter_id);
        let mut work_mode = self.read_work_mode(&work_mode_entity).await?;
        let mut work_mode_ambiguous = false;
        if work_mode == InverterOperationMode::ForceCharge
            && let Some(manual_entity) = self.mapper.get_manual_mode_entity(inverter_id)
        {
            match self.read_manual_sub_mode(&manual_entity).await {
                Some(exact) => work_mode = exact,
                None => work_mode_ambiguous = true,
            }
        }

        // Read optional Load & Grid detailed sensors
        let house_load_w = self
            .read_optional_sensor(inverter_id, |id| self.mapper.get_house_load_entity(id))
            .await;
        let grid_import_w = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_grid_import_power_entity(id)
            })
            .await;
        let grid_export_w = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_grid_export_power_entity(id)
            })
            .await;
        let grid_import_today_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_grid_import_today_entity(id)
            })
            .await;
        let grid_export_today_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_grid_export_today_entity(id)
            })
            .await;
        let inverter_frequency_hz = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_inverter_frequency_entity(id)
            })
            .await;

        // Read optional Inverter aggregate sensors
        let inverter_voltage_v = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_inverter_voltage_entity(id)
            })
            .await;
        let inverter_current_a = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_inverter_current_entity(id)
            })
            .await;
        let inverter_power_w = self
            .read_optional_sensor(inverter_id, |id| self.mapper.get_inverter_power_entity(id))
            .await;

        // Read optional Battery extended sensors
        let battery_capacity_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_battery_capacity_entity(id)
            })
            .await;
        let battery_input_energy_today_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_battery_input_energy_today_entity(id)
            })
            .await;
        let battery_output_energy_today_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_battery_output_energy_today_entity(id)
            })
            .await;

        // Read optional Solar energy sensors
        let today_solar_energy_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_today_solar_energy_entity(id)
            })
            .await;
        let total_solar_energy_kwh = self
            .read_optional_sensor(inverter_id, |id| {
                self.mapper.get_total_solar_energy_entity(id)
            })
            .await;

        // Read-only PV of parallel units (pv_extra_prefixes), added to this inverter's PV.
        // pv1/pv2 stay this inverter's own strings. Energy totals are added only to a value
        // the inverter itself reported.
        let extras = self.read_pv_extras(inverter_id).await;
        let pv_power = pv_power + extras.power_w;
        let today_solar_energy_kwh = today_solar_energy_kwh.map(|kwh| kwh + extras.today_kwh);
        let total_solar_energy_kwh = total_solar_energy_kwh.map(|kwh| kwh + extras.total_kwh);

        // Live export limit and the entity's own maximum, from one state read
        let export_limit_entity = self.mapper.get_export_limit_entity(inverter_id);
        let (export_limit_w, export_limit_max_w) =
            match self.client.get_state(&export_limit_entity).await {
                Ok(state) => (
                    state.state.parse::<f32>().ok().map(|v| v.max(0.0) as u32),
                    number_max_attribute(&state.attributes),
                ),
                Err(e) => {
                    debug!(
                        "Export limit entity {} unreadable: {}",
                        export_limit_entity, e
                    );
                    (None, None)
                }
            };

        let state = GenericInverterState {
            inverter_id: inverter_id.to_string(),
            battery_soc,
            work_mode,
            grid_power_w: grid_power,
            battery_power_w: battery_power,
            pv_power_w: pv_power,
            online: true,
            export_limit_w,
            export_limit_max_w,
            work_mode_ambiguous,
            data_at,
            // Load & Grid detailed
            house_load_w,
            grid_import_w,
            grid_export_w,
            grid_import_today_kwh,
            grid_export_today_kwh,
            inverter_frequency_hz,
            // Inverter aggregates
            inverter_voltage_v,
            inverter_current_a,
            inverter_power_w,
            // Battery extended
            battery_capacity_kwh,
            battery_input_energy_today_kwh,
            battery_output_energy_today_kwh,
            // Solar energy
            today_solar_energy_kwh,
            total_solar_energy_kwh,
            extra_battery_power_w: extras.battery_power_w,
            // All other optional fields default to None
            ..Default::default()
        };

        // With battery weights, show the combined SOC and each battery's SOC behind it
        let soc_detail = if self.battery_soc_weights.contains_key(inverter_id) {
            let units: String = battery_units
                .iter()
                .map(|(prefix, soc)| format!(", {prefix} {soc:.1}%"))
                .collect();
            format!(" (own {own_soc:.1}%{units})")
        } else {
            String::new()
        };
        info!(
            "✅ [ADAPTER] Inverter state: SOC={:.1}%{}, Mode={:?}, Grid={:.0}W, Batt={:.0}W, PV={:.0}W, IM={:.2}kW",
            state.battery_soc,
            soc_detail,
            state.work_mode,
            state.grid_power_w,
            state.battery_power_w,
            state.pv_power_w,
            state.grid_import_today_kwh.unwrap_or_default()
        );

        Ok(state)
    }

    async fn write_command(&self, inverter_id: &str, command: &InverterCommand) -> Result<()> {
        // Control entities (lock, Back Up minimum, awaken button) use the control prefix
        // (brief H36); the mapper resolves its own mode and export limit entities the same way
        let control_prefix = self.mapper.control_prefix(inverter_id);

        // Mode control off (brief H37): only the export limit is written. Checked before the
        // unlock, so a refused mode write never touches the lock either.
        if !actuation::command_allowed(&self.guard, command) {
            if self.actuation.first_mode_refusal() {
                warn!(
                    "Mode control disabled: refusing {:?} for {} (mode, Back Up minimum SOC and awaken writes are not sent; logged once)",
                    command, inverter_id
                );
            } else {
                debug!(
                    "Mode control disabled: refusing {:?} for {}",
                    command, inverter_id
                );
            }
            anyhow::bail!("mode control disabled; {command:?} not written to {inverter_id}");
        }

        info!(
            "📝 [ADAPTER] Writing command to {}: {:?}",
            inverter_id, command
        );

        // Pre-actuation guard (brief H11): the single choke point for every SolaX write.
        // Unlock before any write, including export limits; the write is skipped on error.
        actuation::ensure_unlocked(&self.client, inverter_id, control_prefix, &self.guard).await?;

        // Back Up Mode: the minimum SOC is synced first, then the mode is written as SetMode
        let mode_command;
        let command = match command {
            InverterCommand::SetBackupMode { min_soc_pct } => {
                self.sync_backup_min_soc(control_prefix, *min_soc_pct)
                    .await?;
                mode_command = InverterCommand::SetMode(InverterOperationMode::BackUpMode);
                &mode_command
            }
            other => other,
        };

        match command {
            InverterCommand::SetMode(mode) => {
                if *mode == InverterOperationMode::ForceCharge {
                    actuation::ensure_awake(
                        &self.client,
                        inverter_id,
                        control_prefix,
                        &self.actuation,
                    )
                    .await;
                }

                // Get all entity changes needed from vendor mapper
                let mode_change = self.mapper.get_mode_change_request(inverter_id, *mode);

                if mode_change.entity_changes.is_empty() {
                    warn!(
                        "No entity changes defined for mode {:?} by {:?}",
                        mode,
                        self.mapper.vendor_name()
                    );
                    return Ok(());
                }

                debug!(
                    "   Executing {} entity change(s)",
                    mode_change.entity_changes.len()
                );

                // Execute entity changes in sequence
                for (idx, change) in mode_change.entity_changes.iter().enumerate() {
                    debug!(
                        "   Step {}/{}: {} = '{}'",
                        idx + 1,
                        mode_change.entity_changes.len(),
                        change.entity_id,
                        change.option
                    );

                    self.client
                        .call_service(
                            "select.select_option",
                            serde_json::json!({
                                "entity_id": change.entity_id,
                                "option": change.option,
                            }),
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to set {} to '{}' for mode {:?}",
                                change.entity_id, change.option, mode
                            )
                        })?;

                    info!(
                        "✅ [ADAPTER] Set {} = '{}'",
                        change.entity_id, change.option
                    );
                }

                info!(
                    "✅ [ADAPTER] Mode change complete: {:?} ({} entities changed)",
                    mode,
                    mode_change.entity_changes.len()
                );

                if actuation::note_mode_written(&self.actuation, *mode) {
                    actuation::start_charge_check(
                        self.client.clone(),
                        &self.actuation,
                        inverter_id,
                        self.mapper.get_battery_power_entity(inverter_id),
                        &self.guard,
                    );
                }
            }
            InverterCommand::SetExportLimit(limit_w) => {
                let entity_id = self.mapper.get_export_limit_entity(inverter_id);

                // Final guard, fail closed: the entity max is read just before writing, and the
                // write is refused unless that max is known and not exceeded
                let max_w = match self.client.get_state(&entity_id).await {
                    Ok(state) => number_max_attribute(&state.attributes),
                    Err(e) => {
                        error!(
                            "🛑 [ADAPTER] Refusing export limit {}W for {}: cannot read entity max: {}",
                            limit_w, inverter_id, e
                        );
                        anyhow::bail!(
                            "Refused export limit {}W for {}: cannot read entity max",
                            limit_w,
                            inverter_id
                        );
                    }
                };
                if !export_limit_allowed(*limit_w, max_w) {
                    error!(
                        "🛑 [ADAPTER] Refusing export limit {}W for {}: entity max {:?}W",
                        limit_w, inverter_id, max_w
                    );
                    anyhow::bail!(
                        "Refused export limit {}W for {}: entity max {:?}W",
                        limit_w,
                        inverter_id,
                        max_w
                    );
                }

                debug!("   Entity: {}", entity_id);
                debug!("   Limit: {}W", limit_w);

                self.client
                    .call_service(
                        "number.set_value",
                        serde_json::json!({
                            "entity_id": entity_id,
                            "value": limit_w,
                        }),
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to set export limit to {}W for {}",
                            limit_w, inverter_id
                        )
                    })?;

                info!("✅ [ADAPTER] Export limit set successfully");
            }
            InverterCommand::SetBackupMode { .. } => {
                // Mapped to SetMode above; reaching this arm would be a dispatch bug
                anyhow::bail!("SetBackupMode was not mapped for {}", inverter_id);
            }
        }
        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        // Try to ping the HA API
        self.client.ping().await.map_err(|e| anyhow::anyhow!(e))
    }

    fn name(&self) -> &str {
        "HomeAssistant"
    }
}

/// Czech spot price adapter implementing PriceDataSource
/// Reads from sensor.current_spot_electricity_prices entity
pub struct CzSpotPriceAdapter {
    client: Arc<HomeAssistantClient>,
    entity_id: String,
    tomorrow_entity_id: Option<String>,
    /// Shared timezone for price parsing, synchronized from Home Assistant
    /// Using RwLock for thread-safe read/write access
    timezone: Arc<RwLock<Option<Tz>>>,
}

impl CzSpotPriceAdapter {
    /// Create a new Czech spot price adapter
    pub fn new(client: Arc<HomeAssistantClient>, entity_id: impl Into<String>) -> Self {
        Self {
            client,
            entity_id: entity_id.into(),
            tomorrow_entity_id: None,
            timezone: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a new adapter with separate today and tomorrow sensors
    pub fn with_tomorrow_sensor(
        client: Arc<HomeAssistantClient>,
        today_entity_id: impl Into<String>,
        tomorrow_entity_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            entity_id: today_entity_id.into(),
            tomorrow_entity_id: Some(tomorrow_entity_id.into()),
            timezone: Arc::new(RwLock::new(None)),
        }
    }

    /// Update the timezone used for price parsing
    /// This should be called when the Home Assistant timezone is synchronized
    pub fn set_timezone(&self, tz: Option<Tz>) {
        let mut timezone = self.timezone.write();
        if *timezone != tz {
            let old_tz = timezone.map(|t| t.name().to_string());
            let new_tz = tz.map(|t| t.name().to_string());
            info!(
                "🌍 [CzSpotPriceAdapter] Timezone updated: {:?} -> {:?}",
                old_tz, new_tz
            );
        }
        *timezone = tz;
    }

    /// Get a clone of the shared timezone Arc for external synchronization
    pub fn timezone_handle(&self) -> Arc<RwLock<Option<Tz>>> {
        self.timezone.clone()
    }

    /// Get the current timezone (for internal use)
    fn get_timezone(&self) -> Option<Tz> {
        *self.timezone.read()
    }

    /// Read another price sensor in the spot sensor format and return its `(block start, CZK/kWh)`
    /// pairs (used for the buy price sensor, `pricing.buy_price_entity`)
    pub async fn read_block_prices(&self, entity_id: &str) -> Result<Vec<(DateTime<Utc>, f32)>> {
        let state = self
            .client
            .get_state(entity_id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("Failed to read price entity: {entity_id}"))?;

        let entity_json = serde_json::json!({
            "state": state.state,
            "attributes": state.attributes,
            "last_updated": state.last_updated,
        });
        let price_data = parse_spot_price_response(&entity_json, self.get_timezone())
            .with_context(|| format!("Failed to parse price entity: {entity_id}"))?;

        Ok(price_data
            .time_block_prices
            .iter()
            .map(|block| (block.block_start, block.price_czk_per_kwh))
            .collect())
    }
}

#[async_trait]
impl PriceDataSource for CzSpotPriceAdapter {
    async fn read_prices(&self) -> Result<SpotPriceData> {
        let timezone = self.get_timezone();
        let tz_name = timezone
            .map(|tz| tz.name().to_string())
            .unwrap_or_else(|| "UTC (not set)".to_string());
        debug!(
            "💰 [ADAPTER] Reading spot prices from: {} (timezone: {})",
            self.entity_id, tz_name
        );

        let state = self
            .client
            .get_state(&self.entity_id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| format!("Failed to read price entity: {}", self.entity_id))?;

        debug!("   State value: {}", state.state);
        debug!("   Attributes type: {:?}", state.attributes);
        if let Some(obj) = state.attributes.as_object() {
            debug!("   Attributes keys: {:?}", obj.keys().collect::<Vec<_>>());
        }

        // If tomorrow sensor is configured, fetch from both sensors
        if let Some(tomorrow_entity_id) = &self.tomorrow_entity_id {
            info!("💰 [ADAPTER] Also reading tomorrow prices from: {tomorrow_entity_id}");

            let tomorrow_state = match self.client.get_state(tomorrow_entity_id).await {
                Ok(state) => Some(state),
                Err(e) => {
                    warn!("⚠️ Failed to fetch tomorrow prices (will use today only): {e}");
                    None
                }
            };

            if let Some(tomorrow_state) = tomorrow_state {
                // Try to merge today and tomorrow data
                return self.merge_today_tomorrow_sensors(&state, &tomorrow_state);
            }
        }

        // Fallback to single sensor parsing
        let entity_json = serde_json::json!({
            "state": state.state,
            "attributes": state.attributes,
            "last_updated": state.last_updated,
        });

        // Parse using fluxion_core::pricing::parse_spot_price_response with timezone
        let price_data = parse_spot_price_response(&entity_json, timezone)
            .context("Failed to parse spot price data")?;

        debug!(
            "✅ [ADAPTER] Parsed {} price blocks",
            price_data.time_block_prices.len()
        );
        if !price_data.time_block_prices.is_empty() {
            let first = &price_data.time_block_prices[0];
            let last = price_data.time_block_prices.last().unwrap();
            debug!(
                "   First block: {} at {:.4} CZK/kWh",
                first.block_start.format("%Y-%m-%d %H:%M"),
                first.price_czk_per_kwh
            );
            debug!(
                "   Last block:  {} at {:.4} CZK/kWh",
                last.block_start.format("%Y-%m-%d %H:%M"),
                last.price_czk_per_kwh
            );
        }

        Ok(price_data)
    }

    async fn health_check(&self) -> Result<bool> {
        // Try to read the price entity
        self.client
            .get_state(&self.entity_id)
            .await
            .map(|_| true)
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn name(&self) -> &str {
        "CzSpotPrice"
    }
}

impl CzSpotPriceAdapter {
    /// Merge today and tomorrow sensor data into a single price dataset
    fn merge_today_tomorrow_sensors(
        &self,
        today_state: &crate::ha::types::HaEntityState,
        tomorrow_state: &crate::ha::types::HaEntityState,
    ) -> Result<SpotPriceData> {
        // Extract "today" array from today sensor
        let today_array = today_state
            .attributes
            .get("today")
            .and_then(|v| v.as_array())
            .context("Today sensor missing 'today' array attribute")?;

        // Extract "today" array from tomorrow sensor (represents tomorrow's data)
        let tomorrow_array = tomorrow_state
            .attributes
            .get("today")
            .and_then(|v| v.as_array())
            .context("Tomorrow sensor missing 'today' array attribute")?;

        let timezone = self.get_timezone();
        debug!(
            "   Merging: {} today blocks + {} tomorrow blocks (timezone: {})",
            today_array.len(),
            tomorrow_array.len(),
            timezone.map(|tz| tz.name()).unwrap_or("UTC (not set)")
        );

        // Use the existing parse_price_arrays function from fluxion_core
        let merged_json = serde_json::json!({
            "attributes": {
                "today": today_array,
                "tomorrow": tomorrow_array,
            },
            "last_updated": today_state.last_updated,
        });

        let price_data = parse_spot_price_response(&merged_json, timezone)
            .context("Failed to merge today/tomorrow price data")?;

        info!(
            "✅ [ADAPTER] Merged {} total price blocks ({} today + {} tomorrow)",
            price_data.time_block_prices.len(),
            today_array.len(),
            tomorrow_array.len()
        );

        if !price_data.time_block_prices.is_empty() {
            let first = &price_data.time_block_prices[0];
            let last = price_data.time_block_prices.last().unwrap();
            debug!(
                "   First block: {} at {:.4} CZK/kWh",
                first.block_start.format("%Y-%m-%d %H:%M"),
                first.price_czk_per_kwh
            );
            debug!(
                "   Last block:  {} at {:.4} CZK/kWh",
                last.block_start.format("%Y-%m-%d %H:%M"),
                last.price_czk_per_kwh
            );
        }

        Ok(price_data)
    }
}

/// Configurable price data source that switches between spot and fixed prices
/// based on configuration flags (use_spot_prices_to_buy/sell)
pub struct ConfigurablePriceDataSource {
    spot_adapter: Arc<CzSpotPriceAdapter>,
    use_spot_for_buy: bool,
    use_spot_for_sell: bool,
    fixed_buy_prices: Vec<f32>,
    #[expect(dead_code, reason = "Reserved for fixed sell price schedule support")]
    fixed_sell_prices: Vec<f32>,
    spot_sell_fee_czk: f32,
    /// Separate buy price sensor (`pricing.buy_price_entity`), read only when buying at spot
    buy_price_entity: Option<String>,
    /// Minimum time between two reads of the buy price sensor
    buy_fetch_interval: Duration,
    /// Buy sensor state kept between price reads
    buy_state: Mutex<BuyPriceState>,
    /// Price feed health check and OTE fallback (brief H35)
    fallback: Option<Arc<crate::price_fallback::PriceFallback>>,
    /// Wall clock for the fixed-price day boundaries (replaced in tests)
    clock: fn() -> DateTime<Utc>,
}

/// Default minimum time between two reads of the buy price sensor (prices change per 15 minutes,
/// while the price cache asks for spot prices every few seconds)
pub const BUY_PRICE_FETCH_INTERVAL: Duration = Duration::from_secs(60);

/// Buy price sensor state carried between price reads (brief H13b)
#[derive(Debug, Default)]
struct BuyPriceState {
    /// Last time the sensor was read, successfully or not
    last_fetch: Option<Instant>,
    /// Last good buy price per block start (UTC); kept when a later read fails or drops a block
    last_good: HashMap<DateTime<Utc>, f32>,
    /// The last read failed (WARN once when this changes)
    failing: bool,
    /// Blocks were missing from the last good read (WARN once when this changes)
    missing: bool,
}

impl ConfigurablePriceDataSource {
    pub fn new(
        spot_adapter: Arc<CzSpotPriceAdapter>,
        use_spot_for_buy: bool,
        use_spot_for_sell: bool,
        fixed_buy_prices: Vec<f32>,
        fixed_sell_prices: Vec<f32>,
        spot_sell_fee_czk: f32,
    ) -> Self {
        Self {
            spot_adapter,
            use_spot_for_buy,
            use_spot_for_sell,
            fixed_buy_prices,
            fixed_sell_prices,
            spot_sell_fee_czk,
            buy_price_entity: None,
            buy_fetch_interval: BUY_PRICE_FETCH_INTERVAL,
            buy_state: Mutex::new(BuyPriceState::default()),
            fallback: None,
            clock: Utc::now,
        }
    }

    /// Replace the wall clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> DateTime<Utc>) -> Self {
        self.clock = clock;
        self
    }

    /// Site timezone for the fixed prices: the one synchronised from Home Assistant, else
    /// Europe/Prague (the scheduler's HDO cache uses the same; the container runs in UTC).
    fn local_timezone(&self) -> Tz {
        self.spot_adapter
            .get_timezone()
            .unwrap_or(chrono_tz::Europe::Prague)
    }

    /// The 24 hourly fixed buy prices (the config check allows only 24 here)
    fn fixed_hourly_prices(&self) -> Result<&[f32]> {
        if self.fixed_buy_prices.len() != 24 {
            anyhow::bail!(
                "Fixed buy prices must have exactly 24 hourly values, got {}",
                self.fixed_buy_prices.len()
            );
        }
        Ok(&self.fixed_buy_prices)
    }

    /// Fixed buy prices for today and tomorrow in the site's local time (192 blocks on a day
    /// without a DST change), so the plan horizon does not end at local midnight. Used when
    /// there is no spot timeline (spot sell off, or the spot read failed).
    fn generate_fixed_price_data(&self) -> Result<SpotPriceData> {
        let hourly = self.fixed_hourly_prices()?;
        let tz = self.local_timezone();
        let now = (self.clock)();
        let today = now.with_timezone(&tz).date_naive();
        let midnight = |date: chrono::NaiveDate| -> Result<DateTime<Utc>> {
            let naive = date
                .and_hms_opt(0, 0, 0)
                .context("Invalid local midnight")?;
            Ok(naive
                .and_local_timezone(tz)
                .earliest()
                .context("Local midnight does not exist")?
                .with_timezone(&Utc))
        };
        let start = midnight(today)?;
        let end = midnight(today + chrono::Days::new(2))?;

        let mut time_block_prices = Vec::with_capacity(192);
        let mut block_start = start;
        while block_start < end {
            let price = fixed_price_at(hourly, block_start, tz);
            time_block_prices.push(TimeBlockPrice {
                block_start,
                duration_minutes: 15,
                price_czk_per_kwh: price,
                // Effective price will be calculated by scheduler with HDO fees
                effective_price_czk_per_kwh: price,
                spot_sell_price_czk_per_kwh: None,
                buy_price_czk_per_kwh: None,
            });
            block_start += chrono::Duration::minutes(15);
        }

        let spot_data = SpotPriceData {
            time_block_prices,
            block_duration_minutes: 15,
            fetched_at: now,
            ha_last_updated: now,
            fallback_block_starts: Vec::new(),
        };

        info!(
            "✅ [ConfigurablePrice] Generated fixed price data: {} blocks (today and tomorrow, {}) from {} hourly prices",
            spot_data.time_block_prices.len(),
            tz.name(),
            hourly.len()
        );

        Ok(spot_data)
    }
}

/// Fixed buy price of the block at `start`: the hourly price at the block's local hour
fn fixed_price_at(hourly: &[f32], start: DateTime<Utc>, tz: Tz) -> f32 {
    use chrono::Timelike;
    hourly[start.with_timezone(&tz).hour() as usize % hourly.len()]
}

impl ConfigurablePriceDataSource {
    /// Buy at the price of a separate sensor (`pricing.buy_price_entity`) when buying at spot.
    /// Empty or unset keeps the spot price for buying.
    pub fn with_buy_price_entity(mut self, entity_id: Option<String>) -> Self {
        self.buy_price_entity = entity_id.filter(|entity| !entity.trim().is_empty());
        self
    }

    /// Minimum time between two reads of the buy price sensor (default `BUY_PRICE_FETCH_INTERVAL`)
    pub fn with_buy_fetch_interval(mut self, interval: Duration) -> Self {
        self.buy_fetch_interval = interval;
        self
    }

    /// Check the spot prices for missing slots and fill them from OTE (brief H35)
    pub fn with_price_fallback(
        mut self,
        fallback: Option<Arc<crate::price_fallback::PriceFallback>>,
    ) -> Self {
        self.fallback = fallback;
        self
    }

    /// Spot prices from the entity, checked by the fallback (missing slots filled from OTE).
    /// With `with_buy`, the buy entity is read first and its prices go to the fallback, which
    /// fills fallback blocks only when it can derive their buy price (brief H35). A failed
    /// entity read is reported to the fallback as an entity without prices; the read still fails
    /// unless the fallback filled blocks (second day of an outage).
    async fn read_spot_prices(&self, with_buy: bool) -> Result<SpotPriceData> {
        let read = self.spot_adapter.read_prices().await.map(|mut data| {
            crate::price_fallback::drop_invalid_blocks(&mut data);
            data
        });
        let buy_entity = self.buy_price_entity.as_deref().filter(|_| with_buy);
        let Some(fallback) = &self.fallback else {
            let data = read?;
            if let Some(entity_id) = buy_entity {
                self.refresh_buy_prices(entity_id, &data).await;
            }
            return Ok(data);
        };
        let (mut data, error) = match read {
            Ok(data) => (data, None),
            Err(e) => (SpotPriceData::default(), Some(e)),
        };
        let buy = match buy_entity {
            Some(entity_id) => {
                self.refresh_buy_prices(entity_id, &data).await;
                Some(self.buy_state.lock().last_good.clone())
            }
            None => None,
        };
        fallback.apply(&mut data, buy.as_ref(), fallback.now());
        match error {
            Some(e) if data.fallback_block_starts.is_empty() => Err(e),
            Some(e) => {
                debug!("💰 Price entity unreadable ({e:#}); using OTE fallback prices only");
                Ok(data)
            }
            None => Ok(data),
        }
    }
}

/// Fixed buy with spot sell: the blocks of the spot timeline (today and, once published,
/// tomorrow). Buy is the fixed price at the block's local hour, sell is spot minus `fee`. The
/// OTE fallback markers, `fetched_at` and `ha_last_updated` stay those of the spot data (brief H35).
fn fixed_buy_on_spot_timeline(
    spot_data: SpotPriceData,
    hourly: &[f32],
    tz: Tz,
    fee: f32,
) -> SpotPriceData {
    let time_block_prices = spot_data
        .time_block_prices
        .iter()
        .map(|spot| {
            let price = fixed_price_at(hourly, spot.block_start, tz);
            TimeBlockPrice {
                block_start: spot.block_start,
                duration_minutes: spot.duration_minutes,
                price_czk_per_kwh: price,
                // Effective price will be calculated by scheduler with HDO fees
                effective_price_czk_per_kwh: price,
                spot_sell_price_czk_per_kwh: Some(spot.price_czk_per_kwh - fee),
                buy_price_czk_per_kwh: None,
            }
        })
        .collect();
    SpotPriceData {
        time_block_prices,
        ..spot_data
    }
}

/// Median of buy/spot over the blocks that have both prices (None when there are none)
fn buy_to_spot_ratio(blocks: &[TimeBlockPrice], buy: &HashMap<DateTime<Utc>, f32>) -> Option<f32> {
    let mut ratios: Vec<f32> = blocks
        .iter()
        .filter(|b| b.price_czk_per_kwh.abs() > f32::EPSILON)
        .filter_map(|b| buy.get(&b.block_start).map(|p| p / b.price_czk_per_kwh))
        .collect();
    if ratios.is_empty() {
        return None;
    }
    ratios.sort_by(|a, b| a.total_cmp(b));
    let mid = ratios.len() / 2;
    Some(if ratios.len().is_multiple_of(2) {
        (ratios[mid - 1] + ratios[mid]) / 2.0
    } else {
        ratios[mid]
    })
}

impl ConfigurablePriceDataSource {
    /// Read the buy sensor (brief H13, H13b) at most once per `buy_fetch_interval`; a failed
    /// read keeps the last good buy prices. WARN and INFO only when the state changes.
    async fn refresh_buy_prices(&self, entity_id: &str, data: &SpotPriceData) {
        let due = {
            let state = self.buy_state.lock();
            state
                .last_fetch
                .is_none_or(|t| t.elapsed() >= self.buy_fetch_interval)
        };
        if !due {
            return;
        }
        let result = self.spot_adapter.read_block_prices(entity_id).await;
        let mut state = self.buy_state.lock();
        state.last_fetch = Some(Instant::now());
        match result {
            Ok(prices) => {
                if state.failing {
                    info!("✅ Buy price sensor {entity_id} readable again");
                    state.failing = false;
                }
                state.last_good.extend(prices);
                if let Some(first) = data.time_block_prices.iter().map(|b| b.block_start).min() {
                    state.last_good.retain(|start, _| *start >= first);
                }
            }
            Err(e) => {
                if !state.failing {
                    warn!(
                        "⚠️ Buy price sensor {entity_id} unavailable ({e:#}); keeping last good buy prices"
                    );
                    state.failing = true;
                }
            }
        }
    }

    /// Set the buy price of every entity block (brief H13, H13b): the last good buy price, or
    /// for blocks the sensor never covered spot scaled by the median buy/spot ratio (plain spot
    /// when no ratio is known). OTE fallback blocks keep the buy price the fallback derived.
    fn fill_buy_prices(&self, entity_id: &str, data: &mut SpotPriceData) {
        let mut state = self.buy_state.lock();
        let ratio = buy_to_spot_ratio(&data.time_block_prices, &state.last_good);
        let fallback_starts: std::collections::HashSet<DateTime<Utc>> =
            data.fallback_block_starts.iter().copied().collect();
        let mut missing = 0;
        for block in &mut data.time_block_prices {
            if fallback_starts.contains(&block.block_start) {
                continue;
            }
            if let Some(buy) = state.last_good.get(&block.block_start) {
                block.buy_price_czk_per_kwh = Some(*buy);
            } else {
                missing += 1;
                block.buy_price_czk_per_kwh = ratio.map(|r| block.price_czk_per_kwh * r);
            }
        }

        // A failed read is already reported; missing blocks are judged on successful reads only
        if !state.failing {
            if missing > 0 && !state.missing {
                let how = if ratio.is_some() {
                    "scaled spot used"
                } else {
                    "spot used"
                };
                warn!("⚠️ Buy price sensor {entity_id}: missing {missing} blocks, {how}");
            } else if missing == 0 && state.missing {
                info!("✅ Buy price sensor {entity_id} covers all blocks again");
            }
            state.missing = missing > 0;
        }
    }
}

#[async_trait]
impl PriceDataSource for ConfigurablePriceDataSource {
    async fn read_prices(&self) -> Result<SpotPriceData> {
        // If using spot prices for buying, fetch from spot adapter
        if self.use_spot_for_buy {
            debug!("💰 [ConfigurablePrice] Using spot prices for buy decisions");
            let mut data = self.read_spot_prices(true).await?;
            if let Some(entity_id) = &self.buy_price_entity {
                self.fill_buy_prices(entity_id, &mut data);
            }

            // When using spot for both buy and sell, compute spot sell price from raw spot price
            if self.use_spot_for_sell {
                for block in &mut data.time_block_prices {
                    // Raw spot price minus sell fee = net sell price
                    let sell_price = block.price_czk_per_kwh - self.spot_sell_fee_czk;
                    block.spot_sell_price_czk_per_kwh = Some(sell_price);
                }
                debug!(
                    "💰 [ConfigurablePrice] Populated spot sell prices (fee: {:.2} CZK/kWh)",
                    self.spot_sell_fee_czk
                );
            }

            Ok(data)
        } else {
            // Use fixed prices for buying
            debug!(
                "💰 [ConfigurablePrice] Using fixed prices for buy decisions (use_spot_prices_to_buy=false)"
            );
            // Selling at spot: the blocks follow the spot timeline (today and tomorrow), so the
            // horizon does not stop at local midnight. Without spot data: fixed blocks for
            // today and tomorrow.
            if self.use_spot_for_sell {
                match self.read_spot_prices(false).await {
                    Ok(spot_data) if !spot_data.time_block_prices.is_empty() => {
                        let hourly = self.fixed_hourly_prices()?;
                        let data = fixed_buy_on_spot_timeline(
                            spot_data,
                            hourly,
                            self.local_timezone(),
                            self.spot_sell_fee_czk,
                        );
                        debug!(
                            "💰 [ConfigurablePrice] Fixed buy on the spot timeline: {} blocks (sell fee: {:.2} CZK/kWh)",
                            data.time_block_prices.len(),
                            self.spot_sell_fee_czk
                        );
                        return Ok(data);
                    }
                    Ok(_) => {
                        tracing::warn!(
                            "⚠️ [ConfigurablePrice] Spot price entity has no blocks. Spot sell prices will be unavailable."
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "⚠️ [ConfigurablePrice] Failed to fetch spot prices for sell: {e}. Spot sell prices will be unavailable."
                        );
                    }
                }
            }

            self.generate_fixed_price_data()
        }
    }

    async fn health_check(&self) -> Result<bool> {
        // Health check depends on whether we're using spot or fixed prices
        if self.use_spot_for_buy {
            self.spot_adapter.health_check().await
        } else {
            // Fixed prices are always available
            Ok(true)
        }
    }

    fn name(&self) -> &str {
        if self.use_spot_for_buy {
            "ConfigurablePrice(Spot)"
        } else {
            "ConfigurablePrice(Fixed)"
        }
    }
}

/// Adapter for fetching consumption history from Home Assistant
pub struct HaConsumptionHistoryAdapter {
    client: Arc<HomeAssistantClient>,
}

impl HaConsumptionHistoryAdapter {
    pub fn new(client: Arc<HomeAssistantClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl fluxion_core::traits::ConsumptionHistoryDataSource for HaConsumptionHistoryAdapter {
    async fn get_history(
        &self,
        entity_id: &str,
        start_time: chrono::DateTime<chrono::Utc>,
        end_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<fluxion_core::traits::HistoryDataPoint>> {
        self.get_history_with(entity_id, start_time, end_time, false)
            .await
    }

    async fn get_history_with(
        &self,
        entity_id: &str,
        start_time: chrono::DateTime<chrono::Utc>,
        end_time: Option<chrono::DateTime<chrono::Utc>>,
        minimal: bool,
    ) -> Result<Vec<fluxion_core::traits::HistoryDataPoint>> {
        let ha_history = self
            .client
            .get_history_with(entity_id, start_time, end_time, minimal)
            .await
            .context("Failed to fetch history from HA")?;

        // Map HA history points to core history points
        let core_history = ha_history
            .into_iter()
            .map(|p| fluxion_core::traits::HistoryDataPoint {
                timestamp: p.timestamp,
                value: p.value,
            })
            .collect();

        Ok(core_history)
    }

    async fn health_check(&self) -> Result<bool> {
        self.client.ping().await.map_err(|e| anyhow::anyhow!(e))
    }

    fn name(&self) -> &str {
        "HaConsumptionHistory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxion_core::InverterOperationMode;
    use serde_json::json;

    // Mock mapper for testing
    struct MockMapper;

    impl VendorEntityMapper for MockMapper {
        fn vendor_name(&self) -> fluxion_core::InverterType {
            fluxion_core::InverterType::Solax
        }

        fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
            match mode {
                InverterOperationMode::SelfUse => 0,
                InverterOperationMode::BackUpMode => 1,
                InverterOperationMode::ForceCharge => 2,
                InverterOperationMode::ForceDischarge => 3,
                InverterOperationMode::NoChargeNoDischarge => 5,
                InverterOperationMode::FeedinPriority => 4,
            }
        }

        fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
            match vendor_mode {
                0 => Some(InverterOperationMode::SelfUse),
                1 => Some(InverterOperationMode::BackUpMode),
                2 => Some(InverterOperationMode::ForceCharge),
                5 => Some(InverterOperationMode::NoChargeNoDischarge),
                3 => Some(InverterOperationMode::ForceDischarge),
                _ => None,
            }
        }

        fn get_work_mode_entity(&self, inverter_id: &str) -> String {
            format!("select.{inverter_id}_work_mode")
        }

        fn get_battery_soc_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{inverter_id}_battery_soc")
        }

        fn get_grid_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{inverter_id}_grid_power")
        }

        fn get_battery_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{inverter_id}_battery_power_charge")
        }

        fn get_pv_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{inverter_id}_pv_power")
        }

        fn get_export_limit_entity(&self, inverter_id: &str) -> String {
            format!("number.{inverter_id}_export_limit")
        }

        fn get_mode_change_request(
            &self,
            _inverter_id: &str,
            mode: InverterOperationMode,
        ) -> fluxion_core::ModeChangeRequest {
            fluxion_core::ModeChangeRequest {
                entity_changes: vec![fluxion_core::EntityChange {
                    entity_id: "select.solax_charger_use_mode".to_string(),
                    option: format!("{mode:?}"),
                }],
            }
        }
    }

    #[test]
    fn test_adapter_creation() {
        let client =
            Arc::new(HomeAssistantClient::new("http://localhost:8123", "test_token").unwrap());
        let mapper: Arc<dyn VendorEntityMapper> = Arc::new(MockMapper);

        let adapter = HomeAssistantInverterAdapter::new(client, mapper);
        assert_eq!(adapter.name(), "HomeAssistant");
    }

    #[test]
    fn test_cz_spot_price_adapter_creation() {
        let client =
            Arc::new(HomeAssistantClient::new("http://localhost:8123", "test_token").unwrap());

        let adapter = CzSpotPriceAdapter::new(client, "sensor.current_spot_electricity_prices");
        assert_eq!(adapter.name(), "CzSpotPrice");
        assert_eq!(adapter.entity_id, "sensor.current_spot_electricity_prices");
    }
    // ---- Pre-actuation guards (brief H11): the adapter's write_command, against mockito ----

    use crate::ha::actuation::ActuationGuardConfig;
    use mockito::{Matcher, Server};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    fn state_body(entity: &str, state: &str) -> Vec<u8> {
        format!(
            r#"{{"entity_id":"{entity}","state":"{state}","attributes":{{}},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}}"#
        )
        .into_bytes()
    }

    fn guard(auto_unlock: bool) -> ActuationGuardConfig {
        ActuationGuardConfig {
            auto_unlock,
            unlock_timeout: Duration::from_millis(300),
            unlock_poll: Duration::from_millis(10),
            charge_check_delay: Duration::from_secs(3600),
            run_mode_wait: Duration::from_secs(3600),
            run_mode_poll: Duration::from_millis(10),
            mode_control_enabled: true,
        }
    }

    fn adapter_for(url: String, cfg: ActuationGuardConfig) -> HomeAssistantInverterAdapter {
        let client = Arc::new(HomeAssistantClient::new(url, "test_token").unwrap());
        HomeAssistantInverterAdapter::new(client, Arc::new(MockMapper)).with_guard(cfg)
    }

    type Log = Arc<StdMutex<Vec<&'static str>>>;

    /// Back Up write with the real SolaX mapper: the minimum SOC is set only when it differs,
    /// then the mode is selected as the native "Back Up Mode" option (brief H19).
    async fn backup_write(current_min_soc: &str, target: u8, expected_sets: usize) {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| state_body("select.solax_lock_state", "Unlocked"))
            .create_async()
            .await;
        let min_soc_body = current_min_soc.to_string();
        let _min_soc = server
            .mock("GET", "/api/states/number.solax_backup_discharge_min_soc")
            .with_body_from_request(move |_| {
                state_body("number.solax_backup_discharge_min_soc", &min_soc_body)
            })
            .create_async()
            .await;
        let set_value = server
            .mock("POST", "/api/services/number/set_value")
            .match_body(Matcher::PartialJson(json!({
                "entity_id": "number.solax_backup_discharge_min_soc",
                "value": target,
            })))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(expected_sets)
            .create_async()
            .await;
        let select = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({
                "entity_id": "select.solax_charger_use_mode",
                "option": "Back Up Mode",
            })))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;

        let client = Arc::new(HomeAssistantClient::new(server.url(), "test_token").unwrap());
        let adapter = HomeAssistantInverterAdapter::new(
            client,
            Arc::new(crate::solax::SolaxEntityMapper::new()),
        )
        .with_guard(guard(true));
        adapter
            .write_command(
                "solax",
                &InverterCommand::SetBackupMode {
                    min_soc_pct: target,
                },
            )
            .await
            .unwrap();
        set_value.assert_async().await;
        select.assert_async().await;
    }

    // ---- Read-only PV extras of a parallel unit (pv_extra_prefixes) ----

    async fn mock_state(
        server: &mut mockito::ServerGuard,
        entity: &'static str,
        state: &'static str,
    ) {
        server
            .mock("GET", format!("/api/states/{entity}").as_str())
            .with_body_from_request(move |_| state_body(entity, state))
            .create_async()
            .await;
    }

    /// `/api/states` lists no heartbeat entity: the state age is not checked
    async fn no_heartbeats(server: &mut mockito::ServerGuard) {
        server
            .mock("GET", "/api/states")
            .with_body("[]")
            .create_async()
            .await;
    }

    /// Master `solax_80` with its core reads, PV 2000 W, today 51.3 kWh, total 1000 kWh
    async fn master_states(server: &mut mockito::ServerGuard) {
        no_heartbeats(server).await;
        for (entity, state) in [
            ("sensor.solax_80_battery_capacity", "55"),
            ("sensor.solax_80_grid_import", "-300"),
            ("sensor.solax_80_battery_power_charge", "1200"),
            ("sensor.solax_80_pv_power_total", "2000"),
            ("select.solax_80_charger_use_mode", "Self Use Mode"),
            ("sensor.solax_80_house_load", "800"),
            ("sensor.solax_80_today_s_solar_energy", "51.3"),
            ("sensor.solax_80_total_solar_energy", "1000"),
        ] {
            mock_state(server, entity, state).await;
        }
    }

    fn extras_adapter(url: String) -> HomeAssistantInverterAdapter {
        let client = Arc::new(HomeAssistantClient::new(url, "test_token").unwrap());
        HomeAssistantInverterAdapter::new(client, Arc::new(crate::solax::SolaxEntityMapper::new()))
            .with_guard(guard(true))
            .with_pv_extra_prefixes(HashMap::from([(
                "solax_80".to_owned(),
                vec!["solax_81".to_owned()],
            )]))
    }

    #[tokio::test]
    async fn pv_extras_are_added_to_master_pv_only() {
        let mut server = Server::new_async().await;
        master_states(&mut server).await;
        mock_state(&mut server, "sensor.solax_81_pv_power_total", "5500").await;
        mock_state(&mut server, "sensor.solax_81_today_s_solar_energy", "143.2").await;
        mock_state(&mut server, "sensor.solax_81_total_solar_energy", "3000").await;
        // House load, grid and SOC of the parallel unit are never read (its battery power is,
        // read-only, brief H33)
        let untouched = server
            .mock("GET", Matcher::Regex(r"^/api/states/(sensor|select|number)\.solax_81_(battery_capacity|house|grid|charger|export|lock|manual)".to_owned()))
            .expect(0)
            .create_async()
            .await;

        let state = extras_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap();

        assert!((state.pv_power_w - 7500.0).abs() < 0.01);
        assert!((state.today_solar_energy_kwh.unwrap() - 194.5).abs() < 0.01);
        assert!((state.total_solar_energy_kwh.unwrap() - 4000.0).abs() < 0.01);
        assert!((state.battery_soc - 55.0).abs() < 0.01);
        assert!((state.battery_power_w - 1200.0).abs() < 0.01);
        assert_eq!(state.house_load_w, Some(800.0));
        untouched.assert_async().await;
    }

    /// Two-unit site (brief H33): master `solax` and parallel unit `solax_extra`, each with
    /// its own battery. Each reports only its own share; the master's meter is the site meter.
    #[tokio::test]
    async fn extra_unit_battery_power_is_read_only_and_reported_separately() {
        let mut server = Server::new_async().await;
        for (entity, state) in [
            ("sensor.solax_battery_capacity", "60"),
            ("sensor.solax_grid_import", "0"),
            ("sensor.solax_battery_power_charge", "839"),
            ("sensor.solax_pv_power_total", "1335"),
            ("select.solax_charger_use_mode", "Self Use Mode"),
            ("sensor.solax_house_load", "447"),
            ("sensor.solax_extra_pv_power_total", "2256"),
            ("sensor.solax_extra_battery_power_charge", "703"),
        ] {
            mock_state(&mut server, entity, state).await;
        }
        no_heartbeats(&mut server).await;
        // The extra unit's house load, grid, SOC and controls are never read
        let untouched = server
            .mock("GET", Matcher::Regex(r"^/api/states/(sensor|select|number)\.solax_extra_(battery_capacity|house|grid|charger|export|lock|manual)".to_owned()))
            .expect(0)
            .create_async()
            .await;
        let no_writes = server
            .mock("POST", Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "test_token").unwrap());
        let adapter = HomeAssistantInverterAdapter::new(
            client,
            Arc::new(crate::solax::SolaxEntityMapper::new()),
        )
        .with_guard(guard(true))
        .with_pv_extra_prefixes(HashMap::from([(
            "solax".to_owned(),
            vec!["solax_extra".to_owned()],
        )]));

        let state = adapter.read_state("solax").await.unwrap();

        assert!((state.pv_power_w - 3591.0).abs() < 0.01);
        assert!((state.battery_power_w - 839.0).abs() < 0.01);
        assert_eq!(state.extra_battery_power_w, Some(703.0));
        assert_eq!(state.house_load_w, Some(447.0));
        untouched.assert_async().await;
        no_writes.assert_async().await;
    }

    #[tokio::test]
    async fn extra_unit_without_a_battery_entity_reports_no_battery_power() {
        let mut server = Server::new_async().await;
        master_states(&mut server).await;
        mock_state(&mut server, "sensor.solax_81_pv_power_total", "500").await;
        let state = extras_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap();
        assert_eq!(state.extra_battery_power_w, None);
        assert!((state.battery_power_w - 1200.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn unavailable_pv_extra_keeps_master_value() {
        let mut server = Server::new_async().await;
        master_states(&mut server).await;
        // Only the extra's power exists; its energy sensors are unavailable
        mock_state(&mut server, "sensor.solax_81_pv_power_total", "500").await;
        mock_state(
            &mut server,
            "sensor.solax_81_today_s_solar_energy",
            "unavailable",
        )
        .await;
        let adapter = extras_adapter(server.url());

        let state = adapter.read_state("solax_80").await.unwrap();

        assert!((state.pv_power_w - 2500.0).abs() < 0.01);
        assert!((state.today_solar_energy_kwh.unwrap() - 51.3).abs() < 0.01);
        assert!((state.total_solar_energy_kwh.unwrap() - 1000.0).abs() < 0.01);
        // Warned once per entity, not on every read
        adapter.read_state("solax_80").await.unwrap();
        assert_eq!(adapter.pv_extra_warned.lock().len(), 2);
    }

    #[tokio::test]
    async fn no_write_path_touches_pv_extra_prefixes() {
        let mut server = Server::new_async().await;
        let extra_reads = server
            .mock(
                "GET",
                Matcher::Regex(r"^/api/states/[a-z_]+\.solax_81".to_owned()),
            )
            .expect(0)
            .create_async()
            .await;
        let extra_writes = server
            .mock("POST", Matcher::Regex(r"^/api/services/".to_owned()))
            .match_body(Matcher::Regex("solax_81".to_owned()))
            .expect(0)
            .create_async()
            .await;
        mock_state(&mut server, "select.solax_80_lock_state", "Unlocked").await;
        mock_state(
            &mut server,
            "number.solax_80_backup_discharge_min_soc",
            "20",
        )
        .await;
        // The writes do happen, on the master only
        let master_writes = server
            .mock("POST", Matcher::Regex(r"^/api/services/".to_owned()))
            .match_body(Matcher::Regex("solax_80".to_owned()))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect_at_least(4)
            .create_async()
            .await;
        let adapter = extras_adapter(server.url());

        for command in [
            InverterCommand::SetMode(InverterOperationMode::SelfUse),
            InverterCommand::SetMode(InverterOperationMode::ForceDischarge),
            InverterCommand::SetMode(InverterOperationMode::NoChargeNoDischarge),
            InverterCommand::SetExportLimit(5000),
            InverterCommand::SetBackupMode { min_soc_pct: 30 },
        ] {
            // Results do not matter here, only which entities the writes touch
            let _ = adapter.write_command("solax_80", &command).await;
        }

        extra_reads.assert_async().await;
        extra_writes.assert_async().await;
        master_writes.assert_async().await;
    }

    // ---- Capacity-weighted SOC of parallel units (battery_extra_prefixes) ----

    /// An entity as HA's REST API serves it: `last_reported` does not advance while the value is
    /// flat, so it equals `last_changed` and `last_updated` (`updated_s_ago` seconds ago)
    fn ha_body(entity: &str, state: &str, updated_s_ago: i64) -> String {
        let at = (Utc::now() - chrono::Duration::seconds(updated_s_ago)).to_rfc3339();
        format!(
            r#"{{"entity_id":"{entity}","state":"{state}","attributes":{{}},"last_changed":"{at}","last_updated":"{at}","last_reported":"{at}"}}"#
        )
    }

    async fn mock_entity(
        server: &mut mockito::ServerGuard,
        entity: &str,
        state: &str,
        updated_s_ago: i64,
    ) {
        server
            .mock("GET", format!("/api/states/{entity}").as_str())
            .with_body(ha_body(entity, state, updated_s_ago))
            .create_async()
            .await;
    }

    /// `/api/states` lists these heartbeat entities, each updated the given seconds ago
    async fn heartbeats(server: &mut mockito::ServerGuard, entities: &[(&str, i64)]) {
        let entities: Vec<(&str, &str, i64)> = entities
            .iter()
            .map(|(entity, age)| (*entity, "230.1", *age))
            .collect();
        heartbeat_states(server, &entities).await;
    }

    /// `/api/states` lists these heartbeat entities, each with its state, updated the given
    /// seconds ago
    async fn heartbeat_states(server: &mut mockito::ServerGuard, entities: &[(&str, &str, i64)]) {
        let listing: Vec<String> = entities
            .iter()
            .map(|(entity, state, age)| ha_body(entity, state, *age))
            .collect();
        server
            .mock("GET", "/api/states")
            .with_body(format!("[{}]", listing.join(",")))
            .create_async()
            .await;
        for (entity, state, age) in entities {
            mock_entity(server, entity, state, *age).await;
        }
    }

    /// Master `solax_80` (own 50 kWh, SOC flat for 10 min, grid voltage updated 5 s ago) and
    /// parallel unit `solax_81` (30 kWh) whose SOC was last updated `extra_soc_age_s` ago and whose
    /// grid voltage `slave_heartbeat_age_s` ago
    async fn battery_states(
        server: &mut mockito::ServerGuard,
        master_soc: &'static str,
        extra_soc: &'static str,
        extra_soc_age_s: i64,
        slave_heartbeat_age_s: i64,
    ) {
        mock_entity(server, "sensor.solax_80_battery_capacity", master_soc, 600).await;
        for (entity, state) in [
            ("sensor.solax_80_grid_import", "-300"),
            ("sensor.solax_80_battery_power_charge", "1200"),
            ("sensor.solax_80_pv_power_total", "2000"),
            ("select.solax_80_charger_use_mode", "Self Use Mode"),
        ] {
            mock_state(server, entity, state).await;
        }
        mock_entity(
            server,
            "sensor.solax_81_battery_capacity",
            extra_soc,
            extra_soc_age_s,
        )
        .await;
        heartbeats(
            server,
            &[
                ("sensor.solax_80_grid_voltage_l1", 5),
                ("sensor.solax_81_grid_voltage_l1", slave_heartbeat_age_s),
            ],
        )
        .await;
    }

    fn battery_weights_adapter(url: String) -> HomeAssistantInverterAdapter {
        let client = Arc::new(HomeAssistantClient::new(url, "test_token").unwrap());
        HomeAssistantInverterAdapter::new(client, Arc::new(crate::solax::SolaxEntityMapper::new()))
            .with_guard(guard(true))
            .with_battery_soc_weights(HashMap::from([(
                "solax_80".to_owned(),
                BatterySocWeights {
                    own_kwh: 50.0,
                    extras: vec![("solax_81".to_owned(), 30.0)],
                },
            )]))
    }

    #[tokio::test]
    async fn battery_soc_is_capacity_weighted_with_parallel_unit() {
        let mut server = Server::new_async().await;
        battery_states(&mut server, "40", "80", 0, 5).await;

        let state = battery_weights_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap();

        // (40 * 50 + 80 * 30) / 80
        assert!((state.battery_soc - 55.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn unreadable_parallel_battery_holds_the_last_combined_soc_then_fails() {
        let mut server = Server::new_async().await;
        battery_states(&mut server, "40", "unavailable", 0, 5).await;
        let adapter = battery_weights_adapter(server.url());

        // No combined SOC yet: the read fails rather than report the master's own 40 %
        assert!(adapter.read_state("solax_80").await.is_err());

        // A combined SOC 5 min ago is held
        adapter.battery_soc_last_good.lock().insert(
            "solax_80".to_owned(),
            (55.0, Utc::now() - chrono::Duration::minutes(5)),
        );
        let state = adapter.read_state("solax_80").await.unwrap();
        assert!((state.battery_soc - 55.0).abs() < 0.01);
        // Warned once per entity, not on every read
        adapter.read_state("solax_80").await.unwrap();
        assert_eq!(adapter.battery_soc_warned.lock().len(), 1);

        // Over 10 min old: the read fails again
        adapter.battery_soc_last_good.lock().insert(
            "solax_80".to_owned(),
            (55.0, Utc::now() - chrono::Duration::minutes(11)),
        );
        let error = adapter.read_state("solax_80").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("combined battery SOC"),
            "{error:#}"
        );
    }

    /// PR #647 R4-1: a slave SOC flat for 30 min with a fresh slave heartbeat is used; with every
    /// slave heartbeat 125 s old the unit is not live and its SOC unreadable
    #[tokio::test]
    async fn parallel_battery_is_readable_while_its_unit_is_live() {
        let mut server = Server::new_async().await;
        battery_states(&mut server, "40", "80", 30 * 60, 5).await;
        let adapter = battery_weights_adapter(server.url());
        let state = adapter.read_state("solax_80").await.unwrap();
        assert!((state.battery_soc - 55.0).abs() < 0.01);
        assert_eq!(
            adapter.battery_soc_last_good.lock()["solax_80"].0,
            state.battery_soc
        );
        assert!(adapter.battery_soc_warned.lock().is_empty());

        let mut server = Server::new_async().await;
        battery_states(&mut server, "40", "80", 30 * 60, 125).await;
        let adapter = battery_weights_adapter(server.url());
        let error = adapter.read_state("solax_80").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("combined battery SOC"),
            "{error:#}"
        );
        assert_eq!(adapter.battery_soc_warned.lock().len(), 1);
    }

    /// Master `solax_80` with its core reads; the SOC flat (updated `soc_age_s` ago)
    async fn master_with_heartbeats(
        server: &mut mockito::ServerGuard,
        soc_age_s: i64,
        heartbeat_ages: &[(&str, i64)],
    ) {
        mock_entity(server, "sensor.solax_80_battery_capacity", "55", soc_age_s).await;
        for (entity, state) in [
            ("sensor.solax_80_grid_import", "-300"),
            ("sensor.solax_80_battery_power_charge", "1200"),
            ("select.solax_80_charger_use_mode", "Self Use Mode"),
        ] {
            mock_state(server, entity, state).await;
        }
        heartbeats(server, heartbeat_ages).await;
    }

    fn plain_adapter(url: String) -> HomeAssistantInverterAdapter {
        let client = Arc::new(HomeAssistantClient::new(url, "test_token").unwrap());
        HomeAssistantInverterAdapter::new(client, Arc::new(crate::solax::SolaxEntityMapper::new()))
    }

    /// PR #647 R4-1: liveness is per inverter from its heartbeat entities' `last_updated`. HA
    /// serves `last_reported == last_changed` for a flat value, so a SOC flat for 10 min must
    /// not fail the read while the grid voltage moves.
    #[tokio::test]
    async fn flat_soc_with_a_fresh_heartbeat_reads() {
        let mut server = Server::new_async().await;
        master_with_heartbeats(
            &mut server,
            600,
            &[
                ("sensor.solax_80_grid_voltage_l1", 5),
                ("sensor.solax_80_pv_power_total", 600),
            ],
        )
        .await;
        let adapter = plain_adapter(server.url());
        let state = adapter.read_state("solax_80").await.unwrap();
        assert!((state.battery_soc - 55.0).abs() < 0.01);
        // Discovered once, voltage first
        assert_eq!(
            adapter.heartbeats.lock()["solax_80"],
            vec![
                "sensor.solax_80_grid_voltage_l1".to_owned(),
                "sensor.solax_80_pv_power_total".to_owned()
            ]
        );
    }

    /// PR #647 R4-1: every heartbeat entity older than 125 s fails the read
    #[tokio::test]
    async fn every_heartbeat_over_120_s_old_fails_the_read() {
        let mut server = Server::new_async().await;
        master_with_heartbeats(
            &mut server,
            600,
            &[
                ("sensor.solax_80_grid_voltage_l1", 125),
                ("sensor.solax_80_grid_frequency", 130),
                ("sensor.solax_80_pv_power_total", 600),
            ],
        )
        .await;
        let error = plain_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("solax_80 not live"),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains("newest 125 s ago"),
            "{error:#}"
        );
    }

    /// PR #647 R4-1: an inverter without heartbeat entities is not age-checked
    #[tokio::test]
    async fn no_heartbeat_entity_skips_the_age_check() {
        let mut server = Server::new_async().await;
        master_with_heartbeats(&mut server, 600, &[("sensor.other_grid_voltage", 999)]).await;
        mock_state(&mut server, "sensor.solax_80_pv_power_total", "2000").await;
        let adapter = plain_adapter(server.url());
        adapter.read_state("solax_80").await.unwrap();
        assert!(adapter.heartbeats.lock()["solax_80"].is_empty());
    }

    /// Master `solax_80` live (SOC 40 % flat, voltage 5 s old, PV `master_pv_w`, battery
    /// `master_battery_w`) and parallel unit `solax_81` with every entity flat for `slave_age_s`:
    /// SOC 10 %, PV `slave_pv_w`, battery `slave_battery_w`
    async fn pair_with_flat_slave(
        server: &mut mockito::ServerGuard,
        (master_pv_w, master_battery_w): (&str, &str),
        (slave_pv_w, slave_battery_w): (&str, &str),
        slave_age_s: i64,
    ) {
        mock_entity(server, "sensor.solax_80_battery_capacity", "40", 600).await;
        for (entity, state) in [
            ("sensor.solax_80_grid_import", "-300"),
            ("sensor.solax_80_battery_power_charge", master_battery_w),
            ("sensor.solax_80_pv_power_total", master_pv_w),
            ("select.solax_80_charger_use_mode", "Self Use Mode"),
        ] {
            mock_entity(server, entity, state, 5).await;
        }
        mock_entity(
            server,
            "sensor.solax_81_battery_capacity",
            "10",
            slave_age_s,
        )
        .await;
        heartbeat_states(
            server,
            &[
                ("sensor.solax_80_grid_voltage_l1", "230.1", 5),
                ("sensor.solax_81_grid_voltage_l1", "230.1", slave_age_s),
                ("sensor.solax_81_house_load", "0", slave_age_s),
                ("sensor.solax_81_pv_power_total", slave_pv_w, slave_age_s),
                (
                    "sensor.solax_81_battery_power_charge",
                    slave_battery_w,
                    slave_age_s,
                ),
            ],
        )
        .await;
    }

    thread_local! {
        static CAPTURED_LOGS: std::cell::RefCell<Option<Vec<u8>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Writes an event into this thread's capture buffer, if one is open
    struct ThreadLogWriter;

    impl std::io::Write for ThreadLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CAPTURED_LOGS.with(|logs| {
                if let Some(logs) = logs.borrow_mut().as_mut() {
                    logs.extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Start capturing log lines (INFO and above) emitted on this thread; `#[tokio::test]` runs
    /// on one thread. One global subscriber writes each event to its own thread's buffer.
    fn start_log_capture() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(|| ThreadLogWriter)
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
        CAPTURED_LOGS.with(|logs| *logs.borrow_mut() = Some(Vec::new()));
    }

    /// The lines captured since `start_log_capture`
    fn captured_logs() -> String {
        let logs = CAPTURED_LOGS.with(|logs| logs.borrow_mut().take().unwrap_or_default());
        String::from_utf8_lossy(&logs).into_owned()
    }

    /// PR #647 R5-1, R6-1: Jakub's slave in standby at its 10 % floor, every heartbeat flat for
    /// 4 h; the master live, no PV, discharging 3 kW. The slave's SOC cannot move, so the pair SOC
    /// is used on every read (no hold, no failed read, so no stale exit), and the state's time is
    /// the master's fresh heartbeat. INFO once.
    #[tokio::test]
    async fn slave_in_standby_at_its_floor_keeps_the_pair_soc() {
        let mut server = Server::new_async().await;
        pair_with_flat_slave(&mut server, ("0", "-3000"), ("0", "0"), 4 * 3600).await;
        let adapter = battery_weights_adapter(server.url());
        start_log_capture();
        for _ in 0..3 {
            let state = adapter.read_state("solax_80").await.unwrap();
            // (40 * 50 + 10 * 30) / 80
            assert!((state.battery_soc - 28.75).abs() < 0.01);
            let data_age = Utc::now() - state.data_at.unwrap();
            assert!(data_age.num_seconds() <= 10, "{data_age}");
        }
        let logs = captured_logs();
        assert_eq!(logs.matches("solax_81 in standby").count(), 1, "{logs}");
        assert!(adapter.battery_soc_warned.lock().is_empty());
        assert!(adapter.standby_units.lock().contains("solax_81"));
    }

    /// PR #647 R6-1: a slave frozen at 0 W / 0 W is not believed in standby while the master
    /// shows sun or charges (the slave would be moving too): its SOC is unreadable, WARN once
    #[tokio::test]
    async fn slave_frozen_at_zero_while_the_master_has_sun_or_charges_is_unreadable() {
        for master in [("8000", "6000"), ("201", "0"), ("0", "51")] {
            let mut server = Server::new_async().await;
            pair_with_flat_slave(&mut server, master, ("0", "0"), 20 * 3600).await;
            let adapter = battery_weights_adapter(server.url());
            start_log_capture();
            for _ in 0..3 {
                let error = adapter.read_state("solax_80").await.unwrap_err();
                assert!(
                    format!("{error:#}").contains("combined battery SOC"),
                    "{master:?} {error:#}"
                );
            }
            let logs = captured_logs();
            assert_eq!(
                logs.matches(
                    "Extra battery SOC sensor.solax_81_battery_capacity for solax_80 unreadable"
                )
                .count(),
                1,
                "{master:?} {logs}"
            );
            assert!(adapter.standby_units.lock().is_empty(), "{master:?}");
        }
    }

    /// PR #647 R5-1, R6-2: a slave frozen with a non-zero battery power, or at 5 kW PV with the
    /// battery at 0 W, is not in standby: its SOC is unreadable (master quiet)
    #[tokio::test]
    async fn slave_frozen_at_non_zero_power_is_unreadable() {
        for slave in [("0", "-1200"), ("5000", "0")] {
            let mut server = Server::new_async().await;
            pair_with_flat_slave(&mut server, ("0", "-3000"), slave, 15 * 60).await;
            let adapter = battery_weights_adapter(server.url());
            let error = adapter.read_state("solax_80").await.unwrap_err();
            assert!(
                format!("{error:#}").contains("combined battery SOC"),
                "{slave:?} {error:#}"
            );
            assert!(adapter.standby_units.lock().is_empty(), "{slave:?}");
        }
    }

    /// PR #647 R5-2: the state's time is the newest heartbeat `last_updated`, so its age counts
    /// from when the data last changed, not from the read
    #[tokio::test]
    async fn the_state_time_is_the_newest_heartbeat() {
        let mut server = Server::new_async().await;
        master_with_heartbeats(
            &mut server,
            600,
            &[
                ("sensor.solax_80_grid_voltage_l1", 90),
                ("sensor.solax_80_grid_frequency", 30),
                ("sensor.solax_80_pv_power_total", 600),
            ],
        )
        .await;
        let state = plain_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap();
        let age = (Utc::now() - state.data_at.unwrap()).num_seconds();
        assert!((30..=33).contains(&age), "{age}");
    }

    /// PR #647 R5-4: a failing `/api/states` fails each read (retried every time), with one WARN
    #[tokio::test]
    async fn failing_heartbeat_listing_fails_the_read() {
        let mut server = Server::new_async().await;
        mock_entity(&mut server, "sensor.solax_80_battery_capacity", "55", 9999).await;
        for (entity, state) in [
            ("sensor.solax_80_grid_import", "-300"),
            ("sensor.solax_80_battery_power_charge", "1200"),
            ("select.solax_80_charger_use_mode", "Self Use Mode"),
            ("sensor.solax_80_pv_power_total", "2000"),
        ] {
            mock_state(&mut server, entity, state).await;
        }
        let listing = server
            .mock("GET", "/api/states")
            .with_status(500)
            .expect(3)
            .create_async()
            .await;
        let adapter = plain_adapter(server.url());
        start_log_capture();
        for _ in 0..3 {
            let error = adapter.read_state("solax_80").await.unwrap_err();
            assert!(
                format!("{error:#}").contains("heartbeat discovery for solax_80 failed"),
                "{error:#}"
            );
        }
        listing.assert_async().await;
        assert_eq!(adapter.heartbeat_discovery_warned.lock().len(), 1);
        let logs = captured_logs();
        assert_eq!(
            logs.matches("Heartbeat discovery for solax_80 failed")
                .count(),
            1,
            "{logs}"
        );
    }

    #[test]
    fn heartbeat_entities_match_by_suffix() {
        for id in [
            "sensor.solax_80_grid_voltage",
            "sensor.solax_80_grid_voltage_l2",
            "sensor.solax_80_grid_frequency_l1",
            "sensor.solax_80_measured_power",
            "sensor.solax_80_house_load",
        ] {
            assert!(is_heartbeat_entity("solax_80", id), "{id}");
        }
        for id in [
            "sensor.solax_81_grid_voltage",
            "sensor.solax_80_battery_capacity",
            "sensor.solax_80_house_load_today",
            "number.solax_80_grid_voltage",
        ] {
            assert!(!is_heartbeat_entity("solax_80", id), "{id}");
        }
    }

    #[tokio::test]
    async fn battery_weights_never_write_to_parallel_unit() {
        let mut server = Server::new_async().await;
        battery_states(&mut server, "40", "80", 0, 5).await;
        let no_writes = server
            .mock("POST", Matcher::Regex(r"^/api/services/".to_owned()))
            .match_body(Matcher::Regex("solax_81".to_owned()))
            .expect(0)
            .create_async()
            .await;

        battery_weights_adapter(server.url())
            .read_state("solax_80")
            .await
            .unwrap();

        no_writes.assert_async().await;
    }

    #[tokio::test]
    async fn backup_write_sets_min_soc_only_when_it_differs() {
        // Differs: the minimum SOC is written before the mode
        backup_write("25.0", 40, 1).await;
    }

    #[tokio::test]
    async fn backup_write_skips_min_soc_when_already_equal() {
        backup_write("40.0", 40, 0).await;
    }

    #[tokio::test]
    async fn backup_write_fails_without_mode_write_when_min_soc_cannot_be_read() {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| state_body("select.solax_lock_state", "Unlocked"))
            .create_async()
            .await;
        let _min_soc = server
            .mock("GET", "/api/states/number.solax_backup_discharge_min_soc")
            .with_status(500)
            .create_async()
            .await;
        let select = server
            .mock("POST", "/api/services/select/select_option")
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(0)
            .create_async()
            .await;

        let client = Arc::new(HomeAssistantClient::new(server.url(), "test_token").unwrap());
        let adapter = HomeAssistantInverterAdapter::new(
            client,
            Arc::new(crate::solax::SolaxEntityMapper::new()),
        )
        .with_guard(guard(true));
        let result = adapter
            .write_command("solax", &InverterCommand::SetBackupMode { min_soc_pct: 40 })
            .await;
        assert!(result.is_err());
        select.assert_async().await;
    }

    #[tokio::test]
    async fn locked_inverter_is_unlocked_and_read_back_before_write() {
        let mut server = Server::new_async().await;
        let log: Log = Arc::new(StdMutex::new(Vec::new()));
        let reads = Arc::new(AtomicUsize::new(0));

        let read_log = log.clone();
        let read_count = reads.clone();
        let lock_read = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(move |_| {
                read_log.lock().unwrap().push("read");
                let n = read_count.fetch_add(1, AtomicOrdering::SeqCst);
                let state = if n == 0 { "Locked" } else { "Unlocked" };
                state_body("select.solax_lock_state", state)
            })
            .expect(2)
            .create_async()
            .await;
        let notify_log = log.clone();
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body_from_request(move |_| {
                notify_log.lock().unwrap().push("notify");
                b"[]".to_vec()
            })
            .create_async()
            .await;
        let select_log = log.clone();
        let unlock = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({"option": "Unlocked"})))
            .with_body_from_request(move |_| {
                select_log.lock().unwrap().push("select");
                b"[]".to_vec()
            })
            .create_async()
            .await;
        let write_log = log.clone();
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .with_body_from_request(move |_| {
                write_log.lock().unwrap().push("write");
                b"[]".to_vec()
            })
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(true));
        adapter
            .write_command(
                "solax",
                &InverterCommand::SetMode(InverterOperationMode::SelfUse),
            )
            .await
            .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["read", "notify", "select", "read", "write"]
        );
        lock_read.assert_async().await;
        notify.assert_async().await;
        unlock.assert_async().await;
        write.assert_async().await;
    }

    #[tokio::test]
    async fn still_locked_after_timeout_skips_write_and_errors() {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| state_body("select.solax_lock_state", "Locked"))
            .create_async()
            .await;
        let _notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body_from_request(|_| b"[]".to_vec())
            .create_async()
            .await;
        let unlock = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({"option": "Unlocked"})))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .expect(0)
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(true));
        let result = adapter
            .write_command(
                "solax",
                &InverterCommand::SetMode(InverterOperationMode::SelfUse),
            )
            .await;

        assert!(result.is_err());
        unlock.assert_async().await;
        write.assert_async().await;
    }

    #[tokio::test]
    async fn unlocked_advanced_counts_as_unlocked() {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| {
                state_body("select.solax_lock_state", "Unlocked - Advanced")
            })
            .create_async()
            .await;
        let unlock = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({"option": "Unlocked"})))
            .expect(0)
            .create_async()
            .await;
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(true));
        adapter
            .write_command(
                "solax",
                &InverterCommand::SetMode(InverterOperationMode::SelfUse),
            )
            .await
            .unwrap();

        unlock.assert_async().await;
        write.assert_async().await;
    }

    #[tokio::test]
    async fn locked_without_auto_unlock_notifies_and_skips_write() {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| state_body("select.solax_lock_state", "Locked"))
            .create_async()
            .await;
        let notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;
        let unlock = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({"option": "Unlocked"})))
            .expect(0)
            .create_async()
            .await;
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .expect(0)
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(false));
        let result = adapter
            .write_command(
                "solax",
                &InverterCommand::SetMode(InverterOperationMode::SelfUse),
            )
            .await;

        assert!(result.is_err());
        notify.assert_async().await;
        unlock.assert_async().await;
        write.assert_async().await;
    }

    #[tokio::test]
    async fn missing_lock_entity_lets_write_proceed() {
        let mut server = Server::new_async().await;
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_status(404)
            .create_async()
            .await;
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(true));
        adapter
            .write_command(
                "solax",
                &InverterCommand::SetMode(InverterOperationMode::SelfUse),
            )
            .await
            .unwrap();

        write.assert_async().await;
    }

    #[tokio::test]
    async fn idle_run_mode_wakes_once_before_force_charge() {
        let mut server = Server::new_async().await;
        let log: Log = Arc::new(StdMutex::new(Vec::new()));
        let _lock = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(|_| state_body("select.solax_lock_state", "Unlocked"))
            .create_async()
            .await;
        let _run_mode = server
            .mock("GET", "/api/states/sensor.solax_run_mode")
            .with_body_from_request(|_| state_body("sensor.solax_run_mode", "Idle Mode"))
            .expect_at_least(2)
            .create_async()
            .await;
        let press_log = log.clone();
        let press = server
            .mock("POST", "/api/services/button/press")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "button.solax_battery_awaken"}),
            ))
            .with_body_from_request(move |_| {
                press_log.lock().unwrap().push("button");
                b"[]".to_vec()
            })
            .expect(1)
            .create_async()
            .await;
        let write_log = log.clone();
        let write = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(
                json!({"entity_id": "select.solax_charger_use_mode"}),
            ))
            .with_body_from_request(move |_| {
                write_log.lock().unwrap().push("write");
                b"[]".to_vec()
            })
            .expect(2)
            .create_async()
            .await;

        let adapter = adapter_for(server.url(), guard(true));
        for _ in 0..2 {
            adapter
                .write_command(
                    "solax",
                    &InverterCommand::SetMode(InverterOperationMode::ForceCharge),
                )
                .await
                .unwrap();
        }

        assert_eq!(*log.lock().unwrap(), vec!["button", "write", "write"]);
        press.assert_async().await;
        write.assert_async().await;
    }

    // ---- Separate control prefix (brief H36) ----

    /// Adapter for an install with sensors `sensor.solax_*` and controls `*.solax_inverter_*`
    fn control_prefix_adapter(url: String) -> HomeAssistantInverterAdapter {
        let client = Arc::new(HomeAssistantClient::new(url, "test_token").unwrap());
        let control = crate::ControlPrefixes::new().with_inverter("solax", "solax_inverter");
        HomeAssistantInverterAdapter::new(
            client,
            crate::solax::create_entity_mapper_with_control(
                fluxion_core::InverterType::Solax,
                control,
            ),
        )
        .with_guard(guard(true))
    }

    #[tokio::test]
    async fn control_prefix_reads_sensors_and_controls_from_their_prefixes() {
        let mut server = Server::new_async().await;
        for (entity, state) in [
            ("sensor.solax_battery_capacity", "60"),
            ("sensor.solax_grid_import", "0"),
            ("sensor.solax_battery_power_charge", "839"),
            ("sensor.solax_pv_power_total", "1335"),
            ("sensor.solax_house_load", "447"),
            ("select.solax_inverter_charger_use_mode", "Self Use Mode"),
            ("number.solax_inverter_export_control_user_limit", "3900"),
        ] {
            mock_state(&mut server, entity, state).await;
        }
        no_heartbeats(&mut server).await;
        let wrong = server
            .mock(
                "GET",
                Matcher::Regex(
                    r"^/api/states/(select|number|button|switch|time)\.solax_[a-hj-z]".to_owned(),
                ),
            )
            .expect(0)
            .create_async()
            .await;

        let state = control_prefix_adapter(server.url())
            .read_state("solax")
            .await
            .unwrap();

        assert!((state.battery_soc - 60.0).abs() < 0.01);
        assert_eq!(state.work_mode, InverterOperationMode::SelfUse);
        assert_eq!(state.house_load_w, Some(447.0));
        assert_eq!(state.export_limit_w, Some(3900));
        wrong.assert_async().await;
    }

    #[tokio::test]
    async fn control_prefix_writes_every_control_entity_with_it() {
        let mut server = Server::new_async().await;
        mock_state(&mut server, "select.solax_inverter_lock_state", "Unlocked").await;
        mock_state(&mut server, "sensor.solax_run_mode", "Idle").await;
        mock_state(
            &mut server,
            "number.solax_inverter_backup_discharge_min_soc",
            "20",
        )
        .await;
        server
            .mock(
                "GET",
                "/api/states/number.solax_inverter_export_control_user_limit",
            )
            .with_body(
                r#"{"entity_id":"number.solax_inverter_export_control_user_limit","state":"0","attributes":{"max":10000},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}"#,
            )
            .create_async()
            .await;
        let wrong_reads = server
            .mock(
                "GET",
                Matcher::Regex(
                    r"^/api/states/(select|number|button|switch|time)\.solax_[a-hj-z]".to_owned(),
                ),
            )
            .expect(0)
            .create_async()
            .await;
        let wrong_writes = server
            .mock("POST", Matcher::Regex(r"^/api/services/".to_owned()))
            .match_body(Matcher::Regex(
                r#""(select|number|button|switch|time)\.solax_[a-hj-z]"#.to_owned(),
            ))
            .expect(0)
            .create_async()
            .await;
        let mut expected = Vec::new();
        for (service, entity) in [
            ("button/press", "button.solax_inverter_battery_awaken"),
            (
                "select/select_option",
                "select.solax_inverter_charger_use_mode",
            ),
            (
                "select/select_option",
                "select.solax_inverter_manual_mode_select",
            ),
            (
                "number/set_value",
                "number.solax_inverter_export_control_user_limit",
            ),
            (
                "number/set_value",
                "number.solax_inverter_backup_discharge_min_soc",
            ),
        ] {
            expected.push(
                server
                    .mock("POST", format!("/api/services/{service}").as_str())
                    .match_body(Matcher::PartialJson(json!({ "entity_id": entity })))
                    .with_body_from_request(|_| b"[]".to_vec())
                    .expect_at_least(1)
                    .create_async()
                    .await,
            );
        }
        let adapter = control_prefix_adapter(server.url());

        for command in [
            InverterCommand::SetMode(InverterOperationMode::ForceCharge),
            InverterCommand::SetExportLimit(5000),
            InverterCommand::SetBackupMode { min_soc_pct: 30 },
        ] {
            adapter.write_command("solax", &command).await.unwrap();
        }

        for mock in expected {
            mock.assert_async().await;
        }
        wrong_reads.assert_async().await;
        wrong_writes.assert_async().await;
    }

    // ---- Mode control off (brief H37): only the export limit is written ----

    #[tokio::test]
    async fn mode_control_off_refuses_mode_writes_and_keeps_the_export_limit_write() {
        let mut server = Server::new_async().await;
        // Locked on the first read only: a mode command that reached the lock guard would use
        // it up and the export limit write would then not need to unlock
        let reads = Arc::new(AtomicUsize::new(0));
        let read_count = reads.clone();
        let lock_read = server
            .mock("GET", "/api/states/select.solax_lock_state")
            .with_body_from_request(move |_| {
                let n = read_count.fetch_add(1, AtomicOrdering::SeqCst);
                let state = if n == 0 { "Locked" } else { "Unlocked" };
                state_body("select.solax_lock_state", state)
            })
            .expect(2)
            .create_async()
            .await;
        mock_state(&mut server, "sensor.solax_run_mode", "Standby Mode").await;
        mock_state(&mut server, "number.solax_backup_discharge_min_soc", "20").await;
        server
            .mock("GET", "/api/states/number.solax_export_control_user_limit")
            .with_body(
                r#"{"entity_id":"number.solax_export_control_user_limit","state":"8000","attributes":{"max":10000},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}"#,
            )
            .create_async()
            .await;
        let _notify = server
            .mock("POST", "/api/services/persistent_notification/create")
            .with_body_from_request(|_| b"[]".to_vec())
            .create_async()
            .await;
        let unlock = server
            .mock("POST", "/api/services/select/select_option")
            .match_body(Matcher::PartialJson(json!({
                "entity_id": "select.solax_lock_state", "option": "Unlocked"
            })))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;
        let export_write = server
            .mock("POST", "/api/services/number/set_value")
            .match_body(Matcher::PartialJson(json!({
                "entity_id": "number.solax_export_control_user_limit", "value": 0
            })))
            .with_body_from_request(|_| b"[]".to_vec())
            .expect(1)
            .create_async()
            .await;
        let mut refused = Vec::new();
        for (service, entity) in [
            ("select/select_option", "select.solax_charger_use_mode"),
            ("select/select_option", "select.solax_manual_mode_select"),
            ("number/set_value", "number.solax_backup_discharge_min_soc"),
            ("button/press", "button.solax_battery_awaken"),
        ] {
            refused.push(
                server
                    .mock("POST", format!("/api/services/{service}").as_str())
                    .match_body(Matcher::PartialJson(json!({ "entity_id": entity })))
                    .expect(0)
                    .create_async()
                    .await,
            );
        }

        let client = Arc::new(HomeAssistantClient::new(server.url(), "test_token").unwrap());
        let adapter = HomeAssistantInverterAdapter::new(
            client,
            Arc::new(crate::solax::SolaxEntityMapper::new()),
        )
        .with_guard(ActuationGuardConfig {
            mode_control_enabled: false,
            ..guard(true)
        });

        for _ in 0..2 {
            for command in [
                InverterCommand::SetMode(InverterOperationMode::ForceCharge),
                InverterCommand::SetMode(InverterOperationMode::SelfUse),
                InverterCommand::SetMode(InverterOperationMode::NoChargeNoDischarge),
                InverterCommand::SetBackupMode { min_soc_pct: 30 },
            ] {
                assert!(adapter.write_command("solax", &command).await.is_err());
            }
        }
        // The curtailment write keeps its unlock-if-needed step
        adapter
            .write_command("solax", &InverterCommand::SetExportLimit(0))
            .await
            .unwrap();

        lock_read.assert_async().await;
        unlock.assert_async().await;
        export_write.assert_async().await;
        for mock in refused {
            mock.assert_async().await;
        }
    }

    #[test]
    fn only_the_export_limit_is_allowed_with_mode_control_off() {
        let off = ActuationGuardConfig {
            mode_control_enabled: false,
            ..ActuationGuardConfig::default()
        };
        let on = ActuationGuardConfig::default();
        assert!(on.mode_control_enabled);
        for command in [
            InverterCommand::SetMode(InverterOperationMode::ForceCharge),
            InverterCommand::SetMode(InverterOperationMode::BackUpMode),
            InverterCommand::SetBackupMode { min_soc_pct: 20 },
        ] {
            assert!(!actuation::command_allowed(&off, &command));
            assert!(actuation::command_allowed(&on, &command));
        }
        assert!(actuation::command_allowed(
            &off,
            &InverterCommand::SetExportLimit(0)
        ));
    }
}

#[cfg(test)]
mod export_limit_guard_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn max_attribute_is_read_from_number_entity() {
        assert_eq!(
            number_max_attribute(&json!({"min": 0, "max": 10000.0, "step": 10})),
            Some(10000)
        );
        assert_eq!(number_max_attribute(&json!({"min": 0})), None);
    }

    #[test]
    fn guard_allows_only_values_within_known_max() {
        assert!(export_limit_allowed(6000, Some(6000)));
        assert!(export_limit_allowed(0, Some(6000)));
        assert!(!export_limit_allowed(6001, Some(6000)));
    }

    #[test]
    fn guard_fails_closed_without_known_max() {
        // The entity max cannot be read: the write is refused
        assert!(!export_limit_allowed(0, None));
        assert!(!export_limit_allowed(9000, None));
    }
}

#[cfg(test)]
mod buy_price_tests {
    use super::*;
    use fluxion_types::pricing::TimeBlockPrice;
    use mockito::Server;
    use serde_json::json;

    const SPOT: &str = "sensor.current_spot_electricity_price_15min";
    const BUY: &str = "sensor.current_buy_electricity_price_15min";

    fn entity_body(entity: &str, state: &str, attributes: serde_json::Value) -> String {
        json!({
            "entity_id": entity,
            "state": state,
            "attributes": attributes,
            "last_changed": "2026-10-14T22:00:00Z",
            "last_updated": "2026-10-14T22:00:00Z",
        })
        .to_string()
    }

    fn source(url: String, buy_entity: Option<&str>) -> ConfigurablePriceDataSource {
        source_with(url, buy_entity, BUY_PRICE_FETCH_INTERVAL)
    }

    fn source_with(
        url: String,
        buy_entity: Option<&str>,
        interval: Duration,
    ) -> ConfigurablePriceDataSource {
        let client = Arc::new(HomeAssistantClient::new(url, "t").unwrap());
        let spot = Arc::new(CzSpotPriceAdapter::new(client, SPOT));
        ConfigurablePriceDataSource::new(spot, true, true, vec![], vec![], 0.5)
            .with_buy_price_entity(buy_entity.map(str::to_string))
            .with_buy_fetch_interval(interval)
    }

    fn buy_mock_path() -> String {
        format!("/api/states/{BUY}")
    }

    fn block_at(data: &SpotPriceData, utc: &str) -> TimeBlockPrice {
        let start: DateTime<Utc> = utc.parse().unwrap();
        data.time_block_prices
            .iter()
            .find(|b| b.block_start == start)
            .cloned()
            .expect("block present")
    }

    async fn spot_mock(server: &mut Server) -> mockito::Mock {
        server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(200)
            .with_body(entity_body(
                SPOT,
                "3.0",
                json!({
                    "2026-10-15T00:00:00+02:00": 2.0,
                    "2026-10-15T00:15:00+02:00": 3.0,
                    "2026-10-15T00:30:00+02:00": 4.0,
                }),
            ))
            .create_async()
            .await
    }

    /// Scan #10: a forecast block whose end is before its start never reaches the schedule, with
    /// or without a price fallback; the other blocks come through unchanged
    #[tokio::test]
    async fn reversed_forecast_block_is_dropped_on_the_read_path() {
        let mut server = Server::new_async().await;
        let _spot = server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(200)
            .with_body(entity_body(
                SPOT,
                "3.0",
                json!({"forecast": [
                    {"start": "2026-10-15T00:00:00+02:00", "end": "2026-10-14T23:00:00+02:00", "price": 9.0},
                    {"start": "2026-10-15T00:15:00+02:00", "end": "2026-10-15T00:30:00+02:00", "price": 2.0},
                    {"start": "2026-10-15T00:30:00+02:00", "end": "2026-10-15T00:45:00+02:00", "price": 3.0},
                ]}),
            ))
            .create_async()
            .await;
        let data = source(server.url(), None).read_prices().await.unwrap();
        assert!(
            data.time_block_prices
                .iter()
                .all(|b| b.duration_minutes <= 48 * 60)
        );
        assert!(data.time_block_prices.iter().all(|b| {
            b.block_start
                != "2026-10-15T00:00:00+02:00"
                    .parse::<DateTime<Utc>>()
                    .unwrap()
        }));
        assert_eq!(
            block_at(&data, "2026-10-14T22:15:00Z").price_czk_per_kwh,
            2.0
        );
    }

    #[tokio::test]
    async fn buy_sensor_sets_buy_price_per_block_and_sell_stays_spot() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        // Buy sensor has no 00:30 block; that block falls back to spot
        let _buy = server
            .mock("GET", format!("/api/states/{BUY}").as_str())
            .with_status(200)
            .with_body(entity_body(
                BUY,
                "2.42",
                json!({
                    "2026-10-15T00:00:00+02:00": 2.42,
                    "2026-10-15T00:15:00+02:00": 2.6,
                    "2026-10-15T00:45:00+02:00": 2.9,
                }),
            ))
            .create_async()
            .await;

        let data = source(server.url(), Some(BUY)).read_prices().await.unwrap();

        let first = block_at(&data, "2026-10-14T22:00:00Z");
        assert_eq!(first.price_czk_per_kwh, 2.0);
        assert_eq!(first.buy_price_czk_per_kwh, Some(2.42));
        assert_eq!(first.buy_energy_price_czk_per_kwh(), 2.42);
        assert_eq!(first.spot_sell_price_czk_per_kwh, Some(1.5));

        let second = block_at(&data, "2026-10-14T22:15:00Z");
        assert_eq!(second.buy_price_czk_per_kwh, Some(2.6));
        assert_eq!(second.spot_sell_price_czk_per_kwh, Some(2.5));

        // Uncovered block: spot scaled by the median buy/spot ratio of the covered blocks
        let missing = block_at(&data, "2026-10-14T22:30:00Z");
        let expected = 4.0 * ((2.42 / 2.0 + 2.6 / 3.0) / 2.0);
        assert!((missing.buy_price_czk_per_kwh.unwrap() - expected).abs() < 1e-3);
    }

    #[tokio::test]
    async fn missing_buy_sensor_falls_back_to_spot_for_every_block() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let _buy = server
            .mock("GET", format!("/api/states/{BUY}").as_str())
            .with_status(404)
            .create_async()
            .await;

        let data = source(server.url(), Some(BUY)).read_prices().await.unwrap();

        assert_eq!(data.time_block_prices.len(), 3);
        assert!(
            data.time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_none())
        );
        assert_eq!(
            block_at(&data, "2026-10-14T22:00:00Z").buy_energy_price_czk_per_kwh(),
            2.0
        );
    }

    #[tokio::test]
    async fn unavailable_buy_sensor_falls_back_to_spot() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let _buy = server
            .mock("GET", format!("/api/states/{BUY}").as_str())
            .with_status(200)
            .with_body(entity_body(BUY, "unavailable", json!({})))
            .create_async()
            .await;

        let data = source(server.url(), Some(BUY)).read_prices().await.unwrap();

        assert!(
            data.time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_none())
        );
    }

    #[tokio::test]
    async fn unset_or_empty_buy_entity_is_never_requested() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let buy = server
            .mock("GET", format!("/api/states/{BUY}").as_str())
            .expect(0)
            .create_async()
            .await;

        let unset = source(server.url(), None).read_prices().await.unwrap();
        let empty = source(server.url(), Some("  "))
            .read_prices()
            .await
            .unwrap();

        assert!(
            unset
                .time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_none())
        );
        assert!(
            empty
                .time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_none())
        );
        assert_eq!(
            block_at(&unset, "2026-10-14T22:15:00Z").buy_energy_price_czk_per_kwh(),
            3.0
        );
        buy.assert_async().await;
    }

    #[tokio::test]
    async fn failed_buy_read_reuses_last_good_prices() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let ok = server
            .mock("GET", buy_mock_path().as_str())
            .with_status(200)
            .with_body(entity_body(
                BUY,
                "2.42",
                json!({
                    "2026-10-15T00:00:00+02:00": 2.42,
                    "2026-10-15T00:15:00+02:00": 2.6,
                }),
            ))
            .expect(1)
            .create_async()
            .await;
        let src = source_with(server.url(), Some(BUY), Duration::ZERO);
        let first = src.read_prices().await.unwrap();
        ok.assert_async().await;

        // The sensor now fails twice: both reads keep the last good buy prices
        let failing = server
            .mock("GET", buy_mock_path().as_str())
            .with_status(500)
            .expect(2)
            .create_async()
            .await;
        let second = src.read_prices().await.unwrap();
        let third = src.read_prices().await.unwrap();
        failing.assert_async().await;

        for data in [&first, &second, &third] {
            assert_eq!(
                block_at(data, "2026-10-14T22:00:00Z").buy_price_czk_per_kwh,
                Some(2.42)
            );
            assert_eq!(
                block_at(data, "2026-10-14T22:15:00Z").buy_price_czk_per_kwh,
                Some(2.6)
            );
        }
    }

    #[tokio::test]
    async fn partial_buy_coverage_scales_spot_for_uncovered_blocks() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let _buy = server
            .mock("GET", buy_mock_path().as_str())
            .with_status(200)
            .with_body(entity_body(
                BUY,
                "2.42",
                json!({
                    "2026-10-15T00:00:00+02:00": 2.42,
                    "2026-10-15T00:15:00+02:00": 3.63,
                }),
            ))
            .create_async()
            .await;

        let data = source(server.url(), Some(BUY)).read_prices().await.unwrap();

        // Covered blocks take the buy sensor; the uncovered 00:30 block is spot x 1.21
        assert_eq!(
            block_at(&data, "2026-10-14T22:00:00Z").buy_price_czk_per_kwh,
            Some(2.42)
        );
        let scaled = block_at(&data, "2026-10-14T22:30:00Z");
        assert!((scaled.buy_price_czk_per_kwh.unwrap() - 4.0 * 1.21).abs() < 1e-3);
        assert_eq!(scaled.spot_sell_price_czk_per_kwh, Some(3.5));
    }

    #[tokio::test]
    async fn buy_sensor_without_covered_blocks_falls_back_to_spot() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        // Buy values only for another day: no ratio can be known for the spot blocks
        let _buy = server
            .mock("GET", buy_mock_path().as_str())
            .with_status(200)
            .with_body(entity_body(
                BUY,
                "2.42",
                json!({ "2026-10-16T00:00:00+02:00": 2.42 }),
            ))
            .create_async()
            .await;

        let data = source(server.url(), Some(BUY)).read_prices().await.unwrap();

        assert!(
            data.time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_none())
        );
    }

    #[tokio::test]
    async fn buy_sensor_is_read_at_most_once_per_interval() {
        let mut server = Server::new_async().await;
        let _spot = spot_mock(&mut server).await;
        let buy = server
            .mock("GET", buy_mock_path().as_str())
            .with_status(200)
            .with_body(entity_body(
                BUY,
                "2.42",
                json!({ "2026-10-15T00:00:00+02:00": 2.42 }),
            ))
            .expect(1)
            .create_async()
            .await;
        let src = source(server.url(), Some(BUY));

        // Spot is read on every call; the buy sensor only once within the interval
        let _first = src.read_prices().await.unwrap();
        let _second = src.read_prices().await.unwrap();
        let third = src.read_prices().await.unwrap();

        buy.assert_async().await;
        assert_eq!(
            block_at(&third, "2026-10-14T22:00:00Z").buy_price_czk_per_kwh,
            Some(2.42)
        );
    }

    #[test]
    fn buy_to_spot_ratio_is_median_over_covered_blocks() {
        fn block(secs: i64, spot: f32) -> TimeBlockPrice {
            TimeBlockPrice {
                block_start: DateTime::<Utc>::from_timestamp(secs, 0).unwrap(),
                duration_minutes: 15,
                price_czk_per_kwh: spot,
                effective_price_czk_per_kwh: spot,
                spot_sell_price_czk_per_kwh: None,
                buy_price_czk_per_kwh: None,
            }
        }
        let buy: HashMap<DateTime<Utc>, f32> = [(0, 2.42), (900, 4.84), (1800, 1.0)]
            .into_iter()
            .map(|(secs, price)| (DateTime::<Utc>::from_timestamp(secs, 0).unwrap(), price))
            .collect();
        // Block at 1800 has no buy value; block at 2700 has zero spot and is ignored
        let blocks = vec![
            block(0, 2.0),
            block(900, 4.0),
            block(1800, 5.0),
            block(2700, 0.0),
        ];
        let ratio = buy_to_spot_ratio(&blocks, &buy).unwrap();
        assert!((ratio - 1.21).abs() < 1e-4);

        // Even count: median of 1.2 and 1.4
        let buy: HashMap<DateTime<Utc>, f32> = [(0, 2.4), (900, 5.6)]
            .into_iter()
            .map(|(secs, price)| (DateTime::<Utc>::from_timestamp(secs, 0).unwrap(), price))
            .collect();
        let ratio = buy_to_spot_ratio(&blocks[..2], &buy).unwrap();
        assert!((ratio - 1.3).abs() < 1e-4);

        assert_eq!(buy_to_spot_ratio(&blocks, &HashMap::new()), None);
    }
}

/// The OTE price fallback inside the price source (brief H35); fixtures as in
/// `price_fallback::tests`
#[cfg(test)]
mod price_fallback_source_tests {
    use super::*;
    use crate::price_fallback::{PriceFallback, PriceFallbackConfig, parse_check_time};
    use chrono::TimeZone;
    use chrono_tz::Europe::Prague;
    use mockito::{Matcher, Server};
    use serde_json::json;

    const SPOT: &str = "sensor.current_spot_electricity_price_15min";
    const BUY: &str = "sensor.current_buy_electricity_price_15min";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/price_fallback/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        Prague
            .with_ymd_and_hms(2026, 10, 13, 18, 0, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Recorded integration values of 2026-10-13 (00:00–21:45 Prague)
    fn recorded() -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&fixture(
            "integration-spot-attrs-2026-10-13.json",
        ))
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
    }

    fn entity_body(
        entity: &str,
        attributes: &serde_json::Map<String, serde_json::Value>,
    ) -> String {
        json!({
            "entity_id": entity,
            "state": "4.0",
            "attributes": attributes,
            "last_changed": "2026-10-13T16:00:00Z",
            "last_updated": "2026-10-13T16:00:00Z",
        })
        .to_string()
    }

    /// Buy entity from spot: `buy(local time, spot)`
    fn buy_attrs(buy: impl Fn(&str, f64) -> f64) -> serde_json::Map<String, serde_json::Value> {
        recorded()
            .into_iter()
            .map(|(k, v)| {
                let price = buy(&k, v.as_f64().unwrap());
                (k, json!(price))
            })
            .collect()
    }

    async fn mocks(server: &mut Server, buy: serde_json::Map<String, serde_json::Value>) {
        server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(200)
            .with_body(entity_body(SPOT, &recorded()))
            .create_async()
            .await;
        server
            .mock("GET", format!("/api/states/{BUY}").as_str())
            .with_status(200)
            .with_body(entity_body(BUY, &buy))
            .create_async()
            .await;
        server
            .mock("POST", "/ote")
            .with_status(200)
            .with_body(fixture("ote-dam-2026-10-13_14.xml"))
            .create_async()
            .await;
        server
            .mock("GET", "/cnb")
            .match_query(Matcher::UrlEncoded("date".into(), "2026-10-12".into()))
            .with_status(200)
            .with_body(fixture("cnb-daily-2026-10-12.json"))
            .create_async()
            .await;
        server
            .mock("GET", "/cnb")
            .match_query(Matcher::UrlEncoded("date".into(), "2026-10-13".into()))
            .with_status(200)
            .with_body(fixture("cnb-daily-2026-10-13.json"))
            .create_async()
            .await;
    }

    fn source(url: &str) -> (ConfigurablePriceDataSource, Arc<PriceFallback>) {
        let mut config =
            PriceFallbackConfig::new(SPOT, parse_check_time("13:45").unwrap(), true, false);
        config.ote_url = format!("{url}/ote");
        config.cnb_url = format!("{url}/cnb");
        config.fixed_now = Some(now());
        config.alert_delay = chrono::Duration::zero();
        let fallback = Arc::new(
            PriceFallback::new(config, Arc::new(RwLock::new(Default::default()))).unwrap(),
        );
        let client = Arc::new(HomeAssistantClient::new(url, "t").unwrap());
        let spot = Arc::new(CzSpotPriceAdapter::new(client, SPOT));
        let source = ConfigurablePriceDataSource::new(spot, true, true, vec![], vec![], 0.5)
            .with_buy_price_entity(Some(BUY.to_owned()))
            .with_buy_fetch_interval(Duration::ZERO)
            .with_price_fallback(Some(fallback.clone()));
        (source, fallback)
    }

    fn block(data: &SpotPriceData, start: DateTime<Utc>) -> TimeBlockPrice {
        data.time_block_prices
            .iter()
            .find(|b| b.block_start == start)
            .cloned()
            .unwrap()
    }

    #[tokio::test]
    async fn fallback_slots_get_buy_from_an_exact_fit() {
        let mut server = Server::new_async().await;
        mocks(
            &mut server,
            buy_attrs(|_, s| (s * 1.21 * 10_000.0).round() / 10_000.0),
        )
        .await;
        let (source, fallback) = source(&server.url());

        let first = source.read_prices().await.unwrap();
        assert!(first.fallback_block_starts.is_empty());
        assert!(fallback.wants_fetch());
        fallback.maybe_fetch(now()).await;
        let data = source.read_prices().await.unwrap();

        // 22:00–23:45 today and all of tomorrow came from OTE
        assert_eq!(data.fallback_block_starts.len(), 8 + 96);
        let tomorrow = Prague
            .with_ymd_and_hms(2026, 10, 14, 8, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let b = block(&data, tomorrow);
        let buy = b.buy_price_czk_per_kwh.unwrap();
        assert!((buy - b.price_czk_per_kwh * 1.21).abs() < 1e-3, "{b:?}");
        assert_eq!(
            b.spot_sell_price_czk_per_kwh,
            Some(b.price_czk_per_kwh - 0.5)
        );
        // Entity slots keep the entity's buy price
        let entity_slot = Prague
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let e = block(&data, entity_slot);
        assert!(!data.is_fallback_block(entity_slot));
        assert!((e.buy_price_czk_per_kwh.unwrap() - e.price_czk_per_kwh * 1.21).abs() < 1e-3);

        let status = fallback.status();
        let fit = status.buy_fit.unwrap();
        assert!(fit.used && (fit.a - 1.21).abs() < 1e-3, "{fit:?}");
        assert_eq!(
            status.banner.as_deref(),
            Some(
                "Today's and tomorrow's prices missing from sensor.current_spot_electricity_price_15min — using OTE fallback"
            )
        );
    }

    /// Owner ruling: a buy fit that is not exact fills nothing (wrong prices are worse than a
    /// shorter horizon)
    #[tokio::test]
    async fn fallback_fills_nothing_when_the_buy_fit_is_not_exact() {
        let mut server = Server::new_async().await;
        // A buy price with a tariff step: not a × spot + b
        mocks(
            &mut server,
            buy_attrs(|k, s| {
                let hour: u32 = k[11..13].parse().unwrap();
                s * 1.21 + if hour < 6 { 0.3 } else { 1.2 }
            }),
        )
        .await;
        let (source, fallback) = source(&server.url());

        source.read_prices().await.unwrap();
        fallback.maybe_fetch(now()).await;
        let data = source.read_prices().await.unwrap();

        assert!(data.fallback_block_starts.is_empty());
        assert_eq!(data.time_block_prices.len(), 88);
        assert!(
            data.time_block_prices
                .iter()
                .all(|b| b.buy_price_czk_per_kwh.is_some())
        );
        let status = fallback.status();
        assert_eq!(
            status.fallback,
            fluxion_core::price_feed::FallbackState::Rejected
        );
        assert!(
            status
                .banner
                .unwrap()
                .contains("— no fallback available (buy price fit failed:")
        );
    }

    /// Second day of an outage: the entities are unavailable, the stored validation of
    /// yesterday stands in, and the price read returns the OTE prices instead of failing
    #[tokio::test]
    async fn unreadable_entity_on_day_two_uses_the_stored_validation() {
        let mut server = Server::new_async().await;
        for entity in [SPOT, BUY] {
            server
                .mock("GET", format!("/api/states/{entity}").as_str())
                .with_status(404)
                .create_async()
                .await;
        }
        server
            .mock("POST", "/ote")
            .with_status(200)
            .with_body(fixture("ote-dam-2026-10-13_14.xml"))
            .create_async()
            .await;
        server
            .mock("GET", "/cnb")
            .match_query(Matcher::UrlEncoded("date".into(), "2026-10-13".into()))
            .with_status(200)
            .with_body(fixture("cnb-daily-2026-10-13.json"))
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("price_fallback_validation.json");
        std::fs::write(
            &path,
            json!({
                "validated_at": "2026-10-13T16:00:00Z",
                "day": "2026-10-13",
                "rate_source": "before_today",
                "rate_valid_for": "2026-10-12",
                "eur_czk": 24.39,
                "match_share": 1.0,
                "buy_fit": {"a": 1.21, "b": 0.0, "max_residual": 0.0001, "slots": 88, "used": true}
            })
            .to_string(),
        )
        .unwrap();
        let day_two = Prague
            .with_ymd_and_hms(2026, 10, 14, 10, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let mut config =
            PriceFallbackConfig::new(SPOT, parse_check_time("13:45").unwrap(), true, false);
        config.ote_url = format!("{}/ote", server.url());
        config.cnb_url = format!("{}/cnb", server.url());
        config.fixed_now = Some(day_two);
        config.alert_delay = chrono::Duration::zero();
        config.validation_path = Some(path);
        let fallback = Arc::new(
            PriceFallback::new(config, Arc::new(RwLock::new(Default::default()))).unwrap(),
        );
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let source = ConfigurablePriceDataSource::new(
            Arc::new(CzSpotPriceAdapter::new(client, SPOT)),
            true,
            true,
            vec![],
            vec![],
            0.5,
        )
        .with_buy_price_entity(Some(BUY.to_owned()))
        .with_buy_fetch_interval(Duration::ZERO)
        .with_price_fallback(Some(fallback.clone()));

        assert!(source.read_prices().await.is_err());
        fallback.maybe_fetch(day_two).await;
        let data = source.read_prices().await.unwrap();

        assert_eq!(data.fallback_block_starts.len(), 56);
        let b = block(&data, day_two);
        assert!((b.buy_price_czk_per_kwh.unwrap() - b.price_czk_per_kwh * 1.21).abs() < 1e-3);
        assert_eq!(
            b.spot_sell_price_czk_per_kwh,
            Some(b.price_czk_per_kwh - 0.5)
        );
        assert!(
            fallback
                .status()
                .banner
                .unwrap()
                .ends_with("— using OTE fallback, validated 2026-10-13")
        );
    }

    /// Review #7: on the fixed-buy path the spot sell price of a fallback block keeps the
    /// `ote_fallback` marker
    #[test]
    fn fixed_buy_blocks_keep_the_fallback_marker_of_their_spot_sell_price() {
        let t0 = now();
        let blk = |i: i64, price: f32| TimeBlockPrice {
            block_start: t0 + chrono::Duration::minutes(15 * i),
            duration_minutes: 15,
            price_czk_per_kwh: price,
            effective_price_czk_per_kwh: price,
            spot_sell_price_czk_per_kwh: None,
            buy_price_czk_per_kwh: None,
        };
        let spot = SpotPriceData {
            time_block_prices: (0..4).map(|i| blk(i, 2.0)).collect(),
            fallback_block_starts: vec![t0 + chrono::Duration::minutes(30)],
            ..SpotPriceData::default()
        };

        let fixed = fixed_buy_on_spot_timeline(spot, &[5.0; 24], Prague, 0.5);

        assert!(
            fixed
                .time_block_prices
                .iter()
                .all(|b| b.spot_sell_price_czk_per_kwh == Some(1.5) && b.price_czk_per_kwh == 5.0)
        );
        assert_eq!(
            fixed.fallback_block_starts,
            vec![t0 + chrono::Duration::minutes(30)]
        );
        assert!(!fixed.is_fallback_block(t0));
    }

    #[tokio::test]
    async fn unreadable_entity_is_reported_as_missing_prices() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(404)
            .create_async()
            .await;
        let (source, fallback) = source(&server.url());

        assert!(source.read_prices().await.is_err());

        let status = fallback.status();
        assert!(status.today_missing && status.tomorrow_missing);
        assert!(
            status
                .banner
                .unwrap()
                .starts_with("Today's and tomorrow's prices missing from sensor.current_spot_electricity_price_15min — no fallback available")
        );
    }
}

/// Fixed buy prices keep the spot horizon (today and tomorrow), brief fixed-buy-horizon
#[cfg(test)]
mod fixed_buy_horizon_tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use chrono_tz::Europe::Prague;
    use fluxion_core::strategy::winter_reserve::{
        BatteryParams, PlanBlock, PlanMode, WinterReserveParams, plan_from,
    };
    use mockito::Server;
    use serde_json::json;

    const SPOT: &str = "sensor.current_spot_electricity_price_15min";
    const FEE: f32 = 0.5;

    /// 2026-11-02 17:00 Prague (CET, no DST change on 2026-11-02 or 2026-11-03)
    fn now_day1() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 2, 16, 0, 0).unwrap()
    }

    /// Local midnight 2026-11-02 in UTC
    fn day1_start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 1, 23, 0, 0).unwrap()
    }

    fn hourly() -> Vec<f32> {
        (0..24).map(|h| 2.0 + h as f32 * 0.1).collect()
    }

    /// Spot entity with 192 blocks (2026-11-02 and 2026-11-03 Prague), `spot(i)` per block
    async fn spot_mock(server: &mut Server, spot: impl Fn(i64) -> f32) -> mockito::Mock {
        let attrs: serde_json::Map<String, serde_json::Value> = (0..192)
            .map(|i| {
                let start =
                    (day1_start() + chrono::Duration::minutes(15 * i)).with_timezone(&Prague);
                (start.to_rfc3339(), json!(spot(i)))
            })
            .collect();
        server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(200)
            .with_body(
                json!({
                    "entity_id": SPOT,
                    "state": "3.0",
                    "attributes": attrs,
                    "last_changed": "2026-11-02T12:00:00Z",
                    "last_updated": "2026-11-02T12:00:00Z",
                })
                .to_string(),
            )
            .create_async()
            .await
    }

    fn source(url: &str, sell_at_spot: bool, fixed: Vec<f32>) -> ConfigurablePriceDataSource {
        let client = Arc::new(HomeAssistantClient::new(url.to_owned(), "t").unwrap());
        let spot = Arc::new(CzSpotPriceAdapter::new(client, SPOT));
        spot.set_timezone(Some(Prague));
        ConfigurablePriceDataSource::new(spot, false, sell_at_spot, fixed, vec![], FEE)
            .with_clock(now_day1)
    }

    fn local_hour(start: DateTime<Utc>) -> usize {
        start.with_timezone(&Prague).hour() as usize
    }

    /// Test 1: fixed buy with spot sell follows the spot timeline (today and tomorrow)
    #[tokio::test]
    async fn fixed_buy_with_spot_sell_keeps_the_spot_horizon() {
        let mut server = Server::new_async().await;
        let spot = |i: i64| 1.0 + i as f32 * 0.01;
        let _m = spot_mock(&mut server, spot).await;
        let fixed = hourly();

        let data = source(&server.url(), true, fixed.clone())
            .read_prices()
            .await
            .unwrap();

        assert_eq!(data.time_block_prices.len(), 192);
        for (i, b) in data.time_block_prices.iter().enumerate() {
            assert_eq!(
                b.block_start,
                day1_start() + chrono::Duration::minutes(15 * i as i64)
            );
            assert_eq!(
                b.price_czk_per_kwh,
                fixed[local_hour(b.block_start)],
                "block {i}"
            );
            assert_eq!(
                b.effective_price_czk_per_kwh,
                fixed[local_hour(b.block_start)]
            );
            assert_eq!(b.buy_price_czk_per_kwh, None);
            let sell = b.spot_sell_price_czk_per_kwh.expect("spot sell");
            assert!((sell - (spot(i as i64) - FEE)).abs() < 1e-5, "block {i}");
        }
        // 23:15 UTC on day 1 is 00:15 local on day 2: hour 0, not hour 23
        let late = Utc.with_ymd_and_hms(2026, 11, 2, 23, 15, 0).unwrap();
        let b = data
            .time_block_prices
            .iter()
            .find(|b| b.block_start == late)
            .expect("block after local midnight");
        assert_eq!(b.price_czk_per_kwh, fixed[0]);
        assert_eq!(
            data.ha_last_updated,
            Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap()
        );
    }

    /// Test 2: without spot data the fixed blocks cover today and tomorrow locally
    #[tokio::test]
    async fn fixed_buy_without_spot_covers_today_and_tomorrow() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(404)
            .create_async()
            .await;
        let fixed = hourly();

        for sell_at_spot in [true, false] {
            let data = source(&server.url(), sell_at_spot, fixed.clone())
                .read_prices()
                .await
                .unwrap();
            assert_eq!(
                data.time_block_prices.len(),
                192,
                "sell_at_spot={sell_at_spot}"
            );
            for (i, b) in data.time_block_prices.iter().enumerate() {
                assert_eq!(
                    b.block_start,
                    day1_start() + chrono::Duration::minutes(15 * i as i64)
                );
                assert_eq!(b.price_czk_per_kwh, fixed[local_hour(b.block_start)]);
                assert_eq!(b.spot_sell_price_czk_per_kwh, None);
            }
        }
    }

    /// Review R4: "today" is the local date. At 23:30 UTC on 2026-11-02 it is 00:30 on
    /// 2026-11-03 in Prague, so the blocks start at local midnight of 2026-11-03.
    #[tokio::test]
    async fn fixed_blocks_take_today_from_the_local_date() {
        fn late_utc() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 11, 2, 23, 30, 0).unwrap()
        }
        let data = source("http://127.0.0.1:9", false, hourly())
            .with_clock(late_utc)
            .read_prices()
            .await
            .unwrap();
        let local_midnight = Utc.with_ymd_and_hms(2026, 11, 2, 23, 0, 0).unwrap();
        assert_eq!(data.time_block_prices.len(), 192);
        assert_eq!(data.time_block_prices[0].block_start, local_midnight);
        assert_eq!(
            data.time_block_prices[191].block_start,
            local_midnight + chrono::Duration::minutes(15 * 191)
        );
    }

    /// Test 3: the WinterReserve horizon from fixed-buy data passes local midnight and finds
    /// tomorrow's morning, so the night floor is no longer the whole battery and the plan exports
    /// in the evening peak (the owner's site at 17:00 on 2026-11-02 logged `first_morning=none
    /// head_floor_kwh=23.00`). Tomorrow's own peak is followed by the end of the horizon (an open
    /// night, floor Emax by design) until the day after's prices arrive, so the export goes to
    /// today's peak.
    #[tokio::test]
    async fn winter_reserve_horizon_reaches_tomorrows_morning_and_exports_in_the_peak() {
        let mut server = Server::new_async().await;
        // Sell 8.4 net from 17:00 to 20:00 local on both days, else 2.0 net
        let peak = |i: i64| (68..80).contains(&(i % 96));
        let today_peak = |i: i64| (68..80).contains(&i);
        let _m = spot_mock(&mut server, |i| if peak(i) { 8.4 + FEE } else { 2.0 + FEE }).await;

        let data = source(&server.url(), true, vec![3.22; 24])
            .read_prices()
            .await
            .unwrap();

        let tz = Prague;
        let blocks: Vec<PlanBlock> = data
            .time_block_prices
            .iter()
            .filter(|b| b.block_start >= now_day1())
            .map(|b| {
                let h = local_hour(b.block_start);
                let day2 = b.block_start >= day1_start() + chrono::Duration::days(1);
                let pv = if day2 && (8..15).contains(&h) {
                    1.0
                } else {
                    0.0
                };
                PlanBlock {
                    start: b.block_start,
                    duration_minutes: b.duration_minutes,
                    buy_czk: b.effective_price_czk_per_kwh,
                    sell_czk: b.spot_sell_price_czk_per_kwh.unwrap_or(FEE),
                    load_kwh: 0.25,
                    pv_kwh: pv,
                    reserve_load_kwh: 0.25 * 1.1,
                    reserve_pv_kwh: pv * 0.7,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                }
            })
            .collect();
        let local_midnight = day1_start() + chrono::Duration::days(1);
        assert!(
            blocks.last().unwrap().start > local_midnight,
            "horizon stops at midnight"
        );
        // first_morning: pv >= load after a deficit run of at least 4 h, on day 2
        let first_morning = blocks
            .iter()
            .enumerate()
            .find(|(i, b)| *i >= 16 && b.pv_kwh >= b.load_kwh)
            .map(|(_, b)| b.start);
        assert_eq!(
            first_morning,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 7, 0, 0).unwrap())
        );

        let battery = BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.95,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 9.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: true,
            hold_pv_to_battery: false,
            min_charge_kwh: 0.0,
            charge_cost_czk_per_kwh: None,
        };
        let plan = plan_from(
            &blocks,
            95.0,
            tz,
            &battery,
            &WinterReserveParams::default(),
            None,
        );
        assert!(
            plan.blocks[0].floor_kwh < 23.0,
            "open night: floor is the whole battery"
        );
        let exports_in_peak: Vec<_> = plan
            .blocks
            .iter()
            .filter(|b| b.mode == PlanMode::Export)
            .map(|b| b.start)
            .collect();
        assert!(!exports_in_peak.is_empty(), "no export planned");
        assert!(
            exports_in_peak.iter().all(|s| {
                let i = (*s - day1_start()).num_minutes() / 15;
                today_peak(i)
            }),
            "exports outside the peak: {exports_in_peak:?}"
        );
    }

    /// Test 4: spot buy output is unchanged
    #[tokio::test]
    async fn spot_buy_output_is_unchanged() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", format!("/api/states/{SPOT}").as_str())
            .with_status(200)
            .with_body(
                json!({
                    "entity_id": SPOT,
                    "state": "3.0",
                    "attributes": {
                        "2026-11-02T23:30:00+01:00": 2.0,
                        "2026-11-02T23:45:00+01:00": 3.0,
                        "2026-11-03T00:00:00+01:00": 4.0,
                    },
                    "last_changed": "2026-11-02T12:00:00Z",
                    "last_updated": "2026-11-02T12:00:00Z",
                })
                .to_string(),
            )
            .create_async()
            .await;
        let client = Arc::new(HomeAssistantClient::new(server.url(), "t").unwrap());
        let spot = Arc::new(CzSpotPriceAdapter::new(client, SPOT));
        spot.set_timezone(Some(Prague));
        let data = ConfigurablePriceDataSource::new(spot, true, true, hourly(), vec![], FEE)
            .with_clock(now_day1)
            .read_prices()
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_string(&data.time_block_prices).unwrap(),
            EXPECTED_SPOT_BUY
        );
        assert_eq!(data.block_duration_minutes, 15);
        assert!(data.fallback_block_starts.is_empty());
        assert_eq!(
            data.ha_last_updated,
            Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap()
        );
    }

    const EXPECTED_SPOT_BUY: &str = concat!(
        r#"[{"block_start":"2026-11-02T22:30:00Z","duration_minutes":15,"price_czk_per_kwh":2.0,"effective_price_czk_per_kwh":2.0,"spot_sell_price_czk_per_kwh":1.5},"#,
        r#"{"block_start":"2026-11-02T22:45:00Z","duration_minutes":15,"price_czk_per_kwh":3.0,"effective_price_czk_per_kwh":3.0,"spot_sell_price_czk_per_kwh":2.5},"#,
        r#"{"block_start":"2026-11-02T23:00:00Z","duration_minutes":15,"price_czk_per_kwh":4.0,"effective_price_czk_per_kwh":4.0,"spot_sell_price_czk_per_kwh":3.5}]"#
    );
}
