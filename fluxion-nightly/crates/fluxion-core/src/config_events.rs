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

use bevy_ecs::prelude::*;
use fluxion_types::UserControlState;
use std::collections::HashSet;

/// Event triggered when configuration is updated via web UI
#[derive(Event, Clone)]
pub struct ConfigUpdateEvent {
    /// New configuration to apply
    pub new_config: serde_json::Value,
    /// Sections that changed (for targeted updates)
    pub changed_sections: HashSet<ConfigSection>,
}

/// Event triggered when user control state changes via web UI
#[derive(Event, Clone)]
pub struct UserControlUpdateEvent {
    /// New user control state to apply
    pub new_state: UserControlState,
    /// Type of change that occurred
    pub change_type: UserControlChangeType,
}

/// Type of change to user control state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserControlChangeType {
    /// FluxION enabled/disabled
    EnabledChanged,
    /// Disallow charge/discharge changed
    RestrictionsChanged,
    /// Fixed time slot added
    SlotAdded,
    /// Fixed time slot removed
    SlotRemoved,
    /// Fixed time slot modified
    SlotModified,
    /// Full state update
    FullUpdate,
}

impl UserControlUpdateEvent {
    /// Create a new user control update event
    pub fn new(new_state: UserControlState, change_type: UserControlChangeType) -> Self {
        Self {
            new_state,
            change_type,
        }
    }

    /// Create an event for enabled state change
    pub fn enabled_changed(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::EnabledChanged)
    }

    /// Create an event for restrictions change
    pub fn restrictions_changed(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::RestrictionsChanged)
    }

    /// Create an event for slot added
    pub fn slot_added(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::SlotAdded)
    }

    /// Create an event for slot removed
    pub fn slot_removed(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::SlotRemoved)
    }

    /// Create an event for slot modified
    pub fn slot_modified(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::SlotModified)
    }

    /// Create a full update event
    pub fn full_update(new_state: UserControlState) -> Self {
        Self::new(new_state, UserControlChangeType::FullUpdate)
    }
}

/// Configuration sections that can be updated
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigSection {
    /// System settings (debug mode, update interval, log level, etc.)
    System,
    /// Inverter configuration (topology, entity prefixes, etc.)
    Inverters,
    /// Pricing configuration (spot prices, fixed prices)
    Pricing,
    /// Control parameters (SOC limits, force hours, battery settings)
    Control,
    /// Strategy configuration (enable/disable, parameters)
    Strategies,
}

impl ConfigUpdateEvent {
    /// Create a new config update event
    pub fn new(new_config: serde_json::Value, changed_sections: HashSet<ConfigSection>) -> Self {
        Self {
            new_config,
            changed_sections,
        }
    }

    /// Create a full config update (all sections changed)
    pub fn full_update(new_config: serde_json::Value) -> Self {
        let mut changed_sections = HashSet::new();
        changed_sections.insert(ConfigSection::System);
        changed_sections.insert(ConfigSection::Inverters);
        changed_sections.insert(ConfigSection::Pricing);
        changed_sections.insert(ConfigSection::Control);
        changed_sections.insert(ConfigSection::Strategies);

        Self {
            new_config,
            changed_sections,
        }
    }

    /// Check if a specific section changed
    pub fn section_changed(&self, section: ConfigSection) -> bool {
        self.changed_sections.contains(&section)
    }
}
