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

//! Health of the spot price feed and the OTE fallback (brief H35).
//!
//! The price source in `fluxion-adapters` keeps this status up to date; the dashboard shows its
//! banner and the runtime JSON (`/chart-data`, `/export`) carries it.

use bevy_ecs::prelude::Resource;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// `price_source` value of blocks filled from the OTE fallback
pub const PRICE_SOURCE_OTE_FALLBACK: &str = "ote_fallback";

/// State of the OTE fallback
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackState {
    /// `pricing.ote_fallback_enabled` is off
    Disabled,
    /// Not needed: the price entity has the prices
    #[default]
    Idle,
    /// Needed, no usable OTE prices yet (fetch pending or retrying)
    Pending,
    /// OTE prices fill the slots missing from the price entity
    Active,
    /// OTE prices fetched but not used (validation failed)
    Rejected,
}

/// Fit of the buy price entity as `a × spot + b` over today's slots (brief H35)
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BuyPriceFit {
    pub a: f64,
    pub b: f64,
    /// Largest |buy − (a × spot + b)| over the fitted slots (CZK/kWh)
    pub max_residual: f64,
    /// Number of slots the fit rests on
    pub slots: usize,
    /// The fit is used to fill the buy price of fallback slots
    pub used: bool,
}

/// Status of the price feed for the dashboard and the runtime JSON
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceFeedStatus {
    /// The configured spot price entity
    pub entity: String,
    /// Time of day (Europe/Prague) after which tomorrow's prices are expected, "HH:MM"
    pub check_time: String,
    /// The entity lacks tomorrow's slots after `check_time`
    pub tomorrow_missing: bool,
    /// The entity lacks slots of the rest of today
    pub today_missing: bool,
    /// Tomorrow's slots the entity has / a full day has (92, 96 or 100)
    pub sensor_tomorrow_slots: usize,
    pub expected_tomorrow_slots: usize,
    /// When the current outage was first seen
    pub missing_since: Option<DateTime<Utc>>,
    pub fallback: FallbackState,
    /// Why the fallback is not in use (validation, fetch error, ...)
    pub fallback_reason: Option<String>,
    /// Slots filled from OTE in the current price data
    pub fallback_slots: usize,
    /// Share of today's overlapping slots where OTE matched the entity (0..1)
    pub validation_match: Option<f64>,
    /// EUR/CZK rate (CNB) of the fallback conversion
    pub eur_czk_rate: Option<f64>,
    pub last_fetch_attempt: Option<DateTime<Utc>>,
    pub last_fetch_error: Option<String>,
    /// Buy entity fit, when a buy entity is configured and fallback slots needed a buy price
    pub buy_fit: Option<BuyPriceFit>,
    /// Banner text for the dashboard; `None` when all is well
    pub banner: Option<String>,
}

impl PriceFeedStatus {
    /// True when the dashboard should show a banner
    #[must_use]
    pub fn has_problem(&self) -> bool {
        self.banner.is_some()
    }
}

/// Shared price feed status (written by the price source, read by the web query system)
#[derive(Resource, Clone, Debug, Default)]
pub struct PriceFeedStatusResource(pub Arc<RwLock<PriceFeedStatus>>);

impl PriceFeedStatusResource {
    #[must_use]
    pub fn snapshot(&self) -> PriceFeedStatus {
        self.0.read().clone()
    }
}
