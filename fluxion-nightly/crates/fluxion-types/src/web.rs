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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Battery SOC history point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySocHistoryPoint {
    pub timestamp: DateTime<Utc>,
    pub soc: f32,
}

/// Battery SOC prediction point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySocPredictionPoint {
    pub timestamp: DateTime<Utc>,
    pub soc: f32,
}

/// PV generation history point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PvGenerationHistoryPoint {
    pub timestamp: DateTime<Utc>,
    pub power_w: f32,
}

/// Aggregated consumption statistics used by strategies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionStats {
    /// Historical EMA of daily consumption (kWh/day), if available
    pub ema_kwh: Option<f32>,
    /// Number of days used for EMA calculation
    pub ema_days: usize,
    /// Total grid import today (kWh), if available
    pub today_import_kwh: Option<f32>,
    /// Total grid import yesterday (kWh), if available
    pub yesterday_import_kwh: Option<f32>,
}

/// Response containing ECS component data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebQueryResponse {
    pub timestamp: DateTime<Utc>,
    pub debug_mode: bool,
    pub inverters: Vec<InverterData>,
    pub schedule: Option<super::scheduling::ScheduleData>,
    pub prices: Option<super::pricing::PriceData>,
    pub health: super::health::SystemHealthData,
    pub timezone: Option<String>,
    pub battery_soc_history: Option<Vec<BatterySocHistoryPoint>>,
    pub battery_soc_prediction: Option<Vec<BatterySocPredictionPoint>>,
    pub pv_generation_history: Option<Vec<PvGenerationHistoryPoint>>,
    /// Aggregated consumption statistics (EMA, imports)
    pub consumption_stats: Option<ConsumptionStats>,
}

/// Inverter component data bundle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterData {
    // Core identification
    pub id: String,
    pub topology: String,

    // Current mode (FluxION's internal planned mode)
    pub mode: String,
    pub mode_reason: String,
    // Actual mode reported by the inverter hardware
    pub actual_mode: Option<String>,
    // Whether actual mode matches the planned mode
    pub mode_synced: bool,

    // Battery
    pub battery_soc: f32,
    pub battery_power_w: f32,
    pub battery_voltage_v: f32,
    pub battery_current_a: f32,
    pub battery_temperature_c: f32,

    // Grid
    pub grid_power_w: f32,
    pub grid_voltage_v: f32,
    pub grid_frequency_hz: f32,

    // PV Generation
    pub pv_power_w: f32,
    pub pv1_power_w: f32,
    pub pv2_power_w: f32,
    pub daily_energy_kwh: f32,
    pub total_energy_kwh: f32,

    // Status
    pub online: bool,
    pub run_mode: String,
    pub error_code: u16,
    pub inverter_temperature_c: f32,

    // Extended data from RawInverterState
    pub house_load_w: Option<f32>,
    pub grid_import_w: Option<f32>,
    pub grid_export_w: Option<f32>,
    pub grid_import_today_kwh: Option<f32>,
    pub grid_export_today_kwh: Option<f32>,
    pub inverter_frequency_hz: Option<f32>,
    pub inverter_voltage_v: Option<f32>,
    pub inverter_current_a: Option<f32>,
    pub inverter_power_w: Option<f32>,
    pub battery_capacity_kwh: Option<f32>,
    pub battery_input_energy_today_kwh: Option<f32>,
    pub battery_output_energy_today_kwh: Option<f32>,
    pub today_solar_energy_kwh: Option<f32>,
    pub total_solar_energy_kwh: Option<f32>,
    /// Grid import EMA (historical average consumption per day in kWh)
    pub grid_import_ema_kwh: Option<f32>,
}

// ============= Compact Export Structures for Space Efficiency =============

/// Compact battery SOC history point with Unix timestamp and rounded SOC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBatterySocHistoryPoint {
    #[serde(rename = "ts")]
    pub timestamp_unix: i64,
    #[serde(rename = "soc")]
    pub soc_percent: f32, // Rounded to 1 decimal
}

