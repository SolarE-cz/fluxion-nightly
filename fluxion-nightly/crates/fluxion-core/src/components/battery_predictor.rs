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

use super::{InverterOperationMode, OperationSchedule};
use crate::resources::ControlConfig;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Calculate SOC percentage change from energy in kWh
///
/// This is the single source of truth for battery SOC calculations.
/// Formula: SOC_change = (energy_kwh / battery_capacity_kwh) * 100
///
/// # Arguments
/// * `energy_kwh` - Energy in kWh (positive for charge, negative for discharge)
/// * `battery_capacity_kwh` - Total battery capacity in kWh
///
/// # Returns
/// SOC change in percentage points
#[inline]
pub fn calculate_soc_change(energy_kwh: f32, battery_capacity_kwh: f32) -> f32 {
    if battery_capacity_kwh <= 0.0 {
        return 0.0;
    }
    (energy_kwh / battery_capacity_kwh) * 100.0
}

/// A single predicted battery SOC point
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryPredictionPoint {
    /// Timestamp of this prediction
    pub timestamp: DateTime<Utc>,

    /// Predicted battery state of charge (%)
    pub soc_percent: f32,
}

/// Predicted battery SOC trajectory over time
#[derive(Debug, Clone, Default)]
pub struct BatteryPrediction {
    /// Sequence of predicted SOC points aligned with time blocks
    points: VecDeque<BatteryPredictionPoint>,
}

impl BatteryPrediction {
    /// Create a new empty battery prediction
    pub fn new() -> Self {
        Self {
            points: VecDeque::new(),
        }
    }

    /// Add a prediction point
    pub fn add_point(&mut self, point: BatteryPredictionPoint) {
        self.points.push_back(point);
    }

    /// Get all prediction points
    pub fn points(&self) -> &VecDeque<BatteryPredictionPoint> {
        &self.points
    }

    /// Clear all prediction points
    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Get the number of prediction points
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Check if prediction is empty
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// PV power below this (W) counts as no generation for night detection
const PV_DAYLIGHT_THRESHOLD_W: f32 = 100.0;

/// Per-block energy inputs for the SOC simulation, aligned with `schedule.scheduled_blocks`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PredictionInputs {
    /// Household consumption per block (kWh)
    pub load_kwh_per_block: Vec<f32>,
    /// PV generation per block (kWh)
    pub pv_kwh_per_block: Vec<f32>,
}

/// Local time-of-day window (minutes since midnight) in which PV is treated as zero.
/// When `start_min > end_min` the window wraps midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NightWindow {
    pub start_min: u32,
    pub end_min: u32,
}

impl NightWindow {
    /// Whether a local minute of day falls inside the window
    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start_min <= self.end_min {
            (self.start_min..self.end_min).contains(&minute_of_day)
        } else {
            minute_of_day >= self.start_min || minute_of_day < self.end_min
        }
    }
}

/// Inputs for [`build_prediction_inputs`]
pub struct PredictionSources<'a> {
    /// Reference time: "yesterday" and the "within 1 h of now" fallback
    pub now: DateTime<Utc>,
    /// Timezone for local time of day (night window, yesterday's PV)
    pub timezone: Tz,
    /// 7-day average consumption per UTC hour (kWh per hour), 24 values
    pub hourly_load_profile_kwh: Option<&'a [f32]>,
    /// PV forecast per schedule block (kWh), aligned with `scheduled_blocks`; 0.0 = unknown
    pub solar_block_kwh: Option<&'a [f32]>,
    /// PV history as (timestamp, W), in any order
    pub pv_history_w: &'a [(DateTime<Utc>, f32)],
    /// Current household load (W), used when there is no hourly profile
    pub current_house_load_w: Option<f32>,
    /// Current PV power (W), used only when night is unknown and there is no forecast or history
    pub current_pv_power_w: Option<f32>,
    /// Configured average household load (kW), used when no load is known
    pub average_household_load_kw: f32,
}

fn local_minute_of_day(ts: DateTime<Utc>, tz: Tz) -> u32 {
    let local = ts.with_timezone(&tz);
    local.hour() * 60 + local.minute()
}

