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
            Ok(persisted) => {
                info!("✅ Loaded configuration from web UI persistent storage");
                return with_addon_options(persisted.config, Path::new(HA_OPTIONS_PATH));
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

/// A web-UI-saved configuration with the add-on-only options of `options_path` laid over it.
/// Invalid `battery_extra_*` are dropped with a WARN rather than stop the add-on (PR #647 N5).
pub fn with_addon_options(mut config: AppConfig, options_path: &Path) -> Result<AppConfig> {
    overlay_addon_options_file(&mut config, options_path);
    config.drop_invalid_battery_extras();
    config.validate()?;
    Ok(config)
}

/// The add-on options written by the HA Supervisor
pub const HA_OPTIONS_PATH: &str = "/data/options.json";

/// Add-on options that only the add-on UI sets (the web UI has no field for them). On an existing
/// install `/data/config.json` wins over `options.json`, so without this overlay a change of
/// these in the add-on UI would never apply (H33, H34, H35, H37, EEPROM HOLD routing, peak export
/// floor, direct VPP). Each entry is the key's path from the root of the options.
pub const ADDON_ONLY_OPTIONS: &[&[&str]] = &[
    &["control", "battery_wear_cost_czk_per_kwh"],
    &["control", "battery_efficiency"],
    &["solar_forecast", "pv_forecast_correction_enabled"],
    &["pricing", "ote_fallback_enabled"],
    &["pricing", "price_alert_notification"],
    &["pricing", "tomorrow_prices_check_time"],
    &["control", "mode_control_enabled"],
    &["control", "hold_pv_to_battery"],
    &["strategies", "winter_reserve", "export_floor_until_cheap"],
    &[
        "strategies",
        "winter_reserve",
        "direction_reversal_cost_czk",
    ],
    &[
        "strategies",
        "winter_reserve",
        "export_window_continuity_pct",
    ],
    &["strategies", "winter_reserve", "export_selection"],
    &["strategies", "winter_reserve", "daily_export_hours"],
    &["control", "vpp_direct_enabled"],
];

/// Per-inverter add-on-only options (brief H36, pair SOC), applied to the inverter of
/// `config.json` with the same `id` as the add-on option's inverter, with the value a missing key
/// stands for (the Supervisor drops an empty `str?` or list)
pub const ADDON_ONLY_INVERTER_OPTIONS: &[(&str, EmptyAs)] = &[
    ("control_entity_prefix", EmptyAs::String),
    ("battery_extra_prefixes", EmptyAs::List),
    ("battery_extra_capacities_kwh", EmptyAs::List),
];

/// The value of a per-inverter add-on option the Supervisor dropped because it was empty
#[derive(Debug, Clone, Copy)]
pub enum EmptyAs {
    String,
    List,
}

impl EmptyAs {
    fn value(self) -> serde_json::Value {
        match self {
            EmptyAs::String => serde_json::Value::String(String::new()),
            EmptyAs::List => serde_json::Value::Array(Vec::new()),
        }
    }

    /// The add-on option sets something: a non-blank string or a non-empty list
    fn is_set(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(s) => !s.trim().is_empty(),
            serde_json::Value::Array(items) => !items.is_empty(),
            _ => false,
        }
    }
}

