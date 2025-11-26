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

pub mod async_runtime;
pub mod async_systems;
pub mod async_tasks;
pub mod components;
pub mod config_events;
pub mod continuous_systems;
pub mod debug;
pub mod execution;
pub mod ote_market_data;
pub mod pricing;
pub mod resources;
pub mod scheduling;
pub mod strategy;
pub mod traits;
pub mod utils;
pub mod web_bridge;

pub use async_runtime::*;
pub use async_tasks::*;
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
pub use components::*;
pub use config_events::{ConfigSection, ConfigUpdateEvent};
pub use continuous_systems::{
    ContinuousSystemsPlugin, InverterDataSourceResource, PriceDataSourceResource,
    schedule_execution_system,
};
pub use debug::*;
pub use execution::*;
pub use pricing::*;
pub use resources::*;
pub use scheduling::*;
pub use strategy::*;
pub use traits::{
    EntityChange, GenericInverterState, InverterDataSource, InverterType, ModeChangeRequest,
    PriceDataSource, VendorEntityMapper,
};
pub use utils::*;
pub use web_bridge::{
    ConfigUpdateChannel, ConfigUpdateSender, InverterData, PriceBlockData, PriceData,
    PvGenerationHistoryPoint, ScheduleData, SystemHealthData, WebQueryChannel, WebQueryResponse,
    WebQuerySender, web_query_system,
};

/// Core plugin that registers fundamental ECS resources and systems
pub struct FluxionCorePlugin;

impl Plugin for FluxionCorePlugin {
    fn build(&self, app: &mut App) {
        app
            // Initialize debug mode (default: enabled for safety)
            .init_resource::<DebugModeConfig>()
            // Note: ExecutionConfig is now inserted by main.rs with configured values
            .add_systems(Startup, debug_mode_startup_system)
            // Add continuous systems plugin
            .add_plugins(ContinuousSystemsPlugin);
    }
}

/// Startup system to log debug mode status
fn debug_mode_startup_system(debug_config: Res<DebugModeConfig>) {
    if debug_config.is_enabled() {
        tracing::info!("🔍 DEBUG MODE: Enabled (safe mode - no real changes will be made)");
        tracing::info!("🔍 Set debug_mode: false in config to enable production mode");
    } else {
        DebugModeConfig::warn_production_mode();
    }
}
