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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::{ClientSyncData, TelemetrySnapshot};

#[derive(Debug, Deserialize, Serialize)]
pub struct HeartbeatRequest {
    pub instance_id: String,
    pub shared_secret: String,
    pub timestamp: DateTime<Utc>,
    pub fluxion_version: String,
    pub status: HeartbeatStatus,
    #[serde(default)]
    pub telemetry: Option<TelemetrySnapshot>,
    #[serde(default)]
    pub sync_data: Option<ClientSyncData>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HeartbeatStatus {
    pub friendly_name: Option<String>,
    pub online: bool,
    pub strategy_name: Option<String>,
    pub battery_soc: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub ok: bool,
    pub server_time: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
