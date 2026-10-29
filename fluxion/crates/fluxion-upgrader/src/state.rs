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

//! State persistence module for the upgrader

use crate::error::{Result, UpgraderError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

const STATE_PATH: &str = "/data/upgrader_state.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpgraderState {
    /// Currently running fluxion version (semver string)
    pub current_version: Option<String>,

    /// Last time we checked for updates
    pub last_check_at: Option<DateTime<Utc>>,

    /// Last successful upgrade timestamp
    pub last_upgrade_at: Option<DateTime<Utc>>,

    /// Timestamp when current version started running successfully
    pub upgrade_success_since: Option<DateTime<Utc>>,

    /// Version in the backup (if any)
    pub backup_version: Option<String>,

    /// Path to current backup directory
    pub backup_path: Option<String>,

    /// Number of consecutive upgrade failures
    pub consecutive_failures: u32,

    /// Version that keeps failing (skip after 3 failures)
    pub failed_version: Option<String>,
}

pub fn load_state() -> Result<UpgraderState> {
    let path = Path::new(STATE_PATH);
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| {
            UpgraderError::State(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse state: {e}"),
            ))
        })
    } else {
        // Create with defaults
        let state = UpgraderState::default();
        save_state(&state)?;
        Ok(state)
    }
}

pub fn save_state(state: &UpgraderState) -> Result<()> {
    let path = Path::new(STATE_PATH);
    let temp_path = path.with_extension("tmp");
    let content = serde_json::to_string_pretty(state)?;

    // Atomic write
    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_state() {
        let state = UpgraderState::default();
        assert!(state.current_version.is_none());
        assert!(state.last_check_at.is_none());
        assert!(state.last_upgrade_at.is_none());
        assert!(state.upgrade_success_since.is_none());
        assert!(state.backup_version.is_none());
        assert!(state.backup_path.is_none());
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.failed_version.is_none());
    }

    #[test]
    fn test_state_roundtrip() {
        let state = UpgraderState {
            current_version: Some("0.2.38".to_string()),
            last_check_at: Some(Utc::now()),
            last_upgrade_at: Some(Utc::now()),
            upgrade_success_since: Some(Utc::now()),
            backup_version: Some("0.2.37".to_string()),
            backup_path: Some("/data/backups/2024-01-15T10:30:00Z".to_string()),
            consecutive_failures: 1,
            failed_version: Some("0.2.39".to_string()),
        };

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(path, &content).unwrap();

        let loaded: UpgraderState =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(loaded.current_version, state.current_version);
        assert_eq!(loaded.consecutive_failures, state.consecutive_failures);
        assert_eq!(loaded.failed_version, state.failed_version);
    }

    #[test]
    fn test_atomic_state_save() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Simulate atomic save logic
        let temp_path = path.with_extension("tmp");
        let state = UpgraderState::default();

        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&temp_path, &content).unwrap();
        std::fs::rename(&temp_path, &path).unwrap();

        // Verify temp file was removed
        assert!(!temp_path.exists());
        assert!(path.exists());
    }
}
