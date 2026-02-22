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
use chrono::{DateTime, Utc};
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

/// Calculate predicted battery SOC trajectory based on scheduled modes
///
/// This function simulates battery behavior by estimating charge/discharge rates
/// and calculating the resulting SOC at each time block.
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
    let mut prediction = BatteryPrediction::new();

    if schedule.scheduled_blocks.is_empty() {
        return prediction;
    }

    // Default charge/discharge rates for typical residential inverters
    let charge_rate = max_charge_rate_kw.unwrap_or(3.5); // kW
    let discharge_rate = max_discharge_rate_kw.unwrap_or(3.5); // kW

    let battery_capacity = control_config.battery_capacity_kwh;
    let max_soc = control_config.max_battery_soc;
    let hardware_min_soc = control_config.hardware_min_battery_soc;

    // Start prediction from actual current SOC, not clamped to min_soc
    // This allows showing realistic predictions even when battery is below configured minimum
    let mut soc = current_soc.clamp(0.0, max_soc);

    for block in &schedule.scheduled_blocks {
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
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                // In self-use or backup mode, SOC changes depend on solar generation vs household consumption
                // Backup mode behaves the same as self-use for SOC prediction purposes
                // Calculate net power flow to/from battery

                // Use actual current values or fallback to configured average
                let house_load_kw = current_house_load_w
                    .map(|w| w / 1000.0)
                    .unwrap_or(control_config.average_household_load_kw);

                let pv_power_kw = current_pv_power_w.map(|w| w / 1000.0).unwrap_or(0.0);

                // Net power: positive = excess solar (charging), negative = deficit (discharging)
                let net_power_kw = pv_power_kw - house_load_kw;

                if net_power_kw > 0.0 {
                    // Excess solar - battery charges from surplus
                    let energy_kwh = net_power_kw * duration_hours;
                    let soc_increase = calculate_soc_change(energy_kwh, battery_capacity);
                    soc = (soc + soc_increase).min(max_soc);
                } else if net_power_kw < 0.0 {
                    // Power deficit - battery discharges to cover household load
                    let energy_kwh = net_power_kw.abs() * duration_hours;
                    let soc_decrease = calculate_soc_change(energy_kwh, battery_capacity);
                    // Use hardware minimum SOC enforced by inverter firmware
                    soc = (soc - soc_decrease).max(hardware_min_soc);
                }
                // If net_power_kw == 0.0, SOC remains stable (solar exactly matches load)
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
}
