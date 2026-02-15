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

    /// When user control state was last modified.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

impl Default for UserControlState {
    fn default() -> Self {
        Self {
            enabled: true,
            disallow_charge: false,
            disallow_discharge: false,
            fixed_time_slots: Vec::new(),
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
