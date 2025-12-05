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

use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use crossbeam_channel::Receiver;

// InverterCommand import removed - no longer using channel components

// ============= Price Fetcher (REMOVED) =============
// Price fetcher channel components have been replaced with PriceCache resource
// See resources.rs for the new PriceCache implementation

// ============= Inverter Command Writer (REMOVED) =============
// Inverter command writer channel components have been replaced with AsyncInverterWriter resource
// See resources.rs for the new AsyncInverterWriter implementation

// ============= Health Checker (REMOVED) =============
// Health check channel components have been replaced with HealthChecker resource
// See resources.rs for the new HealthChecker implementation

// ============= Inverter State Reader (REMOVED) =============
// Inverter state reader channel components have been replaced with InverterStateReader resource
// See resources.rs for the new InverterStateReader implementation

// ============= Battery History Fetcher =============

/// Component marking this entity as the battery history fetcher worker
#[derive(Component)]
pub struct BatteryHistoryFetcher {
    pub fetch_interval_secs: u64,
}

/// Component that holds a channel receiver for battery history fetch requests
/// The tuple contains (entity_id, start_time)
#[derive(Component)]
pub struct BatteryHistoryChannel {
    pub receiver: Receiver<(String, DateTime<Utc>)>,
}

// ============= Consumption History Fetcher =============

/// Component marking this entity as the consumption history fetcher worker
#[derive(Component)]
pub struct ConsumptionHistoryFetcher {
    pub source_name: String,
    pub fetch_interval_hours: u64,
}

/// Component that holds a channel receiver for consumption history updates
#[derive(Component)]
pub struct ConsumptionHistoryChannel {
    pub receiver: Receiver<Vec<crate::components::DailyEnergySummary>>,
}
