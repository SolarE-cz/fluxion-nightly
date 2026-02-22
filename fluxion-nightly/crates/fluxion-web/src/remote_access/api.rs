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

use askama::Template;
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
};
use fluxion_mobile_types::QrPayload;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

use super::{DeviceStore, TorManager};

#[derive(Template)]
#[template(path = "remote_access.html")]
struct RemoteAccessPageTemplate {
    ingress_path: String,
}

#[derive(Debug, Template)]
#[template(path = "mobile.html")]
pub struct MobileBundleTemplate {
    pub ui_version: String,
    pub initial_state: Option<String>,
}

/// Shared state for remote access API endpoints.
#[derive(Debug, Clone)]
pub struct RemoteAccessApiState {
    pub device_store: Arc<DeviceStore>,
    pub tor_manager: Arc<parking_lot::Mutex<TorManager>>,
    pub instance_name: String,
}

#[derive(Serialize)]
struct StatusResponse {
    enabled: bool,
    tor_running: bool,
    onion_address: Option<String>,
    device_count: usize,
}

#[derive(Deserialize)]
struct PairRequest {
    name: String,
    #[serde(default = "default_access_mode")]
    mode: String,
}

fn default_access_mode() -> String {
    "full".to_owned()
}

#[derive(Serialize)]
struct PairResponse {
    device_id: String,
    qr_payload: String,
    qr_svg: String,
}

#[derive(Serialize)]
struct DeviceResponse {
    id: String,
    name: String,
    access_mode: String,
    created_at: String,
    last_seen: Option<String>,
}

#[derive(Serialize)]
struct DeleteResponse {
    ok: bool,
}

/// GET /api/remote/status
async fn status_handler(State(state): State<RemoteAccessApiState>) -> impl IntoResponse {
    let mut tor = state.tor_manager.lock();
    let tor_running = tor.is_running();
    let onion_address = tor.read_onion_address();
    let devices = state.device_store.load_devices();

    Json(StatusResponse {
        enabled: true,
        tor_running,
        onion_address,
        device_count: devices.len(),
    })
}

/// POST /api/remote/pair
async fn pair_handler(
    State(state): State<RemoteAccessApiState>,
    Json(req): Json<PairRequest>,
) -> impl IntoResponse {
    let mode = match req.mode.as_str() {
        "full" | "readonly" => req.mode.clone(),
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "mode must be 'full' or 'readonly'"})),
            )
                .into_response();
        }
    };

    let (entry, privkey_b64) = match state.device_store.register_device(&req.name, &mode) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to register device: {e}");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to register device"})),
            )
                .into_response();
        }
    };

    // Reload Tor to pick up new auth client
    if let Err(e) = state.tor_manager.lock().reload() {
        error!("Failed to reload Tor after pairing: {e}");
    }

    // Build QR payload
    let onion_address = state
        .tor_manager
        .lock()
        .read_onion_address()
        .unwrap_or_default();

    let qr_payload = serde_json::to_string(&QrPayload {
        v: fluxion_mobile_types::API_VERSION,
        onion: onion_address,
        key: privkey_b64,
        name: state.instance_name.clone(),
        mode: entry.access_mode.clone(),
    })
    .expect("QrPayload serialization cannot fail");

    // Generate QR code SVG
    let qr_svg = render_qr_svg(&qr_payload);

    info!(
        "Paired device '{}' (id={}, mode={})",
        entry.name, entry.id, entry.access_mode
    );

    Json(PairResponse {
        device_id: entry.id,
        qr_payload,
        qr_svg,
    })
    .into_response()
}

/// GET /api/remote/devices
async fn devices_handler(State(state): State<RemoteAccessApiState>) -> impl IntoResponse {
    let devices: Vec<DeviceResponse> = state
        .device_store
        .load_devices()
        .into_iter()
        .map(|d| DeviceResponse {
            id: d.id,
            name: d.name,
            access_mode: d.access_mode,
            created_at: d.created_at.to_rfc3339(),
            last_seen: d.last_seen.map(|dt| dt.to_rfc3339()),
        })
        .collect();

    Json(devices)
}

/// DELETE /api/remote/devices/{id}
async fn revoke_handler(
    State(state): State<RemoteAccessApiState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    match state.device_store.revoke_device(&device_id) {
        Ok(true) => {
            if let Err(e) = state.tor_manager.lock().reload() {
                error!("Failed to reload Tor after revoke: {e}");
            }
            info!("Revoked device {device_id}");
            Json(DeleteResponse { ok: true }).into_response()
        }
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Device not found"})),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to revoke device: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to revoke device"})),
            )
                .into_response()
        }
    }
}

/// Render a QR code as SVG from the given payload string.
fn render_qr_svg(payload: &str) -> String {
    use qrcode::QrCode;

    match QrCode::new(payload.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(256, 256)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .quiet_zone(true)
            .build(),
        Err(e) => {
            error!("Failed to generate QR code: {e}");
            String::from("<svg></svg>")
        }
    }
}

/// GET /remote-access — management page
async fn page_handler(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let ingress_path = crate::extract_ingress_path(&headers);
    let template = RemoteAccessPageTemplate { ingress_path };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            Html(format!("<h1>Error</h1><p>{e}</p>")).into_response()
        }
    }
}

/// Build the router for remote access API endpoints.
pub fn remote_access_routes(state: RemoteAccessApiState) -> Router {
    Router::new()
        .route("/remote-access", get(page_handler))
        .route("/api/remote/status", get(status_handler))
        .route("/api/remote/pair", post(pair_handler))
        .route("/api/remote/devices", get(devices_handler))
        .route("/api/remote/devices/{id}", delete(revoke_handler))
        .with_state(state)
}

impl RemoteAccessApiState {
    #[must_use]
    pub fn new(data_dir: &std::path::Path, listen_port: u16, instance_name: String) -> Self {
        Self {
            device_store: Arc::new(DeviceStore::new(data_dir)),
            tor_manager: Arc::new(parking_lot::Mutex::new(TorManager::new(
                data_dir,
                listen_port,
            ))),
            instance_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_qr_svg() {
        let payload = r#"{"v":1,"onion":"test.onion","key":"abc","name":"Test","mode":"full"}"#;
        let svg = render_qr_svg(payload);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn test_qr_payload_format() {
        let payload = QrPayload {
            v: fluxion_mobile_types::API_VERSION,
            onion: "xyz.onion".to_owned(),
            key: "base64key==".to_owned(),
            name: "FluxION Home".to_owned(),
            mode: "full".to_owned(),
        };
        let s = serde_json::to_string(&payload).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["mode"], "full");
    }
}
