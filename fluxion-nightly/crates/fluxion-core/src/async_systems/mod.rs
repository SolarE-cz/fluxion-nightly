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

//! Async systems module
//!
//! This module contains all async worker tasks and their corresponding ECS systems.
//! Each submodule handles a specific aspect of the async operation:
//! - `price_fetcher`: Price data fetching and schedule generation
//! - `inverter_writer`: Inverter command execution
//! - `health_checker`: Health monitoring for data sources
//! - `history_fetcher`: Consumption history fetching
//! - `state_reader`: Inverter state polling and decomposition
//! - `config_handler`: Configuration update processing

use bevy_ecs::prelude::*;
use tracing::info;

mod config_handler;
mod health_checker;
mod history_fetcher;
mod inverter_writer;
mod price_fetcher;
mod state_reader;

// Re-export public functions and types
pub use config_handler::{ConfigEventParams, config_event_handler};
pub use health_checker::{check_health_system, setup_health_checker};
pub use history_fetcher::{poll_consumption_history_channel, spawn_history_fetcher_worker};
pub use inverter_writer::setup_async_inverter_writer;
pub use price_fetcher::{setup_price_cache, update_prices_system};
pub use state_reader::{
    decompose_inverter_state, read_inverter_states_system, setup_inverter_state_reader,
};

use super::{InverterDataSourceResource, PriceDataSourceResource};
use crate::resources::{ConsumptionHistoryDataSourceResource, SystemConfig};

/// Resource to store the backup discharge minimum SOC read from HA sensor
/// This is read from number.<prefix>_backup_discharge_min_soc
#[derive(Resource, Clone)]
pub struct BackupDischargeMinSoc {
    pub value: f32,
}

impl Default for BackupDischargeMinSoc {
    fn default() -> Self {
        Self { value: 10.0 } // Default fallback
    }
}

/// Channel for receiving backup discharge min SOC updates
#[derive(Component)]
pub struct BackupSocChannel {
    pub receiver: crossbeam_channel::Receiver<f32>,
}

/// Resource to send backup discharge min SOC values from the async worker
#[derive(Resource)]
pub struct BackupSocSender {
    pub sender: crossbeam_channel::Sender<f32>,
}

/// Channel capacity for backup discharge min SOC updates
const BACKUP_SOC_CHANNEL_CAPACITY: usize = 10;

/// Startup system that spawns all long-running async worker tasks
/// These tasks run in the background and communicate via channels
pub fn setup_async_workers(
    mut commands: Commands,
    price_source: Res<PriceDataSourceResource>,
    inverter_source: Res<InverterDataSourceResource>,
    history_source: Res<ConsumptionHistoryDataSourceResource>,
    config: Res<SystemConfig>,
) {
    use bevy_tasks::AsyncComputeTaskPool;

    // Ensure AsyncComputeTaskPool is initialized
    let pool = AsyncComputeTaskPool::get();
    info!(
        "🚀 Setting up async worker entities (pool threads: {})...",
        pool.thread_num()
    );

    // Note: We don't actually use Bevy's task pool for spawning because our async code
    // uses reqwest/tokio which requires tokio runtime. Instead we rely on tokio runtime
    // being active in the main thread.

    // Note: Price cache setup is now done as a separate Bevy Startup system

    // ============= Inverter Command Writer Setup =============
    setup_async_inverter_writer(&mut commands, &inverter_source);

    // ============= Health Checker Setup =============
    setup_health_checker(&mut commands, &inverter_source, &price_source);

    // ============= Inverter State Reader Setup =============
    setup_inverter_state_reader(&mut commands, &inverter_source);

    // ============= Consumption History Fetcher Worker =============
    spawn_history_fetcher_worker(&mut commands, &history_source, &config.history);

    // ============= Backup Discharge Min SOC Fetcher Worker =============
    // Note: This fetcher requires HaClientResource which is inserted by main.rs
    // The actual worker will be spawned in a separate startup system that has access to HaClientResource
    info!(
        "🔋 Backup discharge min SOC fetcher will be initialized after HaClientResource is available"
    );

    let (backup_soc_tx, backup_soc_rx) = crossbeam_channel::bounded(BACKUP_SOC_CHANNEL_CAPACITY);
    commands.spawn(BackupSocChannel {
        receiver: backup_soc_rx,
    });
    // Store the sender in a resource so the separate startup system can use it
    commands.insert_resource(BackupSocSender {
        sender: backup_soc_tx,
    });

    info!("🎉 All async workers initialized successfully");
}
