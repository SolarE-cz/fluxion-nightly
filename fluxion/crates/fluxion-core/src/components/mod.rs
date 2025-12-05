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

pub mod battery_history;
pub mod battery_predictor;
pub mod consumption_history;
pub mod pv_history;

pub use battery_history::{BatteryHistory, BatteryHistoryPoint};
pub use battery_predictor::{
    BatteryPrediction, BatteryPredictionPoint, calculate_soc_change, predict_battery_soc,
};
pub use consumption_history::{
    ConsumptionHistory, ConsumptionHistoryConfig, DailyEnergySummary, aggregate_daily_consumption,
};
pub use pv_history::{PvHistory, PvHistoryPoint};

use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

// Re-export component types from fluxion-types to avoid duplication
pub use fluxion_types::inverter::{
    BatteryStatus, EpsStatus, ExtendedPv, GridPower, Inverter, InverterStatus, PowerGeneration,
    RunMode,
};

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

/// Component storing the raw inverter state from data source
/// This contains all sensor data including Solax-specific extended sensors
#[derive(Component, Debug, Clone)]
pub struct RawInverterState {
    pub state: crate::GenericInverterState,
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

// ============= System Health Components =============

/// Component tracking health status for a data source
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Name of the data source (e.g., "inverter_source", "price_source")
    pub source_name: String,

    /// Whether the source is currently healthy
    pub is_healthy: bool,

    /// Last successful health check
    pub last_check: DateTime<Utc>,

    /// Error messages from recent checks
    pub recent_errors: Vec<String>,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            source_name: String::new(),
            is_healthy: false,
            last_check: Utc::now(),
            recent_errors: Vec::new(),
        }
    }
}

// ============= Pricing Components (Imported from fluxion-types) =============
pub use fluxion_types::pricing::{
    FixedPriceData, PriceAnalysis, PriceRange, SpotPriceData, TimeBlockPrice,
};

// ============= Scheduling Components (Imported from fluxion-types) =============
pub use fluxion_types::inverter::InverterOperationMode;
pub use fluxion_types::scheduling::{CurrentMode, OperationSchedule, ScheduledMode};

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