impl CompactBatterySocHistoryPoint {
    pub fn from_history_point(point: &BatterySocHistoryPoint) -> Self {
        Self {
            timestamp_unix: point.timestamp.timestamp(),
            soc_percent: (point.soc * 10.0).round() / 10.0,
        }
    }
}

/// Compact battery SOC prediction point with Unix timestamp and rounded SOC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBatterySocPredictionPoint {
    #[serde(rename = "ts")]
    pub timestamp_unix: i64,
    #[serde(rename = "soc")]
    pub predicted_soc_percent: f32, // Rounded to 1 decimal
}

impl CompactBatterySocPredictionPoint {
    pub fn from_prediction_point(point: &BatterySocPredictionPoint) -> Self {
        Self {
            timestamp_unix: point.timestamp.timestamp(),
            predicted_soc_percent: (point.soc * 10.0).round() / 10.0,
        }
    }
}

/// Compact PV generation history point with Unix timestamp and rounded power
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPvGenerationHistoryPoint {
    #[serde(rename = "ts")]
    pub timestamp_unix: i64,
    #[serde(rename = "pwr")]
    pub power_watts: f32, // Rounded to nearest watt
}

impl CompactPvGenerationHistoryPoint {
    pub fn from_history_point(point: &PvGenerationHistoryPoint) -> Self {
        Self {
            timestamp_unix: point.timestamp.timestamp(),
            power_watts: point.power_w.round(),
        }
    }
}

/// Compact export response with all optimizations applied
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactWebQueryResponse {
    /// Unix timestamp
    #[serde(rename = "ts")]
    pub timestamp_unix: i64,

    /// Debug mode flag
    #[serde(rename = "dbg")]
    pub debug_mode: bool,

    /// Compact inverter data
    #[serde(rename = "inv")]
    pub inverters: Vec<CompactInverterData>,

    /// Schedule data (using existing ScheduleData - already compact)
    #[serde(rename = "sched", skip_serializing_if = "Option::is_none")]
    pub schedule: Option<super::scheduling::ScheduleData>,

    /// Compact price data
    #[serde(rename = "prices", skip_serializing_if = "Option::is_none")]
    pub prices: Option<CompactPriceData>,

    /// Health data (using existing - small enough)
    #[serde(rename = "health")]
    pub health: super::health::SystemHealthData,

    /// Timezone
    #[serde(rename = "tz", skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,

    /// Compact battery SOC history
    #[serde(rename = "bat_hist", skip_serializing_if = "Option::is_none")]
    pub battery_soc_history: Option<Vec<CompactBatterySocHistoryPoint>>,

    /// Compact battery SOC predictions
    #[serde(rename = "bat_pred", skip_serializing_if = "Option::is_none")]
    pub battery_soc_prediction: Option<Vec<CompactBatterySocPredictionPoint>>,

    /// Compact PV generation history
    #[serde(rename = "pv_hist", skip_serializing_if = "Option::is_none")]
    pub pv_generation_history: Option<Vec<CompactPvGenerationHistoryPoint>>,

    /// Consumption stats (using existing - already compact)
    #[serde(rename = "consumption", skip_serializing_if = "Option::is_none")]
    pub consumption_stats: Option<ConsumptionStats>,
}

