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
use chrono_tz::Tz;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::ha::client::HomeAssistantClient;
use crate::ha::levers;
use crate::ha::solar_forecast_fetcher;

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
    /// Prefix of the control entities (`select.*`, `number.*`, ...) when it differs from
    /// `inverter_prefix` (brief H36). Empty = `inverter_prefix`.
    #[serde(default)]
    pub control_prefix: String,
    /// Polling interval for reading entities
    pub polling_interval: Duration,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            base_url: "http://homeassistant.local:8123".to_string(),
            token: None,
            inverter_prefix: "solax".to_string(),
            control_prefix: String::new(),
            polling_interval: Duration::from_secs(5),
        }
    }
}

impl HaConfig {
    /// Prefix of the control entities: `control_prefix`, else `inverter_prefix` (brief H36)
    #[must_use]
    pub fn control_prefix(&self) -> &str {
        Some(self.control_prefix.trim())
            .filter(|p| !p.is_empty())
            .unwrap_or(&self.inverter_prefix)
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

/// Resource: Shared timezone handle for price adapter
/// This allows the timezone sync system to update the price adapter's timezone
/// when the Home Assistant timezone changes.
#[derive(Resource, Clone)]
pub struct PriceAdapterTimezoneHandle(pub Arc<RwLock<Option<Tz>>>);

impl PriceAdapterTimezoneHandle {
    /// Create a new timezone handle with the given shared lock
    pub fn new(handle: Arc<RwLock<Option<Tz>>>) -> Self {
        Self(handle)
    }

    /// Update the timezone value
    pub fn set_timezone(&self, tz: Option<Tz>) {
        let mut guard = self.0.write();
        if *guard != tz {
            let old_tz = guard.map(|t| t.name().to_string());
            let new_tz = tz.map(|t| t.name().to_string());
            tracing::info!(
                "🌍 [PriceAdapterTimezoneHandle] Updating timezone: {:?} -> {:?}",
                old_tz,
                new_tz
            );
        }
        *guard = tz;
    }
}

pub struct HaPlugin;

impl Plugin for HaPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(HaConfig::default())
            .insert_resource(HaPollTicker::default())
            .add_message::<HaSelectWrite>()
            .init_resource::<fluxion_core::resources::UserLeversResource>()
            .init_resource::<levers::LeversChannel>()
            .init_resource::<super::dashboard_entities::DashboardChannels>()
            .init_resource::<super::dashboard_entities::DashboardState>()
            .init_resource::<super::hot_standby::HotStandbyChannel>()
            .init_resource::<super::hot_standby::HotStandbyState>()
            .init_resource::<super::actuation::PreWakeState>()
            .insert_resource(levers::LeversSyncConfig::default())
            .add_systems(Startup, ha_init_config_system)
            // Use PostStartup for fetchers that depend on resources created via Commands
            // in setup_async_workers (which runs in Startup). Commands are deferred, so
            // resources like HdoSender/BackupSocSender aren't available until PostStartup.
            .add_systems(PostStartup, spawn_backup_soc_fetcher)
            .add_systems(PostStartup, spawn_hdo_fetcher)
            .add_systems(PostStartup, levers::spawn_levers_sync)
            .add_systems(
                PostStartup,
                solar_forecast_fetcher::spawn_solar_forecast_fetcher,
            )
            .add_systems(Update, ha_poll_select_manual_mode_system)
            .add_systems(Update, ha_write_select_system)
            .add_systems(Update, timezone_sync_system)
            .add_systems(Update, poll_backup_soc_channel)
            .add_systems(Update, poll_hdo_channel)
            .add_systems(Update, levers::poll_levers_channel)
            .add_systems(PostStartup, super::hot_standby::spawn_hot_standby_resolver)
            .add_systems(Update, super::hot_standby::hot_standby_system)
            .add_systems(Update, super::actuation::prewake_system)
            .add_systems(Update, solar_forecast_fetcher::poll_solar_forecast_channel)
            .add_systems(Update, solar_forecast_fetcher::poll_pv_correction_channel)
            .add_systems(
                Update,
                (
                    super::dashboard_entities::publish_dashboard_entities_system,
                    super::dashboard_entities::night_usage_system,
                    super::dashboard_entities::poll_lock_state_system,
                    super::dashboard_entities::lock_guard_system,
                    super::dashboard_entities::daily_plan_notification_system,
                ),
            );
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

    // Compute the list of prefixes to poll: per-entity if present, otherwise the global control
    // prefix (the manual mode select is a control entity, brief H36)
    let prefixes: Vec<String> = if q_prefixes.is_empty() {
        vec![cfg.control_prefix().to_owned()]
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
/// Also updates the price adapter's timezone handle for correct price block parsing
fn timezone_sync_system(
    ha_client: Option<Res<HaClientResource>>,
    timezone_config: Option<ResMut<fluxion_core::TimezoneConfig>>,
    system_config: Option<ResMut<fluxion_core::SystemConfig>>,
    price_adapter_tz_handle: Option<Res<PriceAdapterTimezoneHandle>>,
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
            sys_config.system_config.timezone = Some(tz_str.clone());
        }

        // Update the price adapter's timezone handle for correct price block parsing
        // This ensures the adapter uses the correct timezone when parsing index-based price arrays
        if let Some(tz_handle) = price_adapter_tz_handle {
            if let Ok(parsed_tz) = tz_str.parse::<Tz>() {
                tz_handle.set_timezone(Some(parsed_tz));
            } else {
                tracing::warn!(
                    "⚠️ [TIMEZONE SYNC] Failed to parse timezone '{}' for price adapter",
                    tz_str
                );
            }
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

    // A number entity: the control prefix when one is set (brief H36)
    let entity_prefix = first_inverter
        .control_prefix_override()
        .unwrap_or(&first_inverter.entity_prefix)
        .to_owned();
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

// ============================================================================
// HDO Schedule Fetcher (High/Low Tariff from CEZ HDO sensor)
// ============================================================================

/// Startup system: spawn async worker to fetch HDO schedule from HA
pub fn spawn_hdo_fetcher(
    ha_client: Option<Res<HaClientResource>>,
    sender: Option<Res<fluxion_core::async_systems::HdoSender>>,
    system_config: Res<fluxion_core::SystemConfig>,
) {
    let Some(client_res) = ha_client else {
        tracing::warn!("⚠️ HaClientResource not available, cannot fetch HDO schedule");
        return;
    };
    let Some(sender_res) = sender else {
        tracing::warn!("⚠️ HdoSender not available, cannot fetch HDO schedule");
        return;
    };

    // Get HDO sensor entity from V3 config
    let hdo_sensor_entity = system_config
        .strategies_config
        .winter_adaptive_v3
        .hdo_sensor_entity
        .clone();

    // Skip if HDO sensor not configured or empty
    if hdo_sensor_entity.is_empty() {
        tracing::info!("ℹ️ HDO sensor entity not configured, skipping HDO fetcher");
        return;
    }

    let client = client_res.0.clone();
    let sender = sender_res.sender.clone();

    tracing::info!(
        "⚡ Spawning HDO schedule fetcher for entity: {}",
        hdo_sensor_entity
    );

    tokio::spawn(async move {
        let mut discovery = HdoDiscovery::default();

        // Fetch immediately on startup
        tracing::debug!(
            "📊 Fetching initial HDO schedule from HA: {}",
            hdo_sensor_entity
        );
        fetch_and_send_hdo(&client, &hdo_sensor_entity, &sender, &mut discovery).await;

        // Poll every 60 minutes (HDO schedules typically update daily)
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            tracing::debug!("📊 Polling HDO schedule from HA: {}", hdo_sensor_entity);
            fetch_and_send_hdo(&client, &hdo_sensor_entity, &sender, &mut discovery).await;
        }
    });
}

/// Discovery state kept across polls, so the chosen entity and the "not found" warning log once
#[derive(Default)]
struct HdoDiscovery {
    chosen: Option<String>,
    warned_missing: bool,
}

/// Entity carries the CEZ HDO schedule as `attributes.schedule[]` (`start`, `end`, `tariff` per entry)
fn is_hdo_schedule_entity(state: &crate::ha::types::HaEntityState) -> bool {
    state
        .attributes
        .get("schedule")
        .and_then(|s| s.as_array())
        .is_some_and(|entries| {
            !entries.is_empty()
                && entries.iter().all(|e| {
                    e.get("start").is_some() && e.get("end").is_some() && e.get("tariff").is_some()
                })
        })
}

/// Entity carries the raw CEZ HDO signals at `attributes.raw_json.data.data.signals`
fn is_hdo_raw_entity(state: &crate::ha::types::HaEntityState) -> bool {
    state
        .attributes
        .get("raw_json")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("data"))
        .and_then(|d| d.get("signals"))
        .and_then(|s| s.as_array())
        .is_some_and(|signals| !signals.is_empty())
}

/// Pick the HDO entity from all HA states. Chooses by content, not by name, so renamed sensors work.
/// Order: the configured ID if present; then schedule entities; then raw-signal entities;
/// then any entity whose ID starts with the configured value. Within a tier, one whose ID starts
/// with the configured value wins, then the lowest entity ID.
pub fn select_hdo_entity<'a>(
    states: &'a [crate::ha::types::HaEntityState],
    configured: &str,
) -> Option<&'a crate::ha::types::HaEntityState> {
    if let Some(exact) = states.iter().find(|s| s.entity_id == configured) {
        return Some(exact);
    }

