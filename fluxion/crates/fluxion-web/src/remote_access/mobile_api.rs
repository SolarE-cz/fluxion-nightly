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
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use chrono::Utc;
use fluxion_core::{
    UserControlChangeType, UserControlPersistence, UserControlUpdateEvent, WebQuerySender,
};
use fluxion_i18n::I18n;
use fluxion_mobile_types::{
    API_VERSION, MobileChartPoint, MobileControlRequest, MobileControlResponse,
    MobileStateResponse, MobileTimeSlot, MobileUserControl, VersionResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::error;

use super::MobileBundleTemplate;
use crate::UserControlApiState;

/// Shared state for mobile-facing API endpoints (served over Tor to mobile devices).
#[derive(Clone, Debug)]
pub struct MobileApiState {
    pub query_sender: WebQuerySender,
    pub i18n: Arc<I18n>,
    pub user_control_api_state: Option<UserControlApiState>,
    pub ui_version: String,
}

// ==================== Query params ====================

#[derive(Deserialize)]
struct UiBundleQuery {
    #[serde(default)]
    initial: Option<u8>,
}

// ==================== Handlers ====================

/// GET /mobile/api/version — return the current UI bundle version.
///
/// Lightweight endpoint for mobile clients to check if their cached UI is outdated
/// without downloading the full bundle.
async fn version_handler(State(state): State<MobileApiState>) -> Json<VersionResponse> {
    Json(VersionResponse {
        version: state.ui_version.clone(),
    })
}

/// GET /mobile/api/ui — serve the UI bundle HTML.
///
/// Renders the mobile template with all CSS/JS inlined. When `?initial=1` is
/// passed, the current system state is embedded as `window.__initialState` to
/// avoid a second Tor round-trip on first launch.
async fn ui_bundle_handler(
    State(state): State<MobileApiState>,
    Query(params): Query<UiBundleQuery>,
) -> impl IntoResponse {
    let initial_state = if params.initial == Some(1) {
        build_state_json(&state).await.ok()
    } else {
        None
    };

    let template = MobileBundleTemplate {
        ui_version: state.ui_version.clone(),
        initial_state,
    };

    match template.render() {
        Ok(html) => {
            let mut headers = axum::http::HeaderMap::new();
            if let Ok(val) = state.ui_version.parse() {
                headers.insert("X-UI-Version", val);
            }
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            (headers, Html(html)).into_response()
        }
        Err(e) => {
            error!("Failed to render mobile bundle: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to render UI bundle"})),
            )
                .into_response()
        }
    }
}

/// GET /mobile/api/state — return current system state as JSON snapshot.
async fn state_handler(State(state): State<MobileApiState>) -> impl IntoResponse {
    match build_state_response(&state).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => {
            error!("Failed to build mobile state: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to query system state"})),
            )
                .into_response()
        }
    }
}