fn inverter_option_names() -> String {
    ADDON_ONLY_INVERTER_OPTIONS
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>()
        .join(", ")
}

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
    for path in ADDON_ONLY_OPTIONS {
        let Some(value) = path.iter().try_fold(options, |v, key| v.get(key)) else {
            continue;
        };
        let Some(slot) = path.iter().try_fold(&mut current, |v, key| v.get_mut(key)) else {
            continue;
        };
        if slot != value {
            *slot = value.clone();
            changed.push(path.join("."));
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
/// with the same `id`. A key missing from a matched add-on inverter resets to `""` or `[]`: the
/// Supervisor drops empty `str?` keys and lists, so that is how clearing it in the add-on UI arrives.
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
        // The last entry with this id, compared trimmed: the control map takes the last one too,
        // so both consumers agree when ids repeat (scan #15)
        let Some(inverter) = inverters
            .iter_mut()
            .rev()
            .find(|inv| {
                inv.get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .is_some_and(|config_id| config_id == id.trim())
            })
            .and_then(|inv| inv.as_object_mut())
        else {
            // Only an add-on inverter that sets one of the keys (non-empty) has something that
            // is not applied; one without them is the usual add-on default and stays quiet
            let sets_a_key = ADDON_ONLY_INVERTER_OPTIONS
                .iter()
                .any(|(key, _)| option_inverter.get(*key).is_some_and(EmptyAs::is_set));
            if !sets_a_key {
                continue;
            }
            warnings.push(format!(
                "Add-on inverter '{}' has no match in the web UI config (inverter ids there: {}); its {} not applied",
                id,
                config_ids.join(", "),
                inverter_option_names()
            ));
            continue;
        };
        for (key, empty) in ADDON_ONLY_INVERTER_OPTIONS {
            let value = option_inverter
                .get(*key)
                .cloned()
                .unwrap_or_else(|| empty.value());
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
        saved.strategies.winter_reserve.direction_reversal_cost_czk = 0.5;
        saved.strategies.winter_reserve.export_window_continuity_pct = 1.0;
        saved.strategies.winter_reserve.daily_export_hours = 3;
        saved.strategies.winter_reserve.mode_switch_min_benefit_czk = 1.5;
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({
                "strategies": {"winter_reserve": {
                    "direction_reversal_cost_czk": 3.5,
                    "export_window_continuity_pct": 10.0,
                    "export_selection": "daily_top_n",
                    "daily_export_hours": 8,
                    "mode_switch_min_benefit_czk": 1.0
                }},
                "pricing": {
                    "spot_price_entity": "sensor.from_addon_ui",
                    "ote_fallback_enabled": false,
                    "price_alert_notification": false,
                    "tomorrow_prices_check_time": "15:10"
                },
                "control": {
                    "battery_wear_cost_czk_per_kwh": 0.42,
                    "battery_efficiency": 0.9,
                    "min_battery_soc": 55.0,
                    "hold_pv_to_battery": true
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
        assert!(config.control.hold_pv_to_battery);
        assert!(!config.solar_forecast.pv_forecast_correction_enabled);
        assert!((config.strategies.winter_reserve.direction_reversal_cost_czk - 3.5).abs() < 1e-6);
        assert!(
            (config
                .strategies
                .winter_reserve
                .export_window_continuity_pct
                - 10.0)
                .abs()
                < 1e-6
        );
        assert_eq!(
            config.strategies.winter_reserve.export_selection,
            fluxion_core::ExportSelection::DailyTopN
        );
        assert_eq!(config.strategies.winter_reserve.daily_export_hours, 8);
        // Other keys: config.json still wins, also beside an add-on-only key of the same section
        assert!((config.strategies.winter_reserve.mode_switch_min_benefit_czk - 1.5).abs() < 1e-6);
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

    /// `control.vpp_direct_enabled` from the add-on UI applies over `config.json`, both ways;
    /// options without the key keep the saved value (default on)
    #[test]
    fn addon_vpp_direct_enabled_overrides_the_web_ui_config() {
        let (persistence, dir) = create_test_persistence();
        let mut saved = AppConfig::default();
        assert!(saved.control.vpp_direct_enabled);
        // A config.json saved with the option off
        saved.control.vpp_direct_enabled = false;
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({"control": {"vpp_direct_enabled": true}}).to_string(),
        )
        .unwrap();
        overlay_addon_options_file(&mut config, &options);
        assert!(config.control.vpp_direct_enabled);
        config.validate().unwrap();

        let changed = overlay_addon_options(
            &mut config,
            &serde_json::json!({"control": {"vpp_direct_enabled": false}}),
        );
        assert_eq!(changed, vec!["control.vpp_direct_enabled".to_owned()]);
        assert!(!config.control.vpp_direct_enabled);

        config.control.vpp_direct_enabled = true;
        assert!(overlay_addon_options(&mut config, &serde_json::json!({"control": {}})).is_empty());
        assert!(config.control.vpp_direct_enabled);
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

    /// Review F1 of #647: the nested `strategies.winter_reserve.export_floor_until_cheap` and the
    /// per-inverter pair-SOC lists from the add-on UI apply over `config.json`
    #[test]
    fn addon_peak_export_and_pair_soc_options_override_the_web_ui_config() {
        let (persistence, dir) = create_test_persistence();
        let mut saved = AppConfig::default();
        saved.control.battery_capacity_kwh = 80.0;
        persistence.save(&saved, "web_ui").unwrap();
        let mut config = persistence.load().unwrap().config;
        assert!(!config.strategies.winter_reserve.export_floor_until_cheap);
        let id = config.inverters[0].id.clone();

        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({
                "strategies": {"winter_reserve": {"export_floor_until_cheap": true}},
                "inverters": [
                    {"id": id, "vendor": "solax", "entity_prefix": "from_addon",
                     "topology": "independent",
                     "battery_extra_prefixes": ["solax_81"],
                     "battery_extra_capacities_kwh": [30.0]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        overlay_addon_options_file(&mut config, &options);

        assert!(config.strategies.winter_reserve.export_floor_until_cheap);
        assert_eq!(config.inverters[0].battery_extra_prefixes, vec!["solax_81"]);
        assert_eq!(config.inverters[0].battery_extra_capacities_kwh, vec![30.0]);
        // Other inverter keys: config.json still wins
        assert_eq!(
            config.inverters[0].entity_prefix,
            saved.inverters[0].entity_prefix
        );
        config.validate().unwrap();

        // Cleared in the add-on UI: the Supervisor drops the lists, and they clear here too
        let changed =
            overlay_addon_options(&mut config, &serde_json::json!({"inverters": [{"id": id}]}));
        assert!(changed.contains(&format!("inverters[{id}].battery_extra_prefixes")));
        assert!(config.inverters[0].battery_extra_prefixes.is_empty());
        assert!(config.inverters[0].battery_extra_capacities_kwh.is_empty());
        // A missing nested key leaves the saved value alone
        assert!(config.strategies.winter_reserve.export_floor_until_cheap);
    }

    /// PR #647 N5 / R3-4: on an install with a web-UI config, add-on options with invalid
    /// extras (lengths differ) load: the extras are dropped with one WARN, the rest applies
    #[test]
    fn addon_options_with_invalid_battery_extras_start_without_them() {
        let (persistence, dir) = create_test_persistence();
        let mut saved = AppConfig::default();
        saved.control.battery_capacity_kwh = 80.0;
        persistence.save(&saved, "web_ui").unwrap();
        let config = persistence.load().unwrap().config;
        let id = config.inverters[0].id.clone();
        let options = dir.path().join("options.json");
        fs::write(
            &options,
            serde_json::json!({
                "strategies": {"winter_reserve": {"export_floor_until_cheap": true}},
                "inverters": [
                    {"id": id, "battery_extra_prefixes": ["solax_81"],
                     "battery_extra_capacities_kwh": []}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut loaded = None;
        let logs = crate::config::tests::captured_logs(|| {
            loaded = Some(with_addon_options(config, &options));
        });
        let config = loaded.unwrap().expect("the add-on starts");
        assert!(config.inverters[0].battery_extra_prefixes.is_empty());
        assert!(config.strategies.winter_reserve.export_floor_until_cheap);
        assert_eq!(
            logs.matches("ignored, the SOC is the inverter's own")
                .count(),
            1,
            "{logs}"
        );
    }

    /// Scan #15: two inverters with one id (the second written with a space): the add-on overlay
    /// and the control map both use the last entry, and startup validation logs and carries on
    #[test]
    fn duplicate_inverter_ids_overlay_and_control_map_pick_the_same_entry() {
        let mut config = AppConfig::default();
        config.inverters[0].control_entity_prefix = "solax_first".to_owned();
        let mut second = config.inverters[0].clone();
        second.id = format!(" {}", second.id);
        second.control_entity_prefix = "solax_second".to_owned();
        config.inverters.push(second);
        let id = config.inverters[0].id.clone();
        let options = serde_json::json!({
            "inverters": [{"id": id, "control_entity_prefix": "solax_addon"}]
        });

        let mut overlaid = config.clone();
        overlay_addon_options(&mut overlaid, &options);

        assert_eq!(overlaid.inverters[0].control_entity_prefix, "solax_first");
        assert_eq!(overlaid.inverters[1].control_entity_prefix, "solax_addon");
        assert_eq!(overlaid.control_prefixes().resolve(&id), "solax_addon");
    }

    /// Scan #15: startup with duplicate ids logs the id and still starts
    #[test]
    fn duplicate_inverter_ids_do_not_stop_startup() {
        let dir = TempDir::new().unwrap();
        let options = dir.path().join("options.json");
        fs::write(&options, "{}").unwrap();
        let mut config = AppConfig::default();
        let mut second = config.inverters[0].clone();
        second.id = config.inverters[0].id.clone();
        config.inverters.push(second);
        let id = config.inverters[0].id.clone();

        let mut loaded = None;
        let logs = crate::config::tests::captured_logs(|| {
            loaded = Some(with_addon_options(config, &options));
        });

        assert!(loaded.unwrap().is_ok());
        assert!(
            logs.contains(&format!(
                "Inverter id \"{id}\" is used by more than one inverter"
            )),
            "{logs}"
        );
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
                "Add-on inverter 'renamed' has no match in the web UI config (inverter ids there: {}); its {} not applied",
                config.inverters[0].id,
                inverter_option_names()
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
