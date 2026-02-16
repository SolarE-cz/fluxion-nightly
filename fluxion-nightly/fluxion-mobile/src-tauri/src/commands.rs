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

//! Tauri IPC commands — the bridge between the WebView and Rust backend.
//!
//! Each command is callable from JavaScript via `window.__TAURI__.invoke("command_name", {...})`.
//! Commands handle:
//! - QR code scanning and pairing
//! - Data fetching (via Tor) and cache management
//! - Sending control changes to the server
//! - Loading cached UI bundle

use chrono::Utc;
use serde::Serialize;
use tauri::State;

use fluxion_mobile_types::{MobileControlResponse, VersionResponse};

use crate::credentials::{parse_qr_payload, save_connection, save_settings};
use crate::state::AppState;

/// Response for get_state command.
#[derive(Serialize)]
pub struct StateResponse {
    pub data: Option<String>,
    pub from_cache: bool,
    pub error: Option<String>,
}

/// Response for save_controls command.
#[derive(Serialize)]
pub struct SaveResponse {
    pub ok: bool,
    pub updated_state: Option<String>,
    pub error: Option<String>,
}

/// Response for UI update check.
#[derive(Serialize)]
pub struct UiUpdateResult {
    pub updated: bool,
    pub version: Option<String>,
}

/// Response for connection info.
#[derive(Serialize)]
pub struct ConnectionInfo {
    pub connected: bool,
    pub instance_name: Option<String>,
    pub access_mode: Option<String>,
    pub onion_address: Option<String>,
}

/// Scan a QR code and store the connection credentials.
///
/// Parses the QR payload, configures the Tor client, and attempts bootstrap.
#[tauri::command]
pub async fn scan_qr(
    payload: String,
    state: State<'_, AppState>,
) -> Result<ConnectionInfo, String> {
    let conn = parse_qr_payload(&payload)?;

    let info = ConnectionInfo {
        connected: false,
        instance_name: Some(conn.instance_name.clone()),
        access_mode: Some(conn.access_mode.clone()),
        onion_address: Some(conn.onion_address.clone()),
    };

    // Configure Tor client with the new credentials
    {
        let mut tor = state.tor.write().await;
        let key_bytes = base64_decode(&conn.client_auth_key_b64)
            .map_err(|e| format!("Invalid auth key: {e}"))?;
        let mut key = [0u8; 32];
        if key_bytes.len() != 32 {
            return Err("Auth key must be 32 bytes".to_owned());
        }
        key.copy_from_slice(&key_bytes);
        tor.configure(conn.onion_address.clone(), key);
    }

    // Store connection and persist to disk
    if let Err(e) = save_connection(&state.app_handle, &conn) {
        tracing::warn!("Failed to persist connection: {e}");
    }
    *state.connection.write().await = Some(conn);

    // Attempt Tor bootstrap (don't fail the pairing if bootstrap doesn't work yet)
    if let Err(e) = state.tor.write().await.bootstrap().await {
        tracing::warn!("Tor bootstrap failed (will retry): {e}");
    }

    Ok(info)
}

/// Fetch the current system state from the server (or cache).
///
/// 1. Returns cached data immediately if available.
/// 2. If Tor is ready, fetches fresh data from server.
/// 3. Updates cache with fresh data.
#[tauri::command]
pub async fn get_state(state: State<'_, AppState>) -> Result<StateResponse, ()> {
    let conn = state.connection.read().await;
    if conn.is_none() {
        return Ok(StateResponse {
            data: None,
            from_cache: false,
            error: Some("Not connected — scan QR code to pair".to_owned()),
        });
    }
    drop(conn);

    // Try cached data first (for offline-first display)
    let cached = state.cache.load_cached_data();

    // Try fetching fresh data if Tor is ready
    let tor = state.tor.read().await;
    if tor.is_ready() {
        match tor.get("/mobile/api/state").await {
            Ok(fresh_data) => {
                let _ = state.cache.store_data(&fresh_data, Utc::now());
                return Ok(StateResponse {
                    data: Some(fresh_data),
                    from_cache: false,
                    error: None,
                });
            }
            Err(e) => {
                tracing::warn!("Failed to fetch fresh state: {e}");
                // Fall through to cached data
            }
        }
    }
    drop(tor);

    // Return cached data if available
    if let Some((data, _timestamp)) = cached {
        return Ok(StateResponse {
            data: Some(data),
            from_cache: true,
            error: None,
        });
    }

    Ok(StateResponse {
        data: None,
        from_cache: false,
        error: Some("Connecting via Tor...".to_owned()),
    })
}

