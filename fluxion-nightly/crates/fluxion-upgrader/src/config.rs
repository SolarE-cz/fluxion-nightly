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

//! Configuration module for the upgrader

use crate::error::{Result, UpgraderError};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CONFIG_PATH: &str = "/data/upgrader_config.json";

fn default_true() -> bool {
    true
}

fn default_3600() -> u64 {
    3600
}

fn default_8099() -> u16 {
    8099
}

fn default_6() -> u64 {
    6
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgraderConfig {
    /// Enable automatic updates
    #[serde(default = "default_true")]
    pub auto_update: bool,

    /// Which release branch to track: "staging", "nightly", "stable"
    #[serde(default = "default_nightly")]
    pub release_branch: ReleaseBranch,

    /// GitHub token for private staging repo (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_token: Option<String>,

    /// How often to check for updates (seconds)
    #[serde(default = "default_3600")]
    pub check_interval_secs: u64,

    /// Port where fluxion web UI runs
    #[serde(default = "default_8099")]
    pub fluxion_port: u16,

    /// Max hours to wait for calm time before forcing upgrade
    #[serde(default = "default_6")]
    pub max_calm_wait_hours: u64,

    /// Custom API base URL for testing (overrides default GitHub API)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
}

fn default_nightly() -> ReleaseBranch {
    ReleaseBranch::Nightly
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseBranch {
    Staging,
    #[default]
    Nightly,
    Stable,
}

impl ReleaseBranch {
    pub fn github_repo(&self) -> &str {
        match self {
            Self::Staging => "SolarE-cz/fluxion-staging",
            Self::Nightly => "SolarE-cz/fluxion-nightly",
            Self::Stable => "SolarE-cz/fluxion",
        }
    }

    pub fn is_private(&self) -> bool {
        matches!(self, Self::Staging)
    }
}

impl Default for UpgraderConfig {
    fn default() -> Self {
        Self {
            auto_update: true,
            release_branch: ReleaseBranch::default(),
            staging_token: None,
            check_interval_secs: 3600,
            fluxion_port: 8099,
            max_calm_wait_hours: 6,
            api_base_url: None,
        }
    }
}

pub fn load_config() -> Result<UpgraderConfig> {
    let path = Path::new(CONFIG_PATH);
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| UpgraderError::Config(format!("Failed to parse config: {e}")))
    } else {
        // Create with defaults
        let config = UpgraderConfig::default();
        save_config(&config)?;
        Ok(config)
    }
}

pub fn save_config(config: &UpgraderConfig) -> Result<()> {
    let path = Path::new(CONFIG_PATH);
    let temp_path = path.with_extension("tmp");
    let content = serde_json::to_string_pretty(config)?;

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
    fn test_default_config() {
        let config = UpgraderConfig::default();
        assert!(config.auto_update);
        assert_eq!(config.release_branch, ReleaseBranch::Nightly);
        assert_eq!(config.check_interval_secs, 3600);
        assert_eq!(config.fluxion_port, 8099);
        assert_eq!(config.max_calm_wait_hours, 6);
        assert!(config.staging_token.is_none());
    }

    #[test]
    fn test_branch_to_repo_mapping() {
        assert_eq!(
            ReleaseBranch::Staging.github_repo(),
            "SolarE-cz/fluxion-staging"
        );
        assert_eq!(
            ReleaseBranch::Nightly.github_repo(),
            "SolarE-cz/fluxion-nightly"
        );
        assert_eq!(ReleaseBranch::Stable.github_repo(), "SolarE-cz/fluxion");
    }

    #[test]
    fn test_is_private() {
        assert!(ReleaseBranch::Staging.is_private());
        assert!(!ReleaseBranch::Nightly.is_private());
        assert!(!ReleaseBranch::Stable.is_private());
    }

    #[test]
    fn test_config_roundtrip() {
        let config = UpgraderConfig {
            auto_update: false,
            release_branch: ReleaseBranch::Stable,
            staging_token: Some("test-token".to_string()),
            check_interval_secs: 7200,
            fluxion_port: 9000,
            max_calm_wait_hours: 12,
            api_base_url: None,
        };

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let content = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(path, &content).unwrap();

        let loaded: UpgraderConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(loaded.auto_update, config.auto_update);
        assert_eq!(loaded.release_branch, config.release_branch);
        assert_eq!(loaded.staging_token, config.staging_token);
        assert_eq!(loaded.check_interval_secs, config.check_interval_secs);
        assert_eq!(loaded.fluxion_port, config.fluxion_port);
        assert_eq!(loaded.max_calm_wait_hours, config.max_calm_wait_hours);
    }
}
