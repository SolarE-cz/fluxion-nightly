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

use crate::modes::{SolaxChargerUseMode, SolaxManualMode};
use fluxion_core::{EntityChange, InverterOperationMode, ModeChangeRequest, VendorEntityMapper};

/// Solax-specific entity mapper for Home Assistant integration
/// Maps generic FluxION modes to Solax HA entity names and values
pub struct SolaxEntityMapper;

impl SolaxEntityMapper {
    /// Create a new Solax entity mapper
    pub fn new() -> Self {
        Self
    }
}

impl Default for SolaxEntityMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl VendorEntityMapper for SolaxEntityMapper {
    fn vendor_name(&self) -> fluxion_core::InverterType {
        fluxion_core::InverterType::Solax
    }

    fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
        // Map generic mode to Solax charger mode enum, then get discriminant
        let charger_mode = match mode {
            InverterOperationMode::SelfUse => SolaxChargerUseMode::SelfUseMode,
            InverterOperationMode::BackUpMode => SolaxChargerUseMode::BackUpMode,
            // Both ForceCharge and ForceDischarge use ManualMode
            // The manual_mode_select entity differentiates between them
            InverterOperationMode::ForceCharge => SolaxChargerUseMode::ManualMode,
            InverterOperationMode::ForceDischarge => SolaxChargerUseMode::ManualMode,
        };
        charger_mode.to_i32()
    }

    fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
        // Convert numeric value to Solax enum using from_i32, then map to generic mode
        let charger_mode = SolaxChargerUseMode::from_i32(vendor_mode)?;

        match charger_mode {
            SolaxChargerUseMode::SelfUseMode => Some(InverterOperationMode::SelfUse),
            SolaxChargerUseMode::BackUpMode => Some(InverterOperationMode::BackUpMode),
            // ManualMode can be either ForceCharge or ForceDischarge
            // Future: Read manual_mode_select entity to determine actual mode
            // Current behavior: Default to ForceCharge (safe assumption for most cases)
            // Improvement would require reading select.{inverter_id}_manual_mode_select
            // and mapping SolaxManualMode enum to the correct InverterOperationMode
            SolaxChargerUseMode::ManualMode => Some(InverterOperationMode::ForceCharge),
            // Other modes not mapped to generic modes
            SolaxChargerUseMode::FeedinPriority => None,
            SolaxChargerUseMode::PeakShaving => None,
            SolaxChargerUseMode::SmartSchedule => None,
        }
    }

    fn get_work_mode_entity(&self, inverter_id: &str) -> String {
        // Solax uses charger_use_mode as the primary mode selector
        format!("select.{}_charger_use_mode", inverter_id)
    }

    fn get_mode_change_request(
        &self,
        inverter_id: &str,
        mode: InverterOperationMode,
    ) -> ModeChangeRequest {
        // Solax requires changing TWO entities in sequence:
        // 1. charger_use_mode (primary mode)
        // 2. manual_mode_select (force charge/discharge control)

        // Step 1: Map to charger_use_mode
        let charger_mode = match mode {
            InverterOperationMode::SelfUse => SolaxChargerUseMode::SelfUseMode,
            InverterOperationMode::BackUpMode => SolaxChargerUseMode::BackUpMode,
            // Both ForceCharge and ForceDischarge use ManualMode
            InverterOperationMode::ForceCharge => SolaxChargerUseMode::ManualMode,
            InverterOperationMode::ForceDischarge => SolaxChargerUseMode::ManualMode,
        };

        // Step 2: Map to manual_mode_select
        let manual_mode = match mode {
            InverterOperationMode::SelfUse => SolaxManualMode::StopChargeAndDischarge,
            InverterOperationMode::BackUpMode => SolaxManualMode::StopChargeAndDischarge,
            InverterOperationMode::ForceCharge => SolaxManualMode::ForceCharge,
            InverterOperationMode::ForceDischarge => SolaxManualMode::ForceDischarge,
        };

        // Use serde to serialize enums to their string values
        let charger_option = serde_json::to_value(charger_mode)
            .and_then(serde_json::from_value)
            .expect("Failed to serialize charger mode");
        let manual_option = serde_json::to_value(manual_mode)
            .and_then(serde_json::from_value)
            .expect("Failed to serialize manual mode");

        ModeChangeRequest {
            entity_changes: vec![
                EntityChange {
                    entity_id: format!("select.{inverter_id}_charger_use_mode"),
                    option: charger_option,
                },
                EntityChange {
                    entity_id: format!("select.{inverter_id}_manual_mode_select"),
                    option: manual_option,
                },
            ],
        }
    }

    fn get_battery_soc_entity(&self, inverter_id: &str) -> String {
        format!("sensor.{inverter_id}_battery_capacity")
    }

    fn get_grid_power_entity(&self, inverter_id: &str) -> String {
        // Positive = export to grid, Negative = import from grid
        format!("sensor.{}_grid_import", inverter_id)
    }

    fn get_battery_power_entity(&self, inverter_id: &str) -> String {
        // Positive = charging, Negative = discharging
        format!("sensor.{}_battery_power_charge", inverter_id)
    }

    fn get_pv_power_entity(&self, inverter_id: &str) -> String {
        format!("sensor.{}_pv_power_total", inverter_id)
    }

    fn get_export_limit_entity(&self, inverter_id: &str) -> String {
        format!("number.{}_export_control_user_limit", inverter_id)
    }

    // ============= Extended PV (Optional) =============

    fn get_pv1_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_pv1_power", inverter_id))
    }

    fn get_pv2_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_pv2_power", inverter_id))
    }

    fn get_pv3_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_pv3_power", inverter_id))
    }

    fn get_pv4_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_pv4_power", inverter_id))
    }

    // ============= Three-Phase Data (Optional) =============

    fn get_l1_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l1_voltage", inverter_id))
    }

    fn get_l1_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l1_current", inverter_id))
    }

    fn get_l1_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l1_power", inverter_id))
    }

    fn get_l2_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l2_voltage", inverter_id))
    }

    fn get_l2_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l2_current", inverter_id))
    }

    fn get_l2_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l2_power", inverter_id))
    }

    fn get_l3_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l3_voltage", inverter_id))
    }

    fn get_l3_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l3_current", inverter_id))
    }

    fn get_l3_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_phase_l3_power", inverter_id))
    }

    // ============= Battery Extended (Optional) =============

    fn get_battery_soh_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_health", inverter_id))
    }

    fn get_battery_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_voltage", inverter_id))
    }

    fn get_battery_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_current", inverter_id))
    }

    fn get_battery_output_energy_total_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_discharge_total", inverter_id))
    }

    fn get_battery_input_energy_total_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_charge_total", inverter_id))
    }

    fn get_bms_charge_max_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_bms_max_charge_current", inverter_id))
    }

    fn get_bms_discharge_max_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_bms_max_discharge_current", inverter_id))
    }

    // ============= Grid Totals (Optional) =============

    fn get_battery_capacity_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_capacity", inverter_id))
    }

    fn get_battery_input_energy_today_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_input_energy_today", inverter_id))
    }

    fn get_battery_output_energy_today_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!(
            "sensor.{}_battery_output_energy_today",
            inverter_id
        ))
    }

    fn get_house_load_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_house_load", inverter_id))
    }

    // ============= Temperatures (Optional) =============

    fn get_grid_import_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_grid_import", inverter_id))
    }

    fn get_grid_export_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_grid_export", inverter_id))
    }

    fn get_grid_import_today_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_today_s_import_energy", inverter_id))
    }

    fn get_grid_export_today_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_today_s_export_energy", inverter_id))
    }

    // ============= EPS Status (Optional) =============

    fn get_inverter_frequency_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_inverter_frequency", inverter_id))
    }

    fn get_inverter_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_inverter_voltage", inverter_id))
    }

    fn get_inverter_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_inverter_current", inverter_id))
    }

    // ============= Fault/Diagnostic (Optional) =============

    fn get_inverter_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_inverter_power", inverter_id))
    }

    fn get_grid_export_total_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_grid_export_total", inverter_id))
    }

    // ============= Battery Extended (continued) =============

    fn get_grid_import_total_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_grid_import_total", inverter_id))
    }

    fn get_today_solar_energy_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_today_s_solar_energy", inverter_id))
    }

    fn get_total_solar_energy_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_total_solar_energy", inverter_id))
    }

    // ============= Load & Grid Detailed (Optional) =============

    fn get_today_yield_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_today_energy", inverter_id))
    }

    fn get_total_yield_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_total_energy", inverter_id))
    }

    fn get_inverter_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_inverter_temperature", inverter_id))
    }

    fn get_battery_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_battery_temperature", inverter_id))
    }

    fn get_board_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_board_temperature", inverter_id))
    }

    fn get_boost_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_boost_temperature", inverter_id))
    }

    // ============= Inverter Aggregates (Optional) =============

    fn get_eps_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_eps_voltage", inverter_id))
    }

    fn get_eps_current_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_eps_current", inverter_id))
    }

    fn get_eps_power_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_eps_power", inverter_id))
    }

    // ============= Solar Energy (Optional) =============

    fn get_fault_code_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_fault_code", inverter_id))
    }

    fn get_bus_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        Some(format!("sensor.{}_bus_voltage", inverter_id))
    }
}

