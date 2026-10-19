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

//! Async systems module
//!
//! This module contains all async worker tasks and their corresponding ECS systems.
//! Each submodule handles a specific aspect of the async operation:
//! - `price_fetcher`: Price data fetching and schedule generation
//! - `inverter_writer`: Inverter command execution
//! - `health_checker`: Health monitoring for data sources
//! - `history_fetcher`: Consumption history fetching
//! - `state_reader`: Inverter state polling and decomposition
//! - `config_handler`: Configuration update processing

use bevy_ecs::prelude::*;
use tracing::info;

mod config_handler;
mod health_checker;
mod history_fetcher;
mod inverter_writer;
mod price_fetcher;
mod state_reader;

// Re-export public functions and types
pub use config_handler::{ConfigEventParams, config_event_handler, user_control_event_handler};
pub use health_checker::{check_health_system, setup_health_checker};
pub use history_fetcher::{poll_consumption_history_channel, spawn_history_fetcher_worker};
pub use inverter_writer::setup_async_inverter_writer;
pub use price_fetcher::{setup_price_cache, update_prices_system};
pub use state_reader::{
    decompose_inverter_state, read_inverter_states_system, setup_inverter_state_reader,
};

use super::{InverterDataSourceResource, PriceDataSourceResource};
use crate::resources::{ConsumptionHistoryDataSourceResource, SystemConfig};

/// Resource to store the backup discharge minimum SOC read from HA sensor
/// This is read from number.<prefix>_backup_discharge_min_soc
#[derive(Resource, Clone)]
pub struct BackupDischargeMinSoc {
    pub value: f32,
}

impl Default for BackupDischargeMinSoc {
    fn default() -> Self {
        Self { value: 10.0 } // Default fallback
    }
}

/// Channel for receiving backup discharge min SOC updates
#[derive(Component)]
pub struct BackupSocChannel {
    pub receiver: crossbeam_channel::Receiver<f32>,
}

/// Resource to send backup discharge min SOC values from the async worker
#[derive(Resource)]
pub struct BackupSocSender {
    pub sender: crossbeam_channel::Sender<f32>,
}

/// Channel capacity for backup discharge min SOC updates
const BACKUP_SOC_CHANNEL_CAPACITY: usize = 10;

// ============================================================================
// HDO (High/Low Tariff) Schedule Resources
// ============================================================================

/// Cached HDO schedule data from Home Assistant sensor
/// Contains raw JSON data for V3 strategy and parsed periods for chart display
#[derive(Resource, Clone, Default)]
pub struct HdoScheduleData {
    /// Raw JSON/attributes data from HA sensor (for V3 strategy parsing)
    pub raw_data: Option<String>,
    /// Low tariff periods for today: (start_time, end_time) in local time format "HH:MM"
    pub low_tariff_periods: Vec<(String, String)>,
    /// High tariff fee in CZK/kWh (from config)
    pub high_tariff_czk: f32,
    /// Low tariff fee in CZK/kWh (from config)
    pub low_tariff_czk: f32,
    /// Last successful update timestamp
    pub last_updated: Option<chrono::DateTime<chrono::Utc>>,
}

/// Channel for receiving HDO schedule updates from async fetcher
#[derive(Component)]
pub struct HdoChannel {
    pub receiver: crossbeam_channel::Receiver<HdoUpdateMessage>,
}

/// Message sent through HDO channel containing sensor data
#[derive(Debug, Clone)]
pub struct HdoUpdateMessage {
    /// Raw sensor data (JSON attributes)
    pub raw_data: String,
    /// Parsed low tariff periods: (start "HH:MM", end "HH:MM")
    pub low_tariff_periods: Vec<(String, String)>,
}

/// Resource to send HDO schedule values from the async worker
#[derive(Resource)]
pub struct HdoSender {
    pub sender: crossbeam_channel::Sender<HdoUpdateMessage>,
}

/// Channel capacity for HDO schedule updates
const HDO_CHANNEL_CAPACITY: usize = 5;

