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
pub use battery_predictor::{BatteryPrediction, BatteryPredictionPoint, predict_battery_soc};
pub use consumption_history::{
    ConsumptionHistory, ConsumptionHistoryConfig, DailyEnergySummary, aggregate_daily_consumption,
};
pub use pv_history::{PvHistory, PvHistoryPoint};

use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

// ============= Core Inverter Components =============

/// Component identifying an inverter entity
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct Inverter {
    pub id: String,
    pub model: InverterModel,
}

/// Inverter models supported by the system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InverterModel {
    SolaxX3HybridG4,
    SolaxX3UltraG5,
    // Future models can be added here
}

impl InverterModel {
    /// Parse inverter model from InverterType enum
    pub fn from_inverter_type(inverter_type: crate::InverterType) -> Self {
        match inverter_type {
            crate::InverterType::Solax => Self::SolaxX3HybridG4,
            crate::InverterType::SolaxUltra => Self::SolaxX3UltraG5,
            // Future inverter types will map to their specific models:
            // InverterType::Fronius => Self::FroniusSymo,
            // InverterType::Sma => Self::SmaSunnyBoy,
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

// ============= Pricing Components (FluxION MVP) =============

/// Spot price data from HA price integration
/// Uses 15-minute time blocks for granular scheduling
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct SpotPriceData {
    /// Array of time block prices (CZK/kWh)
    /// 15-minute blocks: 96 blocks per day, 96-140 blocks available (24-35 hours)
    pub time_block_prices: Vec<TimeBlockPrice>,

    /// Block duration in minutes (15 for current API)
    pub block_duration_minutes: u32,

    /// Timestamp when this data was fetched from HA
    pub fetched_at: DateTime<Utc>,

    /// HA entity last_updated timestamp
    pub ha_last_updated: DateTime<Utc>,
}

impl Default for SpotPriceData {
    fn default() -> Self {
        Self {
            time_block_prices: Vec::new(),
            block_duration_minutes: 15,
            fetched_at: Utc::now(),
            ha_last_updated: Utc::now(),
        }
    }
}

/// A single time block with price
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBlockPrice {
    /// Start time of this block
    pub block_start: DateTime<Utc>,

    /// Duration of this block (typically 15 minutes)
    pub duration_minutes: u32,

    /// Price for this time block (CZK/kWh)
    pub price_czk_per_kwh: f32,
}

/// Fixed price data when spot prices are disabled
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct FixedPriceData {
    /// Time block prices for buy (96 for 15-minute blocks, or 24 for hourly)
    /// Will be expanded to 96 blocks if only 24 provided
    pub buy_prices: Vec<f32>,

    /// Time block prices for sell (96 for 15-minute blocks, or 24 for hourly)
    /// Will be expanded to 96 blocks if only 24 provided
    pub sell_prices: Vec<f32>,

    /// Block duration in minutes
    pub block_duration_minutes: u32,
}

impl Default for FixedPriceData {
    fn default() -> Self {
        Self {
            buy_prices: vec![0.05; 24], // Default 24 hourly values
            sell_prices: vec![0.08; 24],
            block_duration_minutes: 60, // Hourly by default
        }
    }
}

impl FixedPriceData {
    /// Expand hourly prices (24 values) to 15-minute blocks (96 values)
    pub fn expand_to_15min_blocks(&mut self) {
        if self.buy_prices.len() == 24 {
            self.buy_prices = self
                .buy_prices
                .iter()
                .flat_map(|&price| vec![price; 4]) // Each hour = 4 blocks
                .collect();
        }

        if self.sell_prices.len() == 24 {
            self.sell_prices = self
                .sell_prices
                .iter()
                .flat_map(|&price| vec![price; 4])
                .collect();
        }

        self.block_duration_minutes = 15;
    }
}

/// Result of price analysis - identifies cheap/expensive blocks
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct PriceAnalysis {
    /// Indices of time blocks for force-charging (cheapest)
    pub charge_blocks: Vec<usize>,

    /// Indices of time blocks for force-discharging (most expensive)
    pub discharge_blocks: Vec<usize>,

    /// Price statistics
    pub price_range: PriceRange,

    /// When this analysis was generated
    pub analyzed_at: DateTime<Utc>,
}

impl Default for PriceAnalysis {
    fn default() -> Self {
        Self {
            charge_blocks: Vec::new(),
            discharge_blocks: Vec::new(),
            price_range: PriceRange::default(),
            analyzed_at: Utc::now(),
        }
    }
}

/// Price statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceRange {
    pub min_czk_per_kwh: f32,
    pub max_czk_per_kwh: f32,
    pub avg_czk_per_kwh: f32,
}

impl Default for PriceRange {
    fn default() -> Self {
        Self {
            min_czk_per_kwh: 0.0,
            max_czk_per_kwh: 0.0,
            avg_czk_per_kwh: 0.0,
        }
    }
}

// ============= Scheduling Components (FluxION MVP) =============

/// Generic inverter operation modes (vendor-agnostic)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum InverterOperationMode {
    /// Normal self-use mode: use solar, battery for self-consumption
    #[default]
    SelfUse,
    /// Backup mode: prioritize battery reserve for power outages (Solax-specific)
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
            Self::BackUpMode => write!(f, "Back Up Mode"),
            Self::ForceCharge => write!(f, "Force-Charge"),
            Self::ForceDischarge => write!(f, "Force-Discharge"),
        }
    }
}