/// Solax Ultra-specific entity mapper for Home Assistant integration
/// Uses different battery capacity sensor name: battery_total_capacity_charge
pub struct SolaxUltraEntityMapper;

impl SolaxUltraEntityMapper {
    /// Create a new Solax Ultra entity mapper
    pub fn new() -> Self {
        Self
    }
}

impl Default for SolaxUltraEntityMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl VendorEntityMapper for SolaxUltraEntityMapper {
    fn vendor_name(&self) -> fluxion_core::InverterType {
        fluxion_core::InverterType::SolaxUltra
    }

    // Inherit all mode mapping and entity methods from standard Solax
    // Only override the battery SOC entity

    fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
        SolaxEntityMapper::new().map_mode_to_vendor(mode)
    }

    fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
        SolaxEntityMapper::new().map_mode_from_vendor(vendor_mode)
    }

    fn get_work_mode_entity(&self, inverter_id: &str) -> String {
        SolaxEntityMapper::new().get_work_mode_entity(inverter_id)
    }

    fn get_mode_change_request(
        &self,
        inverter_id: &str,
        mode: InverterOperationMode,
    ) -> ModeChangeRequest {
        SolaxEntityMapper::new().get_mode_change_request(inverter_id, mode)
    }

    fn get_battery_soc_entity(&self, inverter_id: &str) -> String {
        // Solax Ultra uses different sensor name
        format!("sensor.{inverter_id}_battery_total_capacity_charge")
    }

    fn get_grid_power_entity(&self, inverter_id: &str) -> String {
        // Solax Ultra: grid_import exists (same as standard Solax)
        format!("sensor.{}_grid_import", inverter_id)
    }

    fn get_battery_power_entity(&self, inverter_id: &str) -> String {
        SolaxEntityMapper::new().get_battery_power_entity(inverter_id)
    }

    fn get_pv_power_entity(&self, inverter_id: &str) -> String {
        SolaxEntityMapper::new().get_pv_power_entity(inverter_id)
    }

    fn get_export_limit_entity(&self, inverter_id: &str) -> String {
        SolaxEntityMapper::new().get_export_limit_entity(inverter_id)
    }

    // All optional entity methods delegate to standard Solax
    fn get_pv1_power_entity(&self, inverter_id: &str) -> Option<String> {
        // Solax Ultra uses same naming
        Some(format!("sensor.{}_pv_power_1", inverter_id))
    }

    fn get_pv2_power_entity(&self, inverter_id: &str) -> Option<String> {
        // Solax Ultra uses same naming
        Some(format!("sensor.{}_pv_power_2", inverter_id))
    }

    fn get_pv3_power_entity(&self, inverter_id: &str) -> Option<String> {
        // Solax Ultra has pv3
        Some(format!("sensor.{}_pv_power_3", inverter_id))
    }

    fn get_pv4_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_pv4_power_entity(inverter_id)
    }

    fn get_l1_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l1_voltage_entity(inverter_id)
    }

    fn get_l1_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l1_current_entity(inverter_id)
    }

    fn get_l1_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l1_power_entity(inverter_id)
    }

    fn get_l2_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l2_voltage_entity(inverter_id)
    }

    fn get_l2_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l2_current_entity(inverter_id)
    }

    fn get_l2_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l2_power_entity(inverter_id)
    }

    fn get_l3_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l3_voltage_entity(inverter_id)
    }

    fn get_l3_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l3_current_entity(inverter_id)
    }

    fn get_l3_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_l3_power_entity(inverter_id)
    }

    fn get_battery_soh_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_soh_entity(inverter_id)
    }

    fn get_battery_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_voltage_entity(inverter_id)
    }

    fn get_battery_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_current_entity(inverter_id)
    }

    fn get_battery_output_energy_total_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_output_energy_total_entity(inverter_id)
    }

    fn get_battery_input_energy_total_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_input_energy_total_entity(inverter_id)
    }

    fn get_bms_charge_max_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_bms_charge_max_current_entity(inverter_id)
    }

    fn get_bms_discharge_max_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_bms_discharge_max_current_entity(inverter_id)
    }

    fn get_battery_capacity_entity(&self, inverter_id: &str) -> Option<String> {
        // Use the same as battery_soc for consistency
        Some(format!(
            "sensor.{inverter_id}_battery_total_capacity_charge"
        ))
    }

    fn get_battery_input_energy_today_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_input_energy_today_entity(inverter_id)
    }

    fn get_battery_output_energy_today_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_output_energy_today_entity(inverter_id)
    }

    fn get_house_load_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_house_load_entity(inverter_id)
    }

    fn get_grid_import_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_grid_import_power_entity(inverter_id)
    }

    fn get_grid_export_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_grid_export_power_entity(inverter_id)
    }

    fn get_grid_import_today_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_grid_import_today_entity(inverter_id)
    }

    fn get_grid_export_today_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_grid_export_today_entity(inverter_id)
    }

    fn get_inverter_frequency_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_inverter_frequency_entity(inverter_id)
    }

    fn get_inverter_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_inverter_voltage_entity(inverter_id)
    }

    fn get_grid_import_total_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_grid_import_total_entity(inverter_id)
    }

    fn get_today_solar_energy_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_today_solar_energy_entity(inverter_id)
    }

    fn get_total_solar_energy_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_total_solar_energy_entity(inverter_id)
    }

    fn get_today_yield_entity(&self, inverter_id: &str) -> Option<String> {
        // Solax Ultra uses today_s_yield instead of today_energy
        Some(format!("sensor.{}_today_s_yield", inverter_id))
    }

    fn get_total_yield_entity(&self, inverter_id: &str) -> Option<String> {
        // Solax Ultra uses total_yield
        Some(format!("sensor.{}_total_yield", inverter_id))
    }

    fn get_inverter_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_inverter_temperature_entity(inverter_id)
    }

    fn get_battery_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_battery_temperature_entity(inverter_id)
    }

    fn get_board_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_board_temperature_entity(inverter_id)
    }

    fn get_boost_temperature_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_boost_temperature_entity(inverter_id)
    }

    fn get_eps_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_eps_voltage_entity(inverter_id)
    }

    fn get_eps_current_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_eps_current_entity(inverter_id)
    }

    fn get_eps_power_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_eps_power_entity(inverter_id)
    }

    fn get_fault_code_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_fault_code_entity(inverter_id)
    }

    fn get_bus_voltage_entity(&self, inverter_id: &str) -> Option<String> {
        SolaxEntityMapper::new().get_bus_voltage_entity(inverter_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_solax_mapper_vendor_name() {
        let mapper = SolaxEntityMapper::new();
        assert_eq!(mapper.vendor_name(), fluxion_core::InverterType::Solax);
    }

    #[test]
    fn test_solax_mode_mapping_to_vendor() {
        let mapper = SolaxEntityMapper::new();

        assert_eq!(mapper.map_mode_to_vendor(InverterOperationMode::SelfUse), 0);
        // Both ForceCharge and ForceDischarge map to ManualMode (3)
        assert_eq!(
            mapper.map_mode_to_vendor(InverterOperationMode::ForceCharge),
            3
        );
        assert_eq!(
            mapper.map_mode_to_vendor(InverterOperationMode::ForceDischarge),
            3
        );
    }

    #[test]
    fn test_solax_mode_mapping_from_vendor() {
        let mapper = SolaxEntityMapper::new();

        assert_eq!(
            mapper.map_mode_from_vendor(0),
            Some(InverterOperationMode::SelfUse)
        );
        // ManualMode (3) defaults to ForceCharge
        // (actual mode determined by manual_mode_select entity)
        assert_eq!(
            mapper.map_mode_from_vendor(3),
            Some(InverterOperationMode::ForceCharge)
        );
        // Other modes not mapped
        assert_eq!(mapper.map_mode_from_vendor(1), None); // FeedinPriority
        assert_eq!(
            mapper.map_mode_from_vendor(2),
            Some(InverterOperationMode::BackUpMode)
        ); // BackUpMode
        assert_eq!(mapper.map_mode_from_vendor(99), None); // Invalid mode
    }

    #[test]
    fn test_solax_mode_mapping_round_trip() {
        let mapper = SolaxEntityMapper::new();

        // SelfUse round-trips correctly
        let vendor_mode = mapper.map_mode_to_vendor(InverterOperationMode::SelfUse);
        assert_eq!(vendor_mode, 0);
        assert_eq!(
            mapper.map_mode_from_vendor(vendor_mode),
            Some(InverterOperationMode::SelfUse)
        );

        // ForceCharge maps to ManualMode (3) and back to ForceCharge
        let vendor_mode = mapper.map_mode_to_vendor(InverterOperationMode::ForceCharge);
        assert_eq!(vendor_mode, 3);
        assert_eq!(
            mapper.map_mode_from_vendor(vendor_mode),
            Some(InverterOperationMode::ForceCharge)
        );

        // ForceDischarge maps to ManualMode (3) but maps back to ForceCharge
        // (the manual_mode_select entity differentiates them)
        let vendor_mode = mapper.map_mode_to_vendor(InverterOperationMode::ForceDischarge);
        assert_eq!(vendor_mode, 3);
        assert_eq!(
            mapper.map_mode_from_vendor(vendor_mode),
            Some(InverterOperationMode::ForceCharge) // Not ForceDischarge!
        );
    }

    #[test]
    fn test_solax_entity_names() {
        let mapper = SolaxEntityMapper::new();

        assert_eq!(
            mapper.get_work_mode_entity("my_solax"),
            "select.my_solax_charger_use_mode"
        );
        assert_eq!(
            mapper.get_battery_soc_entity("my_solax"),
            "sensor.my_solax_battery_capacity"
        );
        assert_eq!(
            mapper.get_grid_power_entity("my_solax"),
            "sensor.my_solax_grid_import"
        );
        assert_eq!(
            mapper.get_battery_power_entity("my_solax"),
            "sensor.my_solax_battery_power_charge"
        );
        assert_eq!(
            mapper.get_pv_power_entity("my_solax"),
            "sensor.my_solax_pv_power_total"
        );
        assert_eq!(
            mapper.get_export_limit_entity("my_solax"),
            "number.my_solax_export_control_user_limit"
        );
    }

    #[test]
    fn test_solax_entity_names_with_different_prefix() {
        let mapper = SolaxEntityMapper::new();

        assert_eq!(
            mapper.get_battery_soc_entity("solax_inverter_1"),
            "sensor.solax_inverter_1_battery_capacity"
        );
    }
}