/// Startup system that spawns all long-running async worker tasks
/// These tasks run in the background and communicate via channels
pub fn setup_async_workers(
    mut commands: Commands,
    price_source: Res<PriceDataSourceResource>,
    inverter_source: Res<InverterDataSourceResource>,
    history_source: Res<ConsumptionHistoryDataSourceResource>,
    config: Res<SystemConfig>,
) {
    use bevy_tasks::AsyncComputeTaskPool;

    // Ensure AsyncComputeTaskPool is initialized
    let pool = AsyncComputeTaskPool::get();
    info!(
        "🚀 Setting up async worker entities (pool threads: {})...",
        pool.thread_num()
    );

    // Note: We don't actually use Bevy's task pool for spawning because our async code
    // uses reqwest/tokio which requires tokio runtime. Instead we rely on tokio runtime
    // being active in the main thread.

    // Note: Price cache setup is now done as a separate Bevy Startup system

    // ============= Inverter Command Writer Setup =============
    setup_async_inverter_writer(&mut commands, &inverter_source);

    // ============= Health Checker Setup =============
    setup_health_checker(&mut commands, &inverter_source, &price_source);

    // ============= Inverter State Reader Setup =============
    setup_inverter_state_reader(&mut commands, &inverter_source);

    // ============= Consumption History Fetcher Worker =============
    spawn_history_fetcher_worker(&mut commands, &history_source, &config.history);

    // ============= Backup Discharge Min SOC Fetcher Worker =============
    // Note: This fetcher requires HaClientResource which is inserted by main.rs
    // The actual worker will be spawned in a separate startup system that has access to HaClientResource
    info!(
        "🔋 Backup discharge min SOC fetcher will be initialized after HaClientResource is available"
    );

    let (backup_soc_tx, backup_soc_rx) = crossbeam_channel::bounded(BACKUP_SOC_CHANNEL_CAPACITY);
    commands.spawn(BackupSocChannel {
        receiver: backup_soc_rx,
    });
    // Store the sender in a resource so the separate startup system can use it
    commands.insert_resource(BackupSocSender {
        sender: backup_soc_tx,
    });

    // ============= HDO Schedule Fetcher Worker =============
    // Note: This fetcher requires HaClientResource and V3 config which are inserted by main.rs
    // The actual worker will be spawned in a separate startup system
    info!("⚡ HDO schedule fetcher will be initialized after HaClientResource is available");

    let (hdo_tx, hdo_rx) = crossbeam_channel::bounded(HDO_CHANNEL_CAPACITY);
    commands.spawn(HdoChannel { receiver: hdo_rx });
    commands.insert_resource(HdoSender { sender: hdo_tx });

    // ============= Solar Forecast Fetcher Worker =============
    // Note: This fetcher requires HaClientResource which is inserted by main.rs
    // The actual worker will be spawned in a separate startup system
    info!("☀️ Solar forecast fetcher will be initialized after HaClientResource is available");

    let (solar_forecast_tx, solar_forecast_rx) = crossbeam_channel::bounded(10);
    commands.spawn(SolarForecastChannel {
        receiver: solar_forecast_rx,
    });
    commands.insert_resource(SolarForecastSender {
        sender: solar_forecast_tx,
    });
    commands.init_resource::<SolarForecastData>();
    // PV forecast correction factor (brief H34): sent by the correction worker, applied to
    // SolarForecastData by the HA plugin's poll system
    let (pv_correction_tx, pv_correction_rx) = crossbeam_channel::bounded(4);
    commands.insert_resource(PvCorrectionChannel {
        sender: pv_correction_tx,
        receiver: pv_correction_rx,
    });

    info!("🎉 All async workers initialized successfully");
}

// ============================================================================
// Solar Forecast Resources
// ============================================================================

use chrono::{DateTime, Utc};
use std::time::Instant;

#[derive(Resource)]
pub struct SolarForecastData {
    /// Total solar production forecast for today (kWh)
    pub total_today_kwh: f32,

    /// Remaining solar production forecast for today (kWh)
    pub remaining_today_kwh: f32,

    /// Solar production forecast for tomorrow (kWh)
    pub tomorrow_kwh: f32,

    /// Per-block PV forecast (UTC block start, kWh) as the provider gave it, uncorrected.
    /// Empty when unavailable.
    pub block_kwh: Vec<(DateTime<Utc>, f32)>,

    /// Last successful update timestamp
    pub last_updated: Instant,

    /// PV forecast correction (brief H34) in force; `None` = no correction (option off or not
    /// computed yet). The totals and `block_kwh` above stay raw; the planner reads them through
    /// the corrected accessors.
    pub correction: Option<crate::pv_correction::CorrectionResult>,
}

impl SolarForecastData {
    /// Factor applied to the PV forecast the planner uses (1.0 = none)
    #[must_use]
    pub fn correction_factor(&self) -> f32 {
        self.correction.map_or(1.0, |c| c.factor)
    }

    /// Corrected total-today forecast (kWh)
    #[must_use]
    pub fn planner_total_today_kwh(&self) -> f32 {
        self.total_today_kwh * self.correction_factor()
    }

