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

//! User control types for manual FluxION overrides.
//!
//! This module provides types for:
//! - Enabling/disabling FluxION mode changes
//! - Disallowing specific modes (charge/discharge)
//! - Fixed time slots that override the generated schedule

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::inverter::InverterOperationMode;

/// Lowest minimum SOC (%) kept in Back Up Mode (the SolaX `backup_discharge_min_soc` range)
pub const BACKUP_MIN_SOC_MIN: u8 = 15;
/// Highest minimum SOC (%) kept in Back Up Mode
pub const BACKUP_MIN_SOC_MAX: u8 = 100;
/// Back Up minimum SOC must stay this far below the battery maximum (brief H19 item 5)
pub const BACKUP_MIN_SOC_MARGIN_BELOW_MAX: f32 = 5.0;

/// The mode used when no strategy or override applies: Self-Use, or Back Up (brief H19)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultMode {
    #[default]
    SelfUse,
    Backup,
}

impl DefaultMode {
    /// Wire name used by the API and the persisted state
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfUse => "self_use",
            Self::Backup => "backup",
        }
    }

    /// Parse the wire name; `None` for anything else
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "self_use" => Some(Self::SelfUse),
            "backup" => Some(Self::Backup),
            _ => None,
        }
    }
}

/// User control state - persisted to ./data/user_control.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserControlState {
    /// When true, FluxION is enabled and can send mode change commands.
    /// When false, FluxION sets inverter to SelfUse and stops mode commands.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// When true, FluxION won't plan or execute ForceCharge modes.
    #[serde(default)]
    pub disallow_charge: bool,

    /// When true, FluxION won't plan or execute ForceDischarge modes.
    #[serde(default)]
    pub disallow_discharge: bool,

    /// User-locked time slots that override generated schedule.
    #[serde(default)]
    pub fixed_time_slots: Vec<FixedTimeSlot>,

    /// Default mode for planned Self-Use blocks: Self-Use, or Back Up Mode.
    #[serde(default)]
    pub default_mode: DefaultMode,

    /// Minimum battery SOC (%) kept in Back Up Mode, 15..=100.
    #[serde(default = "default_backup_min_soc_pct")]
    pub backup_min_soc_pct: u8,

    /// When user control state was last modified.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

fn default_backup_min_soc_pct() -> u8 {
    25
}

/// True when a requested Back Up minimum SOC is inside the allowed range
#[must_use]
pub fn backup_min_soc_in_range(value: i64) -> bool {
    (i64::from(BACKUP_MIN_SOC_MIN)..=i64::from(BACKUP_MIN_SOC_MAX)).contains(&value)
}

/// Highest Back Up minimum SOC (%) allowed for a battery maximum: `max_battery_soc - 5`
#[must_use]
pub fn backup_min_soc_limit(max_battery_soc: f32) -> f32 {
    max_battery_soc - BACKUP_MIN_SOC_MARGIN_BELOW_MAX
}

impl Default for UserControlState {
    fn default() -> Self {
        Self {
            enabled: true,
            disallow_charge: false,
            disallow_discharge: false,
            fixed_time_slots: Vec::new(),
            default_mode: DefaultMode::SelfUse,
            backup_min_soc_pct: default_backup_min_soc_pct(),
            last_modified: None,
        }
    }
}

impl UserControlState {
    /// Clean up expired fixed time slots (slots whose end time has passed).
    pub fn cleanup_expired_slots(&mut self) {
        let now = Utc::now();
        self.fixed_time_slots.retain(|slot| !slot.has_passed(now));
    }

    /// Get the fixed slot covering a specific time, if any.
    pub fn get_fixed_slot_at(&self, time: DateTime<Utc>) -> Option<&FixedTimeSlot> {
        self.fixed_time_slots.iter().find(|slot| slot.covers(time))
    }

    /// Check if a mode is allowed given current restrictions.
    pub fn is_mode_allowed(&self, mode: InverterOperationMode) -> bool {
        match mode {
            InverterOperationMode::ForceCharge => !self.disallow_charge,
            InverterOperationMode::ForceDischarge => !self.disallow_discharge,
            _ => true,
        }
    }

    /// Minimum SOC the planner and the execution guards must respect. In Back Up default mode
    /// the inverter keeps `backup_min_soc_pct`, capped at `max_battery_soc - 5`, so the floor is
    /// the higher of the control minimum and that target. Self-Use returns the control minimum.
    pub fn planner_min_soc_pct(&self, min_battery_soc: f32, max_battery_soc: f32) -> f32 {
        match self.default_mode {
            DefaultMode::Backup => min_battery_soc
                .max(f32::from(self.backup_min_soc_pct).min(backup_min_soc_limit(max_battery_soc))),
            DefaultMode::SelfUse => min_battery_soc,
        }
    }

