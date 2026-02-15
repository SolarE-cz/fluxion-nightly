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

//! Encrypted credential storage for connection secrets.
//!
//! Uses `tauri-plugin-store` for encrypted on-device storage.
//! The store is encrypted with the device's keystore (Android Keystore / iOS Keychain).
//!
//! Cached data (UI bundle, state snapshots) is managed separately by `cache.rs` —
//! the credential store holds only connection secrets.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Stored connection credentials (persisted via tauri-plugin-store).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredConnection {
    /// Full v3 .onion address
    pub onion_address: String,
    /// x25519 private key (base64-encoded for JSON serialization)
    pub client_auth_key_b64: String,
    /// User-chosen name for this FluxION instance
    pub instance_name: String,
    /// Access level: "full" or "readonly"
    pub access_mode: String,
    /// When this connection was added
    pub added_at: DateTime<Utc>,
}

/// App-level settings stored alongside credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// Hashed PIN (never transmitted, UI-level lock only)
    pub pin_hash: Option<String>,
    /// Whether biometric unlock is enabled
    pub biometric_enabled: bool,
    /// Lock timeout in seconds when app is backgrounded (default: 300 = 5 min)
    pub lock_timeout_secs: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            pin_hash: None,
            biometric_enabled: false,
            lock_timeout_secs: 300,
        }
    }
}

/// Parse a QR code payload into a StoredConnection.
pub fn parse_qr_payload(payload: &str) -> Result<StoredConnection, String> {
    let json: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| format!("Invalid QR payload: {e}"))?;

    let version = json["v"]
        .as_i64()
        .ok_or("Missing 'v' field in QR payload")?;
    if version != 1 {
        return Err(format!("Unsupported QR protocol version: {version}"));
    }

    let onion_address = json["onion"]
        .as_str()
        .ok_or("Missing 'onion' field")?
        .to_owned();

    let client_auth_key_b64 = json["key"]
        .as_str()
        .ok_or("Missing 'key' field")?
        .to_owned();

    let instance_name = json["name"]
        .as_str()
        .ok_or("Missing 'name' field")?
        .to_owned();

    let access_mode = json["mode"].as_str().unwrap_or("full").to_owned();

    Ok(StoredConnection {
        onion_address,
        client_auth_key_b64,
        instance_name,
        access_mode,
        added_at: Utc::now(),
    })
}

const STORE_FILE: &str = "credentials.json";
const KEY_CONNECTION: &str = "connection";
const KEY_SETTINGS: &str = "settings";

/// Persist connection credentials to the encrypted store.
pub fn save_connection(app: &tauri::AppHandle, conn: &StoredConnection) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    store.set(KEY_CONNECTION, json!(conn));
    store
        .save()
        .map_err(|e| format!("Failed to save store: {e}"))?;
    Ok(())
}

/// Load persisted connection credentials, if any.
pub fn load_connection(app: &tauri::AppHandle) -> Option<StoredConnection> {
    use tauri_plugin_store::StoreExt;
    let store = app.store(STORE_FILE).ok()?;
    let val = store.get(KEY_CONNECTION)?;
    serde_json::from_value(val).ok()
}

/// Persist app settings to the encrypted store.
pub fn save_settings(app: &tauri::AppHandle, settings: &AppSettings) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    store.set(KEY_SETTINGS, json!(settings));
    store
        .save()
        .map_err(|e| format!("Failed to save store: {e}"))?;
    Ok(())
}

/// Load persisted app settings, falling back to defaults.
pub fn load_settings(app: &tauri::AppHandle) -> AppSettings {
    use tauri_plugin_store::StoreExt;
    let store = match app.store(STORE_FILE) {
        Ok(s) => s,
        Err(_) => return AppSettings::default(),
    };
    match store.get(KEY_SETTINGS) {
        Some(val) => serde_json::from_value(val).unwrap_or_default(),
        None => AppSettings::default(),
    }
}

/// Clear all persisted credentials and settings.
pub fn clear_credentials(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    store.delete(KEY_CONNECTION);
    store.delete(KEY_SETTINGS);
    store
        .save()
        .map_err(|e| format!("Failed to save store: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_qr_payload() {
        let payload = r#"{
            "v": 1,
            "onion": "exampleonionaddress.onion",
            "key": "base64privatekey==",
            "name": "FluxION Home",
            "mode": "full"
        }"#;

        let conn = parse_qr_payload(payload).unwrap();
        assert_eq!(conn.onion_address, "exampleonionaddress.onion");
        assert_eq!(conn.client_auth_key_b64, "base64privatekey==");
        assert_eq!(conn.instance_name, "FluxION Home");
        assert_eq!(conn.access_mode, "full");
    }

    #[test]
    fn test_parse_qr_payload_readonly() {
        let payload = r#"{"v":1,"onion":"test.onion","key":"abc","name":"Test","mode":"readonly"}"#;
        let conn = parse_qr_payload(payload).unwrap();
        assert_eq!(conn.access_mode, "readonly");
    }

    #[test]
    fn test_parse_qr_payload_invalid_version() {
        let payload = r#"{"v":2,"onion":"test.onion","key":"abc","name":"Test"}"#;
        let err = parse_qr_payload(payload).unwrap_err();
        assert!(err.contains("Unsupported"));
    }

    #[test]
    fn test_parse_qr_payload_missing_fields() {
        let payload = r#"{"v":1}"#;
        assert!(parse_qr_payload(payload).is_err());
    }

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        assert!(settings.pin_hash.is_none());
        assert!(!settings.biometric_enabled);
        assert_eq!(settings.lock_timeout_secs, 300);
    }
}