/// A scheduled mode for a specific time block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledMode {
    /// Start time of this block
    pub block_start: DateTime<Utc>,

    /// Duration of this block (typically 15 minutes)
    pub duration_minutes: u32,

    /// Target inverter(s) for this command
    /// Empty = all inverters, Some(ids) = specific inverters only
    pub target_inverters: Option<Vec<String>>,

    /// Operation mode for this block
    pub mode: InverterOperationMode,

    /// Human-readable reason for this mode
    pub reason: String,

    /// Debug information (only populated when log_level = debug)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_info: Option<crate::strategy::BlockDebugInfo>,
}

/// Generated operation schedule for an inverter or set of inverters
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct OperationSchedule {
    /// Block-by-block mode assignments (15-minute granularity)
    pub scheduled_blocks: Vec<ScheduledMode>,

    /// When this schedule was generated
    pub generated_at: DateTime<Utc>,

    /// What price data version this schedule is based on
    pub based_on_price_version: DateTime<Utc>,
}

impl Default for OperationSchedule {
    fn default() -> Self {
        Self {
            scheduled_blocks: Vec::new(),
            generated_at: Utc::now(),
            based_on_price_version: Utc::now(),
        }
    }
}

impl OperationSchedule {
    /// Get the scheduled mode for the current time
    pub fn get_current_mode(&self, now: DateTime<Utc>) -> Option<&ScheduledMode> {
        self.scheduled_blocks.iter().find(|block| {
            let block_end =
                block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
            now >= block.block_start && now < block_end
        })
    }

    /// Get the scheduled mode for a specific time
    pub fn get_mode_at(&self, time: DateTime<Utc>) -> Option<&ScheduledMode> {
        self.scheduled_blocks.iter().find(|block| {
            let block_end =
                block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
            time >= block.block_start && time < block_end
        })
    }

    /// Check if schedule needs regeneration based on price data version
    pub fn needs_regeneration(&self, price_data_version: DateTime<Utc>) -> bool {
        self.based_on_price_version != price_data_version
    }
}

/// Current active mode for an inverter
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct CurrentMode {
    /// Current operation mode
    pub mode: InverterOperationMode,

    /// When this mode was set
    pub set_at: DateTime<Utc>,

    /// Why this mode was set
    pub reason: String,
}

impl Default for CurrentMode {
    fn default() -> Self {
        Self {
            mode: InverterOperationMode::SelfUse,
            // Set to far past to avoid debounce blocking initial mode changes
            set_at: Utc::now() - chrono::Duration::hours(24),
            reason: "Initial state".to_string(),
        }
    }
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