    /// Seed the default mode from the config's `default_battery_mode` when the user has never
    /// set it (`last_modified` is `None`), so the dashboard shows the mode in effect. Once the
    /// user sets it from the dashboard, the dashboard value wins. Returns `true` when seeded.
    pub fn seed_default_mode_from_config(
        &mut self,
        config_default_mode: InverterOperationMode,
        min_battery_soc: f32,
    ) -> bool {
        if self.last_modified.is_some() || config_default_mode != InverterOperationMode::BackUpMode
        {
            return false;
        }
        self.default_mode = DefaultMode::Backup;
        // The config minimum is the closest known value to what the inverter keeps
        self.backup_min_soc_pct = (min_battery_soc.round() as i64)
            .clamp(i64::from(BACKUP_MIN_SOC_MIN), i64::from(BACKUP_MIN_SOC_MAX))
            as u8;
        true
    }

    /// Check if there are any active restrictions.
    pub fn has_restrictions(&self) -> bool {
        self.disallow_charge || self.disallow_discharge
    }

    /// Get the number of active (non-expired) fixed time slots.
    pub fn active_slot_count(&self) -> usize {
        let now = Utc::now();
        self.fixed_time_slots
            .iter()
            .filter(|slot| !slot.has_passed(now))
            .count()
    }
}

/// A user-defined fixed time slot that overrides the generated schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FixedTimeSlot {
    /// Unique identifier for this slot.
    pub id: String,

    /// Start time of the locked slot.
    pub from: DateTime<Utc>,

    /// End time of the locked slot.
    pub to: DateTime<Utc>,

    /// Operation mode for this slot.
    pub mode: InverterOperationMode,

    /// Optional user note/reason.
    #[serde(default)]
    pub note: Option<String>,

    /// When this slot was created.
    pub created_at: DateTime<Utc>,
}

impl FixedTimeSlot {
    /// Generate a unique ID for a new slot.
    pub fn generate_id() -> String {
        format!("slot_{}", Utc::now().timestamp_millis())
    }

    /// Create a new fixed time slot.
    pub fn new(
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        mode: InverterOperationMode,
        note: Option<String>,
    ) -> Self {
        Self {
            id: Self::generate_id(),
            from,
            to,
            mode,
            note,
            created_at: Utc::now(),
        }
    }