/// POST /mobile/api/control — accept bulk control changes from mobile device.
///
/// Returns `403 Forbidden` for read-only devices (TODO: enforce via auth middleware).
/// Returns the updated state snapshot so the app can refresh its cache immediately.
async fn control_handler(
    State(state): State<MobileApiState>,
    Json(req): Json<MobileControlRequest>,
) -> impl IntoResponse {
    let Some(uc_api) = &state.user_control_api_state else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "User control not available"})),
        )
            .into_response();
    };

    // Apply changes to user control state
    let new_state = {
        let mut user_state = uc_api.state.write();

        if let Some(charge_enabled) = req.charge_from_grid_enabled {
            user_state.disallow_charge = !charge_enabled;
        }

        // Handle forced_mode: set restrictions based on mode.
        // "SelfUse" / "" / absent → clear all restrictions (automatic).
        // "ForceCharge" → disallow discharge.
        // "ForceDischarge" → disallow charge.
        if let Some(ref mode) = req.forced_mode {
            match mode.as_str() {
                "ForceCharge" => {
                    user_state.disallow_discharge = true;
                }
                "ForceDischarge" => {
                    user_state.disallow_charge = true;
                }
                "SelfUse" | "" => {
                    user_state.disallow_charge = false;
                    user_state.disallow_discharge = false;
                }
                _ => {}
            }
        }

        // Replace fixed time slots if provided
        if let Some(slots) = &req.fixed_time_slots {
            user_state.fixed_time_slots = slots
                .iter()
                .filter_map(|s| {
                    let from = chrono::DateTime::parse_from_rfc3339(&s.start)
                        .ok()?
                        .with_timezone(&Utc);
                    let to = chrono::DateTime::parse_from_rfc3339(&s.end)
                        .ok()?
                        .with_timezone(&Utc);
                    let mode = parse_mobile_mode(&s.mode)?;
                    Some(fluxion_types::user_control::FixedTimeSlot {
                        id: if s.id.is_empty() {
                            fluxion_types::user_control::FixedTimeSlot::generate_id()
                        } else {
                            s.id.clone()
                        },
                        from,
                        to,
                        mode,
                        note: None,
                        created_at: Utc::now(),
                    })
                })
                .collect();
        }

        user_state.last_modified = Some(Utc::now());
        user_state.clone()
    };

    // Persist to disk
    let persistence = UserControlPersistence::new(&uc_api.persistence_path);
    if let Err(e) = persistence.save(&new_state) {
        error!("Failed to persist mobile control changes: {e}");
        return Json(MobileControlResponse {
            ok: false,
            applied_at: Utc::now().to_rfc3339(),
            state: None,
            error: Some("Failed to save changes".to_owned()),
        })
        .into_response();
    }

    // Notify ECS
    if let Some(sender) = &uc_api.update_sender {
        let event =
            UserControlUpdateEvent::new(new_state, UserControlChangeType::RestrictionsChanged);
        if let Err(e) = sender.send(event) {
            error!("Failed to notify ECS of mobile control changes: {e}");
        }
    }

    // Return updated state snapshot
    let state_response = build_state_response(&state).await.ok();

    Json(MobileControlResponse {
        ok: true,
        applied_at: Utc::now().to_rfc3339(),
        state: state_response,
        error: None,
    })
    .into_response()
}

// ==================== Helpers ====================

fn parse_mobile_mode(mode: &str) -> Option<fluxion_types::InverterOperationMode> {
    use fluxion_types::InverterOperationMode;
    match mode {
        "SelfUse" => Some(InverterOperationMode::SelfUse),
        "ForceCharge" => Some(InverterOperationMode::ForceCharge),
        "ForceDischarge" => Some(InverterOperationMode::ForceDischarge),
        "BackUpMode" => Some(InverterOperationMode::BackUpMode),
        "NoChargeNoDischarge" => Some(InverterOperationMode::NoChargeNoDischarge),
        _ => None,
    }
}

async fn build_state_json(state: &MobileApiState) -> Result<String, String> {
    let response = build_state_response(state).await?;
    serde_json::to_string(&response).map_err(|e| e.to_string())
}

