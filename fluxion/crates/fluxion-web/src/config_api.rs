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

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Shared state for config API endpoints
#[derive(Clone)]
pub struct ConfigApiState {
    /// Configuration JSON stored in memory
    pub config: Arc<RwLock<serde_json::Value>>,
    /// Path to persistent config file
    pub config_path: String,
    /// Sender for config update events to ECS
    pub config_update_sender: Option<fluxion_core::ConfigUpdateSender>,
}

impl std::fmt::Debug for ConfigApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigApiState")
            .field("config", &"<RwLock>")
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
    Json(request): Json<ValidateRequest>,
) -> Json<ValidateResponse> {
    // Parse as AppConfig and validate
    let validation = match serde_json::from_value::<serde_json::Value>(request.config.clone()) {
        Ok(_) => {
            // For now, just do basic JSON validation
            // In full implementation, deserialize to AppConfig and call validate_detailed()
            ValidateResponse {
                valid: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                restart_required: false,
            }
        }
        Err(e) => ValidateResponse {
            valid: false,
            errors: vec![ValidationIssue {
                field: "config".to_owned(),
                message: format!("Invalid configuration format: {e}"),
                severity: "error".to_owned(),
            }],
            warnings: Vec::new(),
            restart_required: false,
        },
    };

    Json(validation)
}

/// POST /api/config/update - Update configuration
pub async fn update_config_handler(
    State(state): State<ConfigApiState>,
    Json(request): Json<UpdateConfigRequest>,
) -> Result<Json<UpdateConfigResponse>, StatusCode> {
    // Validate the new configuration
    let validation = match serde_json::from_value::<serde_json::Value>(request.config.clone()) {
        Ok(_) => ValidateResponse {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
            restart_required: false,
        },
        Err(e) => ValidateResponse {
            valid: false,
            errors: vec![ValidationIssue {
                field: "config".to_owned(),
                message: format!("Invalid configuration format: {e}"),
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

    // Merge the partial config update with the existing in-memory config
    let mut current_config = state.config.read().clone();
    if let (Some(current_obj), Some(new_obj)) =
        (current_config.as_object_mut(), request.config.as_object())
    {
        // Deep merge: iterate over top-level keys
        for (key, new_value) in new_obj {
            if let (Some(current_nested), Some(new_nested)) = (
                current_obj.get_mut(key).and_then(|v| v.as_object_mut()),
                new_value.as_object(),
            ) {
                // If both are objects, merge their fields
                for (nested_key, nested_value) in new_nested {
                    current_nested.insert(nested_key.clone(), nested_value.clone());
                }
            } else {
                // Otherwise, replace the entire value
                current_obj.insert(key.clone(), new_value.clone());
            }
        }
    }

    // Update in-memory config with merged result
    *state.config.write() = current_config.clone();

    // Save to persistent storage (optional when running outside HA)
    let persisted = serde_json::json!({
        "config": current_config,
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