/// Compact inverter data with abbreviated field names and rounded values
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactInverterData {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "topo")]
    pub topology: String,

    #[serde(rename = "mode")]
    pub mode: String,

    #[serde(rename = "reason")]
    pub mode_reason: String,

    // Battery (rounded to appropriate precision)
    #[serde(rename = "soc")]
    pub battery_soc: f32, // 1 decimal

    #[serde(rename = "bat_pwr")]
    pub battery_power: f32, // Nearest watt

    #[serde(rename = "bat_v")]
    pub battery_voltage: f32, // 1 decimal

    #[serde(rename = "bat_a")]
    pub battery_current: f32, // 1 decimal

    #[serde(rename = "bat_temp")]
    pub battery_temperature: f32, // Nearest degree

    // Grid (rounded values)
    #[serde(rename = "grid_pwr")]
    pub grid_power: f32, // Nearest watt

    #[serde(rename = "grid_v")]
    pub grid_voltage: f32, // 1 decimal

    #[serde(rename = "grid_hz")]
    pub grid_frequency: f32, // 2 decimals

    // Solar (rounded values)
    #[serde(rename = "pv_pwr")]
    pub pv_power: f32, // Nearest watt

    #[serde(rename = "pv1_pwr")]
    pub pv1_power: f32, // Nearest watt

    #[serde(rename = "pv2_pwr")]
    pub pv2_power: f32, // Nearest watt

    #[serde(rename = "solar_today")]
    pub today_solar_energy: f32, // 1 decimal kWh

    #[serde(rename = "solar_total")]
    pub total_solar_energy: f32, // Nearest kWh

    // Status
    #[serde(rename = "online")]
    pub online: bool,

    #[serde(rename = "run_mode")]
    pub run_mode: u8,

    #[serde(rename = "err_code")]
    pub error_code: u16,

    #[serde(rename = "inv_temp")]
    pub inverter_temperature: f32, // Nearest degree

    // Optional extended data (rounded and abbreviated)
    #[serde(rename = "house", skip_serializing_if = "Option::is_none")]
    pub house_load: Option<f32>, // Nearest watt

    #[serde(rename = "grid_in", skip_serializing_if = "Option::is_none")]
    pub grid_import: Option<f32>, // Nearest watt

    #[serde(rename = "grid_out", skip_serializing_if = "Option::is_none")]
    pub grid_export: Option<f32>, // Nearest watt

    #[serde(rename = "bat_cap", skip_serializing_if = "Option::is_none")]
    pub battery_capacity: Option<f32>, // 1 decimal kWh
}

/// Compact price data with abbreviated field names and compact price blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPriceData {
    #[serde(rename = "cur")]
    pub current_price: f32, // 2 decimals

    #[serde(rename = "min")]
    pub min_price: f32, // 2 decimals

    #[serde(rename = "max")]
    pub max_price: f32, // 2 decimals

    #[serde(rename = "avg")]
    pub avg_price: f32, // 2 decimals

    #[serde(rename = "blocks")]
    pub blocks: Vec<super::pricing::CompactPriceBlockData>,

    // Today's stats (abbreviated)
    #[serde(rename = "today_min")]
    pub today_min_price: f32,

    #[serde(rename = "today_max")]
    pub today_max_price: f32,

    #[serde(rename = "today_avg")]
    pub today_avg_price: f32,

    #[serde(rename = "today_med")]
    pub today_median_price: f32,

    // Tomorrow's stats (optional, abbreviated)
    #[serde(rename = "tom_min", skip_serializing_if = "Option::is_none")]
    pub tomorrow_min_price: Option<f32>,

    #[serde(rename = "tom_max", skip_serializing_if = "Option::is_none")]
    pub tomorrow_max_price: Option<f32>,

    #[serde(rename = "tom_avg", skip_serializing_if = "Option::is_none")]
    pub tomorrow_avg_price: Option<f32>,

    #[serde(rename = "tom_med", skip_serializing_if = "Option::is_none")]
    pub tomorrow_median_price: Option<f32>,
}

impl CompactWebQueryResponse {
    /// Convert from full WebQueryResponse to compact format
    pub fn from_web_query_response(response: &WebQueryResponse) -> Self {
        Self {
            timestamp_unix: response.timestamp.timestamp(),
            debug_mode: response.debug_mode,
            inverters: response
                .inverters
                .iter()
                .map(CompactInverterData::from_inverter_data)
                .collect(),
            schedule: response.schedule.clone(), // Keep as-is, already reasonably compact
            prices: response
                .prices
                .as_ref()
                .map(CompactPriceData::from_price_data),
            health: response.health.clone(),
            timezone: response.timezone.clone(),
            battery_soc_history: response.battery_soc_history.as_ref().map(|hist| {
                hist.iter()
                    .map(CompactBatterySocHistoryPoint::from_history_point)
                    .collect()
            }),
            battery_soc_prediction: response.battery_soc_prediction.as_ref().map(|pred| {
                pred.iter()
                    .map(CompactBatterySocPredictionPoint::from_prediction_point)
                    .collect()
            }),
            pv_generation_history: response.pv_generation_history.as_ref().map(|hist| {
                hist.iter()
                    .map(CompactPvGenerationHistoryPoint::from_history_point)
                    .collect()
            }),
            consumption_stats: response.consumption_stats.clone(),
        }
    }
}

