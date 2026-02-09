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
use chrono_tz::Tz;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::ha::client::HomeAssistantClient;
use fluxion_core::pricing::parse_spot_price_response;
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

        // Read current work mode
        let work_mode_entity = self.mapper.get_work_mode_entity(inverter_id);
        let work_mode = self.read_work_mode(&work_mode_entity).await?;

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

        match command {
            InverterCommand::SetMode(mode) => {
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
    #[expect(dead_code, reason = "Reserved for future sell-back functionality")]
    use_spot_for_sell: bool,
    fixed_buy_prices: Vec<f32>,
    #[expect(dead_code, reason = "Reserved for future sell-back functionality")]
    fixed_sell_prices: Vec<f32>,
}

impl ConfigurablePriceDataSource {
    pub fn new(
        spot_adapter: Arc<CzSpotPriceAdapter>,
        use_spot_for_buy: bool,
        use_spot_for_sell: bool,
        fixed_buy_prices: Vec<f32>,
        fixed_sell_prices: Vec<f32>,
    ) -> Self {
        Self {
            spot_adapter,
            use_spot_for_buy,
            use_spot_for_sell,
            fixed_buy_prices,
            fixed_sell_prices,
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

#[async_trait]
impl PriceDataSource for ConfigurablePriceDataSource {
    async fn read_prices(&self) -> Result<SpotPriceData> {
        // If using spot prices for buying, fetch from spot adapter
        if self.use_spot_for_buy {
            debug!("💰 [ConfigurablePrice] Using spot prices for buy decisions");
            self.spot_adapter.read_prices().await
        } else {
            // Use fixed prices
            debug!(
                "💰 [ConfigurablePrice] Using fixed prices for buy decisions (use_spot_prices_to_buy=false)"
            );
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
        let ha_history = self
            .client
            .get_history(entity_id, start_time, end_time)
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
