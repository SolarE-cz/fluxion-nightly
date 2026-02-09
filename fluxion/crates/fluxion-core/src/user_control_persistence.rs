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

//! Persistence layer for user control state.
//!
//! Handles loading and saving of `UserControlState` to/from disk.

use anyhow::{Context, Result};
use fluxion_types::UserControlState;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

/// Default path for user control state file.
/// Uses relative path for portability (works in both dev and HA addon).
pub const DEFAULT_USER_CONTROL_PATH: &str = "./data/user_control.json";

/// User control state persistence manager.
#[derive(Debug, Clone)]
pub struct UserControlPersistence {
    /// Path to user control state file.
    state_path: PathBuf,
}

impl UserControlPersistence {
    /// Create a new persistence manager with the given path.
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
        }
    }

    /// Create a persistence manager using the default production path.
    pub fn default_production() -> Self {
        Self::new(DEFAULT_USER_CONTROL_PATH)
    }

    /// Get the path being used for persistence.
    pub fn path(&self) -> &Path {
        &self.state_path
    }

    /// Load user control state from disk.
    ///
    /// Returns the default state if the file doesn't exist.
    /// Automatically cleans up expired slots on load.
    pub fn load(&self) -> Result<UserControlState> {
        if !self.state_path.exists() {
            info!(
                "User control state file not found at {}, using defaults",
                self.state_path.display()
            );
            return Ok(UserControlState::default());
        }

        let contents = fs::read_to_string(&self.state_path).with_context(|| {
            format!(
                "Failed to read user control state from {}",
                self.state_path.display()
            )
        })?;

        let mut state: UserControlState = serde_json::from_str(&contents).with_context(|| {
            format!(
                "Failed to parse user control state from {}",
                self.state_path.display()
            )
        })?;

        // Clean up expired slots on load
        let slots_before = state.fixed_time_slots.len();
        state.cleanup_expired_slots();
        let slots_cleaned = slots_before - state.fixed_time_slots.len();

        if slots_cleaned > 0 {
            info!(
                "Cleaned up {} expired fixed time slots on load",
                slots_cleaned
            );
        }

        info!(
            "Loaded user control state: enabled={}, disallow_charge={}, disallow_discharge={}, fixed_slots={}",
            state.enabled,
            state.disallow_charge,
            state.disallow_discharge,
            state.fixed_time_slots.len()
        );

        Ok(state)
    }

    /// Save user control state to disk.
    ///
    /// Uses atomic write (temp file + rename) to prevent corruption.
    pub fn save(&self, state: &UserControlState) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.state_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let json = serde_json::to_string_pretty(state)
            .context("Failed to serialize user control state")?;

        // Atomic write using temp file
        let temp_path = self.state_path.with_extension("tmp");
        fs::write(&temp_path, &json)
            .with_context(|| format!("Failed to write temp file {}", temp_path.display()))?;
        fs::rename(&temp_path, &self.state_path).with_context(|| {
            format!(
                "Failed to rename temp file to {}",
                self.state_path.display()
            )
        })?;

        info!(
            "Saved user control state to {} (enabled={}, {} slots)",
            self.state_path.display(),
            state.enabled,
            state.fixed_time_slots.len()
        );

        Ok(())
    }

    /// Check if a state file exists.
    pub fn exists(&self) -> bool {
        self.state_path.exists()
    }
}

impl Default for UserControlPersistence {
    fn default() -> Self {
        Self::default_production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use fluxion_types::InverterOperationMode;
    use fluxion_types::user_control::FixedTimeSlot;
    use tempfile::tempdir;

    #[test]
    fn test_load_nonexistent_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let persistence = UserControlPersistence::new(path);

        let state = persistence.load().unwrap();
        assert!(state.enabled);
        assert!(!state.disallow_charge);
        assert!(state.fixed_time_slots.is_empty());
    }

    #[test]
    fn test_save_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("user_control.json");
        let persistence = UserControlPersistence::new(path);

        let mut state = UserControlState {
            enabled: false,
            disallow_charge: true,
            ..Default::default()
        };
        state.fixed_time_slots.push(FixedTimeSlot::new(
            Utc::now(),
            Utc::now() + Duration::hours(1),
            InverterOperationMode::ForceCharge,
            Some("Test slot".to_string()),
        ));

        persistence.save(&state).unwrap();
        let loaded = persistence.load().unwrap();

        assert!(!loaded.enabled);
        assert!(loaded.disallow_charge);
        assert_eq!(loaded.fixed_time_slots.len(), 1);
        assert_eq!(
            loaded.fixed_time_slots[0].mode,
            InverterOperationMode::ForceCharge
        );
    }

    #[test]
    fn test_expired_slots_cleaned_on_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("user_control.json");
        let persistence = UserControlPersistence::new(&path);

        // Create state with an expired slot
        let mut state = UserControlState::default();
        state.fixed_time_slots.push(FixedTimeSlot::new(
            Utc::now() - Duration::hours(2),
            Utc::now() - Duration::hours(1),
            InverterOperationMode::ForceCharge,
            None,
        ));

        // Write directly to file without cleaning
        let json = serde_json::to_string_pretty(&state).unwrap();
        fs::write(&path, &json).unwrap();

        // Load should clean expired slots
        let loaded = persistence.load().unwrap();
        assert!(loaded.fixed_time_slots.is_empty());
    }
}
