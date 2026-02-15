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

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use tracing::{info, warn};

use fluxion_shared::heartbeat::{HeartbeatRequest, HeartbeatResponse};

use crate::config::ServerConfig;
use crate::db::Database;
use crate::notifications::EmailNotifier;

#[derive(Debug, Clone)]
pub struct HeartbeatState {
    pub db: Arc<Database>,
    pub config: Arc<ServerConfig>,
    pub notifier: Arc<EmailNotifier>,
}

#[expect(
    clippy::too_many_lines,
    reason = "sequential handler logic, not worth splitting"
)]
#[expect(clippy::unused_async, reason = "axum handler must be async")]
pub async fn heartbeat_handler(
    State(state): State<HeartbeatState>,
    Json(request): Json<HeartbeatRequest>,
) -> impl IntoResponse {
    // Validate shared secret
    if request.shared_secret != state.config.auth.shared_secret {
        warn!(
            instance_id = %request.instance_id,
            "Heartbeat rejected: invalid shared secret"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(HeartbeatResponse {
                ok: false,
                server_time: Utc::now(),
                message: Some("Invalid shared secret".to_owned()),
            }),
        );
    }

    // Check if client was previously offline (for recovery notification)
    let was_offline = state
        .db
        .get_all_clients()
        .ok()
        .and_then(|clients| {
            clients
                .iter()
                .find(|c| c.instance_id == request.instance_id)
                .map(|c| c.status == "offline")
        })
        .unwrap_or(false);

    // Upsert client record
    let payload_json = serde_json::to_string(&request).unwrap_or_default();
    if let Err(e) = state.db.upsert_client(
        &request.instance_id,
        request.status.friendly_name.as_deref(),
        Some(&request.fluxion_version),
        request.status.strategy_name.as_deref(),
        request.status.battery_soc,
        None,
    ) {
        warn!(error = %e, "Failed to upsert client");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(HeartbeatResponse {
                ok: false,
                server_time: Utc::now(),
                message: Some("Database error".to_owned()),
            }),
        );
    }

    // Log heartbeat
    if let Err(e) = state.db.log_heartbeat(&request.instance_id, &payload_json) {
        warn!(error = %e, "Failed to log heartbeat");
    }

    // Store telemetry snapshot if present
    if let Some(ref snapshot) = request.telemetry {
        if let Err(e) = state
            .db
            .insert_telemetry_snapshot(&request.instance_id, snapshot)
        {
            warn!(error = %e, "Failed to insert telemetry snapshot");
        }
        if let Ok(json) = serde_json::to_string(snapshot)
            && let Err(e) = state
                .db
                .update_latest_telemetry(&request.instance_id, &json)
        {
            warn!(error = %e, "Failed to update latest telemetry");
        }
    }

    // Store sync data if present
    if let Some(ref sync) = request.sync_data
        && let Err(e) = state.db.update_client_sync_data(
            &request.instance_id,
            sync.battery_capacity_kwh,
            sync.target_soc_max,
            sync.target_soc_min,
        )
    {
        warn!(error = %e, "Failed to update client sync data");
    }

    // Send recovery notification if client was offline
    if was_offline {
        let friendly_name = request
            .status
            .friendly_name
            .clone()
            .unwrap_or_else(|| request.instance_id.clone());
        info!(
            instance_id = %request.instance_id,
            "Client recovered from offline status"
        );

        let notifier = Arc::clone(&state.notifier);
        let db = Arc::clone(&state.db);
        let instance_id = request.instance_id.clone();
        tokio::spawn(async move {
            if let Err(e) = notifier
                .send_recovery_alert(&instance_id, &friendly_name)
                .await
            {
                tracing::error!(error = %e, "Failed to send recovery alert");
            }
            if let Err(e) = db.log_notification(&instance_id, "recovery", &notifier.recipients()) {
                tracing::error!(error = %e, "Failed to log recovery notification");
            }
        });
    }

    info!(
        instance_id = %request.instance_id,
        version = %request.fluxion_version,
        has_telemetry = request.telemetry.is_some(),
        has_sync_data = request.sync_data.is_some(),
        "Heartbeat received"
    );

    (
        StatusCode::OK,
        Json(HeartbeatResponse {
            ok: true,
            server_time: Utc::now(),
            message: None,
        }),
    )
}
