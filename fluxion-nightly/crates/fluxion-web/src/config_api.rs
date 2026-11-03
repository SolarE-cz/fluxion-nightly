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

use crate::validation;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use fluxion_core::resources::SystemConfig;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

/// Shared state for config API endpoints
#[derive(Clone)]
pub struct ConfigApiState {
    /// Configuration JSON stored in memory
    pub config: Arc<RwLock<serde_json::Value>>,
    /// Configuration the process started with (startup-only settings are compared against it)
    pub startup_config: serde_json::Value,
    /// Path to persistent config file
    pub config_path: String,
    /// Sender for config update events to ECS
    pub config_update_sender: Option<fluxion_core::ConfigUpdateSender>,
}

impl std::fmt::Debug for ConfigApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigApiState")
            .field("config", &"<RwLock>")
            .field("startup_config", &"<Value>")
            .field("config_path", &self.config_path)
            .field("config_update_sender", &self.config_update_sender.is_some())
            .finish()
    }
}

impl ConfigApiState {
    /// Create new config API state
    pub fn new(
        config: serde_json::Value,
        config_path: impl Into<String>,
        config_update_sender: Option<fluxion_core::ConfigUpdateSender>,
    ) -> Self {
        Self {
            startup_config: config.clone(),
            config: Arc::new(RwLock::new(config)),
            config_path: config_path.into(),
            config_update_sender,
        }
    }
}

/// Response for GET /api/config
#[derive(Serialize)]
pub struct ConfigResponse {
    /// Current configuration
    pub config: serde_json::Value,
    /// Configuration metadata
    pub metadata: ConfigMetadataResponse,
}

/// Configuration metadata
#[derive(Serialize)]
pub struct ConfigMetadataResponse {
    /// When the config was last modified (in persistent storage)
    pub last_modified: Option<String>,
    /// Who modified it last
    pub modified_by: Option<String>,
    /// Config version
    pub version: String,
    /// Whether a restart is needed to apply pending changes
    pub restart_required: bool,
}

/// Request body for POST /api/config/validate
#[derive(Deserialize)]
pub struct ValidateRequest {
    /// Configuration to validate
    pub config: serde_json::Value,
}

/// Response for POST /api/config/validate
#[derive(Serialize)]
pub struct ValidateResponse {
    /// Whether the configuration is valid
    pub valid: bool,
    /// Validation errors
    pub errors: Vec<ValidationIssue>,
    /// Validation warnings
    pub warnings: Vec<ValidationIssue>,
    /// Whether a restart is required for this config
    pub restart_required: bool,
}

/// A validation issue
#[derive(Serialize)]
pub struct ValidationIssue {
    /// Field path (e.g., "control.min_battery_soc")
    pub field: String,
    /// Human-readable message
    pub message: String,
    /// Severity (error or warning)
    pub severity: String,
}

/// Request body for POST /api/config/update
#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    /// New configuration
    pub config: serde_json::Value,
    /// Whether to create a backup before updating
    #[serde(default = "default_create_backup")]
    pub create_backup: bool,
}

fn default_create_backup() -> bool {
    true
}

/// Response for POST /api/config/update
#[derive(Serialize)]
pub struct UpdateConfigResponse {
    /// Whether the update was successful
    pub success: bool,
    /// Validation result
    pub validation: ValidateResponse,
    /// Backup ID if backup was created
    pub backup_id: Option<String>,
    /// Whether the config was applied to runtime
    pub applied: bool,
    /// Whether a restart is required
    pub restart_required: bool,
    /// Error message if update failed
    pub error: Option<String>,
}

/// Request body for POST /api/config/reset
#[derive(Deserialize)]
pub struct ResetSectionRequest {
    /// Section to reset (system, inverters, pricing, control, strategies)
    #[expect(dead_code, reason = "Section reset not yet implemented")]
    pub section: String,
}

