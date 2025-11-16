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
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::client::HomeAssistantClient;
use fluxion_core::{
    GenericInverterState, InverterCommand, InverterDataSource, InverterOperationMode,
    PriceDataSource, SpotPriceData, VendorEntityMapper,
};

/// Home Assistant adapter implementing InverterDataSource
/// Uses VendorEntityMapper to map between generic and vendor-specific entities
pub struct HomeAssistantInverterAdapter {
    client: Arc<HomeAssistantClient>,
    mapper: Arc<dyn VendorEntityMapper>,
}

impl HomeAssistantInverterAdapter {
    /// Create a new HA inverter adapter
    pub fn new(client: Arc<HomeAssistantClient>, mapper: Arc<dyn VendorEntityMapper>) -> Self {
        Self { client, mapper }
    }

    /// Get reference to the underlying HA client (for history queries, etc.)
    pub fn client(&self) -> &Arc<HomeAssistantClient> {
        &self.client
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
                info!(
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
            info!(
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

#[async_trait]
impl InverterDataSource for HomeAssistantInverterAdapter {
    async fn read_state(&self, inverter_id: &str) -> Result<GenericInverterState> {
        info!("🔋 [ADAPTER] Reading inverter state for: {}", inverter_id);

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

        // Read current work mode
        let work_mode_entity = self.mapper.get_work_mode_entity(inverter_id);
        let work_mode = self.read_work_mode(&work_mode_entity).await?;

        // Helper macro to read optional sensors
        macro_rules! read_optional {
            ($method:ident) => {
                if let Some(entity) = self.mapper.$method(inverter_id) {
                    self.read_sensor_float(&entity).await.ok()
                } else {
                    None
                }
            };
        }

        // Read optional Load & Grid detailed sensors
        let house_load_w = read_optional!(get_house_load_entity);
        let grid_import_w = read_optional!(get_grid_import_power_entity);
        let grid_export_w = read_optional!(get_grid_export_power_entity);
        let grid_import_today_kwh = read_optional!(get_grid_import_today_entity);
        let grid_export_today_kwh = read_optional!(get_grid_export_today_entity);
        let inverter_frequency_hz = read_optional!(get_inverter_frequency_entity);

        // Read optional Inverter aggregate sensors
        let inverter_voltage_v = read_optional!(get_inverter_voltage_entity);
        let inverter_current_a = read_optional!(get_inverter_current_entity);
        let inverter_power_w = read_optional!(get_inverter_power_entity);

        // Read optional Battery extended sensors
        let battery_capacity_kwh = read_optional!(get_battery_capacity_entity);
        let battery_input_energy_today_kwh = read_optional!(get_battery_input_energy_today_entity);
        let battery_output_energy_today_kwh =
            read_optional!(get_battery_output_energy_today_entity);

        // Read optional Solar energy sensors
        let today_solar_energy_kwh = read_optional!(get_today_solar_energy_entity);
        let total_solar_energy_kwh = read_optional!(get_total_solar_energy_entity);

        let state = GenericInverterState {
            inverter_id: inverter_id.to_string(),
            battery_soc,
            work_mode,
            grid_power_w: grid_power,
            battery_power_w: battery_power,
            pv_power_w: pv_power,
            online: true,
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
            // All other optional fields default to None
            ..Default::default()
        };

        info!(
            "✅ [ADAPTER] Inverter state: SOC={:.1}%, Mode={:?}, Grid={:.0}W, Batt={:.0}W, PV={:.0}W",
            state.battery_soc,
            state.work_mode,
            state.grid_power_w,
            state.battery_power_w,
            state.pv_power_w
        );

        Ok(state)
    }

    async fn write_command(&self, inverter_id: &str, command: &InverterCommand) -> Result<()> {
        info!(
            "📝 [ADAPTER] Writing command to {}: {:?}",
            inverter_id, command
        );

        match command {
            InverterCommand::SetMode(mode) => {
                // Get all entity changes needed from vendor mapper
                let mode_change = self.mapper.get_mode_change_request(inverter_id, *mode);

                if mode_change.entity_changes.is_empty() {
                    warn!(
                        "No entity changes defined for mode {:?} by {}",
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
            }
            InverterCommand::SetExportLimit(limit_w) => {
                let entity_id = self.mapper.get_export_limit_entity(inverter_id);

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
}

impl CzSpotPriceAdapter {
    /// Create a new Czech spot price adapter
    pub fn new(client: Arc<HomeAssistantClient>, entity_id: impl Into<String>) -> Self {
        Self {
            client,
            entity_id: entity_id.into(),
            tomorrow_entity_id: None,
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
        }
    }
}

#[async_trait]
impl PriceDataSource for CzSpotPriceAdapter {
    async fn read_prices(&self) -> Result<SpotPriceData> {
        info!("💰 [ADAPTER] Reading spot prices from: {}", self.entity_id);

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

        // Parse using existing fluxion_core::pricing::parse_spot_price_response
        let price_data = fluxion_core::parse_spot_price_response(&entity_json)
            .context("Failed to parse spot price data")?;

        info!(
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
        today_state: &crate::types::HaEntityState,
        tomorrow_state: &crate::types::HaEntityState,
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

        debug!(
            "   Merging: {} today blocks + {} tomorrow blocks",
            today_array.len(),
            tomorrow_array.len()
        );

        // Use the existing parse_price_arrays function from fluxion_core
        let merged_json = serde_json::json!({
            "attributes": {
                "today": today_array,
                "tomorrow": tomorrow_array,
            },
            "last_updated": today_state.last_updated,
        });

        let price_data = fluxion_core::parse_spot_price_response(&merged_json)
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

#[cfg(test)]
mod tests {
    use super::*;
    use fluxion_core::InverterOperationMode;

    // Mock mapper for testing
    struct MockMapper;

    impl VendorEntityMapper for MockMapper {
        fn vendor_name(&self) -> &str {
            "Mock"
        }

        fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
            match mode {
                InverterOperationMode::SelfUse => 0,
                InverterOperationMode::BackUpMode => 1,
                InverterOperationMode::ForceCharge => 2,
                InverterOperationMode::ForceDischarge => 3,
            }
        }

        fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
            match vendor_mode {
                0 => Some(InverterOperationMode::SelfUse),
                1 => Some(InverterOperationMode::BackUpMode),
                2 => Some(InverterOperationMode::ForceCharge),
                3 => Some(InverterOperationMode::ForceDischarge),
                _ => None,
            }
        }

        fn get_work_mode_entity(&self, inverter_id: &str) -> String {
            format!("select.{}_work_mode", inverter_id)
        }

        fn get_battery_soc_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{}_battery_soc", inverter_id)
        }

        fn get_grid_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{}_grid_power", inverter_id)
        }

        fn get_battery_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{}_battery_power_charge", inverter_id)
        }

        fn get_pv_power_entity(&self, inverter_id: &str) -> String {
            format!("sensor.{}_pv_power", inverter_id)
        }

        fn get_export_limit_entity(&self, inverter_id: &str) -> String {
            format!("number.{}_export_limit", inverter_id)
        }

        fn get_mode_change_request(
            &self,
            _inverter_id: &str,
            _mode: InverterOperationMode,
        ) -> fluxion_core::ModeChangeRequest {
            fluxion_core::ModeChangeRequest {
                entity_changes: vec![],
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
}