    /// Check if this slot is currently active (now is within from..to).
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.from && now < self.to
    }

    /// Check if this slot has passed (expired).
    pub fn has_passed(&self, now: DateTime<Utc>) -> bool {
        now >= self.to
    }

    /// Check if this slot covers a specific time.
    pub fn covers(&self, time: DateTime<Utc>) -> bool {
        time >= self.from && time < self.to
    }

    /// Get the duration of this slot in minutes.
    pub fn duration_minutes(&self) -> i64 {
        (self.to - self.from).num_minutes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_default_state() {
        let state = UserControlState::default();
        assert!(state.enabled);
        assert!(!state.disallow_charge);
        assert!(!state.disallow_discharge);
        assert!(state.fixed_time_slots.is_empty());
    }

    #[test]
    fn test_is_mode_allowed() {
        let mut state = UserControlState::default();

        // All modes allowed by default
        assert!(state.is_mode_allowed(InverterOperationMode::SelfUse));
        assert!(state.is_mode_allowed(InverterOperationMode::ForceCharge));
        assert!(state.is_mode_allowed(InverterOperationMode::ForceDischarge));
        assert!(state.is_mode_allowed(InverterOperationMode::BackUpMode));
        assert!(state.is_mode_allowed(InverterOperationMode::NoChargeNoDischarge));

        // Disallow charge
        state.disallow_charge = true;
        assert!(!state.is_mode_allowed(InverterOperationMode::ForceCharge));
        assert!(state.is_mode_allowed(InverterOperationMode::ForceDischarge));

        // Disallow discharge too
        state.disallow_discharge = true;
        assert!(!state.is_mode_allowed(InverterOperationMode::ForceCharge));
        assert!(!state.is_mode_allowed(InverterOperationMode::ForceDischarge));
        assert!(state.is_mode_allowed(InverterOperationMode::SelfUse));
    }

    #[test]
    fn default_mode_is_self_use_with_backup_floor_25() {
        let state = UserControlState::default();
        assert_eq!(state.default_mode, DefaultMode::SelfUse);
        assert_eq!(state.backup_min_soc_pct, 25);
    }

    #[test]
    fn default_mode_wire_names_round_trip() {
        assert_eq!(DefaultMode::parse("self_use"), Some(DefaultMode::SelfUse));
        assert_eq!(DefaultMode::parse("backup"), Some(DefaultMode::Backup));
        assert_eq!(DefaultMode::parse("BackUp"), None);
        assert_eq!(DefaultMode::Backup.as_str(), "backup");
        assert_eq!(DefaultMode::SelfUse.as_str(), "self_use");
    }

    #[test]
    fn backup_min_soc_range_is_15_to_100() {
        assert!(!backup_min_soc_in_range(14));
        assert!(backup_min_soc_in_range(15));
        assert!(backup_min_soc_in_range(100));
        assert!(!backup_min_soc_in_range(101));
        assert!(!backup_min_soc_in_range(-1));
    }

    #[test]
    fn planner_floor_is_backup_minimum_only_in_backup() {
        let mut state = UserControlState {
            backup_min_soc_pct: 40,
            ..Default::default()
        };
        // Self-Use: the control minimum applies unchanged
        assert!((state.planner_min_soc_pct(10.0, 100.0) - 10.0).abs() < 1e-6);
        state.default_mode = DefaultMode::Backup;
        assert!((state.planner_min_soc_pct(10.0, 100.0) - 40.0).abs() < 1e-6);
        // The higher control minimum wins
        assert!((state.planner_min_soc_pct(55.0, 100.0) - 55.0).abs() < 1e-6);
    }

    #[test]
    fn planner_floor_caps_backup_target_at_max_minus_5() {
        let state = UserControlState {
            default_mode: DefaultMode::Backup,
            backup_min_soc_pct: 98,
            ..Default::default()
        };
        assert!((state.planner_min_soc_pct(10.0, 100.0) - 95.0).abs() < 1e-6);
        assert!((backup_min_soc_limit(100.0) - 95.0).abs() < 1e-6);
    }

    #[test]
    fn seed_applies_only_when_never_set_and_config_is_backup() {
        let mut state = UserControlState::default();
        assert!(state.seed_default_mode_from_config(InverterOperationMode::BackUpMode, 20.0));
        assert_eq!(state.default_mode, DefaultMode::Backup);
        assert_eq!(state.backup_min_soc_pct, 20);

        // Config SelfUse: nothing to seed
        let mut state = UserControlState::default();
        assert!(!state.seed_default_mode_from_config(InverterOperationMode::SelfUse, 20.0));
        assert_eq!(state.default_mode, DefaultMode::SelfUse);

        // Dashboard already set it: the dashboard value wins
        let mut state = UserControlState {
            last_modified: Some(Utc::now()),
            ..Default::default()
        };
        assert!(!state.seed_default_mode_from_config(InverterOperationMode::BackUpMode, 20.0));
        assert_eq!(state.default_mode, DefaultMode::SelfUse);
    }

    #[test]
    fn test_fixed_slot_covers() {
        let now = Utc::now();
        let slot = FixedTimeSlot::new(
            now,
            now + Duration::hours(1),
            InverterOperationMode::ForceCharge,
            None,
        );

        assert!(slot.covers(now));
        assert!(slot.covers(now + Duration::minutes(30)));
        assert!(!slot.covers(now - Duration::minutes(1)));
        assert!(!slot.covers(now + Duration::hours(1))); // End time is exclusive
    }

    #[test]
    fn test_cleanup_expired_slots() {
        let now = Utc::now();
        let mut state = UserControlState::default();

        // Add an expired slot
        state.fixed_time_slots.push(FixedTimeSlot::new(
            now - Duration::hours(2),
            now - Duration::hours(1),
            InverterOperationMode::ForceCharge,
            None,
        ));

        // Add a current slot
        state.fixed_time_slots.push(FixedTimeSlot::new(
            now,
            now + Duration::hours(1),
            InverterOperationMode::ForceDischarge,
            None,
        ));

        assert_eq!(state.fixed_time_slots.len(), 2);
        state.cleanup_expired_slots();
        assert_eq!(state.fixed_time_slots.len(), 1);
        assert_eq!(
            state.fixed_time_slots[0].mode,
            InverterOperationMode::ForceDischarge
        );
    }

    #[test]
    fn test_get_fixed_slot_at() {
        let now = Utc::now();
        let mut state = UserControlState::default();

        state.fixed_time_slots.push(FixedTimeSlot::new(
            now,
            now + Duration::hours(1),
            InverterOperationMode::ForceCharge,
            Some("Test slot".to_string()),
        ));

        assert!(state.get_fixed_slot_at(now).is_some());
        assert!(
            state
                .get_fixed_slot_at(now + Duration::minutes(30))
                .is_some()
        );
        assert!(
            state
                .get_fixed_slot_at(now - Duration::minutes(1))
                .is_none()
        );
        assert!(state.get_fixed_slot_at(now + Duration::hours(2)).is_none());
    }
}
