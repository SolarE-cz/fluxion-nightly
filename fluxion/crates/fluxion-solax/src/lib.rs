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

mod entity_mapper;
mod modes;

pub use entity_mapper::{SolaxEntityMapper, SolaxUltraEntityMapper};
pub use modes::{SolaxChargerUseMode, SolaxManualMode};

use fluxion_core::{InverterType, VendorEntityMapper};
use std::sync::Arc;

/// Factory function to create the appropriate entity mapper for a given inverter type
///
/// # Arguments
/// * `inverter_type` - The type of inverter to create a mapper for
///
/// # Returns
/// An Arc-wrapped VendorEntityMapper trait object
///
/// # Panics
/// Panics if an unsupported inverter type is passed (should never happen with the enum)
pub fn create_entity_mapper(inverter_type: InverterType) -> Arc<dyn VendorEntityMapper> {
    match inverter_type {
        InverterType::Solax => Arc::new(SolaxEntityMapper::new()),
        InverterType::SolaxUltra => Arc::new(SolaxUltraEntityMapper::new()),
    }
}