fn local_date(ts: DateTime<Utc>, tz: Tz) -> NaiveDate {
    ts.with_timezone(&tz).date_naive()
}

/// Round a minute of day down to the 15-minute block grid
fn floor_to_block(minute_of_day: u32) -> u32 {
    minute_of_day / 15 * 15
}

/// Detect the night window from yesterday's PV history.
///
/// Night starts at the first sample after yesterday's last sample >= 100 W and ends at the first
/// sample >= 100 W after that (the following morning). Both are rounded down to the 15-minute grid,
/// so a PV drop at 17:40 starts night in the 17:30 block. Returns `None` when yesterday has no
/// daylight sample or the morning is not in the history yet.
pub fn detect_night_window(
    pv_history_w: &[(DateTime<Utc>, f32)],
    timezone: Tz,
    now: DateTime<Utc>,
) -> Option<NightWindow> {
    let yesterday = local_date(now, timezone).pred_opt()?;
    let mut samples = pv_history_w.to_vec();
    samples.sort_by_key(|(ts, _)| *ts);

    let last_bright = samples.iter().rposition(|(ts, w)| {
        *w >= PV_DAYLIGHT_THRESHOLD_W && local_date(*ts, timezone) == yesterday
    })?;
    let (evening_ts, _) = samples
        .get(last_bright + 1)
        .copied()
        .unwrap_or(samples[last_bright]);
    let (morning_ts, _) = samples[last_bright + 1..]
        .iter()
        .find(|(_, w)| *w >= PV_DAYLIGHT_THRESHOLD_W)?;

    Some(NightWindow {
        start_min: floor_to_block(local_minute_of_day(evening_ts, timezone)),
        end_min: floor_to_block(local_minute_of_day(*morning_ts, timezone)),
    })
}

/// Yesterday's PV (kWh) at the same local time of day as a block.
///
/// Averages yesterday's samples inside the block's local time window, treating values below
/// 100 W as 0. Returns `None` when yesterday has no sample for that time.
fn yesterday_pv_kwh(
    pv_history_w: &[(DateTime<Utc>, f32)],
    timezone: Tz,
    now: DateTime<Utc>,
    block_start: DateTime<Utc>,
    duration_h: f32,
) -> Option<f32> {
    let yesterday = local_date(now, timezone).pred_opt()?;
    let block_min = local_minute_of_day(block_start, timezone);
    let duration_min = (duration_h * 60.0).round() as u32;
    let values: Vec<f32> = pv_history_w
        .iter()
        .filter(|(ts, _)| local_date(*ts, timezone) == yesterday)
        .filter(|(ts, _)| {
            (block_min..block_min + duration_min).contains(&local_minute_of_day(*ts, timezone))
        })
        .map(|(_, w)| {
            if *w < PV_DAYLIGHT_THRESHOLD_W {
                0.0
            } else {
                *w
            }
        })
        .collect();
    if values.is_empty() {
        return None;
    }
    let avg_w = values.iter().sum::<f32>() / values.len() as f32;
    Some(avg_w / 1000.0 * duration_h)
}

