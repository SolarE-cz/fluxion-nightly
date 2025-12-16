// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// A single historical plant data record (typically 5-minute intervals)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalRecord {
    pub timestamp: DateTime<Utc>,
    /// Battery state of charge (0-100%)
    pub battery_soc: f32,
    /// Solar PV generation (Watts)
    pub pv_power_w: f32,
    /// Battery power: positive = discharge, negative = charge (Watts)
    pub battery_power_w: f32,
    /// Grid power: positive = import, negative = export (Watts)
    pub grid_power_w: f32,
    /// House load consumption (Watts)
    pub house_load_w: f32,
}

/// A price data record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceRecord {
    pub timestamp: DateTime<Utc>,
    pub price_czk_per_kwh: f32,
}

/// Request to simulate a day with a specific strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationRequest {
    pub date: NaiveDate,
    pub strategy: StrategyChoice,
    #[serde(default)]
    pub config_overrides: Option<StrategyConfigOverrides>,
}

/// Choice of strategy for simulation
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyChoice {
    /// Show actual historical data (no simulation)
    #[default]
    Actual,
    /// Baseline: simple self-use without optimization
    SelfUse,
    /// Winter Adaptive Strategy
    WinterAdaptive,
}

/// Overrides for strategy configuration parameters
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyConfigOverrides {
    /// Target SOC for daily charging (50-100%)
    pub daily_charging_target_soc: Option<f32>,
    /// SOC threshold for conservation mode (50-100%)
    pub conservation_threshold_soc: Option<f32>,
    /// Number of most expensive blocks to target for discharge (1-24)
    pub top_expensive_blocks: Option<usize>,
    /// Safety multiplier for charge calculations (1.0-2.0)
    pub charge_safety_multiplier: Option<f32>,
}

/// Complete analysis of a single day
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayAnalysis {
    pub date: NaiveDate,
    pub strategy: String,
    /// True if this is actual historical data, false if simulated
    pub is_actual: bool,

    // Energy totals (kWh)
    pub pv_generation_kwh: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    pub battery_charge_kwh: f64,
    pub battery_discharge_kwh: f64,
    pub consumption_kwh: f64,

    // Financial totals (CZK)
    pub grid_import_cost_czk: f64,
    pub grid_export_revenue_czk: f64,
    /// Value of battery discharge = discharge_kwh × price at discharge time
    pub battery_value_czk: f64,
    /// Net cost = import_cost - export_revenue
    pub net_cost_czk: f64,

    /// Time series data for charts
    pub hourly_data: Vec<HourlyDataPoint>,
}

/// A single data point for time series charts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyDataPoint {
    pub timestamp: DateTime<Utc>,
    pub price_czk: f64,
    /// Inverter operation mode: "ForceCharge", "ForceDischarge", "SelfUse", etc.
    pub mode: String,
    pub soc_percent: f64,
    /// Grid import power (Watts, positive)
    pub grid_import_w: f64,
    /// Grid export power (Watts, positive)
    pub grid_export_w: f64,
    pub pv_power_w: f64,
    /// Battery power: positive = discharge, negative = charge
    pub battery_power_w: f64,
    pub house_load_w: f64,
}

/// Available data metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestMetadata {
    /// List of days with available data
    pub available_days: Vec<NaiveDate>,
    /// Available strategies
    pub strategies: Vec<StrategyInfo>,
}

/// Information about an available strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub has_parameters: bool,
}

impl BacktestMetadata {
    #[must_use]
    pub fn strategies() -> Vec<StrategyInfo> {
        vec![
            StrategyInfo {
                id: "actual".to_owned(),
                name: "Actual Data".to_owned(),
                description: "Historical data as recorded".to_owned(),
                has_parameters: false,
            },
            StrategyInfo {
                id: "self_use".to_owned(),
                name: "Self-Use Baseline".to_owned(),
                description: "Simple self-consumption without optimization".to_owned(),
                has_parameters: false,
            },
            StrategyInfo {
                id: "winter_adaptive".to_owned(),
                name: "Winter Adaptive".to_owned(),
                description: "Optimized strategy for winter/low-solar conditions".to_owned(),
                has_parameters: true,
            },
        ]
    }
}