    let pick = |qualifies: fn(&crate::ha::types::HaEntityState) -> bool| {
        states
            .iter()
            .filter(|s| qualifies(s))
            // Prefer the configured prefix, then the most recently updated (a stale duplicate from an
            // old integration entry loses), then the lowest entity_id for determinism.
            .min_by_key(|s| {
                (
                    !s.entity_id.starts_with(configured),
                    std::cmp::Reverse(s.last_updated.clone()),
                    s.entity_id.clone(),
                )
            })
    };

    pick(is_hdo_schedule_entity)
        .or_else(|| pick(is_hdo_raw_entity))
        .or_else(|| {
            states
                .iter()
                .filter(|s| s.entity_id.starts_with(configured))
                .min_by_key(|s| s.entity_id.clone())
        })
}

/// Resolve the HDO entity from one HA states fetch (no per-entity lookup, so no "not found" error log)
async fn resolve_hdo_entity(
    client: &HomeAssistantClient,
    configured_entity: &str,
    discovery: &mut HdoDiscovery,
) -> Option<crate::ha::types::HaEntityState> {
    let states = match client.get_all_states().await {
        Ok(states) => states,
        Err(e) => {
            tracing::warn!("Failed to fetch HA states for HDO discovery: {}", e);
            return None;
        }
    };

    let Some(state) = select_hdo_entity(&states, configured_entity) else {
        if !discovery.warned_missing {
            tracing::warn!(
                "No HDO entity found for '{}': no entity has an HDO schedule or signals attribute",
                configured_entity
            );
            discovery.warned_missing = true;
        }
        return None;
    };
    discovery.warned_missing = false;

    if discovery.chosen.as_deref() != Some(state.entity_id.as_str()) {
        let candidates: Vec<&str> = states
            .iter()
            .filter(|s| is_hdo_schedule_entity(s) || is_hdo_raw_entity(s))
            .map(|s| s.entity_id.as_str())
            .collect();
        tracing::info!(
            "HDO entity for '{}' resolved to '{}' (candidates: {:?})",
            configured_entity,
            state.entity_id,
            candidates
        );
        discovery.chosen = Some(state.entity_id.clone());
    }

    Some(state.clone())
}

