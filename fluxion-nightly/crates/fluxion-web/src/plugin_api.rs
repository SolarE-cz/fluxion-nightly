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

//! Plugin management API endpoints.
//!
//! Provides REST API for managing external strategy plugins:
//! - List registered plugins
//! - Register new external plugins
//! - Unregister plugins
//! - Update plugin priorities

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use fluxion_plugins::{
    HttpPlugin, PluginManager, PluginRegistrationRequest, PluginRegistrationResponse,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// State for plugin API handlers
#[derive(Clone, Debug)]
pub struct PluginApiState {
    pub plugin_manager: Arc<RwLock<PluginManager>>,
}

impl PluginApiState {
    /// Create a new plugin API state
    pub fn new(plugin_manager: Arc<RwLock<PluginManager>>) -> Self {
        Self { plugin_manager }
    }
}

/// Plugin info response
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub priority: u8,
    pub enabled: bool,
    pub plugin_type: String,
}

/// List of plugins response
#[derive(Debug, Clone, Serialize)]
pub struct PluginListResponse {
    pub plugins: Vec<PluginInfo>,
    pub count: usize,
}

/// Priority update request
#[derive(Debug, Clone, Deserialize)]
pub struct PriorityUpdateRequest {
    pub priority: u8,
}

/// Priority update response
#[derive(Debug, Clone, Serialize)]
pub struct PriorityUpdateResponse {
    pub success: bool,
    pub message: String,
}

/// List all registered plugins
///
/// GET /api/plugins
pub async fn list_plugins_handler(State(state): State<PluginApiState>) -> impl IntoResponse {
    debug!("Listing all plugins");

    let manager = state.plugin_manager.read();
    let plugins: Vec<PluginInfo> = manager
        .list_plugins()
        .into_iter()
        .map(|(name, priority, enabled)| PluginInfo {
            name: name.to_owned(),
            priority,
            enabled,
            plugin_type: if name.starts_with("http:") {
                "external".to_owned()
            } else {
                "builtin".to_owned()
            },
        })
        .collect();

    let count = plugins.len();
    Json(PluginListResponse { plugins, count })
}

/// Register an external plugin
///
/// POST /api/plugins/register
pub async fn register_plugin_handler(
    State(state): State<PluginApiState>,
    Json(request): Json<PluginRegistrationRequest>,
) -> impl IntoResponse {
    info!(
        "Registering external plugin: {} at {}",
        request.manifest.name, request.callback_url
    );

    // Validate the manifest
    if request.manifest.name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PluginRegistrationResponse {
                success: false,
                error: Some("Plugin name cannot be empty".to_owned()),
                plugin_id: None,
            }),
        );
    }

    if request.callback_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PluginRegistrationResponse {
                success: false,
                error: Some("Callback URL cannot be empty".to_owned()),
                plugin_id: None,
            }),
        );
    }

    // Validate the callback URL format
    if !request.callback_url.starts_with("http://") && !request.callback_url.starts_with("https://")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(PluginRegistrationResponse {
                success: false,
                error: Some("Callback URL must start with http:// or https://".to_owned()),
                plugin_id: None,
            }),
        );
    }

    // Create the HTTP plugin
    let plugin = HttpPlugin::new(request.manifest.clone(), request.callback_url);
    let plugin_id = format!("http:{}", request.manifest.name);

    // Register the plugin
    let mut manager = state.plugin_manager.write();
    manager.register(Arc::new(plugin));

    info!("Successfully registered plugin: {}", plugin_id);

    (
        StatusCode::CREATED,
        Json(PluginRegistrationResponse {
            success: true,
            error: None,
            plugin_id: Some(plugin_id),
        }),
    )
}

/// Unregister a plugin
///
/// DELETE /api/plugins/{name}
pub async fn unregister_plugin_handler(
    State(state): State<PluginApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    info!("Unregistering plugin: {}", name);

    let mut manager = state.plugin_manager.write();

    // Disable the plugin (we can't actually remove it from the HashMap
    // without more complex logic, but disabling achieves the same effect)
    if manager.set_enabled(&name, false) {
        info!("Successfully disabled plugin: {}", name);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "message": format!("Plugin '{}' has been disabled", name)
            })),
        )
    } else {
        warn!("Plugin not found: {}", name);
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' not found", name)
            })),
        )
    }
}

/// Update plugin priority
///
/// PUT /api/plugins/{name}/priority
pub async fn update_priority_handler(
    State(state): State<PluginApiState>,
    Path(name): Path<String>,
    Json(request): Json<PriorityUpdateRequest>,
) -> impl IntoResponse {
    info!(
        "Updating priority for plugin '{}' to {}",
        name, request.priority
    );

    let mut manager = state.plugin_manager.write();

    if manager.set_priority(&name, request.priority) {
        info!(
            "Successfully updated priority for '{}' to {}",
            name, request.priority
        );
        (
            StatusCode::OK,
            Json(PriorityUpdateResponse {
                success: true,
                message: format!("Plugin '{}' priority updated to {}", name, request.priority),
            }),
        )
    } else {
        warn!("Plugin not found: {}", name);
        (
            StatusCode::NOT_FOUND,
            Json(PriorityUpdateResponse {
                success: false,
                message: format!("Plugin '{name}' not found"),
            }),
        )
    }
}

/// Enable or disable a plugin
///
/// PUT /api/plugins/{name}/enabled
#[derive(Debug, Clone, Deserialize)]
pub struct EnabledUpdateRequest {
    pub enabled: bool,
}

pub async fn update_enabled_handler(
    State(state): State<PluginApiState>,
    Path(name): Path<String>,
    Json(request): Json<EnabledUpdateRequest>,
) -> impl IntoResponse {
    info!(
        "Updating enabled state for plugin '{}' to {}",
        name, request.enabled
    );

    let mut manager = state.plugin_manager.write();

    if manager.set_enabled(&name, request.enabled) {
        let state_str = if request.enabled {
            "enabled"
        } else {
            "disabled"
        };
        info!("Successfully {} plugin '{}'", state_str, name);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "message": format!("Plugin '{}' has been {}", name, state_str)
            })),
        )
    } else {
        warn!("Plugin not found: {}", name);
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Plugin '{}' not found", name)
            })),
        )
    }
}
