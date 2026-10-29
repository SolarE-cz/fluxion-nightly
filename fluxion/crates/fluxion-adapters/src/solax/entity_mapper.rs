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

use crate::control_prefix::ControlPrefixes;
use crate::solax::modes::{SolaxChargerUseMode, SolaxManualMode};
use fluxion_core::{EntityChange, InverterOperationMode, ModeChangeRequest, VendorEntityMapper};

/// Solax-specific entity mapper for Home Assistant integration
/// Maps generic FluxION modes to Solax HA entity names and values
#[derive(Debug, Clone, Default)]
pub struct SolaxEntityMapper {
    /// Control prefix per inverter (brief H36); empty = controls use the sensors' prefix
    control: ControlPrefixes,
}

impl SolaxEntityMapper {
    /// Create a new Solax entity mapper
    pub fn new() -> Self {
        Self::default()
    }

    /// Mapper whose control entities (`select.*`, `number.*`) use `control` (brief H36)
    #[must_use]
    pub fn with_control_prefixes(control: ControlPrefixes) -> Self {
        Self { control }
    }
}

impl VendorEntityMapper for SolaxEntityMapper {
    fn vendor_name(&self) -> fluxion_core::InverterType {
        fluxion_core::InverterType::Solax
    }

    fn control_prefix<'a>(&'a self, inverter_id: &'a str) -> &'a str {
        self.control.resolve(inverter_id)
    }

    fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
        // Map generic mode to Solax charger mode enum, then get discriminant
        let charger_mode = match mode {
            InverterOperationMode::SelfUse => SolaxChargerUseMode::SelfUseMode,
            // BackUpMode maps to native Solax Back Up Mode (value 2)
            // The inverter will charge the battery and hold it ready for blackouts
            InverterOperationMode::BackUpMode => SolaxChargerUseMode::BackUpMode,
            // NoChargeNoDischarge uses Manual Mode + Stop Charge and Discharge
            // This prevents battery discharge and allows PV surplus to charge battery
            InverterOperationMode::NoChargeNoDischarge => SolaxChargerUseMode::ManualMode,
            // Both ForceCharge and ForceDischarge use ManualMode
            // The manual_mode_select entity differentiates between them
            InverterOperationMode::ForceCharge => SolaxChargerUseMode::ManualMode,
            InverterOperationMode::ForceDischarge => SolaxChargerUseMode::ManualMode,
            // Feed-in priority is a charger_use_mode of its own, no manual sub-mode
            InverterOperationMode::FeedinPriority => SolaxChargerUseMode::FeedinPriority,
        };
        charger_mode.to_i32()
    }

    fn get_manual_mode_entity(&self, inverter_id: &str) -> Option<String> {
        let prefix = self.control_prefix(inverter_id);
        Some(format!("select.{prefix}_manual_mode_select"))
    }

    fn map_manual_sub_mode(&self, option: &str) -> Option<InverterOperationMode> {
        // Same option strings as the writes (SolaxManualMode serde names)
        let manual: SolaxManualMode =
            serde_json::from_value(serde_json::Value::String(option.to_string())).ok()?;
        Some(match manual {
            SolaxManualMode::StopChargeAndDischarge => InverterOperationMode::NoChargeNoDischarge,
            SolaxManualMode::ForceCharge => InverterOperationMode::ForceCharge,
            SolaxManualMode::ForceDischarge => InverterOperationMode::ForceDischarge,
        })
    }

    fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
        // Convert numeric value to Solax enum using from_i32, then map to generic mode
        let charger_mode = SolaxChargerUseMode::from_i32(vendor_mode)?;

        match charger_mode {
            SolaxChargerUseMode::SelfUseMode => Some(InverterOperationMode::SelfUse),
            // Read-back: hardware reporting Feedin Priority is recognised on initial sync
            SolaxChargerUseMode::FeedinPriority => Some(InverterOperationMode::FeedinPriority),
            // Native Solax BackUpMode (value 2) maps directly to generic BackUpMode
            SolaxChargerUseMode::BackUpMode => Some(InverterOperationMode::BackUpMode),
            // ManualMode can be NoChargeNoDischarge (stop charge/discharge), ForceCharge or ForceDischarge.
            // The manual_mode_select entity tells them apart (see map_manual_sub_mode); without it
            // ForceCharge is the ambiguous default.
            SolaxChargerUseMode::ManualMode => Some(InverterOperationMode::ForceCharge),
            // Other modes not mapped to generic modes
            SolaxChargerUseMode::PeakShaving => None,
            SolaxChargerUseMode::SmartSchedule => None,
        }
    }

    fn get_work_mode_entity(&self, inverter_id: &str) -> String {
        // Solax uses charger_use_mode as the primary mode selector
        format!(
            "select.{}_charger_use_mode",
            self.control_prefix(inverter_id)
        )
    }

    fn get_mode_change_request(
        &self,
        inverter_id: &str,
        mode: InverterOperationMode,
    ) -> ModeChangeRequest {
        // Solax mode changes may require one or two entity changes:
        // 1. charger_use_mode (primary mode) - always required
        // 2. manual_mode_select (force charge/discharge control) - only for ManualMode

        // Step 1: Map to charger_use_mode
        let charger_mode = match mode {
            InverterOperationMode::SelfUse => SolaxChargerUseMode::SelfUseMode,
            // BackUpMode maps to native Solax Back Up Mode (value 2)
            InverterOperationMode::BackUpMode => SolaxChargerUseMode::BackUpMode,
            // NoChargeNoDischarge uses Manual Mode + Stop Charge and Discharge
            InverterOperationMode::NoChargeNoDischarge => SolaxChargerUseMode::ManualMode,
            // Both ForceCharge and ForceDischarge use ManualMode
            InverterOperationMode::ForceCharge => SolaxChargerUseMode::ManualMode,
            InverterOperationMode::ForceDischarge => SolaxChargerUseMode::ManualMode,
            InverterOperationMode::FeedinPriority => SolaxChargerUseMode::FeedinPriority,
        };

        // Step 2: Map to manual_mode_select (only needed for ManualMode-based modes)
        let manual_mode = match mode {
            // NoChargeNoDischarge: Stop Charge and Discharge = don't discharge battery, use grid
            // PV surplus will still charge battery for later use
            InverterOperationMode::NoChargeNoDischarge => {
                Some(SolaxManualMode::StopChargeAndDischarge)
            }
            InverterOperationMode::ForceCharge => Some(SolaxManualMode::ForceCharge),
            InverterOperationMode::ForceDischarge => Some(SolaxManualMode::ForceDischarge),
            // SelfUse, BackUpMode and FeedinPriority don't use ManualMode, no manual_mode_select needed
            InverterOperationMode::SelfUse
            | InverterOperationMode::BackUpMode
            | InverterOperationMode::FeedinPriority => None,
        };

        // Build entity changes (control entities: the control prefix, brief H36)
        let prefix = self.control_prefix(inverter_id);
        let charger_option = serde_json::to_value(charger_mode)
            .and_then(serde_json::from_value)
            .expect("Failed to serialize charger mode");

        let mut entity_changes = vec![EntityChange {
            entity_id: format!("select.{prefix}_charger_use_mode"),
            option: charger_option,
        }];

        // Only add manual_mode_select when the mode requires it
        if let Some(manual) = manual_mode {
            let manual_option = serde_json::to_value(manual)
                .and_then(serde_json::from_value)
                .expect("Failed to serialize manual mode");
            entity_changes.push(EntityChange {
                entity_id: format!("select.{prefix}_manual_mode_select"),
                option: manual_option,
            });
        }

        ModeChangeRequest { entity_changes }
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
        format!(
            "number.{}_export_control_user_limit",
            self.control_prefix(inverter_id)
        )
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
#[derive(Debug, Clone, Default)]
pub struct SolaxUltraEntityMapper {
    /// Standard Solax mapper with the same control prefixes, for the shared control entities
    solax: SolaxEntityMapper,
}

impl SolaxUltraEntityMapper {
    /// Create a new Solax Ultra entity mapper
    pub fn new() -> Self {
        Self::default()
    }

    /// Mapper whose control entities (`select.*`, `number.*`) use `control` (brief H36)
    #[must_use]
    pub fn with_control_prefixes(control: ControlPrefixes) -> Self {
        Self {
            solax: SolaxEntityMapper::with_control_prefixes(control),
        }
    }
}

impl VendorEntityMapper for SolaxUltraEntityMapper {
    fn vendor_name(&self) -> fluxion_core::InverterType {
        fluxion_core::InverterType::SolaxUltra
    }

    fn control_prefix<'a>(&'a self, inverter_id: &'a str) -> &'a str {
        self.solax.control_prefix(inverter_id)
    }

    // Inherit all mode mapping and entity methods from standard Solax
    // Only override the battery SOC entity

    fn map_mode_to_vendor(&self, mode: InverterOperationMode) -> i32 {
        SolaxEntityMapper::new().map_mode_to_vendor(mode)
    }

    fn map_mode_from_vendor(&self, vendor_mode: i32) -> Option<InverterOperationMode> {
        SolaxEntityMapper::new().map_mode_from_vendor(vendor_mode)
    }

    fn get_manual_mode_entity(&self, inverter_id: &str) -> Option<String> {
        self.solax.get_manual_mode_entity(inverter_id)
    }

    fn map_manual_sub_mode(&self, option: &str) -> Option<InverterOperationMode> {
        SolaxEntityMapper::new().map_manual_sub_mode(option)
    }

    fn get_work_mode_entity(&self, inverter_id: &str) -> String {
        self.solax.get_work_mode_entity(inverter_id)
    }

    fn get_mode_change_request(
        &self,
        inverter_id: &str,
        mode: InverterOperationMode,
    ) -> ModeChangeRequest {
        self.solax.get_mode_change_request(inverter_id, mode)
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
        self.solax.get_export_limit_entity(inverter_id)
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
        // BackUpMode maps to native Solax BackUpMode (2)
        assert_eq!(
            mapper.map_mode_to_vendor(InverterOperationMode::BackUpMode),
            2
        );
        // NoChargeNoDischarge, ForceCharge, and ForceDischarge all map to ManualMode (3)
        assert_eq!(
            mapper.map_mode_to_vendor(InverterOperationMode::NoChargeNoDischarge),
            3
        );
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
        // FeedinPriority (1) maps to generic FeedinPriority
        assert_eq!(
            mapper.map_mode_from_vendor(1),
            Some(InverterOperationMode::FeedinPriority)
        );
        // BackUpMode (2) maps directly to generic BackUpMode
        assert_eq!(
            mapper.map_mode_from_vendor(2),
            Some(InverterOperationMode::BackUpMode)
        );
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

        // BackUpMode round-trips correctly via native BackUpMode (2)
        let vendor_mode = mapper.map_mode_to_vendor(InverterOperationMode::BackUpMode);
        assert_eq!(vendor_mode, 2);
        assert_eq!(
            mapper.map_mode_from_vendor(vendor_mode),
            Some(InverterOperationMode::BackUpMode)
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

        // NoChargeNoDischarge maps to ManualMode (3) but maps back to ForceCharge
        // (the manual_mode_select entity differentiates them)
        let vendor_mode = mapper.map_mode_to_vendor(InverterOperationMode::NoChargeNoDischarge);
        assert_eq!(vendor_mode, 3);
        assert_eq!(
            mapper.map_mode_from_vendor(vendor_mode),
            Some(InverterOperationMode::ForceCharge) // Not NoChargeNoDischarge!
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
    fn test_solax_backup_mode_change_request() {
        let mapper = SolaxEntityMapper::new();

        // BackUpMode should only send charger_use_mode (native Back Up Mode, no manual_mode_select)
        let request = mapper.get_mode_change_request("my_solax", InverterOperationMode::BackUpMode);
        assert_eq!(request.entity_changes.len(), 1);
        assert_eq!(
            request.entity_changes[0].entity_id,
            "select.my_solax_charger_use_mode"
        );
        assert_eq!(request.entity_changes[0].option, "Back Up Mode");
    }

    #[test]
    fn test_solax_no_charge_no_discharge_mode_change_request() {
        let mapper = SolaxEntityMapper::new();

        // NoChargeNoDischarge sends both: ManualMode + StopChargeAndDischarge
        let request =
            mapper.get_mode_change_request("my_solax", InverterOperationMode::NoChargeNoDischarge);
        assert_eq!(request.entity_changes.len(), 2);
        assert_eq!(
            request.entity_changes[0].entity_id,
            "select.my_solax_charger_use_mode"
        );
        assert_eq!(request.entity_changes[0].option, "Manual Mode");
        assert_eq!(
            request.entity_changes[1].entity_id,
            "select.my_solax_manual_mode_select"
        );
        assert_eq!(
            request.entity_changes[1].option,
            "Stop Charge and Discharge"
        );
    }

    #[test]
    fn test_solax_self_use_mode_change_request() {
        let mapper = SolaxEntityMapper::new();

        // SelfUse should only send charger_use_mode (no manual_mode_select needed)
        let request = mapper.get_mode_change_request("my_solax", InverterOperationMode::SelfUse);
        assert_eq!(request.entity_changes.len(), 1);
        assert_eq!(
            request.entity_changes[0].entity_id,
            "select.my_solax_charger_use_mode"
        );
        assert_eq!(request.entity_changes[0].option, "Self Use Mode");
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

#[cfg(test)]
mod manual_sub_mode_tests {
    use super::*;

    #[test]
    fn manual_sub_mode_options_map_to_exact_modes() {
        let mapper = SolaxEntityMapper::new();
        assert_eq!(
            mapper.map_manual_sub_mode("Force Charge"),
            Some(InverterOperationMode::ForceCharge)
        );
        assert_eq!(
            mapper.map_manual_sub_mode("Force Discharge"),
            Some(InverterOperationMode::ForceDischarge)
        );
        assert_eq!(
            mapper.map_manual_sub_mode("Stop Charge and Discharge"),
            Some(InverterOperationMode::NoChargeNoDischarge)
        );
        assert_eq!(mapper.map_manual_sub_mode("unavailable"), None);
        assert_eq!(
            mapper.get_manual_mode_entity("my_solax").as_deref(),
            Some("select.my_solax_manual_mode_select")
        );
    }
}

/// Brief H36: a separate prefix for the control entities
#[cfg(test)]
mod control_prefix_tests {
    use super::*;
    use crate::solax::create_entity_mapper_with_control;
    use fluxion_core::InverterType;

    const MODES: [InverterOperationMode; 6] = [
        InverterOperationMode::SelfUse,
        InverterOperationMode::BackUpMode,
        InverterOperationMode::NoChargeNoDischarge,
        InverterOperationMode::ForceCharge,
        InverterOperationMode::ForceDischarge,
        InverterOperationMode::FeedinPriority,
    ];

    /// Every entity id the mapper generates for `prefix`, by method name
    fn all_ids(mapper: &dyn VendorEntityMapper, prefix: &str) -> Vec<(String, Option<String>)> {
        let p = prefix;
        let m = mapper;
        let mut ids: Vec<(&str, Option<String>)> = vec![
            ("control_prefix", Some(m.control_prefix(p).to_owned())),
            ("manual_mode", m.get_manual_mode_entity(p)),
            ("work_mode", Some(m.get_work_mode_entity(p))),
            ("export_limit", Some(m.get_export_limit_entity(p))),
            ("battery_soc", Some(m.get_battery_soc_entity(p))),
            ("grid_power", Some(m.get_grid_power_entity(p))),
            ("battery_power", Some(m.get_battery_power_entity(p))),
            ("pv_power", Some(m.get_pv_power_entity(p))),
            ("pv1", m.get_pv1_power_entity(p)),
            ("pv2", m.get_pv2_power_entity(p)),
            ("pv3", m.get_pv3_power_entity(p)),
            ("pv4", m.get_pv4_power_entity(p)),
            ("l1_voltage", m.get_l1_voltage_entity(p)),
            ("l1_current", m.get_l1_current_entity(p)),
            ("l1_power", m.get_l1_power_entity(p)),
            ("l2_voltage", m.get_l2_voltage_entity(p)),
            ("l2_current", m.get_l2_current_entity(p)),
            ("l2_power", m.get_l2_power_entity(p)),
            ("l3_voltage", m.get_l3_voltage_entity(p)),
            ("l3_current", m.get_l3_current_entity(p)),
            ("l3_power", m.get_l3_power_entity(p)),
            ("battery_soh", m.get_battery_soh_entity(p)),
            ("battery_voltage", m.get_battery_voltage_entity(p)),
            ("battery_current", m.get_battery_current_entity(p)),
            (
                "battery_out_total",
                m.get_battery_output_energy_total_entity(p),
            ),
            (
                "battery_in_total",
                m.get_battery_input_energy_total_entity(p),
            ),
            ("bms_charge_max", m.get_bms_charge_max_current_entity(p)),
            (
                "bms_discharge_max",
                m.get_bms_discharge_max_current_entity(p),
            ),
            ("battery_capacity", m.get_battery_capacity_entity(p)),
            (
                "battery_in_today",
                m.get_battery_input_energy_today_entity(p),
            ),
            (
                "battery_out_today",
                m.get_battery_output_energy_today_entity(p),
            ),
            ("house_load", m.get_house_load_entity(p)),
            ("grid_import_power", m.get_grid_import_power_entity(p)),
            ("grid_export_power", m.get_grid_export_power_entity(p)),
            ("grid_import_today", m.get_grid_import_today_entity(p)),
            ("grid_export_today", m.get_grid_export_today_entity(p)),
            ("inverter_frequency", m.get_inverter_frequency_entity(p)),
            ("inverter_voltage", m.get_inverter_voltage_entity(p)),
            ("inverter_current", m.get_inverter_current_entity(p)),
            ("inverter_power", m.get_inverter_power_entity(p)),
            ("grid_export_total", m.get_grid_export_total_entity(p)),
            ("grid_import_total", m.get_grid_import_total_entity(p)),
            ("today_solar", m.get_today_solar_energy_entity(p)),
            ("total_solar", m.get_total_solar_energy_entity(p)),
            ("today_yield", m.get_today_yield_entity(p)),
            ("total_yield", m.get_total_yield_entity(p)),
            ("inverter_temperature", m.get_inverter_temperature_entity(p)),
            ("battery_temperature", m.get_battery_temperature_entity(p)),
            ("board_temperature", m.get_board_temperature_entity(p)),
            ("boost_temperature", m.get_boost_temperature_entity(p)),
            ("eps_voltage", m.get_eps_voltage_entity(p)),
            ("eps_current", m.get_eps_current_entity(p)),
            ("eps_power", m.get_eps_power_entity(p)),
            ("fault_code", m.get_fault_code_entity(p)),
            ("bus_voltage", m.get_bus_voltage_entity(p)),
        ];
        let mut out: Vec<(String, Option<String>)> =
            ids.drain(..).map(|(k, v)| (k.to_owned(), v)).collect();
        for mode in MODES {
            for (i, change) in m
                .get_mode_change_request(p, mode)
                .entity_changes
                .into_iter()
                .enumerate()
            {
                out.push((
                    format!("mode {mode:?} #{i}"),
                    Some(format!("{} = {}", change.entity_id, change.option)),
                ));
            }
        }
        out
    }

    /// Entity ids generated before brief H36 for prefix `solax`: (method, suffix) where the
    /// id is `<domain>.solax_<suffix>`; `None` = the mapper has no such entity
    fn expected_before_h36(variant: InverterType) -> Vec<(String, Option<String>)> {
        let ultra = variant == InverterType::SolaxUltra;
        let pick = |solax: &'static str, ultra_suffix: Option<&'static str>| {
            if ultra { ultra_suffix } else { Some(solax) }
        };
        let rows: Vec<(&str, Option<&str>)> = vec![
            ("control_prefix", Some("=solax")),
            ("manual_mode", Some("select.manual_mode_select")),
            ("work_mode", Some("select.charger_use_mode")),
            ("export_limit", Some("number.export_control_user_limit")),
            (
                "battery_soc",
                pick(
                    "sensor.battery_capacity",
                    Some("sensor.battery_total_capacity_charge"),
                ),
            ),
            ("grid_power", Some("sensor.grid_import")),
            ("battery_power", Some("sensor.battery_power_charge")),
            ("pv_power", Some("sensor.pv_power_total")),
            ("pv1", pick("sensor.pv1_power", Some("sensor.pv_power_1"))),
            ("pv2", pick("sensor.pv2_power", Some("sensor.pv_power_2"))),
            ("pv3", pick("sensor.pv3_power", Some("sensor.pv_power_3"))),
            ("pv4", Some("sensor.pv4_power")),
            ("l1_voltage", Some("sensor.phase_l1_voltage")),
            ("l1_current", Some("sensor.phase_l1_current")),
            ("l1_power", Some("sensor.phase_l1_power")),
            ("l2_voltage", Some("sensor.phase_l2_voltage")),
            ("l2_current", Some("sensor.phase_l2_current")),
            ("l2_power", Some("sensor.phase_l2_power")),
            ("l3_voltage", Some("sensor.phase_l3_voltage")),
            ("l3_current", Some("sensor.phase_l3_current")),
            ("l3_power", Some("sensor.phase_l3_power")),
            ("battery_soh", Some("sensor.battery_health")),
            ("battery_voltage", Some("sensor.battery_voltage")),
            ("battery_current", Some("sensor.battery_current")),
            ("battery_out_total", Some("sensor.battery_discharge_total")),
            ("battery_in_total", Some("sensor.battery_charge_total")),
            ("bms_charge_max", Some("sensor.bms_max_charge_current")),
            (
                "bms_discharge_max",
                Some("sensor.bms_max_discharge_current"),
            ),
            (
                "battery_capacity",
                pick(
                    "sensor.battery_capacity",
                    Some("sensor.battery_total_capacity_charge"),
                ),
            ),
            (
                "battery_in_today",
                Some("sensor.battery_input_energy_today"),
            ),
            (
                "battery_out_today",
                Some("sensor.battery_output_energy_today"),
            ),
            ("house_load", Some("sensor.house_load")),
            ("grid_import_power", Some("sensor.grid_import")),
            ("grid_export_power", Some("sensor.grid_export")),
            ("grid_import_today", Some("sensor.today_s_import_energy")),
            ("grid_export_today", Some("sensor.today_s_export_energy")),
            ("inverter_frequency", Some("sensor.inverter_frequency")),
            ("inverter_voltage", Some("sensor.inverter_voltage")),
            ("inverter_current", pick("sensor.inverter_current", None)),
            ("inverter_power", pick("sensor.inverter_power", None)),
            ("grid_export_total", pick("sensor.grid_export_total", None)),
            ("grid_import_total", Some("sensor.grid_import_total")),
            ("today_solar", Some("sensor.today_s_solar_energy")),
            ("total_solar", Some("sensor.total_solar_energy")),
            (
                "today_yield",
                pick("sensor.today_energy", Some("sensor.today_s_yield")),
            ),
            (
                "total_yield",
                pick("sensor.total_energy", Some("sensor.total_yield")),
            ),
            ("inverter_temperature", Some("sensor.inverter_temperature")),
            ("battery_temperature", Some("sensor.battery_temperature")),
            ("board_temperature", Some("sensor.board_temperature")),
            ("boost_temperature", Some("sensor.boost_temperature")),
            ("eps_voltage", Some("sensor.eps_voltage")),
            ("eps_current", Some("sensor.eps_current")),
            ("eps_power", Some("sensor.eps_power")),
            ("fault_code", Some("sensor.fault_code")),
            ("bus_voltage", Some("sensor.bus_voltage")),
            (
                "mode SelfUse #0",
                Some("select.charger_use_mode = Self Use Mode"),
            ),
            (
                "mode BackUpMode #0",
                Some("select.charger_use_mode = Back Up Mode"),
            ),
            (
                "mode NoChargeNoDischarge #0",
                Some("select.charger_use_mode = Manual Mode"),
            ),
            (
                "mode NoChargeNoDischarge #1",
                Some("select.manual_mode_select = Stop Charge and Discharge"),
            ),
            (
                "mode ForceCharge #0",
                Some("select.charger_use_mode = Manual Mode"),
            ),
            (
                "mode ForceCharge #1",
                Some("select.manual_mode_select = Force Charge"),
            ),
            (
                "mode ForceDischarge #0",
                Some("select.charger_use_mode = Manual Mode"),
            ),
            (
                "mode ForceDischarge #1",
                Some("select.manual_mode_select = Force Discharge"),
            ),
            (
                "mode FeedinPriority #0",
                Some("select.charger_use_mode = Feedin Priority"),
            ),
        ];
        rows.into_iter()
            .map(|(name, suffix)| {
                let id = suffix.map(|s| match s.strip_prefix('=') {
                    Some(bare) => bare.to_owned(),
                    None => {
                        let (domain, rest) = s.split_once('.').expect("domain.suffix");
                        format!("{domain}.solax_{rest}")
                    }
                });
                (name.to_owned(), id)
            })
            .collect()
    }

    #[test]
    fn empty_control_prefix_generates_every_id_as_before() {
        for variant in [InverterType::Solax, InverterType::SolaxUltra] {
            let expected = expected_before_h36(variant);
            for mapper in [
                crate::solax::create_entity_mapper(variant),
                create_entity_mapper_with_control(variant, ControlPrefixes::new()),
                // A control prefix for another inverter changes nothing for this one
                create_entity_mapper_with_control(
                    variant,
                    ControlPrefixes::new().with_inverter("solax_80", "other"),
                ),
                // Blank control prefix = empty
                create_entity_mapper_with_control(
                    variant,
                    ControlPrefixes::new().with_inverter("solax", " "),
                ),
            ] {
                assert_eq!(all_ids(mapper.as_ref(), "solax"), expected, "{variant:?}");
            }
        }
    }

    #[test]
    fn control_prefix_applies_to_controls_only() {
        for variant in [InverterType::Solax, InverterType::SolaxUltra] {
            let before = all_ids(
                crate::solax::create_entity_mapper(variant).as_ref(),
                "solax",
            );
            let mapper = create_entity_mapper_with_control(
                variant,
                ControlPrefixes::new().with_inverter("solax", "solax_inverter"),
            );
            let after = all_ids(mapper.as_ref(), "solax");
            assert_eq!(before.len(), after.len());
            for ((name, old), (_, new)) in before.iter().zip(&after) {
                let (Some(old), Some(new)) = (old, new) else {
                    assert_eq!(old, new, "{variant:?} {name}");
                    continue;
                };
                if name == "control_prefix" {
                    assert_eq!(new, "solax_inverter");
                } else if old.starts_with("sensor.") {
                    assert_eq!(
                        old, new,
                        "{variant:?} {name}: sensors keep the entity prefix"
                    );
                } else {
                    assert_eq!(
                        new,
                        &old.replacen(".solax_", ".solax_inverter_", 1),
                        "{variant:?} {name}: controls take the control prefix"
                    );
                }
            }

            assert_eq!(
                mapper.get_work_mode_entity("solax"),
                "select.solax_inverter_charger_use_mode"
            );
            assert_eq!(
                mapper.get_export_limit_entity("solax"),
                "number.solax_inverter_export_control_user_limit"
            );
            assert_eq!(
                mapper.get_house_load_entity("solax").unwrap(),
                "sensor.solax_house_load"
            );
            assert_eq!(
                crate::ha::actuation::lock_entity(mapper.control_prefix("solax")),
                "select.solax_inverter_lock_state"
            );
        }
        let solax = create_entity_mapper_with_control(
            InverterType::Solax,
            ControlPrefixes::new().with_inverter("solax", "solax_inverter"),
        );
        assert_eq!(
            solax.get_battery_soc_entity("solax"),
            "sensor.solax_battery_capacity"
        );
    }
}
