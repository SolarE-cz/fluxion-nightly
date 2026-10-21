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

//! Separate prefix for an inverter's control entities (brief H36).
//!
//! Some SolaX Modbus installs name the sensors `sensor.solax_*` but the controls
//! `select.solax_inverter_*`, `number.solax_inverter_*` and `button.solax_inverter_*`. Every
//! `select.*`, `number.*`, `button.*`, `switch.*` and `time.*` entity FluxION reads or writes
//! for such an inverter takes the control prefix; sensors keep the entity prefix.

use std::collections::HashMap;

/// Control prefix per inverter id. Keyed by id only (callers always pass the id), so one
/// inverter's control prefix can never apply to another. An id without an entry resolves to
/// itself, so an empty configuration changes no entity id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlPrefixes(HashMap<String, String>);

impl ControlPrefixes {
    /// No overrides: every control entity uses the prefix it is asked for
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Map the inverter id to `control`. An empty (or blank) `control` or id adds nothing.
    #[must_use]
    pub fn with_inverter(mut self, id: &str, control: &str) -> Self {
        let (id, control) = (id.trim(), control.trim());
        if !id.is_empty() && !control.is_empty() {
            self.0.insert(id.to_owned(), control.to_owned());
        }
        self
    }

    /// Control prefix of inverter `id`; the id itself when not mapped
    #[must_use]
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        self.0.get(id).map_or(id, String::as_str)
    }

    /// True when no inverter has a separate control prefix
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmapped_prefix_resolves_to_itself() {
        let prefixes = ControlPrefixes::new().with_inverter("solax", "  ");
        assert!(prefixes.is_empty());
        assert_eq!(prefixes.resolve("solax"), "solax");
    }

    /// Review nit 3: keyed by id only; another inverter's entity prefix or id never inherits
    #[test]
    fn only_the_inverter_id_resolves_to_the_control_prefix() {
        let prefixes = ControlPrefixes::new()
            .with_inverter(" inv1 ", " solax_inverter ")
            .with_inverter("inv2", "");
        assert_eq!(prefixes.resolve("inv1"), "solax_inverter");
        // The entity prefix of inv1 is not a key
        assert_eq!(prefixes.resolve("solax"), "solax");
        assert_eq!(prefixes.resolve("inv2"), "inv2");
        assert_eq!(prefixes.resolve("solax_80"), "solax_80");
    }
}
