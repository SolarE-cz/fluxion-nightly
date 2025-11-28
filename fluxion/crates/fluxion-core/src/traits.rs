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

use crate::components::*;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Supported inverter types in FluxION
/// This enum defines all inverter vendors and models that FluxION can control
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InverterType {
    /// Standard Solax inverters (uses battery_capacity sensor)
    Solax,
    /// Solax Ultra inverters (uses battery_total_capacity_charge sensor)
    SolaxUltra,
    // Future inverter types can be added here:
    // Fronius,
    // Sma,
    // Huawei,
}

impl InverterType {
    /// Get human-readable name for the inverter type
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Solax => "Solax",
            Self::SolaxUltra => "Solax Ultra",
        }
    }

    /// Get config string value (kebab-case)
    pub fn to_config_value(&self) -> &'static str {
        match self {
            Self::Solax => "solax",
            Self::SolaxUltra => "solax-ultra",
        }
    }

    /// List all supported inverter types
    pub fn all() -> &'static [InverterType] {
        &[Self::Solax, Self::SolaxUltra]
    }
}

impl fmt::Display for InverterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl FromStr for InverterType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "solax" => Ok(Self::Solax),
            "solax-ultra" => Ok(Self::SolaxUltra),
            _ => Err(anyhow::anyhow!(
                "Unknown inverter type: '{}'. Supported types: {}",
                s,
                Self::all()
                    .iter()
                    .map(|t| t.to_config_value())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Represents a single entity change in Home Assistant
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityChange {
    /// Entity ID (e.g., "select.solax_charger_use_mode")
    pub entity_id: String,
    /// Option value to set (e.g., "Self Use Mode")
    pub option: String,
}

/// Represents all entity changes needed for a mode change
/// Vendors can return multiple entity changes that should be executed in sequence
#[derive(Debug, Clone, Default)]
pub struct ModeChangeRequest {
    /// List of entity changes to execute in order
    /// For Solax: [charger_use_mode, manual_mode_select]
    /// For simpler vendors: just one entity
    pub entity_changes: Vec<EntityChange>,
}

// ============= FluxION MVP Data Source Traits =============

/// Generic inverter state data (vendor-agnostic)
/// This is what all business logic works with
#[derive(Debug, Clone, Default)]
pub struct GenericInverterState {
    /// Inverter identifier
    pub inverter_id: String,

    // ============= Core Sensors (MVP) =============
    /// Battery state of charge (0-100%)
    pub battery_soc: f32,

    /// Current work mode (generic)
    pub work_mode: InverterOperationMode,

    /// Grid power (positive = export, negative = import)
    pub grid_power_w: f32,

    /// Battery power (positive = charge, negative = discharge)
    pub battery_power_w: f32,

    /// PV generation power (total)
    pub pv_power_w: f32,

    /// Is inverter reachable?
    pub online: bool,

    // ============= Load & Grid Detailed (Optional) =============
    /// House/load consumption (W)
    pub house_load_w: Option<f32>,

    /// Grid import power (W) - separate from grid_power for clarity
    pub grid_import_w: Option<f32>,

    /// Grid export power (W) - separate from grid_power for clarity
    pub grid_export_w: Option<f32>,

    /// Grid import energy today (kWh)
    pub grid_import_today_kwh: Option<f32>,

    /// Grid export energy today (kWh)
    pub grid_export_today_kwh: Option<f32>,

    /// Grid frequency (Hz)
    pub inverter_frequency_hz: Option<f32>,

    // ============= Inverter Aggregates (Optional) =============
    /// Total inverter voltage (V) - aggregate across all phases
    pub inverter_voltage_v: Option<f32>,

    /// Total inverter current (A) - aggregate across all phases
    pub inverter_current_a: Option<f32>,

    /// Total inverter power (W) - aggregate across all phases
    pub inverter_power_w: Option<f32>,

    // ============= Extended PV (Optional) =============
    /// Individual PV string powers (for multi-MPPT systems)
    pub pv1_power_w: Option<f32>,
    pub pv2_power_w: Option<f32>,
    pub pv3_power_w: Option<f32>,
    pub pv4_power_w: Option<f32>,

    // ============= Three-Phase Data (Optional) =============
    /// Phase L1 voltage, current, power
    pub l1_voltage_v: Option<f32>,
    pub l1_current_a: Option<f32>,
    pub l1_power_w: Option<f32>,

    /// Phase L2 voltage, current, power
    pub l2_voltage_v: Option<f32>,
    pub l2_current_a: Option<f32>,
    pub l2_power_w: Option<f32>,

    /// Phase L3 voltage, current, power
    pub l3_voltage_v: Option<f32>,
    pub l3_current_a: Option<f32>,
    pub l3_power_w: Option<f32>,

    // ============= Battery Extended (Optional) =============
    /// Battery state of health (0-100%)
    pub battery_soh_percent: Option<f32>,

    /// Battery voltage
    pub battery_voltage_v: Option<f32>,

    /// Battery current (A)
    pub battery_current_a: Option<f32>,

    /// Battery capacity (kWh)
    pub battery_capacity_kwh: Option<f32>,

    /// Battery input energy today (kWh)
    pub battery_input_energy_today_kwh: Option<f32>,

    /// Battery output energy today (kWh)
    pub battery_output_energy_today_kwh: Option<f32>,

    /// Total battery output energy (kWh)
    pub battery_output_energy_total_kwh: Option<f32>,

    /// Total battery input energy (kWh)
    pub battery_input_energy_total_kwh: Option<f32>,

    /// BMS maximum charge current (A)
    pub bms_charge_max_current_a: Option<f32>,

    /// BMS maximum discharge current (A)
    pub bms_discharge_max_current_a: Option<f32>,

    // ============= Grid Totals (Optional) =============
    /// Total energy exported to grid (kWh)
    pub grid_export_total_kwh: Option<f32>,

    /// Total energy imported from grid (kWh)
    pub grid_import_total_kwh: Option<f32>,

    /// Today's solar energy (kWh)
    pub today_solar_energy_kwh: Option<f32>,

    /// Total solar energy (kWh)
    pub total_solar_energy_kwh: Option<f32>,

    /// Today's solar yield (kWh)
    pub today_yield_kwh: Option<f32>,

    /// Total lifetime solar yield (kWh)
    pub total_yield_kwh: Option<f32>,

    // ============= Temperatures (Optional) =============
    /// Inverter temperature (°C)
    pub inverter_temperature_c: Option<f32>,

    /// Battery temperature (°C)
    pub battery_temperature_c: Option<f32>,

    /// Control board temperature (°C)
    pub board_temperature_c: Option<f32>,

    /// Boost converter temperature (°C)
    pub boost_temperature_c: Option<f32>,

    // ============= EPS Status (Optional) =============
    /// Emergency Power Supply voltage (V)
    pub eps_voltage_v: Option<f32>,

    /// EPS current (A)
    pub eps_current_a: Option<f32>,

    /// EPS power (W)
    pub eps_power_w: Option<f32>,

    // ============= Fault/Diagnostic (Optional) =============
    /// Current fault code
    pub fault_code: Option<u16>,

    /// DC bus voltage (V)
    pub bus_voltage_v: Option<f32>,
}

/// Generic data source for reading inverter state
/// Business logic uses this trait, never knows about HA/Modbus details
#[async_trait]
pub trait InverterDataSource: Send + Sync {
    /// Read current generic inverter state
    async fn read_state(&self, inverter_id: &str) -> Result<GenericInverterState>;

    /// Write command to inverter
    async fn write_command(&self, inverter_id: &str, command: &InverterCommand) -> Result<()>;

    /// Check if data source is available
    async fn health_check(&self) -> Result<bool>;

    /// Get data source name for logging
    fn name(&self) -> &str;
}

/// Generic data source for reading price data
#[async_trait]
pub trait PriceDataSource: Send + Sync {
    /// Read current spot price forecast
    async fn read_prices(&self) -> Result<SpotPriceData>;

    /// Check if price data is available
    async fn health_check(&self) -> Result<bool>;

    /// Get data source name for logging
    fn name(&self) -> &str;
}

/// Vendor-specific entity mapper trait
/// Maps generic modes/commands to vendor-specific HA entities and values
///
/// All methods return Option<String> for entity IDs to indicate if a sensor is supported.
/// Methods return None if the vendor doesn't have that particular sensor.
pub trait VendorEntityMapper: Send + Sync {
    /// Get the inverter type
    fn vendor_name(&self) -> InverterType;

    // ============= Mode Control (Required) =============

    /// Map generic operation mode to vendor-specific work mode value
    /// Returns the numeric value to send to the vendor's work mode entity
    fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32;

    /// Map vendor-specific work mode value to generic operation mode
    fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode>;

    /// Get the entity ID for work mode control (Required)
    /// Example: "select.{inverter_id}_charger_use_mode"
    fn get_work_mode_entity(&self, inverter_id: &str) -> String;

    /// Get all entity changes needed to switch to the specified mode
    ///
    /// This is the main method for mode changes. It returns a list of entity changes
    /// that should be executed in sequence.
    ///
    /// # Examples
    ///
    /// Simple vendor (one entity):
    /// ```ignore
    /// ModeChangeRequest {
    ///     entity_changes: vec![
    ///         EntityChange {
    ///             entity_id: "select.fronius_operating_mode".to_string(),
    ///             option: "Normal".to_string(),
    ///         }
    ///     ]
    /// }
    /// ```
    ///
    /// Complex vendor like Solax (two entities):
    /// ```ignore
    /// ModeChangeRequest {
    ///     entity_changes: vec![
    ///         EntityChange {
    ///             entity_id: "select.solax_charger_use_mode".to_string(),
    ///             option: "Self Use Mode".to_string(),
    ///         },
    ///         EntityChange {
    ///             entity_id: "select.solax_manual_mode_select".to_string(),
    ///             option: "Stop Charge and Discharge".to_string(),
    ///         }
    ///     ]
    /// }
    /// ```
    fn get_mode_change_request(
        &self,
        inverter_id: &str,
        mode: InverterOperationMode,
    ) -> ModeChangeRequest;

    // ============= Core Sensors (MVP - Required) =============

    /// Get the entity ID for battery SOC sensor (Required)
    /// Example: "sensor.{inverter_id}_battery_capacity"
    fn get_battery_soc_entity(&self, _inverter_id: &str) -> String;

    /// Get the entity ID for grid power sensor (Required)
    /// Example: "sensor.{inverter_id}_grid_power"
    fn get_grid_power_entity(&self, _inverter_id: &str) -> String;

    /// Get the entity ID for battery power sensor (Required)
    /// Example: "sensor.{inverter_id}_battery_power"
    fn get_battery_power_entity(&self, _inverter_id: &str) -> String;

    /// Get the entity ID for total PV power sensor (Required)
    /// Example: "sensor.{inverter_id}_pv_power_total"
    fn get_pv_power_entity(&self, inverter_id: &str) -> String;

    /// Get the entity ID for export power limit control (Required)
    /// Example: "number.{inverter_id}_export_control_user_limit"
    fn get_export_limit_entity(&self, inverter_id: &str) -> String;

    // ============= Extended PV (Optional) =============

    /// Get entity ID for individual PV string 1 power
    fn get_pv1_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for individual PV string 2 power
    fn get_pv2_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for individual PV string 3 power
    fn get_pv3_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for individual PV string 4 power
    fn get_pv4_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Three-Phase Data (Optional) =============

    /// Get entity ID for phase L1 voltage
    fn get_l1_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L1 current
    fn get_l1_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L1 power
    fn get_l1_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L2 voltage
    fn get_l2_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L2 current
    fn get_l2_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L2 power
    fn get_l2_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L3 voltage
    fn get_l3_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L3 current
    fn get_l3_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for phase L3 power
    fn get_l3_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Battery Extended (Optional) =============

    /// Get entity ID for battery state of health
    fn get_battery_soh_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for battery voltage
    fn get_battery_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for battery current
    fn get_battery_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total battery output energy
    fn get_battery_output_energy_total_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total battery input energy
    fn get_battery_input_energy_total_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for BMS maximum charge current
    fn get_bms_charge_max_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for BMS maximum discharge current
    fn get_bms_discharge_max_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Battery Extended (continued) =============

    /// Get entity ID for battery capacity
    fn get_battery_capacity_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for battery input energy today
    fn get_battery_input_energy_today_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for battery output energy today
    fn get_battery_output_energy_today_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Load & Grid Detailed (Optional) =============

    /// Get entity ID for house/load consumption
    fn get_house_load_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for grid import power
    fn get_grid_import_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for grid export power
    fn get_grid_export_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for grid import energy today
    fn get_grid_import_today_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for grid export energy today
    fn get_grid_export_today_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for grid/inverter frequency
    fn get_inverter_frequency_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Inverter Aggregates (Optional) =============

    /// Get entity ID for total inverter voltage
    fn get_inverter_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total inverter current
    fn get_inverter_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total inverter power
    fn get_inverter_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Grid Totals (Optional) =============

    /// Get entity ID for total grid export energy
    fn get_grid_export_total_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total grid import energy
    fn get_grid_import_total_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for today's solar energy
    fn get_today_solar_energy_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total solar energy
    fn get_total_solar_energy_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for today's solar yield
    fn get_today_yield_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for total lifetime solar yield
    fn get_total_yield_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Temperatures (Optional) =============

    /// Get entity ID for inverter temperature
    fn get_inverter_temperature_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for battery temperature
    fn get_battery_temperature_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for control board temperature
    fn get_board_temperature_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for boost converter temperature
    fn get_boost_temperature_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= EPS Status (Optional) =============

    /// Get entity ID for EPS voltage
    fn get_eps_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for EPS current
    fn get_eps_current_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for EPS power
    fn get_eps_power_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    // ============= Fault/Diagnostic (Optional) =============

    /// Get entity ID for fault code
    fn get_fault_code_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }

    /// Get entity ID for DC bus voltage
    fn get_bus_voltage_entity(&self, _inverter_id: &str) -> Option<String> {
        None
    }
}

// ============= History Data Traits =============

/// Historical data point with timestamp and value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryDataPoint {
    /// Timestamp of the data point
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Numeric value
    pub value: f32,
}

/// Trait for fetching historical consumption data
#[async_trait]
pub trait ConsumptionHistoryDataSource: Send + Sync {
    /// Fetch history for a specific entity over a time range
    async fn get_history(
        &self,
        entity_id: &str,
        start_time: chrono::DateTime<chrono::Utc>,
        end_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<HistoryDataPoint>>;

    /// Check if data source is available
    async fn health_check(&self) -> Result<bool>;

    /// Get data source name for logging
    fn name(&self) -> &str;
}