/// Helper function to fetch HDO data and send through channel
async fn fetch_and_send_hdo(
    client: &HomeAssistantClient,
    entity_id: &str,
    sender: &crossbeam_channel::Sender<fluxion_core::async_systems::HdoUpdateMessage>,
    discovery: &mut HdoDiscovery,
) {
    match resolve_hdo_entity(client, entity_id, discovery).await {
        Some(state) => {
            // The HDO sensor typically stores schedule data in attributes
            // Try to serialize the full state (including attributes) as JSON
            let raw_data = if let Ok(json) = serde_json::to_string(&state) {
                json
            } else {
                // Fallback: just use the state value
                state.state.clone()
            };

            // Debug: Log all sensor attributes to understand the format
            if let Some(attrs) = state.attributes.as_object() {
                tracing::debug!(
                    "📊 HDO sensor state: '{}', attributes: {:?}",
                    state.state,
                    attrs.keys().collect::<Vec<_>>()
                );
                for (key, value) in attrs {
                    tracing::debug!("   HDO attr '{}': {:?}", key, value);
                }
            } else {
                tracing::debug!(
                    "📊 HDO sensor state: '{}', attributes: {:?}",
                    state.state,
                    state.attributes
                );
            }

            // Parse low tariff periods from the raw data
            // The V3 strategy expects format like:
            // { "data": { "signals": [{ "datum": "14.01.2026", "casy": "00:00-06:00; 07:00-09:00" }] } }
            let low_tariff_periods = parse_hdo_periods_from_state(&state);

            if low_tariff_periods.is_empty() {
                tracing::warn!(
                    "⚠️ HDO schedule fetched but NO low tariff periods found! Sensor state: '{}'. Check sensor attributes format.",
                    state.state
                );
            } else {
                tracing::info!(
                    "✅ HDO schedule fetched: {} low tariff periods for today (sensor state: '{}')",
                    low_tariff_periods.len(),
                    state.state
                );
                for (start, end) in &low_tariff_periods {
                    tracing::info!("   ⚡ Low tariff: {} - {}", start, end);
                }
            }

            let message = fluxion_core::async_systems::HdoUpdateMessage {
                raw_data,
                low_tariff_periods,
            };
            let _ = sender.send(message);
        }
        None => {
            tracing::warn!(
                "⚠️ Failed to fetch HDO schedule from HA: no matching entity found for '{}'",
                entity_id
            );
        }
    }
}

