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

    /// Set `number.<inverter>_backup_discharge_min_soc` to `target` only when it differs (brief
    /// H19). A failed read fails the write, so Back Up Mode never starts with an unknown minimum.
    async fn sync_backup_min_soc(&self, inverter_id: &str, target: u8) -> Result<()> {
        let entity_id = format!(
            "number.{}_backup_discharge_min_soc",
            inverter_id.replace('.', "_")
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
    /// Sensor: number.<prefix>_backup_discharge_min_soc
    pub async fn read_backup_discharge_min_soc(&self, entity_prefix: &str) -> Result<f32> {
        let entity_id = format!(
            "number.{}_backup_discharge_min_soc",
            entity_prefix.replace(".", "_")
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

        // Read all required sensor entities
        let battery_soc = self
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

        info!(
            "✅ [ADAPTER] Inverter state: SOC={:.1}%, Mode={:?}, Grid={:.0}W, Batt={:.0}W, PV={:.0}W, IM={:.2}kW",
            state.battery_soc,
            state.work_mode,
            state.grid_power_w,
            state.battery_power_w,
            state.pv_power_w,
            state.grid_import_today_kwh.unwrap_or_default()
        );

        Ok(state)
    }

    async fn write_command(&self, inverter_id: &str, command: &InverterCommand) -> Result<()> {
        info!(
            "📝 [ADAPTER] Writing command to {}: {:?}",
            inverter_id, command
        );

        // Pre-actuation guard (brief H11): the single choke point for every SolaX write.
        // Unlock before any write, including export limits; the write is skipped on error.
        actuation::ensure_unlocked(&self.client, inverter_id, &self.guard).await?;

        // Back Up Mode: the minimum SOC is synced first, then the mode is written as SetMode
        let mode_command;
        let command = match command {
            InverterCommand::SetBackupMode { min_soc_pct } => {
                self.sync_backup_min_soc(inverter_id, *min_soc_pct).await?;
                mode_command = InverterCommand::SetMode(InverterOperationMode::BackUpMode);
                &mode_command
            }
            other => other,
        };

        match command {
            InverterCommand::SetMode(mode) => {
                if *mode == InverterOperationMode::ForceCharge {
                    actuation::ensure_awake(&self.client, inverter_id, &self.actuation).await;
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
        }
    }

    /// Generate SpotPriceData from fixed hourly prices
    /// Expands 24 hourly prices to 96 15-minute blocks (4 blocks per hour with same price)
    fn generate_fixed_price_data(&self) -> Result<SpotPriceData> {
        use chrono::Utc;
        use fluxion_core::TimeBlockPrice;

        if self.fixed_buy_prices.len() != 24 {
            anyhow::bail!(
                "Fixed buy prices must have exactly 24 hourly values, got {}",
                self.fixed_buy_prices.len()
            );
        }

        let now = Utc::now();
        let today_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .context("Failed to create today start time")?
            .and_local_timezone(chrono::Local)
            .single()
            .context("Ambiguous time")?
            .with_timezone(&Utc);

        let mut time_block_prices = Vec::with_capacity(96);

        // Generate 96 15-minute blocks for today
        for hour in 0..24 {
            let hour_price = self.fixed_buy_prices[hour];

            for quarter in 0..4 {
                let minutes = quarter * 15;
                let block_start = today_start
                    + chrono::Duration::hours(hour as i64)
                    + chrono::Duration::minutes(minutes);

                time_block_prices.push(TimeBlockPrice {
                    block_start,
                    duration_minutes: 15,
                    price_czk_per_kwh: hour_price,
                    // Effective price will be calculated by scheduler with HDO fees
                    effective_price_czk_per_kwh: hour_price,
                    spot_sell_price_czk_per_kwh: None,
                    buy_price_czk_per_kwh: None,
                });
            }
        }

        let spot_data = SpotPriceData {
            time_block_prices,
            block_duration_minutes: 15,
            fetched_at: Utc::now(),
            ha_last_updated: Utc::now(),
        };

        info!(
            "✅ [ConfigurablePrice] Generated fixed price data: {} blocks from {} hourly prices",
            spot_data.time_block_prices.len(),
            self.fixed_buy_prices.len()
        );

        Ok(spot_data)
    }
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
    /// Set the buy price of every block (brief H13, H13b).
    ///
    /// The buy sensor is read at most once per `buy_fetch_interval`; in between, and when a read
    /// fails, the last good buy prices are used. Blocks the sensor never covered get spot scaled by
    /// the median buy/spot ratio, or plain spot when no ratio is known. WARN and INFO only when the
    /// state changes.
    async fn apply_buy_prices(&self, entity_id: &str, data: &mut SpotPriceData) {
        let due = {
            let state = self.buy_state.lock();
            state
                .last_fetch
                .is_none_or(|t| t.elapsed() >= self.buy_fetch_interval)
        };
        if due {
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
                    if let Some(first) = data.time_block_prices.iter().map(|b| b.block_start).min()
                    {
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

        let mut state = self.buy_state.lock();
        let ratio = buy_to_spot_ratio(&data.time_block_prices, &state.last_good);
        let mut missing = 0;
        for block in &mut data.time_block_prices {
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
            let mut data = self.spot_adapter.read_prices().await?;
            if let Some(entity_id) = &self.buy_price_entity {
                self.apply_buy_prices(entity_id, &mut data).await;
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
            let mut data = self.generate_fixed_price_data()?;

            // When selling at spot but buying at fixed price, fetch spot prices separately
            // to populate spot_sell_price_czk_per_kwh for arbitrage strategies
            if self.use_spot_for_sell {
                match self.spot_adapter.read_prices().await {
                    Ok(spot_data) => {
                        // Match spot prices to fixed price blocks by timestamp
                        for block in &mut data.time_block_prices {
                            if let Some(spot_block) = spot_data
                                .time_block_prices
                                .iter()
                                .find(|sb| sb.block_start == block.block_start)
                            {
                                let sell_price =
                                    spot_block.price_czk_per_kwh - self.spot_sell_fee_czk;
                                block.spot_sell_price_czk_per_kwh = Some(sell_price);
                            }
                        }
                        debug!(
                            "💰 [ConfigurablePrice] Populated spot sell prices on fixed-buy blocks (fee: {:.2} CZK/kWh)",
                            self.spot_sell_fee_czk
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "⚠️ [ConfigurablePrice] Failed to fetch spot prices for sell: {e}. Spot sell prices will be unavailable."
                        );
                    }
                }
            }

            Ok(data)
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

    /// Master `solax_80` with its core reads, PV 2000 W, today 51.3 kWh, total 1000 kWh
    async fn master_states(server: &mut mockito::ServerGuard) {
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