async fn build_state_response(state: &MobileApiState) -> Result<MobileStateResponse, String> {
    let response = state
        .query_sender
        .query_dashboard()
        .await
        .map_err(|e| format!("Dashboard query failed: {e}"))?;

    let inv = response.inverters.first();

    let battery_soc = inv.map_or(0.0, |i| i.battery_soc);
    let solar_w = inv.map_or(0.0, |i| i.pv_power_w);
    let grid_w = inv.map_or(0.0, |i| i.grid_power_w);
    let load_w = inv.and_then(|i| i.house_load_w).unwrap_or(0.0);
    let battery_w = inv.map_or(0.0, |i| i.battery_power_w);

    let mode = response.schedule.as_ref().map_or_else(
        || inv.map_or(String::new(), |i| i.mode.clone()),
        |s| s.current_mode.clone(),
    );

    let mode_reason = response.schedule.as_ref().map_or_else(
        || inv.map_or(String::new(), |i| i.mode_reason.clone()),
        |s| s.current_reason.clone(),
    );

    let current_price = response.prices.as_ref().map(|p| p.current_price);

    // Build chart data from price blocks
    let chart_data = response
        .prices
        .as_ref()
        .map(|p| {
            p.blocks
                .iter()
                .map(|b| MobileChartPoint {
                    time: b.timestamp.format("%H:%M").to_string(),
                    price: b.price,
                    mode: b.block_type.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    // Build user control section
    let user_control = if let Some(uc_api) = &state.user_control_api_state {
        let uc = uc_api.state.read();
        MobileUserControl {
            charge_from_grid_enabled: !uc.disallow_charge,
            forced_mode: None, // No direct forced_mode in current system
            fixed_time_slots: uc
                .fixed_time_slots
                .iter()
                .map(|s| MobileTimeSlot {
                    id: s.id.clone(),
                    start: s.from.to_rfc3339(),
                    end: s.to.to_rfc3339(),
                    mode: format!("{:?}", s.mode),
                })
                .collect(),
        }
    } else {
        MobileUserControl {
            charge_from_grid_enabled: true,
            forced_mode: None,
            fixed_time_slots: vec![],
        }
    };

    Ok(MobileStateResponse {
        ui_version: state.ui_version.clone(),
        api_version: API_VERSION,
        battery_soc,
        mode,
        mode_reason,
        solar_w,
        grid_w,
        load_w,
        battery_w,
        current_price,
        currency: "CZK".to_owned(),
        user_control,
        chart_data,
        access_mode: "full".to_owned(), // TODO: derive from device auth header
        timestamp: response.timestamp.to_rfc3339(),
    })
}

/// Build the router for mobile-facing API endpoints.
pub fn mobile_api_routes(state: MobileApiState) -> Router {
    Router::new()
        .route("/mobile/api/version", get(version_handler))
        .route("/mobile/api/ui", get(ui_bundle_handler))
        .route("/mobile/api/state", get(state_handler))
        .route("/mobile/api/control", post(control_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mobile_mode() {
        assert_eq!(
            parse_mobile_mode("SelfUse"),
            Some(fluxion_types::InverterOperationMode::SelfUse)
        );
        assert_eq!(
            parse_mobile_mode("ForceCharge"),
            Some(fluxion_types::InverterOperationMode::ForceCharge)
        );
        assert_eq!(
            parse_mobile_mode("ForceDischarge"),
            Some(fluxion_types::InverterOperationMode::ForceDischarge)
        );
        assert_eq!(parse_mobile_mode("Invalid"), None);
    }

    #[test]
    fn test_control_request_deserialization() {
        let json = r#"{
            "charge_from_grid_enabled": true,
            "forced_mode": "ForceCharge",
            "fixed_time_slots": [
                {"id": "s1", "start": "2026-01-31T22:00:00Z", "end": "2026-02-01T06:00:00Z", "mode": "ForceCharge"}
            ]
        }"#;
        let req: MobileControlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.charge_from_grid_enabled, Some(true));
        assert_eq!(req.forced_mode.as_deref(), Some("ForceCharge"));
        assert_eq!(req.fixed_time_slots.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_control_request_minimal() {
        let json = r#"{"charge_from_grid_enabled": false}"#;
        let req: MobileControlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.charge_from_grid_enabled, Some(false));
        assert!(req.forced_mode.is_none());
        assert!(req.fixed_time_slots.is_none());
    }

    #[test]
    fn test_version_response_serialization() {
        let response = VersionResponse {
            version: "0.2.35".to_owned(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], "0.2.35");
    }

    #[test]
    fn test_state_response_serialization() {
        let response = MobileStateResponse {
            ui_version: "0.2.35".to_owned(),
            api_version: API_VERSION,
            battery_soc: 72.5,
            mode: "SelfUse".to_owned(),
            mode_reason: "Solar covers load".to_owned(),
            solar_w: 1250.0,
            grid_w: -150.0,
            load_w: 1100.0,
            battery_w: 0.0,
            current_price: Some(3.25),
            currency: "CZK".to_owned(),
            user_control: MobileUserControl {
                charge_from_grid_enabled: true,
                forced_mode: None,
                fixed_time_slots: vec![],
            },
            chart_data: vec![MobileChartPoint {
                time: "10:00".to_owned(),
                price: 3.25,
                mode: "self-use".to_owned(),
            }],
            access_mode: "full".to_owned(),
            timestamp: "2026-01-31T10:05:00Z".to_owned(),
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["ui_version"], "0.2.35");
        assert_eq!(parsed["api_version"], 1);
        assert_eq!(parsed["battery_soc"], 72.5);
        assert_eq!(parsed["access_mode"], "full");
    }
}
