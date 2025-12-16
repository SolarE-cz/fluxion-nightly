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

use super::AppConfig;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

/// Configuration metadata for tracking changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigMetadata {
    /// When the configuration was last modified
    pub last_modified: DateTime<Utc>,
    /// Source of modification (web_ui, startup, migration, etc.)
    pub modified_by: String,
    /// Configuration format version
    pub version: String,
}

impl Default for ConfigMetadata {
    fn default() -> Self {
        Self {
            last_modified: Utc::now(),
            modified_by: "startup".to_string(),
            version: "1.0.0".to_string(),
        }
    }
}

/// Configuration with metadata wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub config: AppConfig,
    pub metadata: ConfigMetadata,
}

/// Configuration persistence manager
pub struct ConfigPersistence {
    /// Primary config file path (/data/config.json)
    config_path: PathBuf,
}

impl ConfigPersistence {
    /// Create a new config persistence manager
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        let config_path = config_path.into();

        Self { config_path }
    }

    /// Default persistence manager for production (/data/config.json)
    pub fn default_production() -> Self {
        Self::new("/data/config.json")
    }

    /// Load configuration from persistent storage
    ///
    /// # Errors
    /// Returns error if file cannot be read or parsed
    pub fn load(&self) -> Result<PersistedConfig> {
        let contents = fs::read_to_string(&self.config_path).context(format!(
            "Failed to read config from {}",
            self.config_path.display()
        ))?;

        let persisted: PersistedConfig =
            serde_json::from_str(&contents).context("Failed to parse config JSON")?;

        info!(
            "✅ Loaded configuration from {}",
            self.config_path.display()
        );
        Ok(persisted)
    }

    /// Save configuration to persistent storage
    ///
    /// # Errors
    /// Returns error if file cannot be written
    pub fn save(&self, config: &AppConfig, modified_by: &str) -> Result<()> {
        // Validate before saving
        config.validate()?;

        let persisted = PersistedConfig {
            config: config.clone(),
            metadata: ConfigMetadata {
                last_modified: Utc::now(),
                modified_by: modified_by.to_string(),
                version: "1.0.0".to_string(),
            },
        };

        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent).context(format!(
                "Failed to create config directory {}",
                parent.display()
            ))?;
        }

        let json = serde_json::to_string_pretty(&persisted)?;
        let temp_path = self.config_path.with_extension("bak");
        fs::write(&temp_path, json).context(format!(
            "Failed to write config to {}",
            self.config_path.display()
        ))?;
        fs::rename(&temp_path, &self.config_path)?;

        info!(
            "✅ Saved configuration to {} (modified by: {modified_by})",
            self.config_path.display()
        );
        Ok(())
    }
    /// Check if persistent config exists
    pub fn exists(&self) -> bool {
        self.config_path.exists()
    }
}

/// Load configuration with fallback logic
///
/// 1. Try /data/config.json (web UI config)
/// 2. Try /data/options.json (HA addon options)
/// 3. Try config.toml (development)
/// 4. Try config.json (development)
/// 5. Fall back to defaults with env overrides
///
/// # Errors
/// Returns error if configuration is invalid
pub fn load_config_with_fallback() -> Result<AppConfig> {
    let persistence = ConfigPersistence::default_production();

    // Try web UI persistent config first
    if persistence.exists() {
        match persistence.load() {
            Ok(persisted) => {
                info!("✅ Loaded configuration from web UI persistent storage");
                persisted.config.validate()?;
                return Ok(persisted.config);
            }
            Err(e) => {
                warn!("Failed to load web UI config, trying HA options: {e}");
            }
        }
    }

    // Fall back to original loading logic
    let config = AppConfig::load()?;

    // Save to persistent storage for future web UI edits
    if !persistence.exists() {
        info!("💾 Creating initial web UI config from loaded configuration");
        if let Err(e) = persistence.save(&config, "initial") {
            warn!("Failed to save initial web UI config: {e}");
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_persistence() -> (ConfigPersistence, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        let persistence = ConfigPersistence::new(config_path);
        (persistence, temp_dir)
    }

    #[test]
    fn test_save_and_load() {
        let (persistence, _temp_dir) = create_test_persistence();
        let config = AppConfig::default();

        // Save
        persistence.save(&config, "test").unwrap();
        assert!(persistence.exists());

        // Load
        let loaded = persistence.load().unwrap();
        assert_eq!(loaded.metadata.modified_by, "test");
        assert_eq!(loaded.config.system.debug_mode, config.system.debug_mode);
    }
}