/// Format an HDO time as "HH:MM"; the end-of-day value (23:59:59) shows as "24:00" like `casy`
fn hdo_time_label(time: chrono::NaiveTime) -> String {
    use chrono::Timelike;
    if time.hour() == 23 && time.minute() == 59 && time.second() == 59 {
        "24:00".to_string()
    } else {
        format!("{:02}:{:02}", time.hour(), time.minute())
    }
}

/// Parse low tariff time periods from HA entity state
/// Supports multiple formats:
/// - "casy": "00:00-06:00; 07:00-09:00; ..." (Czech CEZ format)
/// - "times": "00:00-06:00; 07:00-09:00; ..." (English alternative)
/// - "data.signals[].casy" nested structure
/// - State value containing time ranges directly
fn parse_hdo_periods_from_state(state: &crate::ha::types::HaEntityState) -> Vec<(String, String)> {
    let mut periods = Vec::new();

    // Helper function to parse time ranges from a string
    let parse_time_ranges = |text: &str| -> Vec<(String, String)> {
        let mut result = Vec::new();
        // Split by semicolon, comma, or newline
        for range_str in text.split([';', ',', '\n']) {
            let range_str = range_str.trim();
            if range_str.is_empty() {
                continue;
            }

            // Try to parse "HH:MM-HH:MM" format
            let parts: Vec<&str> = range_str.split('-').collect();
            if parts.len() == 2 {
                let start = parts[0].trim();
                let end = parts[1].trim();
                // Validate that both look like times (contain colon)
                if start.contains(':') && end.contains(':') {
                    result.push((start.to_string(), end.to_string()));
                }
            }
        }
        result
    };

    // Try various attribute names for the time schedule
    let time_attr_names = ["casy", "times", "low_tariff_times", "schedule", "periods"];
    for attr_name in time_attr_names {
        if let Some(value) = state.attributes.get(attr_name).and_then(|v| v.as_str()) {
            periods = parse_time_ranges(value);
            if !periods.is_empty() {
                tracing::debug!("HDO: Found periods in '{}' attribute", attr_name);
                break;
            }
        }
    }

    // CEZ schedule sensor: attributes.schedule[] (RFC 3339 entries).
    // Like the signals paths, use the first day only.
    if periods.is_empty()
        && let Some(entries) = state.attributes.get("schedule").and_then(|s| s.as_array())
        && let Some(day) = fluxion_core::strategy::pricing::parse_hdo_schedule_attribute(entries)
            .into_iter()
            .next()
    {
        periods = day
            .low_tariff_ranges
            .iter()
            .map(|r| (hdo_time_label(r.start), hdo_time_label(r.end)))
            .collect();
        if !periods.is_empty() {
            tracing::debug!(
                "HDO: Found periods in 'schedule' attribute (date: {})",
                day.date
            );
        }
    }

    // Try nested structures for signals data
    // Common patterns:
    // - "data.signals[].casy"
    // - "response_json.data.signals[].casy" (CEZ HDO integration)
    if periods.is_empty() {
        // List of possible paths to the signals array
        let signals_paths: Vec<Vec<&str>> = vec![
            vec!["raw_json", "data", "data", "signals"], // new cez_hdo_raw_data sensor
            vec!["data", "signals"],
            vec!["response_json", "data", "signals"], // legacy cez_hdo_lowtariffstart sensor
        ];

        for path in signals_paths {
            let mut current = Some(&state.attributes);
            for key in &path {
                current = current.and_then(|v| v.get(*key));
            }

            if let Some(signals) = current.and_then(|s| s.as_array()) {
                // Log path found
                tracing::debug!(
                    "HDO: Found signals array at path {} with {} entries",
                    path.join("."),
                    signals.len()
                );

                // Since HDO schedules are typically the same for multiple days,
                // just take the first signal's schedule (most relevant/current)
                // This avoids timezone/date matching issues
                if let Some(first_signal) = signals.first() {
                    let datum = first_signal
                        .get("datum")
                        .and_then(|d| d.as_str())
                        .unwrap_or("unknown");

                    // Try both "casy" and "times" keys
                    for key in ["casy", "times"] {
                        if let Some(value) = first_signal.get(key).and_then(|c| c.as_str()) {
                            let parsed = parse_time_ranges(value);
                            if !parsed.is_empty() {
                                tracing::info!(
                                    "HDO: Parsed {} low tariff periods from {}.{} (date: {})",
                                    parsed.len(),
                                    path.join("."),
                                    key,
                                    datum
                                );
                                periods = parsed;
                                break;
                            }
                        }
                    }
                }

                if !periods.is_empty() {
                    break; // Found periods, no need to try other paths
                }
            }
        }
    }

    // Try parsing the state value itself if it looks like time ranges
    if periods.is_empty() && state.state.contains(':') && state.state.contains('-') {
        periods = parse_time_ranges(&state.state);
        if !periods.is_empty() {
            tracing::debug!("HDO: Found periods in state value");
        }
    }

    // Fallback: scan all attributes for anything that looks like time ranges
    if periods.is_empty()
        && let Some(attrs) = state.attributes.as_object()
    {
        for (key, value) in attrs {
            if let Some(text) = value.as_str()
                && text.contains(':')
                && text.contains('-')
            {
                let parsed = parse_time_ranges(text);
                if !parsed.is_empty() {
                    tracing::debug!("HDO: Found periods in attribute '{}'", key);
                    periods = parsed;
                    break;
                }
            }
        }
    }

    periods
}

