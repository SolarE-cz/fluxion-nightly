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

use anyhow::Result;
use bevy_ecs::prelude::Component;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

// ============= Inverter Type Enum =============

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

// ============= Core Inverter Components =============

/// Component identifying an inverter entity
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct Inverter {
    pub id: String,
    pub inverter_type: InverterType,
}

// ============= Operation Modes =============

/// Generic inverter operation modes (vendor-agnostic)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum InverterOperationMode {
    /// Normal self-use mode: use solar, battery for self-consumption
    #[default]
    SelfUse,
    /// Battery preservation mode: prevent battery discharge, use grid for house load
    /// PV surplus still charges battery for later use
    /// Implemented as Manual Mode + Stop Charge and Discharge on Solax
    BackUpMode,
    /// Force charge battery from grid (during cheap price blocks)
    ForceCharge,
    /// Force discharge battery to grid (during expensive price blocks)
    ForceDischarge,
}

impl std::fmt::Display for InverterOperationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelfUse => write!(f, "Self-Use"),
            Self::BackUpMode => write!(f, "Battery Preservation"),
            Self::ForceCharge => write!(f, "Force-Charge"),
            Self::ForceDischarge => write!(f, "Force-Discharge"),
        }
    }
}

// ============= Core Power Components =============

/// Component storing power generation data
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowerGeneration {
    pub current_power_w: u16,
    pub daily_energy_kwh: f32,
    pub total_energy_kwh: f32,
    pub pv1_power_w: u16,
    pub pv2_power_w: u16,
}

/// Component storing grid interaction data
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct GridPower {
    pub export_power_w: i32,
    pub grid_frequency_hz: f32,
    pub grid_voltage_v: f32,
}

/// Component storing battery data
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatteryStatus {
    pub soc_percent: u16,
    pub voltage_v: f32,
    pub current_a: f32,
    pub power_w: i32,
    pub temperature_c: f32,
    pub cycles: u16,
}

/// Component storing inverter status
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct InverterStatus {
    pub run_mode: RunMode,
    pub error_code: u16,
    pub temperature_c: f32,
    pub last_update: Option<DateTime<Utc>>,
    pub connection_healthy: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum RunMode {
    #[default]
    Offline,
    Normal,
    Fault,
    Standby,
    Charging,
    Discharging,
}

// ============= Extended Components =============

/// Component storing extended PV data (MPPT 3-4)
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtendedPv {
    pub pv3_voltage_v: f32,
    pub pv3_current_a: f32,
    pub pv3_power_w: u16,
    pub pv4_voltage_v: f32,
    pub pv4_current_a: f32,
    pub pv4_power_w: u16,
}

/// Component storing EPS (Emergency Power Supply) data
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpsStatus {
    pub voltage_v: f32,
    pub current_a: f32,
    pub power_w: i16,
    pub frequency_hz: f32,
}

/// Component storing extended battery data from BMS
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatteryExtended {
    pub output_energy_total_kwh: f32,
    pub output_energy_today_kwh: f32,
    pub input_energy_total_kwh: f32,
    pub input_energy_today_kwh: f32,
    pub pack_number: u16,
    pub state_of_health_percent: u16,
    pub bms_charge_max_current_a: f32,
    pub bms_discharge_max_current_a: f32,
    pub bms_capacity_ah: u16,
    pub board_temperature_c: f32,
    pub boost_temperature_c: f32,
}

/// Component storing grid import/export totals
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct GridTotals {
    pub export_total_kwh: f32,
    pub import_total_kwh: f32,
    pub today_yield_kwh: f32,
    pub total_yield_kwh: f32,
}

/// Component storing three-phase data
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreePhase {
    pub l1_voltage_v: f32,
    pub l1_current_a: f32,
    pub l1_power_w: i16,
    pub l1_frequency_hz: f32,
    pub l2_voltage_v: f32,
    pub l2_current_a: f32,
    pub l2_power_w: i16,
    pub l2_frequency_hz: f32,
    pub l3_voltage_v: f32,
    pub l3_current_a: f32,
    pub l3_power_w: i16,
    pub l3_frequency_hz: f32,
}

/// Component storing temperature sensors
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Temperatures {
    pub inverter_c: f32,
    pub battery_c: f32,
    pub board_c: f32,
    pub boost_c: f32,
}

/// Component storing inverter work mode
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct InverterWorkMode {
    pub work_mode: u16,
}

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

/// Component storing the raw inverter state from data source
/// This contains all sensor data including Solax-specific extended sensors
#[derive(Component, Debug, Clone)]
pub struct RawInverterState {
    pub state: GenericInverterState,
    pub last_updated: DateTime<Utc>,
}

// ============= Quality and Metadata =============

/// Component for tracking data quality
#[derive(Component, Debug, Clone, Default)]
pub struct DataQuality {
    pub consecutive_failures: u32,
    pub total_reads: u64,
    pub failed_reads: u64,
    pub last_success: Option<DateTime<Utc>>,
}

/// Component marking an entity that needs register reads
#[derive(Component)]
pub struct RegisterReadSchedule {
    pub interval: Duration,
    pub last_read: Option<Instant>,
}

/// Pending command to execute on an inverter
#[derive(Component, Debug, Clone)]
pub struct PendingCommand {
    /// Target inverter ID
    pub target_inverter: String,

    /// Command to execute
    pub command: InverterCommand,

    /// When this command was created
    pub created_at: Instant,

    /// Number of retry attempts
    pub retry_count: u32,
}

/// Commands that can be sent to an inverter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InverterCommand {
    /// Set operation mode
    SetMode(InverterOperationMode),

    /// Set export power limit (watts)
    SetExportLimit(u32),
}
