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

use bevy_ecs::prelude::Component;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Component tracking health status for a data source
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Name of the data source (e.g., "inverter_source", "price_source")
    pub source_name: String,

    /// Whether the source is currently healthy
    pub is_healthy: bool,

    /// Last successful health check
    pub last_check: DateTime<Utc>,

    /// Error messages from recent checks
    pub recent_errors: Vec<String>,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            source_name: String::new(),
            is_healthy: false,
            last_check: Utc::now(),
            recent_errors: Vec::new(),
        }
    }
}

/// System health data for web API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealthData {
    pub inverter_source: bool,
    pub price_source: bool,
    pub last_update: DateTime<Utc>,
    pub errors: Vec<String>,
}
