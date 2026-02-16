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

//! Shared wire-format types for the FluxION mobile API.
//!
//! This crate defines the JSON contract between the server (`fluxion-web`)
//! and the mobile app (`fluxion-mobile`). Any field or type change here causes
//! a compile error in both codebases.

use serde::{Deserialize, Serialize};

/// Current mobile API version. Bump when making breaking changes.
pub const API_VERSION: u8 = 1;

// ==================== State response ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileStateResponse {
    pub ui_version: String,
    pub api_version: u8,
    pub battery_soc: f32,
    pub mode: String,
    pub mode_reason: String,
    pub solar_w: f32,
    pub grid_w: f32,
    pub load_w: f32,
    pub battery_w: f32,
    pub current_price: Option<f32>,
    pub currency: String,
    pub user_control: MobileUserControl,
    pub chart_data: Vec<MobileChartPoint>,
    pub access_mode: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileUserControl {
    pub charge_from_grid_enabled: bool,
    pub forced_mode: Option<String>,
    pub fixed_time_slots: Vec<MobileTimeSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileTimeSlot {
    pub id: String,
    pub start: String,
    pub end: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileChartPoint {
    pub time: String,
    pub price: f32,
    pub mode: String,
}

// ==================== Version response ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionResponse {
    pub version: String,
}

// ==================== Control request/response ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileControlRequest {
    pub charge_from_grid_enabled: Option<bool>,
    pub forced_mode: Option<String>,
    #[serde(default)]
    pub fixed_time_slots: Option<Vec<MobileTimeSlot>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileControlResponse {
    pub ok: bool,
    pub applied_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<MobileStateResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ==================== QR pairing payload ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrPayload {
    pub v: u8,
    pub onion: String,
    pub key: String,
    pub name: String,
    #[serde(default = "default_qr_mode")]
    pub mode: String,
}

fn default_qr_mode() -> String {
    "full".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_payload_roundtrip() {
        let payload = QrPayload {
            v: 1,
            onion: "test.onion".to_owned(),
            key: "base64key==".to_owned(),
            name: "FluxION Home".to_owned(),
            mode: "full".to_owned(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: QrPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.v, 1);
        assert_eq!(parsed.onion, "test.onion");
        assert_eq!(parsed.mode, "full");
    }

    #[test]
    fn test_state_response_field_names() {
        let response = MobileStateResponse {
            ui_version: "0.2.35".to_owned(),
            api_version: API_VERSION,
            battery_soc: 72.5,
            mode: "SelfUse".to_owned(),
            mode_reason: "Solar".to_owned(),
            solar_w: 1000.0,
            grid_w: 0.0,
            load_w: 800.0,
            battery_w: 200.0,
            current_price: Some(3.25),
            currency: "CZK".to_owned(),
            user_control: MobileUserControl {
                charge_from_grid_enabled: true,
                forced_mode: None,
                fixed_time_slots: vec![],
            },
            chart_data: vec![],
            access_mode: "full".to_owned(),
            timestamp: "2026-01-31T10:00:00Z".to_owned(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["ui_version"], "0.2.35");
        assert_eq!(parsed["api_version"], API_VERSION);
        assert_eq!(parsed["battery_soc"], 72.5);
    }

    #[test]
    fn test_control_request_with_defaults() {
        let json = r#"{"charge_from_grid_enabled": false}"#;
        let req: MobileControlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.charge_from_grid_enabled, Some(false));
        assert!(req.fixed_time_slots.is_none());
    }

    #[test]
    fn test_control_response_skips_none() {
        let resp = MobileControlResponse {
            ok: true,
            applied_at: "2026-01-31T10:00:00Z".to_owned(),
            state: None,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("state"));
        assert!(!json.contains("error"));
    }
}
