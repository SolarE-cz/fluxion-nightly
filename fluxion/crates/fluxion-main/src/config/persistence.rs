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
use std::path::{Path, PathBuf};
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
            Ok(mut persisted) => {
                info!("✅ Loaded configuration from web UI persistent storage");
                overlay_addon_options_file(&mut persisted.config, Path::new(HA_OPTIONS_PATH));
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

/// The add-on options written by the HA Supervisor
pub const HA_OPTIONS_PATH: &str = "/data/options.json";

/// Add-on options that only the add-on UI sets (the web UI has no field for them). On an existing
/// install `/data/config.json` wins over `options.json`, so without this overlay a change of
/// these in the add-on UI would never apply (H33, H34, H35, H37).
pub const ADDON_ONLY_OPTIONS: &[(&str, &str)] = &[
    ("control", "battery_wear_cost_czk_per_kwh"),
    ("control", "battery_efficiency"),
    ("solar_forecast", "pv_forecast_correction_enabled"),
    ("pricing", "ote_fallback_enabled"),
    ("pricing", "price_alert_notification"),
    ("pricing", "tomorrow_prices_check_time"),
    ("control", "mode_control_enabled"),
];

/// Per-inverter add-on-only options (brief H36), applied to the inverter of `config.json` with
/// the same `id` as the add-on option's inverter
pub const ADDON_ONLY_INVERTER_OPTIONS: &[&str] = &["control_entity_prefix"];

/// Apply the add-on-only options of `path` (HA `options.json`) over `config`; a missing or
/// unreadable file changes nothing
pub fn overlay_addon_options_file(config: &mut AppConfig, path: &Path) {
    let options = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!("Add-on options {} unreadable: {e}", path.display());
            return;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&options) {
        Ok(options) => {
            overlay_addon_options(config, &options);
        }
        Err(e) => warn!("Add-on options {} invalid: {e}", path.display()),
    }
}

/// Set each of [`ADDON_ONLY_OPTIONS`] present in `options` on `config`; returns the keys that
/// changed. Other keys keep the precedence of `config.json`.
pub fn overlay_addon_options(config: &mut AppConfig, options: &serde_json::Value) -> Vec<String> {
    let mut current = match serde_json::to_value(&*config) {
        Ok(value) => value,
        Err(e) => {
            warn!("Add-on options not applied: {e}");
            return Vec::new();
        }
    };
    let mut changed = Vec::new();
    for (section, key) in ADDON_ONLY_OPTIONS {
        let Some(value) = options.get(section).and_then(|s| s.get(key)) else {
            continue;
        };
        let Some(slot) = current.get_mut(section).and_then(|s| s.get_mut(key)) else {
            continue;
        };
        if slot != value {
            *slot = value.clone();
            changed.push(format!("{section}.{key}"));
        }
    }
    for warning in overlay_inverter_options(&mut current, options, &mut changed) {
        warn!("⚠️ {warning}");
    }
    if changed.is_empty() {
        return changed;
    }
    match serde_json::from_value::<AppConfig>(current) {
        Ok(updated) => {
            info!(
                "⚙️ Add-on options applied over the web UI config: {}",
                changed.join(", ")
            );
            *config = updated;
            changed
        }
        Err(e) => {
            warn!("Add-on options not applied ({}): {e}", changed.join(", "));
            Vec::new()
        }
    }
}

