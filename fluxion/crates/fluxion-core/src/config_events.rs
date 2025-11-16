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
use std::collections::HashSet;

/// Event triggered when configuration is updated via web UI
#[derive(Event, Clone)]
pub struct ConfigUpdateEvent {
    /// New configuration to apply
    pub new_config: serde_json::Value,
    /// Sections that changed (for targeted updates)
    pub changed_sections: HashSet<ConfigSection>,
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
