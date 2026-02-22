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

/// Periodic telemetry snapshot (sent every 5 minutes with heartbeat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub collected_at: DateTime<Utc>,
    pub inverters: Vec<InverterTelemetry>,
    pub instance: InstanceTelemetry,
    #[serde(default)]
    pub schedule: Option<ScheduleTelemetry>,
    #[serde(default)]
    pub soc_predictions: Option<Vec<SocPredictionPoint>>,
}

/// Per-inverter cumulative/status data (no instantaneous power readings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterTelemetry {
    pub id: String,
    // Battery state
    pub battery_soc: f32,
    pub battery_temperature_c: f32,
    pub battery_input_energy_today_kwh: Option<f32>,
    pub battery_output_energy_today_kwh: Option<f32>,
    // Grid cumulative
    pub grid_import_today_kwh: Option<f32>,
    pub grid_export_today_kwh: Option<f32>,
    // Solar cumulative
    pub today_solar_energy_kwh: Option<f32>,
    pub total_solar_energy_kwh: Option<f32>,
    // Status
    pub online: bool,
    pub run_mode: String,
    pub error_code: u16,
    pub inverter_temperature_c: f32,
    pub mode: String,
    pub actual_mode: Option<String>,
    pub mode_synced: bool,
}

/// Instance-level telemetry data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceTelemetry {
    // Schedule
    pub current_mode: String,
    pub current_reason: String,
    pub current_strategy: Option<String>,
    pub expected_profit: Option<f32>,
    pub total_expected_profit: Option<f32>,
    // Health
    pub inverter_source: bool,
    pub price_source: bool,
    pub errors: Vec<String>,
    // Consumption
    pub consumption_ema_kwh: Option<f32>,
    pub today_import_kwh: Option<f32>,
    pub yesterday_import_kwh: Option<f32>,
    // Solar forecast
    pub solar_forecast_total_today_kwh: f32,
    pub solar_forecast_remaining_today_kwh: f32,
    pub solar_forecast_tomorrow_kwh: f32,
    pub solar_forecast_actual_today_kwh: Option<f32>,
    pub solar_forecast_accuracy_percent: Option<f32>,
    // HDO tariff schedule
    pub hdo_low_tariff_periods: Vec<(String, String)>,
    pub hdo_low_tariff_czk: f32,
    pub hdo_high_tariff_czk: f32,
}

/// Schedule block telemetry — captures every strategy decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleBlockTelemetry {
    pub timestamp: DateTime<Utc>,
    pub price_czk: f32,
    pub operation: String,
    pub target_soc: Option<f32>,
    pub strategy: Option<String>,
    pub expected_profit: Option<f32>,
    pub reason: Option<String>,
    pub is_historical: bool,
}

/// Full schedule snapshot included in telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTelemetry {
    pub generated_at: DateTime<Utc>,
    pub total_blocks: usize,
    pub total_expected_profit: Option<f32>,
    pub blocks: Vec<ScheduleBlockTelemetry>,
    // Price statistics
    pub price_min: f32,
    pub price_max: f32,
    pub price_avg: f32,
    pub today_price_min: f32,
    pub today_price_max: f32,
    pub today_price_avg: f32,
    pub today_price_median: f32,
    pub tomorrow_price_min: Option<f32>,
    pub tomorrow_price_max: Option<f32>,
    pub tomorrow_price_avg: Option<f32>,
    pub tomorrow_price_median: Option<f32>,
}

/// SOC prediction point for tracking prediction accuracy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocPredictionPoint {
    pub timestamp: DateTime<Utc>,
    pub predicted_soc: f32,
}

/// One-time sync data (sent at client startup, stored on clients table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSyncData {
    pub battery_capacity_kwh: Option<f32>,
    pub target_soc_max: f32,
    pub target_soc_min: f32,
}