/// Update system: poll HDO channel and update resource
pub fn poll_hdo_channel(
    channel_query: Query<&fluxion_core::async_systems::HdoChannel>,
    mut hdo_data: Option<ResMut<fluxion_core::async_systems::HdoScheduleData>>,
    system_config: Option<Res<fluxion_core::SystemConfig>>,
) {
    let Ok(channel) = channel_query.single() else {
        return; // Channel not yet created
    };

    // Process all pending updates
    while let Ok(message) = channel.receiver.try_recv() {
        if let Some(ref mut resource) = hdo_data {
            tracing::info!(
                "⚡ HDO schedule updated: {} low tariff periods",
                message.low_tariff_periods.len()
            );
            resource.raw_data = Some(message.raw_data);
            resource.low_tariff_periods = message.low_tariff_periods;
            resource.last_updated = Some(chrono::Utc::now());

            // Tariff rates from the pricing config, the ones the planner's effective price uses
            if let Some(ref config) = system_config {
                resource.high_tariff_czk = config.pricing_config.hdo_high_tariff_czk;
                resource.low_tariff_czk = config.pricing_config.hdo_low_tariff_czk;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brief H36: the lock and manual-mode polls use the control prefix, else the entity prefix
    #[test]
    fn ha_config_control_prefix_falls_back_to_the_entity_prefix() {
        let mut cfg = HaConfig::default();
        assert_eq!(cfg.control_prefix(), "solax");
        cfg.control_prefix = " ".to_owned();
        assert_eq!(cfg.control_prefix(), "solax");
        cfg.control_prefix = "solax_inverter".to_owned();
        assert_eq!(cfg.control_prefix(), "solax_inverter");
        assert_eq!(cfg.inverter_prefix, "solax");
    }

    /// Test parsing CEZ HDO sensor format with response_json.data.signals structure
    #[test]
    fn test_parse_cez_hdo_format() {
        // Simulate the actual CEZ HDO sensor response
        let attributes = serde_json::json!({
            "response_json": {
                "data": {
                    "signals": [
                        {
                            "signal": "EVV1",
                            "den": "Čtvrtek",
                            "datum": chrono::Local::now().format("%d.%m.%Y").to_string(),
                            "casy": "00:00-06:00;   07:00-09:00;   10:00-13:00;   14:00-16:00;   17:00-24:00"
                        }
                    ]
                }
            },
            "icon": "mdi:home-clock",
            "friendly_name": "cez_hdo_LowTariffStart"
        });

        let state = crate::ha::types::HaEntityState {
            entity_id: "sensor.cez_hdo_raw_data".to_string(),
            state: "14:00:00".to_string(),
            attributes,
            last_changed: "2026-01-15T12:46:26.269979+00:00".to_string(),
            last_updated: "2026-01-15T12:46:26.269979+00:00".to_string(),
        };

        let periods = parse_hdo_periods_from_state(&state);

        // Should find 5 low tariff periods
        assert_eq!(periods.len(), 5, "Expected 5 low tariff periods");

        // Verify the parsed periods
        assert_eq!(periods[0], ("00:00".to_string(), "06:00".to_string()));
        assert_eq!(periods[1], ("07:00".to_string(), "09:00".to_string()));
        assert_eq!(periods[2], ("10:00".to_string(), "13:00".to_string()));
        assert_eq!(periods[3], ("14:00".to_string(), "16:00".to_string()));
        assert_eq!(periods[4], ("17:00".to_string(), "24:00".to_string()));
    }

    /// Test parsing new CEZ HDO raw_data sensor format with raw_json.data.data.signals structure
    #[test]
    fn test_parse_cez_hdo_raw_data_format() {
        // Simulate the new CEZ HDO raw_data sensor response
        let attributes = serde_json::json!({
            "raw_json": {
                "timestamp": "2026-01-21T19:34:57.476005",
                "data": {
                    "data": {
                        "signals": [
                            {
                                "signal": "EVV1",
                                "den": "Středa",
                                "datum": chrono::Local::now().format("%d.%m.%Y").to_string(),
                                "casy": "00:00-06:00;   07:00-09:00;   10:00-13:00;   14:00-16:00;   17:00-24:00"
                            }
                        ],
                        "amm": false,
                        "switchClock": false
                    },
                    "statusCode": 200
                }
            },
            "icon": "mdi:home-clock",
            "friendly_name": "ČEZ HDO surová data"
        });

        let state = crate::ha::types::HaEntityState {
            entity_id: "sensor.cez_hdo_raw_data".to_string(),
            state: "21.01.2026 19:34".to_string(),
            attributes,
            last_changed: "2026-01-21T18:34:57.485535+00:00".to_string(),
            last_updated: "2026-01-21T18:34:57.485535+00:00".to_string(),
        };

        let periods = parse_hdo_periods_from_state(&state);

        // Should find 5 low tariff periods
        assert_eq!(
            periods.len(),
            5,
            "Expected 5 low tariff periods from raw_data format"
        );

        // Verify the parsed periods
        assert_eq!(periods[0], ("00:00".to_string(), "06:00".to_string()));
        assert_eq!(periods[1], ("07:00".to_string(), "09:00".to_string()));
        assert_eq!(periods[2], ("10:00".to_string(), "13:00".to_string()));
        assert_eq!(periods[3], ("14:00".to_string(), "16:00".to_string()));
        assert_eq!(periods[4], ("17:00".to_string(), "24:00".to_string()));
    }

    /// Test parsing direct casy attribute format
    #[test]
    fn test_parse_direct_casy_format() {
        let attributes = serde_json::json!({
            "casy": "00:00-06:00; 22:00-24:00",
            "friendly_name": "HDO Sensor"
        });

        let state = crate::ha::types::HaEntityState {
            entity_id: "sensor.hdo".to_string(),
            state: "on".to_string(),
            attributes,
            last_changed: "2026-01-15T12:00:00+00:00".to_string(),
            last_updated: "2026-01-15T12:00:00+00:00".to_string(),
        };

        let periods = parse_hdo_periods_from_state(&state);

        assert_eq!(periods.len(), 2);
        assert_eq!(periods[0], ("00:00".to_string(), "06:00".to_string()));
        assert_eq!(periods[1], ("22:00".to_string(), "24:00".to_string()));
    }

    fn hdo_test_state(
        entity_id: &str,
        attributes: serde_json::Value,
    ) -> crate::ha::types::HaEntityState {
        crate::ha::types::HaEntityState {
            entity_id: entity_id.to_string(),
            state: "2026-10-14T00:00:00+02:00".to_string(),
            attributes,
            last_changed: "2026-10-14T00:00:00+00:00".to_string(),
            last_updated: "2026-10-14T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn hdo_selection_prefers_most_recently_updated_duplicate() {
        let mut stale = hdo_test_state("sensor.cez_hdo_schedule_5946", hdo_test_schedule_attrs());
        stale.last_updated = "2026-10-14T00:28:07+00:00".to_string();
        let mut fresh = hdo_test_state(
            "sensor.cez_hdo_schedule_5946_evv1",
            hdo_test_schedule_attrs(),
        );
        fresh.last_updated = "2026-10-14T01:24:08+00:00".to_string();
        let states = vec![stale, fresh];
        let chosen = select_hdo_entity(&states, "sensor.cez_hdo_raw_data").unwrap();
        assert_eq!(chosen.entity_id, "sensor.cez_hdo_schedule_5946_evv1");
    }

    fn hdo_test_schedule_attrs() -> serde_json::Value {
        serde_json::json!({
            "signal": "EVV1",
            "days": 7,
            "schedule": [
                {"start": "2026-10-14T00:00:00+02:00", "end": "2026-10-14T06:00:00+02:00", "tariff": "NT", "value": 1},
                {"start": "2026-10-14T06:00:00+02:00", "end": "2026-10-14T07:00:00+02:00", "tariff": "VT", "value": 0},
                {"start": "2026-10-14T17:00:00+02:00", "end": "2026-10-15T00:00:00+02:00", "tariff": "NT", "value": 1}
            ]
        })
    }

    fn hdo_test_raw_attrs() -> serde_json::Value {
        serde_json::json!({
            "raw_json": {"data": {"data": {"signals": [
                {"signal": "EVV1", "datum": "14.10.2026", "casy": "00:00-06:00; 07:00-09:00"}
            ]}}}
        })
    }

    #[test]
    fn test_select_hdo_entity_prefers_schedule_over_default_name() {
        let states = vec![
            hdo_test_state("sensor.cez_hdo_raw_data_5946_evv1", hdo_test_raw_attrs()),
            hdo_test_state(
                "sensor.cez_hdo_schedule_5946_evv1",
                hdo_test_schedule_attrs(),
            ),
            hdo_test_state(
                "sensor.outdoor_temperature",
                serde_json::json!({"unit": "C"}),
            ),
        ];
        let picked = select_hdo_entity(&states, "sensor.cez_hdo_raw_data").unwrap();
        assert_eq!(picked.entity_id, "sensor.cez_hdo_schedule_5946_evv1");
    }

    #[test]
    fn test_select_hdo_entity_exact_configured_id_wins() {
        let states = vec![
            hdo_test_state("sensor.cez_hdo_raw_data_5946_evv1", hdo_test_raw_attrs()),
            hdo_test_state(
                "sensor.cez_hdo_schedule_5946_evv1",
                hdo_test_schedule_attrs(),
            ),
        ];
        let picked = select_hdo_entity(&states, "sensor.cez_hdo_raw_data_5946_evv1").unwrap();
        assert_eq!(picked.entity_id, "sensor.cez_hdo_raw_data_5946_evv1");
    }

    #[test]
    fn test_select_hdo_entity_finds_renamed_schedule_sensor() {
        let states = vec![
            hdo_test_state(
                "sensor.outdoor_temperature",
                serde_json::json!({"unit": "C"}),
            ),
            hdo_test_state("sensor.my_tariff", hdo_test_schedule_attrs()),
        ];
        let picked = select_hdo_entity(&states, "sensor.cez_hdo_raw_data").unwrap();
        assert_eq!(picked.entity_id, "sensor.my_tariff");
    }

    #[test]
    fn test_select_hdo_entity_none_when_nothing_matches() {
        let states = vec![hdo_test_state(
            "sensor.outdoor_temperature",
            serde_json::json!({"unit": "C"}),
        )];
        assert!(select_hdo_entity(&states, "sensor.cez_hdo_raw_data").is_none());
    }

    #[test]
    fn test_parse_hdo_periods_from_schedule_attribute() {
        let state = hdo_test_state("sensor.my_tariff", hdo_test_schedule_attrs());
        let periods = parse_hdo_periods_from_state(&state);
        assert_eq!(
            periods,
            vec![
                ("00:00".to_string(), "06:00".to_string()),
                ("17:00".to_string(), "24:00".to_string()),
            ]
        );
    }

    /// Test that empty/invalid attributes return no periods
    #[test]
    fn test_parse_no_hdo_data() {
        let attributes = serde_json::json!({
            "friendly_name": "Some Sensor",
            "unit_of_measurement": "W"
        });

        let state = crate::ha::types::HaEntityState {
            entity_id: "sensor.power".to_string(),
            state: "1500".to_string(),
            attributes,
            last_changed: "2026-01-15T12:00:00+00:00".to_string(),
            last_updated: "2026-01-15T12:00:00+00:00".to_string(),
        };

        let periods = parse_hdo_periods_from_state(&state);

        assert!(periods.is_empty(), "Expected no periods for non-HDO sensor");
    }
}
