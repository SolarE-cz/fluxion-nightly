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

pub mod adapters;
pub mod client;
pub mod errors;
pub mod plugin;
pub mod types;

pub use adapters::{CzSpotPriceAdapter, HaConsumptionHistoryAdapter, HomeAssistantInverterAdapter};
pub use client::HomeAssistantClient;
pub use errors::{HaError, HaResult};
pub use plugin::HaPlugin;
pub use types::{HaEntityState, HaHistoryState, HistoryDataPoint};