/// GET /api/config - Get current configuration
pub async fn get_config_handler(
    State(state): State<ConfigApiState>,
) -> Result<Json<ConfigResponse>, StatusCode> {
    let config = state.config.read().clone();

    // Try to read metadata from persistent storage
    let (last_modified, modified_by) =
        if let Ok(contents) = std::fs::read_to_string(&state.config_path) {
            if let Ok(persisted) = serde_json::from_str::<serde_json::Value>(&contents) {
                let metadata = persisted.get("metadata");
                (
                    metadata
                        .and_then(|m| m.get("last_modified"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                    metadata
                        .and_then(|m| m.get("modified_by"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

    Ok(Json(ConfigResponse {
        config,
        metadata: ConfigMetadataResponse {
            last_modified,
            modified_by,
            version: "1.0.0".to_owned(),
            restart_required: false,
        },
    }))
}

/// POST /api/config/validate - Validate configuration without saving
pub async fn validate_config_handler(
    State(state): State<ConfigApiState>,
    Json(request): Json<ValidateRequest>,
) -> Json<ValidateResponse> {
    // Merge partial config with current config
    let mut config_to_validate = state.config.read().clone();
    validation::merge_json(&mut config_to_validate, request.config);

    // Parse as SystemConfig and validate
    let validation = match serde_json::from_value::<SystemConfig>(config_to_validate) {
        Ok(config) => {
            let (errors, warnings) = validation::validate_config(&config);
            ValidateResponse {
                valid: errors.is_empty(),
                errors,
                warnings,
                restart_required: false, // TODO: Check if restart needed based on changed fields
            }
        }
        Err(e) => ValidateResponse {
            valid: false,
            errors: vec![ValidationIssue {
                field: "config".to_owned(),
                message: format!("Invalid configuration structure: {e}"),
                severity: "error".to_owned(),
            }],
            warnings: Vec::new(),
            restart_required: false,
        },
    };

    Json(validation)
}

/// Brief H37: `control.mode_control_enabled` is fixed for the add-on's run (the inverter adapter
/// takes it at startup). An update that sets a different value is refused with this issue.
#[must_use]
pub fn mode_control_change(
    current: &serde_json::Value,
    update: &serde_json::Value,
) -> Option<ValidationIssue> {
    let requested = update
        .get("control")
        .and_then(|c| c.get("mode_control_enabled"))?;
    let running = current
        .get("control")
        .and_then(|c| c.get("mode_control_enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if requested.as_bool() == Some(running) {
        return None;
    }
    Some(ValidationIssue {
        field: "control.mode_control_enabled".to_owned(),
        message: format!(
            "control.mode_control_enabled cannot change at runtime (running with {running}); set it in the add-on configuration and restart the add-on"
        ),
        severity: "error".to_owned(),
    })
}

/// Settings the web API never writes: connection destinations, credentials and the heartbeat
/// endpoint (findings 34, 37)
const WEB_PROTECTED_KEYS: &[&str] = &["ha_base_url", "ha_token", "server_heartbeat"];

/// Keep only the parts of a web update that the typed config has, and none of the protected keys,
/// at any depth. `dropped` collects the dotted paths that were removed.
fn filter_update(
    update: serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    dropped: &mut Vec<String>,
) -> Option<serde_json::Value> {
    match (update, schema) {
        (serde_json::Value::Object(fields), serde_json::Value::Object(schema_fields)) => {
            let mut kept = serde_json::Map::new();
            for (key, value) in fields {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match schema_fields.get(&key) {
                    Some(child) if !WEB_PROTECTED_KEYS.contains(&key.as_str()) => {
                        if let Some(filtered) = filter_update(value, child, &child_path, dropped) {
                            kept.insert(key, filtered);
                        }
                    }
                    _ => dropped.push(child_path),
                }
            }
            Some(serde_json::Value::Object(kept))
        }
        (serde_json::Value::Array(items), serde_json::Value::Array(schema_items)) => {
            // merge_json replaces arrays whole, so element i is checked against typed element i
            // (an externally tagged topology differs per element)
            let kept = items
                .into_iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let item_path = format!("{path}[{i}]");
                    match schema_items.get(i) {
                        Some(template) => filter_update(item, template, &item_path, dropped),
                        // No typed element at this index: scalars pass, objects cannot be checked
                        None if item.is_object() => {
                            dropped.push(item_path);
                            None
                        }
                        None => Some(item),
                    }
                })
                .collect();
            Some(serde_json::Value::Array(kept))
        }
        // An object or array where the typed config has a scalar (or nothing) is not a known path
        (serde_json::Value::Object(_) | serde_json::Value::Array(_), _) => {
            dropped.push(path.to_owned());
            None
        }
        // Scalars are checked by typed validation before this runs
        (leaf, _) => Some(leaf),
    }
}

/// The `config` object of the persisted file, if the file exists and parses
fn read_stored_config(path: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut file: serde_json::Value = serde_json::from_str(&text).ok()?;
    file.get_mut("config").map(serde_json::Value::take)
}

/// Scan #46, #49: `control.auto_unlock_inverter` (the inverter actuation guard) is taken once at
/// startup, and no runtime channel reaches the adapter. An update that changes it is refused until
/// the add-on restarts, as for `mode_control_enabled` (brief H37). It has no UI field.
#[must_use]
pub fn adapter_restart_change(
    current: &serde_json::Value,
    update: &serde_json::Value,
) -> Option<ValidationIssue> {
    let requested = update.get("control")?.get("auto_unlock_inverter")?;
    let running = current
        .get("control")
        .and_then(|c| c.get("auto_unlock_inverter"));
    if running.is_some_and(|running| same_setting(running, requested)) {
        return None;
    }
    let shown = running.map_or_else(|| "unknown".to_owned(), ToString::to_string);
    Some(ValidationIssue {
        field: "control.auto_unlock_inverter".to_owned(),
        message: format!(
            "control.auto_unlock_inverter cannot change at runtime (running with {shown}); set it in the add-on configuration and restart the add-on"
        ),
        severity: "error".to_owned(),
    })
}

/// Scan #46: `control.max_battery_soc` is accepted (the planner uses it at once), but the Back Up
/// reserve limit keeps the startup value until the add-on restarts. Warning only.
#[must_use]
pub fn max_battery_soc_warning(
    current: &serde_json::Value,
    update: &serde_json::Value,
) -> Option<ValidationIssue> {
    let requested = update.get("control")?.get("max_battery_soc")?;
    let running = current
        .get("control")
        .and_then(|c| c.get("max_battery_soc"));
    if running.is_some_and(|running| same_setting(running, requested)) {
        return None;
    }
    Some(ValidationIssue {
        field: "control.max_battery_soc".to_owned(),
        message: "control.max_battery_soc: the Back Up reserve limit keeps the startup value until the add-on restarts".to_owned(),
        severity: "warning".to_owned(),
    })
}

/// The first update refused at runtime: `mode_control_enabled` (brief H37), then the startup-only
/// adapter settings (scan #46, #49)
fn startup_only_change(
    current: &serde_json::Value,
    update: &serde_json::Value,
) -> Option<ValidationIssue> {
    mode_control_change(current, update).or_else(|| adapter_restart_change(current, update))
}

/// Equal settings; numbers compare by value, so `90` and `90.0` are the same
fn same_setting(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a.total_cmp(&b).is_eq(),
        _ => a == b,
    }
}

/// The response to an update refused because of `issue` (nothing is saved)
fn refused_update(issue: ValidationIssue) -> UpdateConfigResponse {
    let error = issue.message.clone();
    UpdateConfigResponse {
        success: false,
        validation: ValidateResponse {
            valid: false,
            errors: vec![issue],
            warnings: Vec::new(),
            restart_required: true,
        },
        backup_id: None,
        applied: false,
        restart_required: true,
        error: Some(error),
    }
}

/// POST /api/config/update - Update configuration
#[expect(clippy::too_many_lines)]
pub async fn update_config_handler(
    State(state): State<ConfigApiState>,
    Json(request): Json<UpdateConfigRequest>,
) -> Result<Json<UpdateConfigResponse>, StatusCode> {
    // Mode control and the startup-only adapter settings are fixed until the add-on restarts
    if let Some(issue) = startup_only_change(&state.config.read(), &request.config) {
        info!("Config update refused: {}", issue.message);
        return Ok(Json(refused_update(issue)));
    }

    // Merge partial config with current config
    let mut config_to_validate = state.config.read().clone();
    validation::merge_json(&mut config_to_validate, request.config.clone());

    // Validate the new configuration
    let startup_warning = max_battery_soc_warning(&state.startup_config, &request.config);
    let mut schema = serde_json::Value::Null;
    let validation = match serde_json::from_value::<SystemConfig>(config_to_validate) {
        Ok(config) => {
            schema = serde_json::to_value(&config).unwrap_or(serde_json::Value::Null);
            let (errors, mut warnings) = validation::validate_config(&config);
            warnings.extend(startup_warning);
            ValidateResponse {
                valid: errors.is_empty(),
                errors,
                warnings,
                restart_required: false, // TODO: Check if restart needed
            }
        }
        Err(e) => ValidateResponse {
            valid: false,
            errors: vec![ValidationIssue {
                field: "config".to_owned(),
                message: format!("Invalid configuration structure: {e}"),
                severity: "error".to_owned(),
            }],
            warnings: Vec::new(),
            restart_required: false,
        },
    };

    if !validation.valid {
        return Ok(Json(UpdateConfigResponse {
            success: false,
            validation,
            backup_id: None,
            applied: false,
            restart_required: false,
            error: Some("Configuration validation failed".to_owned()),
        }));
    }

    // Create backup if requested
    let backup_id = if request.create_backup {
        // TODO: Implement backup creation
        Some(chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string())
    } else {
        None
    };

    // Only keys of the typed config are taken from the update (findings 34, 37)
    let mut dropped = Vec::new();
    let update = filter_update(request.config, &schema, "", &mut dropped)
        .unwrap_or_else(|| serde_json::json!({}));
    if !dropped.is_empty() {
        warn!(
            "Config update dropped keys outside the web schema: {:?}",
            dropped
        );
    }

    // Update in-memory config with the filtered update
    let mut current_config = state.config.write();
    validation::merge_json(&mut current_config, update.clone());

    // Persist on top of the stored file, so keys the web API does not write keep their stored values
    let mut persisted_config =
        read_stored_config(&state.config_path).unwrap_or_else(|| current_config.clone());
    validation::merge_json(&mut persisted_config, update);

    // Save to persistent storage (optional when running outside HA)
    let persisted = serde_json::json!({
        "config": persisted_config,
        "metadata": {
            "last_modified": chrono::Utc::now().to_rfc3339(),
            "modified_by": "web_ui",
            "version": "1.0.0"
        }
    });

    match std::fs::write(
        &state.config_path,
        serde_json::to_string_pretty(&persisted).unwrap(),
    ) {
        Ok(()) => {
            info!(
                "✅ Configuration updated and saved to {}",
                state.config_path
            );
        }
        Err(e) => {
            // When running outside HA, persistence may fail - that's OK
            info!(
                "Configuration updated in memory (persistence skipped: {})",
                e
            );
        }
    }

    // Send ConfigUpdateEvent to ECS if sender is available
    if let Some(sender) = &state.config_update_sender {
        // Send the merged config (not the partial update)
        let event = fluxion_core::ConfigUpdateEvent::full_update(current_config.clone());
        if let Err(e) = sender.send_update(event) {
            info!("Failed to send config update event to ECS: {e}");
        } else {
            info!("🔄 Configuration update event sent to ECS");
        }
    }

    Ok(Json(UpdateConfigResponse {
        success: true,
        validation,
        backup_id,
        applied: true,
        restart_required: false,
        error: None,
    }))
}

/// POST /api/config/reset - Reset a configuration section to defaults
pub async fn reset_section_handler(
    State(_state): State<ConfigApiState>,
    Json(_request): Json<ResetSectionRequest>,
) -> Result<Json<UpdateConfigResponse>, StatusCode> {
    // TODO: Implement section reset logic
    // For now, return an error
    Ok(Json(UpdateConfigResponse {
        success: false,
        validation: ValidateResponse {
            valid: false,
            errors: vec![ValidationIssue {
                field: "section".to_owned(),
                message: "Section reset not yet implemented".to_owned(),
                severity: "error".to_owned(),
            }],
            warnings: Vec::new(),
            restart_required: false,
        },
        backup_id: None,
        applied: false,
        restart_required: false,
        error: Some("Section reset not yet implemented".to_owned()),
    }))
}

/// GET /api/config/export - Export configuration as downloadable file
pub async fn export_config_handler(State(state): State<ConfigApiState>) -> impl IntoResponse {
    let config = state.config.read().clone();
    let filename = format!(
        "fluxion_config_{}.json",
        chrono::Utc::now().format("%Y%m%d_%H%M%S")
    );

    let json_string = serde_json::to_string_pretty(&config).unwrap_or_default();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );

    (headers, json_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Brief H37: the config API refuses to change `control.mode_control_enabled` at runtime
    /// Scan #49: auto-unlock cannot change at runtime (no UI field, safety guard)
    #[test]
    fn auto_unlock_cannot_change_at_runtime() {
        let running = json!({"control": {"auto_unlock_inverter": true, "max_battery_soc": 90.0}});
        let issue = adapter_restart_change(
            &running,
            &json!({"control": {"auto_unlock_inverter": false}}),
        )
        .unwrap();
        assert_eq!(issue.field, "control.auto_unlock_inverter");
        assert_eq!(issue.severity, "error");
        assert!(issue.message.contains("restart the add-on"));
        assert!(
            adapter_restart_change(
                &running,
                &json!({"control": {"auto_unlock_inverter": true}})
            )
            .is_none()
        );
        assert!(
            adapter_restart_change(&running, &json!({"control": {"max_battery_soc": 100}}))
                .is_none()
        );
        assert!(
            adapter_restart_change(&running, &json!({"system": {"debug_mode": false}})).is_none()
        );
    }

    /// Scan #46: a changed max_battery_soc is accepted with a warning that the reserve limit keeps
    /// the startup value; an unchanged one (integer or float) has no warning
    #[test]
    fn max_battery_soc_change_warns_that_the_reserve_limit_keeps_the_startup_value() {
        let running = json!({"control": {"max_battery_soc": 90.0}});
        let issue =
            max_battery_soc_warning(&running, &json!({"control": {"max_battery_soc": 100}}))
                .unwrap();
        assert_eq!(issue.field, "control.max_battery_soc");
        assert_eq!(issue.severity, "warning");
        assert_eq!(
            issue.message,
            "control.max_battery_soc: the Back Up reserve limit keeps the startup value until the add-on restarts"
        );
        assert!(
            max_battery_soc_warning(&running, &json!({"control": {"max_battery_soc": 90}}))
                .is_none()
        );
        assert!(
            max_battery_soc_warning(&running, &json!({"control": {"max_battery_soc": 90.0}}))
                .is_none()
        );
        assert!(
            max_battery_soc_warning(&running, &json!({"control": {"min_battery_soc": 10}}))
                .is_none()
        );
    }

    #[test]
    fn mode_control_cannot_change_at_runtime() {
        let off = json!({"control": {"mode_control_enabled": false, "max_battery_soc": 90.0}});
        let on = json!({"control": {"mode_control_enabled": true}});
        let issue = mode_control_change(&off, &json!({"control": {"mode_control_enabled": true}}))
            .expect("refused");
        assert_eq!(issue.field, "control.mode_control_enabled");
        assert!(
            issue.message.contains("restart the add-on"),
            "{}",
            issue.message
        );
        assert!(
            mode_control_change(&on, &json!({"control": {"mode_control_enabled": false}}))
                .is_some()
        );
        // A key missing from the running config counts as the default (on)
        assert!(
            mode_control_change(
                &json!({}),
                &json!({"control": {"mode_control_enabled": false}})
            )
            .is_some()
        );
        // The same value, or an update without the key, passes
        assert!(
            mode_control_change(&off, &json!({"control": {"mode_control_enabled": false}}))
                .is_none()
        );
        assert!(
            mode_control_change(&off, &json!({"control": {"max_battery_soc": 100.0}})).is_none()
        );
        assert!(mode_control_change(&off, &json!({"system": {"debug_mode": false}})).is_none());
    }

    /// Handler state over a temp config file, optionally pre-filled with a stored `config` object
    fn handler_state(
        stored_config: Option<serde_json::Value>,
    ) -> (ConfigApiState, tempfile::TempDir) {
        let current = serde_json::to_value(crate::validation::tests::default_config()).unwrap();
        handler_state_from(current, stored_config)
    }

    /// Handler state whose startup and running config are `current`
    fn handler_state_from(
        current: serde_json::Value,
        stored_config: Option<serde_json::Value>,
    ) -> (ConfigApiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        if let Some(config) = stored_config {
            std::fs::write(&path, json!({"config": config, "metadata": {}}).to_string()).unwrap();
        }
        let state = ConfigApiState::new(current, path.to_string_lossy().into_owned(), None);
        (state, dir)
    }

    async fn post_update(
        state: &ConfigApiState,
        config: serde_json::Value,
    ) -> UpdateConfigResponse {
        let Json(response) = update_config_handler(
            State(state.clone()),
            Json(UpdateConfigRequest {
                config,
                create_backup: false,
            }),
        )
        .await
        .unwrap();
        response
    }

    /// The `config` object written to the file
    fn persisted(state: &ConfigApiState) -> serde_json::Value {
        let text = std::fs::read_to_string(&state.config_path).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["config"].clone()
    }

    #[tokio::test]
    async fn unknown_keys_are_not_persisted() {
        let (state, _dir) = handler_state(None);
        let update = json!({
            "system": {"update_interval_secs": 30, "bogus": 1},
            "bogus_top": {"x": 1}
        });
        assert!(post_update(&state, update).await.success);
        let config = persisted(&state);
        assert_eq!(config["system"]["update_interval_secs"], 30);
        assert!(config["system"].get("bogus").is_none());
        assert!(config.get("bogus_top").is_none());
    }

    #[tokio::test]
    async fn known_keys_are_persisted() {
        let (state, _dir) = handler_state(None);
        assert!(
            post_update(&state, json!({"system": {"update_interval_secs": 45}}))
                .await
                .success
        );
        assert_eq!(persisted(&state)["system"]["update_interval_secs"], 45);
    }

    #[tokio::test]
    async fn protected_keys_are_not_persisted_and_stored_values_survive() {
        let mut stored = serde_json::to_value(crate::validation::tests::default_config()).unwrap();
        stored["system"]["ha_token"] = json!("orig-token");
        stored["system"]["ha_base_url"] = json!("http://ha.local");
        let (state, _dir) = handler_state(Some(stored));

        let update = json!({
            "system": {
                "update_interval_secs": 30,
                "ha_token": "evil-token",
                "ha_base_url": "http://evil"
            },
            "server_heartbeat": {"enabled": true}
        });
        assert!(post_update(&state, update).await.success);

        let config = persisted(&state);
        assert_eq!(config["system"]["update_interval_secs"], 30);
        assert_eq!(config["system"]["ha_token"], "orig-token");
        assert_eq!(config["system"]["ha_base_url"], "http://ha.local");
        assert!(config.get("server_heartbeat").is_none());
        let in_memory = state.config.read();
        assert!(in_memory["system"].get("ha_token").is_none());
    }

    /// The protected list holds even when the typed schema has the key
    #[test]
    fn protected_keys_are_dropped_even_when_the_schema_has_them() {
        let schema = json!({"system": {"ha_token": null, "ha_base_url": null}, "server_heartbeat": {"enabled": false}});
        let update = json!({
            "system": {"ha_token": "x", "ha_base_url": "http://evil"},
            "server_heartbeat": {"enabled": true}
        });
        let mut dropped = Vec::new();
        let kept = filter_update(update, &schema, "", &mut dropped).unwrap();
        assert_eq!(kept, json!({"system": {}}));
        dropped.sort();
        assert_eq!(
            dropped,
            vec![
                "server_heartbeat".to_owned(),
                "system.ha_base_url".to_owned(),
                "system.ha_token".to_owned(),
            ]
        );
    }

    fn inverter(id: &str, topology: &serde_json::Value) -> serde_json::Value {
        json!({
            "id": id,
            "inverter_type": "solax",
            "entity_prefix": format!("{id}_sensors"),
            "topology": topology
        })
    }

    /// Two inverters are a list of externally tagged topologies; each must keep its own shape
    #[tokio::test]
    async fn inverter_topology_survives_save() {
        let master_slave = json!([
            inverter("master", &json!({"master": {"slave_ids": ["slave"]}})),
            inverter("slave", &json!({"slave": {"master_id": "master"}}))
        ]);
        let independent_master = json!([
            inverter("solo", &json!("independent")),
            inverter("master", &json!({"master": {"slave_ids": []}}))
        ]);
        for inverters in [master_slave, independent_master] {
            let (state, _dir) = handler_state(None);
            let response = post_update(&state, json!({"inverters": inverters.clone()})).await;
            assert!(response.success, "{:?}", response.error);
            let config = persisted(&state);
            assert_eq!(config["inverters"], inverters);
            serde_json::from_value::<SystemConfig>(config.clone())
                .expect("persisted config parses as SystemConfig");

            // A second save of another setting still succeeds and keeps the topology
            assert!(
                post_update(&state, json!({"system": {"update_interval_secs": 40}}))
                    .await
                    .success
            );
            let config = persisted(&state);
            assert_eq!(config["inverters"], inverters);
            serde_json::from_value::<SystemConfig>(config)
                .expect("second save still parses as SystemConfig");
        }
    }

    /// Scan #46: the reserve-limit warning compares against the startup value, not the running one
    #[tokio::test]
    async fn max_battery_soc_warns_against_the_startup_value() {
        let mut startup = serde_json::to_value(crate::validation::tests::default_config()).unwrap();
        startup["control"]["max_battery_soc"] = json!(90.0);
        let (state, _dir) = handler_state_from(startup, None);
        let warned = |response: &UpdateConfigResponse| {
            response
                .validation
                .warnings
                .iter()
                .any(|w| w.field == "control.max_battery_soc")
        };

        let first = post_update(&state, json!({"control": {"max_battery_soc": 100.0}})).await;
        assert!(first.success);
        assert!(warned(&first));

        // Running value is now 100, still differs from the startup 90
        let second = post_update(&state, json!({"control": {"max_battery_soc": 100.0}})).await;
        assert!(second.success);
        assert!(warned(&second));

        // Back to the startup value: no warning
        let back = post_update(&state, json!({"control": {"max_battery_soc": 90.0}})).await;
        assert!(back.success);
        assert!(!warned(&back));
    }

    #[test]
    fn filter_drops_unknown_and_protected_keys_at_any_depth() {
        let schema = json!({"system": {"debug_mode": true}, "inverters": [{"id": "a"}]});
        let update = json!({
            "system": {"debug_mode": false, "ha_token": "x"},
            "inverters": [{"id": "b", "ha_base_url": "http://evil", "extra": 1}],
            "server_heartbeat": {"enabled": true}
        });
        let mut dropped = Vec::new();
        let kept = filter_update(update, &schema, "", &mut dropped).unwrap();
        assert_eq!(
            kept,
            json!({"system": {"debug_mode": false}, "inverters": [{"id": "b"}]})
        );
        dropped.sort();
        assert_eq!(
            dropped,
            vec![
                "inverters[0].extra".to_owned(),
                "inverters[0].ha_base_url".to_owned(),
                "server_heartbeat".to_owned(),
                "system.ha_token".to_owned(),
            ]
        );
    }
}