    /// Corrected remaining-today forecast (kWh)
    #[must_use]
    pub fn planner_remaining_today_kwh(&self) -> f32 {
        self.remaining_today_kwh * self.correction_factor()
    }

    /// Corrected tomorrow forecast (kWh)
    #[must_use]
    pub fn planner_tomorrow_kwh(&self) -> f32 {
        self.tomorrow_kwh * self.correction_factor()
    }
}

/// PV inputs of the planner: the per-block forecast and the day totals, all corrected
#[derive(Debug, Clone, PartialEq)]
pub struct PlannerSolarInputs {
    pub per_block_kwh: Option<Vec<f32>>,
    pub total_today_kwh: f32,
    pub remaining_today_kwh: f32,
    pub tomorrow_kwh: f32,
}

/// The PV forecast the planner uses for `block_starts`, scaled by the correction in force
#[must_use]
pub fn planner_solar_inputs(
    data: Option<&SolarForecastData>,
    block_starts: &[DateTime<Utc>],
) -> PlannerSolarInputs {
    PlannerSolarInputs {
        per_block_kwh: solar_block_forecast(data, block_starts),
        total_today_kwh: data.map_or(0.0, SolarForecastData::planner_total_today_kwh),
        remaining_today_kwh: data.map_or(0.0, SolarForecastData::planner_remaining_today_kwh),
        tomorrow_kwh: data.map_or(0.0, SolarForecastData::planner_tomorrow_kwh),
    }
}

/// The correction to apply: none when the option is off; an invalid factor becomes 1.0
#[must_use]
pub fn correction_to_apply(
    enabled: bool,
    result: crate::pv_correction::CorrectionResult,
) -> Option<crate::pv_correction::CorrectionResult> {
    enabled.then(|| result.sanitized())
}

/// PV forecast (kWh) for each block start, 0.0 where unknown.
/// Returns `None` when no per-block forecast is available.
pub fn solar_block_forecast(
    data: Option<&SolarForecastData>,
    block_starts: &[DateTime<Utc>],
) -> Option<Vec<f32>> {
    solar_block_forecast_covered(data, block_starts)
        .map(|blocks| blocks.into_iter().map(|kwh| kwh.unwrap_or(0.0)).collect())
}

/// Corrected PV forecast (kWh per 15-min forecast block) for each block start, `None` where no
/// forecast key covers the start. Returns `None` when no per-block forecast is available.
pub fn solar_block_forecast_covered(
    data: Option<&SolarForecastData>,
    block_starts: &[DateTime<Utc>],
) -> Option<Vec<Option<f32>>> {
    let factor = data.map_or(1.0, SolarForecastData::correction_factor);
    solar_block_forecast_raw_covered(data, block_starts).map(|blocks| {
        blocks
            .into_iter()
            .map(|kwh| kwh.map(|k| k * factor))
            .collect()
    })
}

/// Raw (uncorrected) PV forecast per block start, `None` where no forecast key covers it.
/// Step-hold: a key covers the block starts up to (not including) one hour after it, so hourly
/// keys fill all four 15-min blocks of their hour.
pub fn solar_block_forecast_raw_covered(
    data: Option<&SolarForecastData>,
    block_starts: &[DateTime<Utc>],
) -> Option<Vec<Option<f32>>> {
    let data = data?;
    if data.block_kwh.is_empty() {
        return None;
    }
    Some(crate::pv_correction::covered_block_kwh(
        &data.block_kwh,
        block_starts,
    ))
}

impl Default for SolarForecastData {
    fn default() -> Self {
        Self {
            total_today_kwh: 0.0,
            remaining_today_kwh: 0.0,
            tomorrow_kwh: 0.0,
            block_kwh: Vec::new(),
            last_updated: Instant::now(),
            correction: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SolarForecastUpdate {
    pub total_today_kwh: f32,
    pub remaining_today_kwh: f32,
    pub tomorrow_kwh: f32,
    pub block_kwh: Vec<(DateTime<Utc>, f32)>,
}

#[derive(Resource)]
pub struct SolarForecastSender {
    pub sender: crossbeam_channel::Sender<SolarForecastUpdate>,
}

/// Correction factors from the PV forecast correction worker (brief H34)
#[derive(Resource, Clone)]
pub struct PvCorrectionChannel {
    pub sender: crossbeam_channel::Sender<crate::pv_correction::CorrectionResult>,
    pub receiver: crossbeam_channel::Receiver<crate::pv_correction::CorrectionResult>,
}

#[derive(Component)]
pub struct SolarForecastChannel {
    pub receiver: crossbeam_channel::Receiver<SolarForecastUpdate>,
}

#[cfg(test)]
mod solar_block_forecast_tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, h, m, 0).unwrap()
    }