/// Build per-block load and PV energy for the SOC simulation.
///
/// Load per block: hourly profile at the block's UTC hour x duration; without a profile, the
/// current house load, then the configured average.
///
/// PV per block, first match wins:
/// 1. inside the night window (from yesterday's PV) -> 0, whatever the forecast says
/// 2. per-block solar forecast, when it is positive
/// 3. yesterday's PV at the same local time (values < 100 W as 0)
/// 4. night unknown and no yesterday value -> current PV within 1 h of now, else 0
pub fn build_prediction_inputs(
    schedule: &OperationSchedule,
    sources: &PredictionSources,
) -> PredictionInputs {
    let night = detect_night_window(sources.pv_history_w, sources.timezone, sources.now);
    let profile = sources.hourly_load_profile_kwh.filter(|p| p.len() == 24);
    let current_load_kw = sources
        .current_house_load_w
        .map(|w| w / 1000.0)
        .unwrap_or(sources.average_household_load_kw);
    let current_pv_kw = sources
        .current_pv_power_w
        .map(|w| w / 1000.0)
        .unwrap_or(0.0);

    let mut inputs = PredictionInputs::default();
    for (i, block) in schedule.scheduled_blocks.iter().enumerate() {
        let duration_h = block.duration_minutes as f32 / 60.0;

        let load_kwh = match profile {
            Some(p) => p[block.block_start.hour() as usize] * duration_h,
            None => current_load_kw * duration_h,
        };

        let in_night = night
            .map(|w| w.contains(local_minute_of_day(block.block_start, sources.timezone)))
            .unwrap_or(false);
        let forecast_kwh = sources
            .solar_block_kwh
            .and_then(|s| s.get(i).copied())
            .filter(|kwh| *kwh > 0.0);
        let pv_kwh = if in_night {
            0.0
        } else if let Some(kwh) = forecast_kwh {
            kwh
        } else if let Some(kwh) = yesterday_pv_kwh(
            sources.pv_history_w,
            sources.timezone,
            sources.now,
            block.block_start,
            duration_h,
        ) {
            kwh
        } else if night.is_none() && (block.block_start - sources.now).num_minutes().abs() <= 60 {
            current_pv_kw * duration_h
        } else {
            0.0
        };

        inputs.load_kwh_per_block.push(load_kwh);
        inputs.pv_kwh_per_block.push(pv_kwh);
    }

    inputs
}

/// Calculate predicted battery SOC trajectory based on scheduled modes
///
/// Builds per-block inputs from the current values only (no profile, forecast or PV history).
/// See [`build_prediction_inputs`] and [`predict_battery_soc_with_inputs`] for the full model.
///
/// # Arguments
/// * `schedule` - The operation schedule with mode assignments
/// * `control_config` - Battery configuration (capacity, efficiency, etc.)
/// * `current_soc` - Current battery state of charge (%)
/// * `max_charge_rate_kw` - Maximum charging power (kW) - if None, uses 3.5 kW default
/// * `max_discharge_rate_kw` - Maximum discharge power (kW) - if None, uses 3.5 kW default
/// * `current_house_load_w` - Current household power consumption (W) for self-use predictions
/// * `current_pv_power_w` - Current PV generation power (W) for self-use predictions
///
/// # Returns
/// A `BatteryPrediction` with SOC values for each scheduled block
pub fn predict_battery_soc(
    schedule: &OperationSchedule,
    control_config: &ControlConfig,
    current_soc: f32,
    max_charge_rate_kw: Option<f32>,
    max_discharge_rate_kw: Option<f32>,
    current_house_load_w: Option<f32>,
    current_pv_power_w: Option<f32>,
) -> BatteryPrediction {
    let sources = PredictionSources {
        now: Utc::now(),
        timezone: chrono_tz::UTC,
        hourly_load_profile_kwh: None,
        solar_block_kwh: None,
        pv_history_w: &[],
        current_house_load_w,
        current_pv_power_w,
        average_household_load_kw: control_config.average_household_load_kw,
    };
    let inputs = build_prediction_inputs(schedule, &sources);
    predict_battery_soc_with_inputs(
        schedule,
        control_config,
        current_soc,
        max_charge_rate_kw,
        max_discharge_rate_kw,
        &inputs,
    )
}

