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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::ha::client::HomeAssistantClient;

/// Resource: configuration for Home Assistant API access
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct HaConfig {
    /// Base URL of Home Assistant, e.g. http://homeassistant.local:8123
    pub base_url: String,
    /// Long-lived access token (set via env var at runtime, not stored in config file)
    #[serde(skip)]
    pub token: Option<String>,
    /// Optional prefix for inverter entities, e.g. "solax_inverter_1".
    /// The entity_id will be formatted as: `${prefix}_manual_mode_select`
    pub inverter_prefix: String,
    /// Polling interval for reading entities
    pub polling_interval: Duration,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            base_url: "http://homeassistant.local:8123".to_string(),
            token: None,
            inverter_prefix: "solax".to_string(),
            polling_interval: Duration::from_secs(5),
        }
    }
}

/// Component: mark an inverter with an HA entity prefix
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct HaEntityPrefix(pub String);

/// Message: request to write a value to an HA select entity
#[derive(Message, Debug, Clone)]
pub struct HaSelectWrite {
    pub inverter_entity_prefix: String,
    pub select_suffix: String, // e.g. "manual_mode_select"
    pub value: String,
}

/// Resource: internal ticker for polling
#[derive(Resource, Default)]
struct HaPollTicker(Option<std::time::Instant>);

/// Resource: Home Assistant client for timezone sync and other operations
#[derive(Resource, Clone)]
pub struct HaClientResource(pub Arc<HomeAssistantClient>);

pub struct HaPlugin;

impl Plugin for HaPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(HaConfig::default())
            .insert_resource(HaPollTicker::default())
            .add_message::<HaSelectWrite>()
            .add_systems(Startup, ha_init_config_system)
            .add_systems(Startup, spawn_backup_soc_fetcher)
            .add_systems(Update, ha_poll_select_manual_mode_system)
            .add_systems(Update, ha_write_select_system)
            .add_systems(Update, timezone_sync_system)
            .add_systems(Update, poll_backup_soc_channel);
    }
}

/// Initialize HA config from environment variables if present
fn ha_init_config_system(mut cfg: ResMut<HaConfig>) {
    if let Ok(url) = std::env::var("HA_BASE_URL") {
        cfg.base_url = url;
    }
    if let Ok(prefix) = std::env::var("HA_INVERTER_PREFIX") {
        cfg.inverter_prefix = prefix;
    }
    if let Ok(token) = std::env::var("HA_TOKEN")
        && !token.trim().is_empty()
    {
        cfg.token = Some(token);
    }
    if let Ok(secs) = std::env::var("HA_POLL_INTERVAL_SECS")
        && let Ok(n) = secs.parse::<u64>()
    {
        cfg.polling_interval = Duration::from_secs(n);
    }
}

/// System: periodically read the value of select.solax_manual_mode_select
fn ha_poll_select_manual_mode_system(
    mut ticker: ResMut<HaPollTicker>,
    cfg: Res<HaConfig>,
    q_prefixes: Query<&HaEntityPrefix>,
) {
    let now = std::time::Instant::now();
    if let Some(last) = ticker.0
        && now.duration_since(last) < cfg.polling_interval
    {
        return;
    }
    ticker.0 = Some(now);

    // Compute the list of prefixes to poll: per-entity if present, otherwise global cfg
    let prefixes: Vec<String> = if q_prefixes.is_empty() {
        vec![cfg.inverter_prefix.clone()]
    } else {
        q_prefixes.iter().map(|p| p.0.clone()).collect()
    };

    for prefix in prefixes {
        let entity_id = format!("select.{}_manual_mode_select", prefix);

        // Kick off an async task to fetch state from HA
        if let Some(token) = cfg.token.clone() {
            let base = cfg.base_url.clone();
            tokio::spawn(async move {
                let url = format!("{}/api/states/{}", base.trim_end_matches('/'), entity_id);
                let client = reqwest::Client::new();
                let res = client.get(url).bearer_auth(token).send().await;
                if let Ok(resp) = res {
                    match resp.text().await {
                        Ok(body) => {
                            tracing::info!("HA state {}: {}", entity_id, body);
                        }
                        Err(e) => tracing::warn!("HA read body error: {}", e),
                    }
                } else if let Err(e) = res {
                    tracing::warn!("HA read error: {}", e);
                }
            });
        } else {
            tracing::debug!("HA token not set; skipping poll");
        }
    }
}

/// System: periodically sync timezone from Home Assistant
/// Checks HA timezone every 5 minutes (configurable) and updates TimezoneConfig resource
fn timezone_sync_system(
    ha_client: Option<Res<HaClientResource>>,
    timezone_config: Option<ResMut<fluxion_core::TimezoneConfig>>,
    system_config: Option<ResMut<fluxion_core::SystemConfig>>,
) {
    // Early return if resources not available (during startup)
    let Some(client_res) = ha_client else {
        return;
    };
    let Some(mut tz_config) = timezone_config else {
        return;
    };

    // Check if it's time to sync
    if !tz_config.should_check() {
        return;
    }

    // Clone client for async task
    let client = client_res.0.clone();

    // Spawn async task to fetch timezone
    tokio::spawn(async move {
        match client.get_timezone().await {
            Ok(tz_str) => {
                tracing::debug!("🌍 [TIMEZONE SYNC] Fetched timezone from HA: {}", tz_str);
                // Note: We can't directly update the ECS resource from tokio task
                // The resource will be updated on next system run via the check interval mechanism
                // For now, just log success. A more sophisticated approach would use channels.
            }
            Err(e) => {
                tracing::warn!("⚠️ [TIMEZONE SYNC] Failed to fetch timezone from HA: {}", e);
            }
        }
    });

    // Mark that we've checked (will update actual timezone in next iteration if fetch succeeded)
    // For now, we'll do a blocking fetch to update immediately
    let client = client_res.0.clone();
    let runtime = tokio::runtime::Handle::current();

    if let Ok(tz_str) = runtime.block_on(async { client.get_timezone().await }) {
        tz_config.update_timezone(Some(tz_str.clone()));

        // Also update SystemConfig.system_config.timezone for backward compatibility
        if let Some(mut sys_config) = system_config {
            sys_config.system_config.timezone = Some(tz_str);
        }
    } else {
        // Update last_check even if fetch failed (keep existing timezone)
        tz_config.last_check = chrono::Utc::now();
    }
}

