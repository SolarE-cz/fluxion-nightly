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

use serde::{Deserialize, Serialize};

/// Solax Charger Use Mode - Primary mode selector
/// Order matches Home Assistant select.{inverter_id}_charger_use_mode options
/// These strings must match EXACTLY what HA expects (case-sensitive, including spaces)
///
/// The enum discriminants (0, 1, 2, 3...) match the numeric values used by Solax API
///
/// Note: "Manual Mode" is used for both Force Charge and Force Discharge.
/// The secondary manual_mode_select entity differentiates between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(i32)]
pub enum SolaxChargerUseMode {
    #[serde(rename = "Self Use Mode")]
    SelfUseMode = 0,

    #[serde(rename = "Feedin Priority")]
    FeedinPriority = 1,

    #[serde(rename = "Back Up Mode")]
    BackUpMode = 2,

    #[serde(rename = "Manual Mode")]
    ManualMode = 3,

    #[serde(rename = "PeakShaving")]
    PeakShaving = 4,

    #[serde(rename = "Smart Schedule")]
    SmartSchedule = 5,
}

impl SolaxChargerUseMode {
    /// Try to create from i32 discriminant
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::SelfUseMode),
            1 => Some(Self::FeedinPriority),
            2 => Some(Self::BackUpMode),
            3 => Some(Self::ManualMode),
            4 => Some(Self::PeakShaving),
            5 => Some(Self::SmartSchedule),
            _ => None,
        }
    }

    /// Convert to i32 discriminant
    pub fn to_i32(self) -> i32 {
        self as i32
    }
}

/// Solax Manual Mode - Secondary mode selector for force charge/discharge
/// Order matches Home Assistant select.{inverter_id}_manual_mode_select options
/// These strings must match EXACTLY what HA expects (case-sensitive, including spaces)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SolaxManualMode {
    #[serde(rename = "Stop Charge and Discharge")]
    StopChargeAndDischarge,

    #[serde(rename = "Force Charge")]
    ForceCharge,

    #[serde(rename = "Force Discharge")]
    ForceDischarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_charger_use_mode_serde() {
        // Test serialization
        let mode = SolaxChargerUseMode::SelfUseMode;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Self Use Mode\"");

        let mode = SolaxChargerUseMode::ManualMode;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Manual Mode\"");

        let mode = SolaxChargerUseMode::FeedinPriority;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Feedin Priority\"");

        // Test deserialization
        let mode: SolaxChargerUseMode = serde_json::from_str("\"Self Use Mode\"").unwrap();
        assert_eq!(mode, SolaxChargerUseMode::SelfUseMode);

        let mode: SolaxChargerUseMode = serde_json::from_str("\"Manual Mode\"").unwrap();
        assert_eq!(mode, SolaxChargerUseMode::ManualMode);

        let mode: SolaxChargerUseMode = serde_json::from_str("\"Feedin Priority\"").unwrap();
        assert_eq!(mode, SolaxChargerUseMode::FeedinPriority);
    }

    #[test]
    fn test_manual_mode_serde() {
        // Test serialization
        let mode = SolaxManualMode::StopChargeAndDischarge;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Stop Charge and Discharge\"");

        let mode = SolaxManualMode::ForceCharge;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Force Charge\"");

        let mode = SolaxManualMode::ForceDischarge;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Force Discharge\"");

        // Test deserialization
        let mode: SolaxManualMode = serde_json::from_str("\"Stop Charge and Discharge\"").unwrap();
        assert_eq!(mode, SolaxManualMode::StopChargeAndDischarge);

        let mode: SolaxManualMode = serde_json::from_str("\"Force Charge\"").unwrap();
        assert_eq!(mode, SolaxManualMode::ForceCharge);

        let mode: SolaxManualMode = serde_json::from_str("\"Force Discharge\"").unwrap();
        assert_eq!(mode, SolaxManualMode::ForceDischarge);
    }
}