    fn data(block_kwh: Vec<(DateTime<Utc>, f32)>) -> SolarForecastData {
        SolarForecastData {
            block_kwh,
            ..SolarForecastData::default()
        }
    }

    #[test]
    fn test_hourly_keys_fill_all_quarter_hour_blocks() {
        let forecast = data(vec![(utc(10, 0), 0.3), (utc(11, 0), 0.5)]);
        let starts = [
            utc(10, 0),
            utc(10, 15),
            utc(10, 30),
            utc(10, 45),
            utc(11, 0),
            utc(11, 15),
            utc(11, 30),
            utc(11, 45),
            utc(12, 0),
        ];

        let values = solar_block_forecast(Some(&forecast), &starts).unwrap();

        assert_eq!(values, vec![0.3, 0.3, 0.3, 0.3, 0.5, 0.5, 0.5, 0.5, 0.0]);
    }

    #[test]
    fn test_quarter_hour_keys_match_exactly() {
        let forecast = data(vec![(utc(10, 0), 0.1), (utc(10, 15), 0.2)]);

        let values = solar_block_forecast(Some(&forecast), &[utc(10, 0), utc(10, 15)]).unwrap();

        assert_eq!(values, vec![0.1, 0.2]);
    }

    #[test]
    fn test_covered_forecast_marks_uncovered_blocks_none() {
        let forecast = data(vec![(utc(10, 0), 0.1), (utc(10, 15), 0.2)]);
        let starts = [utc(9, 45), utc(10, 0), utc(10, 15), utc(11, 15)];

        let values = solar_block_forecast_covered(Some(&forecast), &starts).unwrap();

        assert_eq!(values, vec![None, Some(0.1), Some(0.2), None]);
    }

    #[test]
    fn test_no_forecast_returns_none() {
        assert!(solar_block_forecast(None, &[utc(10, 0)]).is_none());
        assert!(solar_block_forecast(Some(&data(vec![])), &[utc(10, 0)]).is_none());
    }

    fn corrected(factor: f32) -> crate::pv_correction::CorrectionResult {
        crate::pv_correction::CorrectionResult {
            factor,
            days_used: 7,
            measured_kwh: 117.0,
            forecast_kwh: 100.0,
        }
    }

    #[test]
    fn test_planner_receives_scaled_pv() {
        let mut forecast = data(vec![(utc(10, 0), 0.4), (utc(10, 15), 0.2)]);
        forecast.total_today_kwh = 20.0;
        forecast.remaining_today_kwh = 10.0;
        forecast.tomorrow_kwh = 30.0;
        forecast.correction = Some(corrected(1.17));
        let starts = [utc(10, 0), utc(10, 15), utc(11, 30)];

        let inputs = planner_solar_inputs(Some(&forecast), &starts);
        let blocks = inputs.per_block_kwh.unwrap();
        assert!((blocks[0] - 0.468).abs() < 1e-5, "{blocks:?}");
        assert!((blocks[1] - 0.234).abs() < 1e-5, "{blocks:?}");
        assert_eq!(blocks[2], 0.0);
        assert!((inputs.total_today_kwh - 23.4).abs() < 1e-4);
        assert!((inputs.remaining_today_kwh - 11.7).abs() < 1e-4);
        assert!((inputs.tomorrow_kwh - 35.1).abs() < 1e-4);

        // The raw forecast stays available (dashboard) and unscaled
        let raw = solar_block_forecast_raw_covered(Some(&forecast), &starts).unwrap();
        assert_eq!(raw, vec![Some(0.4), Some(0.2), None]);
        let covered = solar_block_forecast_covered(Some(&forecast), &starts).unwrap();
        assert!((covered[0].unwrap() - 0.468).abs() < 1e-5);
    }

    #[test]
    fn test_option_off_applies_factor_one() {
        let mut forecast = data(vec![(utc(10, 0), 0.4)]);
        forecast.total_today_kwh = 20.0;
        forecast.correction = correction_to_apply(false, corrected(1.17));
        assert_eq!(forecast.correction_factor(), 1.0);
        let inputs = planner_solar_inputs(Some(&forecast), &[utc(10, 0)]);
        assert_eq!(inputs.per_block_kwh, Some(vec![0.4]));
        assert_eq!(inputs.total_today_kwh, 20.0);

        forecast.correction = correction_to_apply(true, corrected(1.17));
        assert!((forecast.correction_factor() - 1.17).abs() < 1e-6);
        // Not computed yet: no correction
        assert_eq!(data(vec![]).correction_factor(), 1.0);
    }
}