impl CompactInverterData {
    /// Convert from full InverterData to compact format with rounding
    pub fn from_inverter_data(inverter: &InverterData) -> Self {
        Self {
            id: inverter.id.clone(),
            topology: inverter.topology.clone(),
            mode: inverter.mode.clone(),
            mode_reason: inverter.mode_reason.clone(),

            // Rounded battery values
            battery_soc: (inverter.battery_soc * 10.0).round() / 10.0,
            battery_power: inverter.battery_power_w.round(),
            battery_voltage: (inverter.battery_voltage_v * 10.0).round() / 10.0,
            battery_current: (inverter.battery_current_a * 10.0).round() / 10.0,
            battery_temperature: inverter.battery_temperature_c.round(),

            // Rounded grid values
            grid_power: inverter.grid_power_w.round(),
            grid_voltage: (inverter.grid_voltage_v * 10.0).round() / 10.0,
            grid_frequency: (inverter.grid_frequency_hz * 100.0).round() / 100.0,

            // Rounded solar values
            pv_power: inverter.pv_power_w.round(),
            pv1_power: inverter.pv1_power_w.round(),
            pv2_power: inverter.pv2_power_w.round(),
            today_solar_energy: (inverter.daily_energy_kwh * 10.0).round() / 10.0,
            total_solar_energy: inverter.total_energy_kwh.round(),

            // Status (no rounding needed) - run_mode is String, need to parse
            online: inverter.online,
            run_mode: inverter.run_mode.parse::<u8>().unwrap_or(0),
            error_code: inverter.error_code,
            inverter_temperature: inverter.inverter_temperature_c.round(),

            // Optional rounded values
            house_load: inverter.house_load_w.map(|v| v.round()),
            grid_import: inverter.grid_import_w.map(|v| v.round()),
            grid_export: inverter.grid_export_w.map(|v| v.round()),
            battery_capacity: inverter
                .battery_capacity_kwh
                .map(|v| (v * 10.0).round() / 10.0),
        }
    }
}

impl CompactPriceData {
    /// Convert from full PriceData to compact format with rounded values
    pub fn from_price_data(price_data: &super::pricing::PriceData) -> Self {
        Self {
            current_price: (price_data.current_price * 100.0).round() / 100.0,
            min_price: (price_data.min_price * 100.0).round() / 100.0,
            max_price: (price_data.max_price * 100.0).round() / 100.0,
            avg_price: (price_data.avg_price * 100.0).round() / 100.0,
            blocks: price_data
                .blocks
                .iter()
                .map(super::pricing::CompactPriceBlockData::from_price_block_data)
                .collect(),

            // Today's stats
            today_min_price: (price_data.today_min_price * 100.0).round() / 100.0,
            today_max_price: (price_data.today_max_price * 100.0).round() / 100.0,
            today_avg_price: (price_data.today_avg_price * 100.0).round() / 100.0,
            today_median_price: (price_data.today_median_price * 100.0).round() / 100.0,

            // Tomorrow's stats (optional)
            tomorrow_min_price: price_data
                .tomorrow_min_price
                .map(|v| (v * 100.0).round() / 100.0),
            tomorrow_max_price: price_data
                .tomorrow_max_price
                .map(|v| (v * 100.0).round() / 100.0),
            tomorrow_avg_price: price_data
                .tomorrow_avg_price
                .map(|v| (v * 100.0).round() / 100.0),
            tomorrow_median_price: price_data
                .tomorrow_median_price
                .map(|v| (v * 100.0).round() / 100.0),
        }
    }
}
