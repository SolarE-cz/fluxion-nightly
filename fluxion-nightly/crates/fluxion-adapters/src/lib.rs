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

pub mod control_prefix;
pub mod ha;
pub mod price_fallback;
pub mod solax;

// Re-export commonly used types for convenience
pub use ha::{
    ConfigurablePriceDataSource, CzSpotPriceAdapter, HaClientResource, HaConsumptionHistoryAdapter,
    HaEntityState, HaError, HaHistoryState, HaPlugin, HaResult, HistoryDataPoint,
    HomeAssistantClient, HomeAssistantInverterAdapter, PriceAdapterTimezoneHandle,
};

pub use control_prefix::ControlPrefixes;

pub use price_fallback::{PriceFallback, PriceFallbackConfig};

pub use solax::{
    SolaxChargerUseMode, SolaxEntityMapper, SolaxManualMode, SolaxUltraEntityMapper,
    create_entity_mapper, create_entity_mapper_with_control,
};