/// Send control changes to the server.
///
/// Accepts the full control JSON and POSTs it to /mobile/api/control.
/// Returns the updated state snapshot from the server response.
#[tauri::command]
pub async fn save_controls(
    controls_json: String,
    state: State<'_, AppState>,
) -> Result<SaveResponse, ()> {
    let conn = state.connection.read().await;
    if conn.is_none() {
        return Ok(SaveResponse {
            ok: false,
            updated_state: None,
            error: Some("Not connected".to_owned()),
        });
    }
    drop(conn);

    let tor = state.tor.read().await;
    if !tor.is_ready() {
        return Ok(SaveResponse {
            ok: false,
            updated_state: None,
            error: Some("Tor not connected — try again shortly".to_owned()),
        });
    }

    match tor.post("/mobile/api/control", &controls_json).await {
        Ok(response_body) => {
            // Typed validation — ensures server response matches shared contract
            let parsed = serde_json::from_str::<MobileControlResponse>(&response_body).ok();
            let _ = state.cache.store_data(&response_body, Utc::now());
            Ok(SaveResponse {
                ok: parsed.as_ref().map_or(true, |p| p.ok),
                updated_state: Some(response_body),
                error: parsed.and_then(|p| p.error),
            })
        }
        Err(e) => Ok(SaveResponse {
            ok: false,
            updated_state: None,
            error: Some(format!("Failed to save: {e}")),
        }),
    }
}

/// Get the cached UI bundle HTML, if available.
///
/// Returns None if no bundle is cached (first launch).
#[tauri::command]
pub async fn get_cached_ui(state: State<'_, AppState>) -> Result<Option<String>, ()> {
    Ok(state.cache.load_ui_bundle())
}

