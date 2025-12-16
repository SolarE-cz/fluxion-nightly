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
use tracing::info;

use crate::InverterDataSourceResource;

/// Initialize async inverter writer resource
/// Replaces the complex channel spawning with a simple resource
pub fn setup_async_inverter_writer(
    commands: &mut Commands,
    inverter_source: &InverterDataSourceResource,
) {
    let writer = crate::resources::AsyncInverterWriter::new(inverter_source.0.clone());

    commands.insert_resource(writer);
    info!("✅ Async inverter writer initialized");
}