/// Simulate battery SOC over the schedule using per-block load and PV energy
///
/// # Arguments
/// * `schedule` - The operation schedule with mode assignments
/// * `control_config` - Battery configuration (capacity, efficiency, etc.)
/// * `current_soc` - Current battery state of charge (%)
/// * `max_charge_rate_kw` - Maximum charging power (kW) - if None, uses 3.5 kW default
/// * `max_discharge_rate_kw` - Maximum discharge power (kW) - if None, uses 3.5 kW default
/// * `inputs` - Per-block load and PV energy, aligned with `scheduled_blocks`
///
/// # Returns
/// A `BatteryPrediction` with SOC values for each scheduled block
pub fn predict_battery_soc_with_inputs(
    schedule: &OperationSchedule,
    control_config: &ControlConfig,
    current_soc: f32,
    max_charge_rate_kw: Option<f32>,
    max_discharge_rate_kw: Option<f32>,
    inputs: &PredictionInputs,
) -> BatteryPrediction {
    let mut prediction = BatteryPrediction::new();

    if schedule.scheduled_blocks.is_empty() {
        return prediction;
    }

    debug_assert_eq!(
        inputs.load_kwh_per_block.len(),
        schedule.scheduled_blocks.len()
    );
    debug_assert_eq!(
        inputs.pv_kwh_per_block.len(),
        schedule.scheduled_blocks.len()
    );

    // Default charge/discharge rates for typical residential inverters
    let charge_rate = max_charge_rate_kw.unwrap_or(3.5); // kW
    let discharge_rate = max_discharge_rate_kw.unwrap_or(3.5); // kW

    let battery_capacity = control_config.battery_capacity_kwh;
    let max_soc = control_config.max_battery_soc;
    let hardware_min_soc = control_config.hardware_min_battery_soc;

    // Start prediction from actual current SOC, not clamped to min_soc
    // This allows showing realistic predictions even when battery is below configured minimum
    let mut soc = current_soc.clamp(0.0, max_soc);

    for (i, block) in schedule.scheduled_blocks.iter().enumerate() {
        // Calculate energy transferred in this block (typically 15 minutes = 0.25 hours)
        let duration_hours = block.duration_minutes as f32 / 60.0;

        match block.mode {
            InverterOperationMode::ForceCharge => {
                // Energy charged in this block
                let energy_kwh = charge_rate * duration_hours;
                let soc_increase = calculate_soc_change(energy_kwh, battery_capacity);
                soc = (soc + soc_increase).min(max_soc);
            }
            InverterOperationMode::ForceDischarge => {
                // Energy discharged from battery
                let energy_kwh = discharge_rate * duration_hours;
                let soc_decrease = calculate_soc_change(energy_kwh, battery_capacity);
                // Use hardware minimum SOC enforced by inverter firmware
                soc = (soc - soc_decrease).max(hardware_min_soc);
            }
            InverterOperationMode::NoChargeNoDischarge => {
                // Battery idle - SOC stays constant, grid powers house
            }
            InverterOperationMode::SelfUse
            | InverterOperationMode::BackUpMode
            | InverterOperationMode::FeedinPriority => {
                // In self-use or backup mode, SOC changes depend on solar generation vs household consumption
                // Backup mode behaves the same as self-use for SOC prediction purposes
                // Calculate net energy flow to/from battery for this block
                let load_kwh = inputs.load_kwh_per_block.get(i).copied().unwrap_or(0.0);
                let pv_kwh = inputs.pv_kwh_per_block.get(i).copied().unwrap_or(0.0);

                // Net energy: positive = excess solar (charging), negative = deficit (discharging)
                let net_kwh = pv_kwh - load_kwh;

                if net_kwh > 0.0 {
                    // Excess solar - battery charges from surplus
                    let soc_increase = calculate_soc_change(net_kwh, battery_capacity);
                    soc = (soc + soc_increase).min(max_soc);
                } else if net_kwh < 0.0 {
                    // Power deficit - battery discharges to cover household load
                    let soc_decrease = calculate_soc_change(-net_kwh, battery_capacity);
                    // Use hardware minimum SOC enforced by inverter firmware
                    soc = (soc - soc_decrease).max(hardware_min_soc);
                }
                // If net_kwh == 0.0, SOC remains stable (solar exactly matches load)
            }
        }

        prediction.add_point(BatteryPredictionPoint {
            timestamp: block.block_start,
            soc_percent: soc,
        });
    }

    prediction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ScheduledMode;
    use chrono::Utc;

    fn create_test_config() -> ControlConfig {
        ControlConfig {
            force_charge_hours: 2,
            force_discharge_hours: 2,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            maximum_export_power_w: 0,
            battery_capacity_kwh: 23.0,
            battery_wear_cost_czk_per_kwh: 0.125,
            battery_efficiency: 0.95,
            min_mode_change_interval_secs: 300,
            average_household_load_kw: 0.5,
            hardware_min_battery_soc: 10.0,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            ..Default::default()
        }
    }

    #[test]
    fn test_battery_prediction_empty_schedule() {
        let schedule = OperationSchedule::default();
        let config = create_test_config();

        let prediction = predict_battery_soc(&schedule, &config, 50.0, None, None, None, None);

        assert_eq!(prediction.len(), 0);
    }

    #[test]
    fn test_battery_prediction_force_charge() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::ForceCharge,
                reason: "Test charge".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        let prediction = predict_battery_soc(&schedule, &config, 50.0, Some(3.5), None, None, None);

        assert_eq!(prediction.len(), 1);
        let point = &prediction.points()[0];

        // With 3.5 kW charging for 0.25 hours = 0.875 kWh
        // On 23 kWh battery with 95% efficiency = ~3.6% SOC increase
        assert!(point.soc_percent > 50.0);
        assert!(point.soc_percent < 55.0); // Should be around 53.6%
    }

    #[test]
    fn test_battery_prediction_force_discharge() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::ForceDischarge,
                reason: "Test discharge".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        let prediction = predict_battery_soc(&schedule, &config, 50.0, None, Some(3.5), None, None);

        assert_eq!(prediction.len(), 1);
        let point = &prediction.points()[0];

        // With 3.5 kW discharging for 0.25 hours = 0.875 kWh
        // On 23 kWh battery = ~3.8% SOC decrease
        assert!(point.soc_percent < 50.0);
        assert!(point.soc_percent > 45.0); // Should be around 46.2%
    }

    #[test]
    fn test_battery_prediction_respects_limits() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::ForceCharge,
                reason: "Test".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        let prediction =
            predict_battery_soc(&schedule, &config, 99.0, Some(10.0), None, None, None);

        assert_eq!(prediction.len(), 1);
        let point = &prediction.points()[0];

        // Should not exceed max_soc of 100%
        assert_eq!(point.soc_percent, 100.0);
    }

    #[test]
    fn test_battery_prediction_multiple_blocks() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![
                ScheduledMode {
                    block_start: now,
                    duration_minutes: 15,
                    target_inverters: None,
                    mode: InverterOperationMode::ForceCharge,
                    reason: "Charge".to_string(),
                    decision_uid: None,
                    debug_info: None,
                },
                ScheduledMode {
                    block_start: now + chrono::Duration::minutes(15),
                    duration_minutes: 15,
                    target_inverters: None,
                    mode: InverterOperationMode::ForceCharge,
                    reason: "Charge".to_string(),
                    decision_uid: None,
                    debug_info: None,
                },
                ScheduledMode {
                    block_start: now + chrono::Duration::minutes(30),
                    duration_minutes: 15,
                    target_inverters: None,
                    mode: InverterOperationMode::ForceDischarge,
                    reason: "Discharge".to_string(),
                    decision_uid: None,
                    debug_info: None,
                },
            ],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        let prediction =
            predict_battery_soc(&schedule, &config, 50.0, Some(3.5), Some(3.5), None, None);

        assert_eq!(prediction.len(), 3);

        // SOC should increase in first two blocks
        assert!(prediction.points()[0].soc_percent > 50.0);
        assert!(prediction.points()[1].soc_percent > prediction.points()[0].soc_percent);

        // SOC should decrease in third block
        assert!(prediction.points()[2].soc_percent < prediction.points()[1].soc_percent);
    }

    #[test]
    fn test_battery_prediction_self_use_with_solar() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::SelfUse,
                reason: "Self use".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        // Test with solar covering load (500W PV, 500W load) - SOC should be stable
        let prediction = predict_battery_soc(
            &schedule,
            &config,
            50.0,
            None,
            None,
            Some(500.0),
            Some(500.0),
        );

        assert_eq!(prediction.len(), 1);

        // In self-use mode with balanced solar/load, SOC should remain stable
        assert_eq!(prediction.points()[0].soc_percent, 50.0);
    }

    #[test]
    fn test_battery_prediction_self_use_discharge() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::SelfUse,
                reason: "Self use".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        // Test with no solar and household load (0W PV, 500W load) - SOC should decrease
        let prediction =
            predict_battery_soc(&schedule, &config, 50.0, None, None, Some(500.0), Some(0.0));

        assert_eq!(prediction.len(), 1);

        // In self-use mode without solar, battery discharges to cover load
        // 500W for 0.25h = 0.125 kWh => ~0.54% drop on 23 kWh battery
        assert!(prediction.points()[0].soc_percent < 50.0);
        assert!(prediction.points()[0].soc_percent > 49.0);
    }

    #[test]
    fn test_battery_prediction_self_use_charge() {
        let now = Utc::now();
        let schedule = OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now,
                duration_minutes: 15,
                target_inverters: None,
                mode: InverterOperationMode::SelfUse,
                reason: "Self use".to_string(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        };

        let config = create_test_config();
        // Test with excess solar (1000W PV, 500W load) - SOC should increase
        let prediction = predict_battery_soc(
            &schedule,
            &config,
            50.0,
            None,
            None,
            Some(500.0),
            Some(1000.0),
        );

        assert_eq!(prediction.len(), 1);

        // In self-use mode with excess solar, battery charges from surplus
        // 500W surplus for 0.25h = 0.125 kWh => ~0.52% increase (with 95% efficiency) on 23 kWh battery
        assert!(prediction.points()[0].soc_percent > 50.0);
        assert!(prediction.points()[0].soc_percent < 51.0);
    }

    fn prague() -> Tz {
        chrono_tz::Europe::Prague
    }

    /// UTC instant for a Prague local time (October 2026 is CEST, UTC+2)
    fn prague_at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&prague(), 2026, 10, day, hour, minute, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Yesterday (13 Oct) sunny 06:00-17:35, dark from 17:40. Today (14 Oct) dark until 07:20,
    /// sunny 07:20 onwards. Samples every 5 minutes, 2000 W when sunny.
    fn night_pv_history() -> Vec<(DateTime<Utc>, f32)> {
        let mut history = Vec::new();
        let mut ts = prague_at(13, 0, 0);
        let end = prague_at(14, 15, 0);
        while ts <= end {
            let local = ts.with_timezone(&prague());
            let minute = local.hour() * 60 + local.minute();
            let sunny = if local.date_naive() == NaiveDate::from_ymd_opt(2026, 10, 13).unwrap() {
                (360..=1055).contains(&minute)
            } else {
                minute >= 7 * 60 + 20
            };
            history.push((ts, if sunny { 2000.0 } else { 0.0 }));
            ts += chrono::Duration::minutes(5);
        }
        history
    }

    fn self_use_blocks(start: DateTime<Utc>, count: usize) -> OperationSchedule {
        OperationSchedule {
            scheduled_blocks: (0..count)
                .map(|i| ScheduledMode {
                    block_start: start + chrono::Duration::minutes(15 * i as i64),
                    duration_minutes: 15,
                    target_inverters: None,
                    mode: InverterOperationMode::SelfUse,
                    reason: "Self use".to_string(),
                    decision_uid: None,
                    debug_info: None,
                })
                .collect(),
            generated_at: start,
            based_on_price_version: start,
        }
    }

    #[test]
    fn test_night_window_from_yesterday_pv_history() {
        let history = night_pv_history();
        let window = detect_night_window(&history, prague(), prague_at(14, 15, 0));

        // Last sunny sample 17:35, first dark sample 17:40 -> 17:30 block
        // Morning sunny sample 07:20 -> 07:15 block
        assert_eq!(
            window,
            Some(NightWindow {
                start_min: 17 * 60 + 30,
                end_min: 7 * 60 + 15,
            })
        );
        assert!(window.unwrap().contains(18 * 60));
        assert!(window.unwrap().contains(2 * 60));
        assert!(!window.unwrap().contains(12 * 60));
    }

    #[test]
    fn test_night_window_unknown_without_morning_or_yesterday() {
        let history = night_pv_history();
        // Only yesterday's data: the following morning is not in the history yet
        let before_morning: Vec<_> = history
            .iter()
            .copied()
            .filter(|(ts, _)| *ts < prague_at(14, 6, 0))
            .collect();
        assert_eq!(
            detect_night_window(&before_morning, prague(), prague_at(14, 6, 0)),
            None
        );
        assert_eq!(
            detect_night_window(&[], prague(), prague_at(14, 15, 0)),
            None
        );
    }

    #[test]
    fn test_evening_soc_falls_through_night_without_history() {
        // 15:00 local, current PV 4 kW, house load 1.5 kWh/h at night, 23 kWh battery at 80 %
        let now = prague_at(14, 15, 0);
        let schedule = self_use_blocks(now, 48);
        let config = create_test_config();
        let sources = PredictionSources {
            now,
            timezone: prague(),
            hourly_load_profile_kwh: Some(&[1.5; 24]),
            solar_block_kwh: None,
            pv_history_w: &[],
            current_house_load_w: Some(1500.0),
            current_pv_power_w: Some(4000.0),
            average_household_load_kw: 0.5,
        };
        let inputs = build_prediction_inputs(&schedule, &sources);

        // Within 1 h of now the current PV applies, after that PV is zero
        assert!((inputs.pv_kwh_per_block[0] - 1.0).abs() < 1e-4);
        assert!((inputs.pv_kwh_per_block[4] - 1.0).abs() < 1e-4); // 16:00 block
        assert_eq!(inputs.pv_kwh_per_block[5], 0.0); // 16:15 block onwards

        let prediction =
            predict_battery_soc_with_inputs(&schedule, &config, 80.0, None, None, &inputs);
        let socs: Vec<f32> = prediction.points().iter().map(|p| p.soc_percent).collect();

        // Night blocks: 1.5 kWh/h x 0.25 h = 0.375 kWh = 1.63 % per block until min SOC
        for pair in socs[6..30].windows(2) {
            let drop = pair[0] - pair[1];
            assert!(
                (1.55..=1.70).contains(&drop),
                "drop {drop} between {pair:?}"
            );
        }
        assert!(socs[47] < socs[20], "SOC line must fall through the night");
    }

    #[test]
    fn test_forecast_precedence_and_night_zero() {
        let now = prague_at(14, 10, 0);
        let history = night_pv_history();
        let schedule = self_use_blocks(prague_at(14, 10, 0), 2);
        let mut blocks = schedule.scheduled_blocks.clone();
        blocks.push(ScheduledMode {
            block_start: prague_at(14, 18, 0),
            duration_minutes: 15,
            target_inverters: None,
            mode: InverterOperationMode::SelfUse,
            reason: "Self use".to_string(),
            decision_uid: None,
            debug_info: None,
        });
        let schedule = OperationSchedule {
            scheduled_blocks: blocks,
            ..schedule
        };
        let profile = [1.0_f32; 24];
        let forecast = [0.7_f32, 0.8, 0.5];
        let sources = PredictionSources {
            now,
            timezone: prague(),
            hourly_load_profile_kwh: Some(&profile),
            solar_block_kwh: Some(&forecast),
            pv_history_w: &history,
            current_house_load_w: None,
            current_pv_power_w: None,
            average_household_load_kw: 0.5,
        };
        let inputs = build_prediction_inputs(&schedule, &sources);

        // Daytime: forecast wins over yesterday's 2000 W (0.5 kWh per block)
        assert!((inputs.pv_kwh_per_block[0] - 0.7).abs() < 1e-4);
        assert!((inputs.pv_kwh_per_block[1] - 0.8).abs() < 1e-4);
        // Night block: zero even though the forecast says 0.5 kWh
        assert_eq!(inputs.pv_kwh_per_block[2], 0.0);
        // Load from the profile at the block's UTC hour
        assert!((inputs.load_kwh_per_block[2] - 0.25).abs() < 1e-4);

        // Without a forecast, yesterday's PV at the same local time is used
        let no_forecast = PredictionSources {
            solar_block_kwh: None,
            ..sources
        };
        let inputs = build_prediction_inputs(&schedule, &no_forecast);
        assert!((inputs.pv_kwh_per_block[0] - 0.5).abs() < 1e-4);
        assert_eq!(inputs.pv_kwh_per_block[2], 0.0);
    }
}