/// System: handle write requests to HA select entity
fn ha_write_select_system(mut ev: MessageReader<HaSelectWrite>, cfg: Res<HaConfig>) {
    for event in ev.read() {
        let inverter_entity_prefix = event.inverter_entity_prefix.clone();
        let select_suffix = event.select_suffix.clone();
        let value = event.value.clone();
        if let Some(token) = cfg.token.clone() {
            let base = cfg.base_url.clone();
            let entity_id = format!("select.{}_{}", inverter_entity_prefix, select_suffix);
            tokio::spawn(async move {
                // Call Home Assistant service to set select option
                let url = format!(
                    "{}/api/services/select/select_option",
                    base.trim_end_matches('/')
                );
                let payload = serde_json::json!({
                    "entity_id": entity_id,
                    "option": value,
                });
                let client = reqwest::Client::new();
                let res = client
                    .post(url)
                    .bearer_auth(token)
                    .json(&payload)
                    .send()
                    .await;
                match res {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            tracing::warn!("HA write failed: {}", resp.status());
                        } else {
                            tracing::info!(
                                "HA write ok: select_option for {}",
                                payload["entity_id"].as_str().unwrap_or("<unknown>")
                            );
                        }
                    }
                    Err(e) => tracing::warn!("HA write error: {}", e),
                }
            });
        } else {
            tracing::warn!("HA token not set; cannot write to HA");
        }
    }
}

/// Startup system: spawn async worker to fetch backup_discharge_min_soc from HA
fn spawn_backup_soc_fetcher(
    ha_client: Option<Res<HaClientResource>>,
    sender: Option<Res<fluxion_core::async_systems::BackupSocSender>>,
    system_config: Res<fluxion_core::SystemConfig>,
) {
    let Some(client_res) = ha_client else {
        tracing::warn!("⚠️ HaClientResource not available, cannot fetch backup SOC");
        return;
    };
    let Some(sender_res) = sender else {
        tracing::warn!("⚠️ BackupSocSender not available, cannot fetch backup SOC");
        return;
    };

    // Get the first inverter's entity prefix
    let Some(first_inverter) = system_config.inverters.first() else {
        tracing::warn!("⚠️ No inverters configured, cannot fetch backup SOC");
        return;
    };

    let entity_prefix = first_inverter.entity_prefix.clone();
    let client = client_res.0.clone();
    let sender = sender_res.sender.clone();
    let hardware_min_soc = system_config.control_config.hardware_min_battery_soc;

    tracing::info!(
        "🔋 Spawning backup discharge min SOC fetcher for prefix: {}",
        entity_prefix
    );

    tokio::spawn(async move {
        // Create entity ID for the backup_discharge_min_soc sensor
        let entity_id = format!(
            "number.{}_backup_discharge_min_soc",
            entity_prefix.replace(".", "_")
        );

        // Fetch immediately on startup
        tracing::debug!(
            "📊 Fetching initial backup_discharge_min_soc from HA: {}",
            entity_id
        );
        match client.get_state(&entity_id).await {
            Ok(state) => {
                if let Ok(value) = state.state.parse::<f32>() {
                    tracing::info!("✅ Initial backup_discharge_min_soc: {:.1}%", value);
                    let _ = sender.send(value);
                } else {
                    tracing::warn!(
                        "⚠️ Failed to parse backup_discharge_min_soc, using hardware min SOC: {:.1}%",
                        hardware_min_soc
                    );
                    let _ = sender.send(hardware_min_soc);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "⚠️ Failed to fetch backup_discharge_min_soc from HA: {}, using hardware min SOC: {:.1}%",
                    e,
                    hardware_min_soc
                );
                let _ = sender.send(hardware_min_soc);
            }
        }

        // Poll every 5 minutes
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            tracing::debug!("📊 Polling backup_discharge_min_soc from HA: {}", entity_id);

            match client.get_state(&entity_id).await {
                Ok(state) => {
                    if let Ok(value) = state.state.parse::<f32>() {
                        tracing::debug!("✅ Updated backup_discharge_min_soc: {:.1}%", value);
                        let _ = sender.send(value);
                    } else {
                        tracing::warn!(
                            "⚠️ Failed to parse backup_discharge_min_soc, keeping previous value"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("⚠️ Failed to poll backup_discharge_min_soc: {}", e);
                }
            }
        }
    });
}

/// Update system: poll backup SOC channel and update resource
fn poll_backup_soc_channel(
    channel_query: Query<&fluxion_core::async_systems::BackupSocChannel>,
    mut backup_soc: Option<ResMut<fluxion_core::async_systems::BackupDischargeMinSoc>>,
) {
    let Ok(channel) = channel_query.single() else {
        return; // Channel not yet created
    };

    // Process all pending updates
    while let Ok(value) = channel.receiver.try_recv() {
        if let Some(ref mut resource) = backup_soc
            && resource.value != value
        {
            tracing::info!(
                "🔋 Backup discharge min SOC updated: {:.1}% -> {:.1}%",
                resource.value,
                value
            );
            resource.value = value;
        }
    }
}