/// Check if the server has a newer UI bundle and download it if so.
///
/// Compares the locally cached UI version with the server's version.
/// If different, fetches the full bundle and updates the cache.
#[tauri::command]
pub async fn check_ui_update(state: State<'_, AppState>) -> Result<UiUpdateResult, ()> {
    let tor = state.tor.read().await;
    if !tor.is_ready() {
        return Ok(UiUpdateResult {
            updated: false,
            version: state.cache.cached_ui_version(),
        });
    }

    // Fetch server version
    let server_version = match tor.get("/mobile/api/version").await {
        Ok(body) => serde_json::from_str::<VersionResponse>(&body)
            .ok()
            .map(|v| v.version),
        Err(e) => {
            tracing::warn!("Failed to check UI version: {e}");
            return Ok(UiUpdateResult {
                updated: false,
                version: state.cache.cached_ui_version(),
            });
        }
    };

    let cached_version = state.cache.cached_ui_version();

    // Compare versions — update if different or if we have no cached version
    if server_version != cached_version {
        if let Some(ref ver) = server_version {
            tracing::info!("UI update available: {:?} -> {ver}", cached_version);
            // Fetch the full UI bundle
            match tor.get("/mobile/api/ui").await {
                Ok(html) => {
                    let _ = state.cache.store_ui_bundle(&html, ver);
                    return Ok(UiUpdateResult {
                        updated: true,
                        version: Some(ver.clone()),
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to download UI bundle: {e}");
                }
            }
        }
    }

    Ok(UiUpdateResult {
        updated: false,
        version: cached_version,
    })
}

/// Get current connection info (for the UI to display status).
#[tauri::command]
pub async fn get_connection_info(state: State<'_, AppState>) -> Result<ConnectionInfo, ()> {
    let conn = state.connection.read().await;
    let tor = state.tor.read().await;

    match conn.as_ref() {
        Some(c) => Ok(ConnectionInfo {
            connected: tor.is_ready(),
            instance_name: Some(c.instance_name.clone()),
            access_mode: Some(c.access_mode.clone()),
            onion_address: Some(c.onion_address.clone()),
        }),
        None => Ok(ConnectionInfo {
            connected: false,
            instance_name: None,
            access_mode: None,
            onion_address: None,
        }),
    }
}

/// Check if a PIN is configured.
#[tauri::command]
pub async fn is_pin_set(state: State<'_, AppState>) -> Result<bool, ()> {
    let settings = state.settings.read().await;
    Ok(settings.pin_hash.is_some())
}

/// Set a new PIN. The PIN is hashed before storage.
#[tauri::command]
pub async fn set_pin(pin: String, state: State<'_, AppState>) -> Result<bool, String> {
    if pin.len() < 4 || pin.len() > 8 {
        return Err("PIN must be 4-8 digits".to_owned());
    }
    if !pin.chars().all(|c| c.is_ascii_digit()) {
        return Err("PIN must contain only digits".to_owned());
    }

    let hash = simple_hash(&pin);
    let mut settings = state.settings.write().await;
    settings.pin_hash = Some(hash);
    if let Err(e) = save_settings(&state.app_handle, &settings) {
        tracing::warn!("Failed to persist settings: {e}");
    }
    Ok(true)
}

/// Verify a PIN attempt against the stored hash.
#[tauri::command]
pub async fn verify_pin(pin: String, state: State<'_, AppState>) -> Result<bool, ()> {
    let settings = state.settings.read().await;
    match &settings.pin_hash {
        Some(stored_hash) => Ok(simple_hash(&pin) == *stored_hash),
        None => Ok(true), // No PIN set, always passes
    }
}

/// Remove the PIN lock.
#[tauri::command]
pub async fn remove_pin(state: State<'_, AppState>) -> Result<bool, ()> {
    let mut settings = state.settings.write().await;
    settings.pin_hash = None;
    if let Err(e) = save_settings(&state.app_handle, &settings) {
        tracing::warn!("Failed to persist settings: {e}");
    }
    Ok(true)
}

/// Simple hash for PIN storage (UI-level lock only, not cryptographic security).
/// Uses a basic FNV-1a-like hash since the PIN is only a local UI lock.
fn simple_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Simple base64 decoding (standard alphabet with padding).
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let input = input.trim();
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut buf = Vec::new();
    let mut bits: u32 = 0;
    let mut count = 0;

    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let val = table
            .iter()
            .position(|&c| c == b)
            .ok_or_else(|| format!("Invalid base64 character: {}", b as char))?
            as u32;
        bits = (bits << 6) | val;
        count += 1;
        if count == 4 {
            buf.push((bits >> 16) as u8);
            buf.push((bits >> 8) as u8);
            buf.push(bits as u8);
            bits = 0;
            count = 0;
        }
    }

    match count {
        2 => {
            bits <<= 12;
            buf.push((bits >> 16) as u8);
        }
        3 => {
            bits <<= 6;
            buf.push((bits >> 16) as u8);
            buf.push((bits >> 8) as u8);
        }
        _ => {}
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_decode() {
        let decoded = base64_decode("aGVsbG8=").unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn test_base64_decode_32_bytes() {
        // 32 zero bytes in base64
        let input = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let decoded = base64_decode(input).unwrap();
        assert_eq!(decoded.len(), 32);
        assert!(decoded.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_simple_hash_deterministic() {
        let h1 = simple_hash("1234");
        let h2 = simple_hash("1234");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16); // 16 hex chars
    }

    #[test]
    fn test_simple_hash_different_inputs() {
        let h1 = simple_hash("1234");
        let h2 = simple_hash("5678");
        assert_ne!(h1, h2);
    }
}
