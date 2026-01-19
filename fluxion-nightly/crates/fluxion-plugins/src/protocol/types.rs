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

//! Protocol types for plugin communication.
//! These types are JSON-serializable and language-agnostic.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Price block information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBlock {
    /// Block start time
    pub block_start: DateTime<Utc>,
    /// Block duration in minutes (typically 15)
    pub duration_minutes: u32,
    /// Spot price per kWh in CZK (raw market price without fees)
    pub price_czk_per_kwh: f32,
    /// Effective import price per kWh in CZK (spot price + grid fees from HDO tariff)
    /// This is the final price strategies should use for buy decisions
    pub effective_price_czk_per_kwh: f32,
}

/// Battery state information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// Current state of charge (%)
    pub current_soc_percent: f32,
    /// Total battery capacity (kWh)
    pub capacity_kwh: f32,
    /// Maximum charge rate (kW)
    pub max_charge_rate_kw: f32,
    /// Minimum allowed SOC (%)
    pub min_soc_percent: f32,
    /// Maximum allowed SOC (%)
    pub max_soc_percent: f32,
    /// Round-trip efficiency (0.0 to 1.0)
    pub efficiency: f32,
    /// Wear cost per kWh cycled (CZK/kWh)
    pub wear_cost_czk_per_kwh: f32,
}

/// Forecast data for the evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForecastData {
    /// Expected solar generation for this block (kWh)
    pub solar_kwh: f32,
    /// Expected consumption for this block (kWh)
    pub consumption_kwh: f32,
    /// Grid export price (CZK/kWh)
    pub grid_export_price_czk_per_kwh: f32,
}

/// Historical data for strategy analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalData {
    /// Grid import energy consumed today (kWh)
    pub grid_import_today_kwh: Option<f32>,
    /// Total household consumption today (kWh)
    pub consumption_today_kwh: Option<f32>,
}

/// Complete evaluation request sent to strategy plugins
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRequest {
    /// Current price block being evaluated
    pub block: PriceBlock,
    /// Current battery state
    pub battery: BatteryState,
    /// Forecast for this block
    pub forecast: ForecastData,
    /// All available price blocks for analysis
    pub all_blocks: Vec<PriceBlock>,
    /// Historical data
    pub historical: HistoricalData,
    /// Backup discharge minimum SOC from inverter (%)
    pub backup_discharge_min_soc: f32,
    /// Raw HDO (grid tariff) sensor data for V3 strategy
    /// Contains JSON with low/high tariff periods
    #[serde(default)]
    pub hdo_raw_data: Option<String>,
}

/// Operation mode decision
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum OperationMode {
    /// Normal operation - battery assists household load
    SelfUse,
    /// Force charge from grid
    ForceCharge,
    /// Force discharge to grid
    ForceDischarge,
    /// Backup mode - battery ready for backup
    BackUpMode,
}

impl From<fluxion_types::inverter::InverterOperationMode> for OperationMode {
    fn from(mode: fluxion_types::inverter::InverterOperationMode) -> Self {
        match mode {
            fluxion_types::inverter::InverterOperationMode::SelfUse => Self::SelfUse,
            fluxion_types::inverter::InverterOperationMode::ForceCharge => Self::ForceCharge,
            fluxion_types::inverter::InverterOperationMode::ForceDischarge => Self::ForceDischarge,
            fluxion_types::inverter::InverterOperationMode::BackUpMode => Self::BackUpMode,
        }
    }
}

impl From<OperationMode> for fluxion_types::inverter::InverterOperationMode {
    fn from(mode: OperationMode) -> Self {
        match mode {
            OperationMode::SelfUse => Self::SelfUse,
            OperationMode::ForceCharge => Self::ForceCharge,
            OperationMode::ForceDischarge => Self::ForceDischarge,
            OperationMode::BackUpMode => Self::BackUpMode,
        }
    }
}

/// Decision response from a strategy plugin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDecision {
    /// Block start time (must match request)
    pub block_start: DateTime<Utc>,
    /// Block duration in minutes
    pub duration_minutes: u32,
    /// Recommended operation mode
    pub mode: OperationMode,
    /// Human-readable reason for this decision
    pub reason: String,
    /// Plugin priority (0-100, higher wins in conflicts)
    pub priority: u8,
    /// Name of the strategy/plugin that generated this decision
    #[serde(default)]
    pub strategy_name: Option<String>,
    /// Confidence in this decision (0.0 to 1.0)
    #[serde(default)]
    pub confidence: Option<f32>,
    /// Expected profit from this decision (CZK)
    #[serde(default)]
    pub expected_profit_czk: Option<f32>,
    /// Unique identifier for the decision logic path
    #[serde(default)]
    pub decision_uid: Option<String>,
}

/// Plugin manifest describing a strategy plugin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Unique plugin name
    pub name: String,
    /// Plugin version
    pub version: String,
    /// Description
    pub description: String,
    /// Default priority (0-100)
    pub default_priority: u8,
    /// Whether the plugin is enabled by default
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}