/// Set each of [`ADDON_ONLY_INVERTER_OPTIONS`] of the add-on's inverters on the `config` inverter
/// with the same `id`. A key missing from a matched add-on inverter resets to `""`: the
/// Supervisor drops empty `str?` keys, so that is how clearing it in the add-on UI arrives.
/// An add-on inverter without a match in `config.json` is left alone; when it sets a non-empty
/// key, the returned warnings name it and the ids that `config.json` has.
fn overlay_inverter_options(
    current: &mut serde_json::Value,
    options: &serde_json::Value,
    changed: &mut Vec<String>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(option_inverters) = options.get("inverters").and_then(|v| v.as_array()) else {
        return warnings;
    };
    let Some(inverters) = current.get_mut("inverters").and_then(|v| v.as_array_mut()) else {
        return warnings;
    };
    for option_inverter in option_inverters {
        let Some(id) = option_inverter.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let config_ids: Vec<String> = inverters
            .iter()
            .filter_map(|inv| inv.get("id").and_then(|v| v.as_str()).map(str::to_owned))
            .collect();
        let Some(inverter) = inverters
            .iter_mut()
            .find(|inv| inv.get("id").and_then(|v| v.as_str()) == Some(id))
            .and_then(|inv| inv.as_object_mut())
        else {
            // Only an add-on inverter that sets one of the keys (non-empty) has something that
            // is not applied; one without them is the usual add-on default and stays quiet
            let sets_a_key = ADDON_ONLY_INVERTER_OPTIONS.iter().any(|key| {
                option_inverter
                    .get(*key)
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| !v.trim().is_empty())
            });
            if !sets_a_key {
                continue;
            }
            warnings.push(format!(
                "Add-on inverter '{}' has no match in the web UI config (inverter ids there: {}); its {} not applied",
                id,
                config_ids.join(", "),
                ADDON_ONLY_INVERTER_OPTIONS.join(", ")
            ));
            continue;
        };
        for key in ADDON_ONLY_INVERTER_OPTIONS {
            let value = option_inverter
                .get(*key)
                .cloned()
                .unwrap_or_else(|| serde_json::Value::String(String::new()));
            if inverter.get(*key) != Some(&value) {
                inverter.insert((*key).to_owned(), value);
                changed.push(format!("inverters[{id}].{key}"));
            }
        }
    }
    warnings
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

    /// H33/H34/H35: on an existing install the add-on UI's values of the add-on-only options
    /// apply over `/data/config.json`; other keys keep the precedence of `config.json`
    #[test]
    fn addon_only_options_override_the_web_ui_config() {
        let (persistence, dir) = create_test_persistence();
        let mut saved = AppConfig::default();
        saved.pricing.spot_price_entity = "sensor.from_web_ui".to_owned();
        saved.pricing.buy_price_entity = Some("sensor.buy_from_web_ui".to_owned());
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({
                "pricing": {
                    "spot_price_entity": "sensor.from_addon_ui",
                    "ote_fallback_enabled": false,
                    "price_alert_notification": false,
                    "tomorrow_prices_check_time": "15:10"
                },
                "control": {
                    "battery_wear_cost_czk_per_kwh": 0.42,
                    "battery_efficiency": 0.9,
                    "min_battery_soc": 55.0
                },
                "solar_forecast": {"pv_forecast_correction_enabled": false}
            })
            .to_string(),
        )
        .unwrap();
        overlay_addon_options_file(&mut config, &options);

        assert!(!config.pricing.ote_fallback_enabled);
        assert!(!config.pricing.price_alert_notification);
        assert_eq!(config.pricing.tomorrow_prices_check_time, "15:10");
        assert!((config.control.battery_wear_cost_czk_per_kwh - 0.42).abs() < 1e-6);
        assert!((config.control.battery_efficiency - 0.9).abs() < 1e-6);
        assert!(!config.solar_forecast.pv_forecast_correction_enabled);
        // Other keys: config.json still wins
        assert_eq!(config.pricing.spot_price_entity, "sensor.from_web_ui");
        assert_eq!(
            config.pricing.buy_price_entity.as_deref(),
            Some("sensor.buy_from_web_ui")
        );
        assert!((config.control.min_battery_soc - saved.control.min_battery_soc).abs() < 1e-6);
        config.validate().unwrap();
    }

    /// H37: `control.mode_control_enabled` from the add-on UI applies over `config.json`, both
    /// ways; options without the key keep the saved value
    #[test]
    fn addon_mode_control_enabled_overrides_the_web_ui_config() {
        let (persistence, dir) = create_test_persistence();
        let saved = AppConfig::default();
        assert!(saved.control.mode_control_enabled);
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({"control": {"mode_control_enabled": false}}).to_string(),
        )
        .unwrap();
        overlay_addon_options_file(&mut config, &options);
        assert!(!config.control.mode_control_enabled);
        config.validate().unwrap();

        let changed = overlay_addon_options(
            &mut config,
            &serde_json::json!({"control": {"mode_control_enabled": true}}),
        );
        assert_eq!(changed, vec!["control.mode_control_enabled".to_owned()]);
        assert!(config.control.mode_control_enabled);

        config.control.mode_control_enabled = false;
        assert!(overlay_addon_options(&mut config, &serde_json::json!({"control": {}})).is_empty());
        assert!(!config.control.mode_control_enabled);
    }

    /// H36: the add-on UI's `inverters[].control_entity_prefix` applies over `config.json`,
    /// matched by inverter id; other inverter keys keep the precedence of `config.json`
    #[test]
    fn addon_control_entity_prefix_overrides_the_web_ui_config() {
        let (persistence, dir) = create_test_persistence();
        let saved = AppConfig::default();
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;
        let id = config.inverters[0].id.clone();

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({
                "inverters": [
                    {"id": id, "vendor": "solax", "entity_prefix": "from_addon",
                     "topology": "independent", "control_entity_prefix": "solax_inverter"},
                    {"id": "not_configured", "control_entity_prefix": "other"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        overlay_addon_options_file(&mut config, &options);

        assert_eq!(config.inverters.len(), 1);
        assert_eq!(config.inverters[0].control_entity_prefix, "solax_inverter");
        assert_eq!(
            config.inverters[0].entity_prefix,
            saved.inverters[0].entity_prefix
        );
        config.validate().unwrap();
    }

    /// Review nit 1: the Supervisor drops an empty `str?` key, so a matched add-on inverter
    /// without `control_entity_prefix` clears the value a web UI save wrote into config.json
    #[test]
    fn addon_inverter_without_control_entity_prefix_clears_it() {
        let mut config = AppConfig::default();
        config.inverters[0].control_entity_prefix = "solax_inverter".to_owned();
        let id = config.inverters[0].id.clone();

        let changed = overlay_addon_options(
            &mut config,
            &serde_json::json!({"inverters": [
                {"id": id, "vendor": "solax", "entity_prefix": "solax", "topology": "independent"}
            ]}),
        );

        assert_eq!(
            changed,
            vec![format!("inverters[{id}].control_entity_prefix")]
        );
        assert_eq!(config.inverters[0].control_entity_prefix, "");
        assert_eq!(config.inverters[0].control_prefix_override(), None);
    }

    /// Review nit 2: an add-on inverter whose id is not in config.json changes nothing
    #[test]
    fn addon_inverter_without_a_match_changes_nothing() {
        let mut config = AppConfig::default();
        config.inverters[0].control_entity_prefix = "solax_inverter".to_owned();
        let before = serde_json::to_value(&config).unwrap();

        let options =
            serde_json::json!({"inverters": [{"id": "renamed", "control_entity_prefix": "x"}]});
        let mut current = before.clone();
        let mut changed = Vec::new();
        let warnings = overlay_inverter_options(&mut current, &options, &mut changed);
        assert_eq!(
            warnings,
            vec![format!(
                "Add-on inverter 'renamed' has no match in the web UI config (inverter ids there: {}); its control_entity_prefix not applied",
                config.inverters[0].id
            )]
        );
        assert!(changed.is_empty());
        assert_eq!(current, before);

        assert!(overlay_addon_options(&mut config, &options).is_empty());
        assert_eq!(serde_json::to_value(&config).unwrap(), before);

        // H36 review: an unmatched add-on inverter without the key, or with it empty, has
        // nothing to apply and does not warn
        for options in [
            serde_json::json!({"inverters": [{"id": "renamed"}]}),
            serde_json::json!({"inverters": [{"id": "renamed", "control_entity_prefix": " "}]}),
        ] {
            let mut current = before.clone();
            let mut changed = Vec::new();
            assert!(overlay_inverter_options(&mut current, &options, &mut changed).is_empty());
            assert!(changed.is_empty());
            assert_eq!(current, before);
        }
    }

    #[test]
    fn missing_or_invalid_addon_options_change_nothing() {
        let (_, dir) = create_test_persistence();
        let mut config = AppConfig::default();
        let before = serde_json::to_value(&config).unwrap();

        overlay_addon_options_file(&mut config, &dir.path().join("absent.json"));
        let garbage = dir.path().join("options.json");
        fs::write(&garbage, "{not json").unwrap();
        overlay_addon_options_file(&mut config, &garbage);
        // Keys absent from the options keep the config value; a wrong type is not applied
        let changed = overlay_addon_options(
            &mut config,
            &serde_json::json!({"pricing": {"ote_fallback_enabled": "maybe"}}),
        );
        assert!(changed.is_empty());
        assert_eq!(serde_json::to_value(&config).unwrap(), before);
    }
}
