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
use std::time::Duration;

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

pub struct HaPlugin;

impl Plugin for HaPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(HaConfig::default())
            .insert_resource(HaPollTicker::default())
            .add_message::<HaSelectWrite>()
            .add_systems(Startup, ha_init_config_system)
            .add_systems(Update, ha_poll_select_manual_mode_system)
            .add_systems(Update, ha_write_select_system);
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
