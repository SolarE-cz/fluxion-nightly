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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaEntityState {
    pub entity_id: String,
    pub state: String,
    pub attributes: serde_json::Value,
    pub last_changed: String,
    pub last_updated: String,
}

/// Historical state point from HA history API.
///
/// With `minimal_response` HA sends full objects only for the first and last state; the others
/// carry just `state` and `last_changed`, so `entity_id`, `last_updated` and `attributes` are
/// optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaHistoryState {
    #[serde(default)]
    pub entity_id: Option<String>,
    pub state: String,
    #[serde(default)]
    pub attributes: Option<serde_json::Value>,
    pub last_changed: String,
    #[serde(default)]
    pub last_updated: Option<String>,
}

/// Parsed history data point with numeric value and timestamp
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryDataPoint {
    pub timestamp: DateTime<Utc>,
    pub value: f32,
}
