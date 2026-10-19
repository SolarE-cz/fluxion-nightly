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

use crate::ha::HaClientResource;
use crate::ha::client::HomeAssistantClient;
use crate::ha::hot_standby::{RegistryEntry, fetch_registry};
use crate::ha::pv_forecast_correction::{CorrectionWorker, SharedForecastStore, SharedSensorList};
use crate::ha::solar_forecast_accuracy::{AccuracyMonitor, BIAS_STATE_PATH};
use crate::ha::types::HaEntityState;
use crate::ha::ws::websocket_url;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use fluxion_core::async_systems::{
    PvCorrectionChannel, SolarForecastChannel, SolarForecastData, SolarForecastSender,
    SolarForecastUpdate, correction_to_apply,
};
use fluxion_core::pv_correction::{ForecastStore, PvCorrectionSiteResource, SNAPSHOT_PATH};
use fluxion_core::{SystemConfig, TimezoneConfig};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Cached discovered sensor entity IDs for each pattern
#[derive(Debug, Default)]
struct DiscoveredSensors {
    total_today: Vec<String>,
    remaining_today: Vec<String>,
    tomorrow: Vec<String>,
    initialized: bool,
    /// How the provider filter was applied in the last discovery
    source: Option<ProviderSource>,
    /// When discovery last ran successfully
    last_attempt: Option<Instant>,
}

/// How often provider discovery is retried while it is in a fallback
const DISCOVERY_RETRY: Duration = Duration::from_secs(5 * 60);

/// Startup system: spawn async worker to fetch solar forecast from HA
pub fn spawn_solar_forecast_fetcher(
    ha_client: Option<Res<HaClientResource>>,
    sender: Option<Res<SolarForecastSender>>,
    system_config: Res<SystemConfig>,
    timezone: Option<Res<TimezoneConfig>>,
    correction: (
        Option<Res<PvCorrectionSiteResource>>,
        Option<Res<PvCorrectionChannel>>,
    ),
) {
    let Some(client_res) = ha_client else {
        tracing::warn!("⚠️ HaClientResource not available, cannot fetch solar forecast");
        return;
    };
    let Some(sender_res) = sender else {
        tracing::warn!("⚠️ SolarForecastSender not available, cannot fetch solar forecast");
        return;
    };

    // Check if solar forecast is enabled
    if !system_config.solar_forecast.enabled {
        tracing::info!("ℹ️ Solar forecast disabled in config, skipping fetcher");
        return;
    }

    let client = client_res.0.clone();
    let sender = sender_res.sender.clone();
    let config = system_config.solar_forecast.clone();

    // Forecast accuracy monitor: local days need the HA timezone (UTC when HA does not report one)
    let tz = timezone.as_ref().and_then(|t| t.tz).unwrap_or_else(|| {
        tracing::warn!("⚠️ Home Assistant timezone unknown, forecast accuracy uses UTC days");
        chrono_tz::UTC
    });
    // Each inverter's own PV plus its read-only parallel units (pv_extra_prefixes)
    let prefixes: Vec<&str> = system_config
        .inverters
        .iter()
        .flat_map(|inv| {
            std::iter::once(inv.entity_prefix.as_str())
                .chain(inv.pv_extra_prefixes.iter().map(String::as_str))
        })
        .collect();
    let actual_entities = actual_pv_entities(&config.actual_pv_power_entities, &prefixes);
    let mut accuracy = AccuracyMonitor::new(PathBuf::from(BIAS_STATE_PATH), tz, actual_entities);

    // PV forecast correction (brief H34): daily snapshots of the planner's forecast here, the
    // factor in its own task once the sensors are discovered
    let correction = correction_setup(&config, correction);

    tracing::info!(
        "☀️ Spawning solar forecast fetcher (patterns: {}, {}, {})",
        config.sensor_total_today_pattern,
        config.sensor_remaining_today_pattern,
        config.sensor_tomorrow_pattern
    );

    tokio::spawn(async move {
        // Shared state for discovered sensors (discovered once, reused for updates)
        let discovered = Arc::new(RwLock::new(DiscoveredSensors::default()));

        // Discover sensors on startup
        tracing::info!("🔍 Discovering solar forecast sensors from HA...");
        if let Err(e) = discover_sensors(&client, &config, &discovered).await {
            tracing::error!("❌ Failed to discover solar forecast sensors: {}", e);
            return;
        }

        // Fetch immediately on startup
        tracing::debug!("📊 Fetching initial solar forecast from HA");
        let fetched = fetch_and_send_solar_forecast(&client, &discovered, &sender).await;
        record_snapshot(correction.as_ref(), &fetched);
        accuracy
            .tick(&client, Utc::now(), fetched.total_today)
            .await;

        if let Some(setup) = &correction {
            share_sensors(setup, &discovered).await;
            let forecast_entities = setup.sensors.clone();
            let worker = CorrectionWorker {
                reader: crate::ha::HaBacktestHistory::new(client.clone()),
                site: setup.site.clone(),
                store: setup.store.clone(),
                path: PathBuf::from(SNAPSHOT_PATH),
                forecast_entities,
                sender: setup.sender.clone(),
            };
            tokio::spawn(worker.run());
        }

        // Poll at configured interval (default 60 seconds)
        loop {
            tokio::time::sleep(Duration::from_secs(config.fetch_interval_seconds)).await;
            retry_discovery_if_due(&client, &config, &discovered).await;
            tracing::debug!("📊 Polling solar forecast from HA");
            if let Some(setup) = &correction {
                share_sensors(setup, &discovered).await;
            }
            let fetched = fetch_and_send_solar_forecast(&client, &discovered, &sender).await;
            record_snapshot(correction.as_ref(), &fetched);
            accuracy
                .tick(&client, Utc::now(), fetched.total_today)
                .await;
        }
    });
}

/// What the PV forecast correction needs in the fetcher task
struct CorrectionSetup {
    site: fluxion_core::pv_correction::CorrectionSite,
    store: SharedForecastStore,
    sender: crossbeam_channel::Sender<fluxion_core::pv_correction::CorrectionResult>,
    /// Discovered total-today sensors, for the worker's bootstrap
    sensors: SharedSensorList,
}

/// Publish the current discovered total-today sensors to the correction worker
async fn share_sensors(setup: &CorrectionSetup, discovered: &Arc<RwLock<DiscoveredSensors>>) {
    let current = discovered.read().await.total_today.clone();
    let mut shared = setup.sensors.lock().unwrap_or_else(|e| e.into_inner());
    if *shared != current {
        *shared = current;
    }
}

/// The correction runs when the option is on and the site and channel exist
fn correction_setup(
    config: &fluxion_core::SolarForecastConfigCore,
    (site, channel): (
        Option<Res<PvCorrectionSiteResource>>,
        Option<Res<PvCorrectionChannel>>,
    ),
) -> Option<CorrectionSetup> {
    if !config.pv_forecast_correction_enabled {
        tracing::info!(
            "ℹ️ PV forecast correction off (solar_forecast.pv_forecast_correction_enabled)"
        );
        return None;
    }
    let Some(site) = site.and_then(|s| s.0.clone()) else {
        tracing::warn!(
            "⚠️ PV forecast correction: site entities or timezone unknown, factor stays 1.0"
        );
        return None;
    };
    let channel = channel?;
    Some(CorrectionSetup {
        site,
        store: Arc::new(std::sync::Mutex::new(ForecastStore::load(
            std::path::Path::new(SNAPSHOT_PATH),
        ))),
        sender: channel.sender.clone(),
        sensors: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
}

/// Snapshot today's forecast (raw, as the planner gets it) once after local midnight, only when
/// every discovered total-today sensor forecast the day
fn record_snapshot(setup: Option<&CorrectionSetup>, fetched: &Fetched) {
    let Some(setup) = setup else {
        return;
    };
    let tz = setup.site.timezone;
    let mut store = setup.store.lock().unwrap_or_else(|e| e.into_inner());
    if store.record_snapshot(Utc::now(), tz, &fetched.per_sensor_blocks, fetched.sensors) {
        if let Some(day) = store.days.last() {
            let kwh: f32 = day.hours.iter().map(|h| h.kwh).sum();
            tracing::info!(
                "☀️ PV forecast snapshot for {}: {:.1} kWh over {} hours",
                day.date,
                kwh,
                day.hours.len()
            );
        }
        if let Err(e) = store.save(std::path::Path::new(SNAPSHOT_PATH)) {
            tracing::warn!("⚠️ Failed to save PV forecast snapshots: {}", e);
        }
    }
}

/// Discover sensors matching the configured patterns
/// This is called once at startup to cache sensor entity IDs
async fn discover_sensors(
    client: &HomeAssistantClient,
    config: &fluxion_core::SolarForecastConfigCore,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
) -> Result<(), String> {
    // Fetch all entity states from HA
    let all_states = client.get_all_states().await.map_err(|e| e.to_string())?;

    tracing::debug!(
        "📊 Fetched {} entities from HA for sensor discovery",
        all_states.len()
    );

    // Find matching sensors for each pattern, then keep the configured provider's sensors
    let pattern_sets = [
        find_matching_sensor_ids(&all_states, &config.sensor_total_today_pattern),
        find_matching_sensor_ids(&all_states, &config.sensor_remaining_today_pattern),
        find_matching_sensor_ids(&all_states, &config.sensor_tomorrow_pattern),
    ];
    let registry = if config.provider.is_empty() {
        None
    } else {
        match fetch_registry_for(client).await {
            Ok(entries) => Some(entries),
            Err(e) => {
                tracing::warn!(
                    "⚠️ Entity registry unavailable, solar sensors not filtered by provider: {}",
                    e
                );
                None
            }
        }
    };
    let selection = select_provider_sensors(pattern_sets, registry.as_deref(), &config.provider);
    let previous = discovered.read().await.source;
    match selection.source {
        ProviderSource::PatternOnly => {}
        ProviderSource::Provider => {
            if previous != Some(ProviderSource::Provider) {
                tracing::info!(
                    "☀️ Solar forecast provider '{}': using only its sensors",
                    config.provider
                );
            }
        }
        ProviderSource::RegistryUnavailable => {
            if fallback_warn_due(previous, selection.source) {
                tracing::warn!(
                    "⚠️ Solar forecast provider '{}' not checked (registry unavailable), using pattern match only; retrying every 5 min",
                    config.provider
                );
            }
        }
        ProviderSource::NoProviderMatch => {
            if fallback_warn_due(previous, selection.source) {
                tracing::warn!(
                    "⚠️ No sensors of solar forecast provider '{}' match, using pattern match only; retrying every 5 min",
                    config.provider
                );
            }
        }
    }
    let [total_today, remaining_today, tomorrow] = selection.sets;

    // Log discovered sensors
    if total_today.is_empty() {
        tracing::warn!(
            "⚠️ No sensors found matching pattern '{}' (looking for exact match or _1, _2, etc.)",
            config.sensor_total_today_pattern
        );
    } else {
        tracing::info!(
            "☀️ Discovered {} sensor(s) for total today: {:?}",
            total_today.len(),
            total_today
        );
    }

    if remaining_today.is_empty() {
        tracing::warn!(
            "⚠️ No sensors found matching pattern '{}' (looking for exact match or _1, _2, etc.)",
            config.sensor_remaining_today_pattern
        );
    } else {
        tracing::info!(
            "☀️ Discovered {} sensor(s) for remaining today: {:?}",
            remaining_today.len(),
            remaining_today
        );
    }

    if tomorrow.is_empty() {
        tracing::warn!(
            "⚠️ No sensors found matching pattern '{}' (looking for exact match or _1, _2, etc.)",
            config.sensor_tomorrow_pattern
        );
    } else {
        tracing::info!(
            "☀️ Discovered {} sensor(s) for tomorrow: {:?}",
            tomorrow.len(),
            tomorrow
        );
    }

    // Cache discovered sensors
    let mut cache = discovered.write().await;
    cache.total_today = total_today;
    cache.remaining_today = remaining_today;
    cache.tomorrow = tomorrow;
    cache.initialized = true;
    cache.source = Some(selection.source);
    cache.last_attempt = Some(Instant::now());

    Ok(())
}

/// A fallback source is retried, since the provider may appear later
fn is_fallback(source: ProviderSource) -> bool {
    matches!(
        source,
        ProviderSource::RegistryUnavailable | ProviderSource::NoProviderMatch
    )
}

/// Retry discovery while in a fallback, at most every `DISCOVERY_RETRY`
fn discovery_retry_due(
    source: Option<ProviderSource>,
    last_attempt: Option<Instant>,
    now: Instant,
) -> bool {
    source.is_some_and(is_fallback)
        && last_attempt.is_none_or(|t| now.duration_since(t) >= DISCOVERY_RETRY)
}

/// The fallback WARN is logged once per episode: when a fallback starts, not on each retry
fn fallback_warn_due(previous: Option<ProviderSource>, current: ProviderSource) -> bool {
    is_fallback(current) && !previous.is_some_and(is_fallback)
}

/// Re-run discovery when the provider filter is still in a fallback
async fn retry_discovery_if_due(
    client: &HomeAssistantClient,
    config: &fluxion_core::SolarForecastConfigCore,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
) {
    let (source, last_attempt) = {
        let cache = discovered.read().await;
        (cache.source, cache.last_attempt)
    };
    if !discovery_retry_due(source, last_attempt, Instant::now()) {
        return;
    }
    if let Err(e) = discover_sensors(client, config, discovered).await {
        tracing::warn!("⚠️ Solar sensor discovery retry failed: {}", e);
    }
}

/// Entity registry rows over the websocket, for the provider filter
async fn fetch_registry_for(client: &HomeAssistantClient) -> Result<Vec<RegistryEntry>, String> {
    let url = websocket_url(client.base_url()).ok_or("no websocket URL for the HA base URL")?;
    fetch_registry(&url, client.token())
        .await
        .map_err(|e| e.to_string())
}

/// How the provider filter was applied in one discovery run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSource {
    /// No provider configured: pattern match only
    PatternOnly,
    /// Only the provider's sensors are kept
    Provider,
    /// The entity registry could not be read: pattern match only
    RegistryUnavailable,
    /// The provider matches none of the total-today sensors: pattern match only
    NoProviderMatch,
}

/// Sensor sets for [total today, remaining today, tomorrow] after the provider filter
#[derive(Debug, PartialEq, Eq)]
struct ProviderSelection {
    sets: [Vec<String>; 3],
    source: ProviderSource,
}

/// Keep only the pattern-matched sensors whose registry platform is `provider`.
/// Falls back to the pattern-only sets when there is no provider, no registry, or no provider
/// sensor for total today.
fn select_provider_sensors(
    pattern_sets: [Vec<String>; 3],
    registry: Option<&[RegistryEntry]>,
    provider: &str,
) -> ProviderSelection {
    if provider.is_empty() {
        return ProviderSelection {
            sets: pattern_sets,
            source: ProviderSource::PatternOnly,
        };
    }
    let Some(registry) = registry else {
        return ProviderSelection {
            sets: pattern_sets,
            source: ProviderSource::RegistryUnavailable,
        };
    };
    let platforms: HashMap<&str, &str> = registry
        .iter()
        .map(|e| (e.entity_id.as_str(), e.platform.as_str()))
        .collect();
    let keep = |ids: &[String]| -> Vec<String> {
        ids.iter()
            .filter(|id| platforms.get(id.as_str()) == Some(&provider))
            .cloned()
            .collect()
    };
    let filtered = pattern_sets.each_ref().map(|ids| keep(ids));
    if filtered[0].is_empty() {
        return ProviderSelection {
            sets: pattern_sets,
            source: ProviderSource::NoProviderMatch,
        };
    }
    ProviderSelection {
        sets: filtered,
        source: ProviderSource::Provider,
    }
}

/// Power sensors compared with the forecast: the configured list, or the total PV power of each
/// inverter when none is configured
fn actual_pv_entities(configured: &[String], prefixes: &[&str]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    prefixes
        .iter()
        .map(|prefix| format!("sensor.{prefix}_pv_power_total"))
        .collect()
}

/// Find all sensor entity IDs matching the pattern
/// Pattern matching: "sensor.energy_production_today" matches:
///   - sensor.energy_production_today (exact match)
///   - sensor.energy_production_today_1 (suffix _1 to _99)
///   - sensor.energy_production_today_12 (suffix _1 to _99)
fn find_matching_sensor_ids(all_states: &[HaEntityState], pattern: &str) -> Vec<String> {
    all_states
        .iter()
        .filter(|s| matches_sensor_pattern(&s.entity_id, pattern))
        .map(|s| s.entity_id.clone())
        .collect()
}

/// Check if an entity_id matches the pattern
/// Matches exact pattern or pattern followed by _N or _NN (N = digit)
fn matches_sensor_pattern(entity_id: &str, pattern: &str) -> bool {
    if entity_id == pattern {
        return true;
    }

    // Check for pattern_N or pattern_NN suffix
    if entity_id.starts_with(pattern) && entity_id.len() > pattern.len() {
        let suffix = &entity_id[pattern.len()..];
        // Suffix must be _ followed by 1-2 digits
        if let Some(digits) = suffix.strip_prefix('_')
            && !digits.is_empty()
            && digits.len() <= 2
            && digits.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }

    false
}

/// One fetch: the total-today forecast (kWh) only when every total-today sensor gave a number
/// (`None` when not discovered, or a sensor is missing), and the per-block forecast sent
#[derive(Debug, Default)]
struct Fetched {
    total_today: Option<f32>,
    /// Per-block forecast of each sensor that answered (today and tomorrow sensors)
    per_sensor_blocks: Vec<Vec<(DateTime<Utc>, f32)>>,
    /// Discovered total-today sensors (one per array)
    sensors: usize,
}

/// Fetch and send one solar forecast update
async fn fetch_and_send_solar_forecast(
    client: &HomeAssistantClient,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
    sender: &crossbeam_channel::Sender<SolarForecastUpdate>,
) -> Fetched {
    let cache = discovered.read().await;

    if !cache.initialized {
        tracing::warn!("⚠️ Solar forecast sensors not yet discovered, skipping fetch");
        return Fetched::default();
    }

    // Fetch states for discovered sensors
    let today = fetch_sum_checked(client, &cache.total_today).await;
    let total_today = today.total;
    let remaining_today = fetch_and_sum_sensors(client, &cache.remaining_today).await;
    let tomorrow = fetch_and_sum_sensors(client, &cache.tomorrow).await;

    // Per-block PV forecast from the today/tomorrow sensors' attributes
    let forecast_sensor_ids: Vec<String> = cache
        .total_today
        .iter()
        .chain(cache.tomorrow.iter())
        .cloned()
        .collect();
    let per_sensor_blocks = fetch_block_kwh(client, &forecast_sensor_ids).await;
    let block_kwh = merge_sensor_blocks(&per_sensor_blocks);

    // Send update
    let update = SolarForecastUpdate {
        total_today_kwh: total_today,
        remaining_today_kwh: remaining_today,
        tomorrow_kwh: tomorrow,
        block_kwh,
    };

    tracing::debug!(
        "📊 Solar forecast: today={:.1} kWh, remaining={:.1} kWh, tomorrow={:.1} kWh",
        total_today,
        remaining_today,
        tomorrow
    );

    let _ = sender.send(update);
    Fetched {
        total_today: today.complete.then_some(total_today),
        per_sensor_blocks,
        sensors: cache.total_today.len(),
    }
}

/// Fetch the per-block PV forecast (UTC block start, kWh) of each sensor that answered
async fn fetch_block_kwh(
    client: &HomeAssistantClient,
    entity_ids: &[String],
) -> Vec<Vec<(DateTime<Utc>, f32)>> {
    if entity_ids.is_empty() {
        return Vec::new();
    }

    match client.get_states(entity_ids).await {
        Ok(states) => states
            .iter()
            .map(|state| sensor_block_kwh(&state.attributes))
            .collect(),
        Err(e) => {
            tracing::warn!("⚠️ Failed to fetch solar forecast attributes: {}", e);
            Vec::new()
        }
    }
}

/// Sum per-block PV kWh of all sensors, keyed by UTC block start
#[cfg(test)]
fn block_kwh_from_states(states: &[HaEntityState]) -> Vec<(DateTime<Utc>, f32)> {
    let per_sensor: Vec<Vec<(DateTime<Utc>, f32)>> = states
        .iter()
        .map(|state| sensor_block_kwh(&state.attributes))
        .collect();
    merge_sensor_blocks(&per_sensor)
}

/// Sum per-block PV kWh of the sensors, keyed by UTC block start
fn merge_sensor_blocks(per_sensor: &[Vec<(DateTime<Utc>, f32)>]) -> Vec<(DateTime<Utc>, f32)> {
    let mut per_block: BTreeMap<DateTime<Utc>, f32> = BTreeMap::new();
    for blocks in per_sensor {
        for (block_start, kwh) in blocks {
            *per_block.entry(*block_start).or_insert(0.0) += kwh;
        }
    }
    per_block.into_iter().collect()
}

/// Per-block PV kWh for one sensor.
/// Uses `watts` (W at 15-min block start) when present, otherwise `wh_period`
/// (Wh per hour, split evenly over the four blocks of that hour).
fn sensor_block_kwh(attributes: &serde_json::Value) -> Vec<(DateTime<Utc>, f32)> {
    if let Some(watts) = attributes.get("watts").and_then(|v| v.as_object()) {
        return watts
            .iter()
            .filter_map(|(ts, value)| {
                let block_start = parse_forecast_timestamp(ts)?;
                let w = value.as_f64()? as f32;
                Some((block_start, w / 1000.0 * 0.25))
            })
            .collect();
    }

    if let Some(wh_period) = attributes.get("wh_period").and_then(|v| v.as_object()) {
        let mut blocks = Vec::new();
        for (ts, value) in wh_period {
            let (Some(hour_start), Some(wh)) = (parse_forecast_timestamp(ts), value.as_f64())
            else {
                continue;
            };
            for quarter in 0..4 {
                let block_start = hour_start + chrono::Duration::minutes(15 * quarter);
                blocks.push((block_start, wh as f32 / 4000.0));
            }
        }
        return blocks;
    }

    Vec::new()
}

/// Parse an ISO-8601 timestamp with offset (e.g. "2026-10-13T10:15:00+02:00") to UTC
fn parse_forecast_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Sum of the sensor states, and whether every requested sensor gave a number
#[derive(Debug, Clone, Copy, PartialEq)]
struct SensorSum {
    total: f32,
    complete: bool,
}

/// Sum the states of the requested sensors. Incomplete when a sensor is missing from the states
/// or its value is not a number; the parsable sensors are still summed.
fn sum_sensor_states(entity_ids: &[String], states: &[HaEntityState]) -> SensorSum {
    if entity_ids.is_empty() {
        return SensorSum {
            total: 0.0,
            complete: false,
        };
    }
    let mut total = 0.0;
    let mut complete = states.len() == entity_ids.len();
    for state in states {
        match state.state.parse::<f32>() {
            Ok(value) => {
                total += value;
                tracing::trace!("   {} = {:.2} kWh", state.entity_id, value);
            }
            Err(_) => {
                complete = false;
                // Skip unavailable/unknown states silently at trace level
                if state.state != "unavailable" && state.state != "unknown" {
                    tracing::warn!(
                        "⚠️ Failed to parse sensor {} value '{}', skipping",
                        state.entity_id,
                        state.state
                    );
                }
            }
        }
    }
    SensorSum { total, complete }
}

/// Fetch states for specific entity IDs and sum their values
async fn fetch_and_sum_sensors(client: &HomeAssistantClient, entity_ids: &[String]) -> f32 {
    fetch_sum_checked(client, entity_ids).await.total
}

/// Fetch the states of the entity IDs and sum them, with the completeness check
async fn fetch_sum_checked(client: &HomeAssistantClient, entity_ids: &[String]) -> SensorSum {
    if entity_ids.is_empty() {
        return sum_sensor_states(entity_ids, &[]);
    }
    match client.get_states(entity_ids).await {
        Ok(states) => sum_sensor_states(entity_ids, &states),
        Err(e) => {
            tracing::warn!("⚠️ Failed to fetch sensor states: {}", e);
            sum_sensor_states(entity_ids, &[])
        }
    }
}

/// Update system: poll solar forecast channel and update resource
pub fn poll_solar_forecast_channel(
    channel_query: Query<&SolarForecastChannel>,
    mut forecast_data: Option<ResMut<SolarForecastData>>,
) {
    let Ok(channel) = channel_query.single() else {
        return; // Channel not yet created
    };

    // Process all pending updates
    while let Ok(update) = channel.receiver.try_recv() {
        if let Some(ref mut data) = forecast_data {
            // Check if values changed significantly (log only changes)
            let changed = (data.total_today_kwh - update.total_today_kwh).abs() > 0.1
                || (data.remaining_today_kwh - update.remaining_today_kwh).abs() > 0.1
                || (data.tomorrow_kwh - update.tomorrow_kwh).abs() > 0.1;

            if changed {
                tracing::info!(
                    "☀️ Solar forecast updated: today={:.1} kWh, remaining={:.1} kWh, tomorrow={:.1} kWh",
                    update.total_today_kwh,
                    update.remaining_today_kwh,
                    update.tomorrow_kwh
                );
            }

            data.total_today_kwh = update.total_today_kwh;
            data.remaining_today_kwh = update.remaining_today_kwh;
            data.tomorrow_kwh = update.tomorrow_kwh;
            data.block_kwh = update.block_kwh;
            data.last_updated = std::time::Instant::now();
        }
    }
}

/// Update system: apply the PV forecast correction factor (brief H34) to the forecast data.
/// The option is checked here too, so with it off the factor is 1.0 whatever arrives.
pub fn poll_pv_correction_channel(
    channel: Option<Res<PvCorrectionChannel>>,
    config: Res<SystemConfig>,
    mut forecast_data: Option<ResMut<SolarForecastData>>,
) {
    let Some(channel) = channel else {
        return;
    };
    let enabled = config.solar_forecast.pv_forecast_correction_enabled;
    while let Ok(result) = channel.receiver.try_recv() {
        if let Some(data) = forecast_data.as_mut() {
            apply_correction(data, enabled, result);
        }
    }
}

/// Store a received correction: none when the option is off, and the factor validated
/// (finite, within [0.5, 1.5], else 1.0)
fn apply_correction(
    data: &mut SolarForecastData,
    enabled: bool,
    result: fluxion_core::pv_correction::CorrectionResult,
) {
    data.correction = correction_to_apply(enabled, result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxion_core::pv_correction::CorrectionResult;

    fn correction(factor: f32) -> CorrectionResult {
        CorrectionResult {
            factor,
            days_used: 7,
            measured_kwh: 117.0,
            forecast_kwh: 100.0,
        }
    }

    #[test]
    fn received_factor_is_validated_and_dropped_when_off() {
        let mut data = SolarForecastData::default();
        apply_correction(&mut data, true, correction(1.17));
        assert!((data.correction_factor() - 1.17).abs() < 1e-6);
        for bad in [f32::NAN, f32::INFINITY, 1.6, 0.4, -2.0] {
            apply_correction(&mut data, true, correction(bad));
            assert_eq!(data.correction_factor(), 1.0, "{bad}");
        }
        apply_correction(&mut data, false, correction(1.17));
        assert!(data.correction.is_none());
        assert_eq!(data.correction_factor(), 1.0);
    }

    /// Per-block forecast of one sensor for one local day: `kwh` in each daylight block
    fn sensor_day(date: chrono::NaiveDate, kwh: f32) -> Vec<(DateTime<Utc>, f32)> {
        fluxion_core::backtest::day_block_starts(date, chrono_tz::Europe::Prague)
            .into_iter()
            .map(|t| {
                let hour = chrono::Timelike::hour(&t.with_timezone(&chrono_tz::Europe::Prague));
                (t, if (8..16).contains(&hour) { kwh } else { 0.0 })
            })
            .collect()
    }

    #[test]
    fn snapshot_needs_every_array_sensor_and_a_complete_one_replaces_a_partial() {
        // Three arrays (_4, _5, _6): today sensors forecast the 19th, tomorrow sensors the 20th
        let tz = chrono_tz::Europe::Prague;
        let d19 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let d20 = chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let at = |m: u32| {
            chrono::TimeZone::with_ymd_and_hms(&tz, 2026, 10, 19, 0, m, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        let mut store = ForecastStore::default();
        // One today sensor did not answer: two of three arrays, no snapshot
        let partial = vec![
            sensor_day(d19, 0.25),
            sensor_day(d19, 0.25),
            sensor_day(d20, 0.5),
            sensor_day(d20, 0.5),
            sensor_day(d20, 0.5),
        ];
        assert!(!store.record_snapshot(at(5), tz, &partial, 3));
        assert!(store.day(d19).is_none());
        // The next fetch in the window has all three
        let mut complete = partial.clone();
        complete.insert(2, sensor_day(d19, 0.25));
        assert!(store.record_snapshot(at(6), tz, &complete, 3));
        let day = store.day(d19).unwrap().clone();
        assert_eq!(day.sensors, 3);
        let noon = day
            .hours
            .iter()
            .find(|h| chrono::Timelike::hour(&h.start.with_timezone(&tz)) == 12);
        assert!(
            (noon.unwrap().kwh - 3.0).abs() < 1e-5,
            "3 sensors × 4 blocks × 0.25"
        );
        // Done for the day
        assert!(!store.record_snapshot(at(7), tz, &complete, 3));

        // A partial day stored earlier (before the completeness rule) is replaced
        let mut old = ForecastStore::default();
        let mut stale = day.clone();
        stale.sensors = 0;
        stale.hours.iter_mut().for_each(|h| h.kwh /= 3.0);
        assert!(old.insert(stale));
        assert!(old.record_snapshot(at(8), tz, &complete, 3));
        assert_eq!(old.day(d19).unwrap().sensors, 3);
        assert_eq!(old.days.len(), 1);
    }

    #[test]
    fn merged_forecast_still_sums_every_sensor() {
        let d19 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let merged = merge_sensor_blocks(&[sensor_day(d19, 0.25), sensor_day(d19, 0.5)]);
        assert!(merged.iter().any(|(_, k)| (*k - 0.75).abs() < 1e-6));
    }

    #[test]
    fn test_matches_sensor_pattern_exact() {
        assert!(matches_sensor_pattern(
            "sensor.energy_production_today",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_single_digit() {
        assert!(matches_sensor_pattern(
            "sensor.energy_production_today_1",
            "sensor.energy_production_today"
        ));
        assert!(matches_sensor_pattern(
            "sensor.energy_production_today_9",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_double_digit() {
        assert!(matches_sensor_pattern(
            "sensor.energy_production_today_12",
            "sensor.energy_production_today"
        ));
        assert!(matches_sensor_pattern(
            "sensor.energy_production_today_99",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_rejects_triple_digit() {
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today_123",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_rejects_non_numeric() {
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today_abc",
            "sensor.energy_production_today"
        ));
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today_1a",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_rejects_different_prefix() {
        assert!(!matches_sensor_pattern(
            "sensor.other_energy_production_today",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_matches_sensor_pattern_rejects_no_underscore() {
        // Should not match sensor.energy_production_today2 (missing underscore)
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today2",
            "sensor.energy_production_today"
        ));
    }

    #[test]
    fn test_block_kwh_sums_watts_and_wh_period_per_utc_block() {
        let states: Vec<HaEntityState> = serde_json::from_value(serde_json::json!([
            {
                "entity_id": "sensor.energy_production_today_4",
                "state": "5.0",
                "attributes": {
                    "watts": {
                        "2026-10-13T10:00:00+02:00": 1000,
                        "2026-10-13T10:15:00+02:00": 2000
                    }
                },
                "last_changed": "2026-10-13T08:00:00+00:00",
                "last_updated": "2026-10-13T08:00:00+00:00"
            },
            {
                "entity_id": "sensor.energy_production_today_5",
                "state": "2.0",
                "attributes": {
                    "watts": { "2026-10-13T10:15:00+02:00": 1000 }
                },
                "last_changed": "2026-10-13T08:00:00+00:00",
                "last_updated": "2026-10-13T08:00:00+00:00"
            },
            {
                "entity_id": "sensor.energy_production_tomorrow",
                "state": "0.4",
                "attributes": {
                    "wh_period": { "2026-10-13T10:00:00+02:00": 400 }
                },
                "last_changed": "2026-10-13T08:00:00+00:00",
                "last_updated": "2026-10-13T08:00:00+00:00"
            }
        ]))
        .unwrap();

        let blocks = block_kwh_from_states(&states);

        let utc = |h: u32, m: u32| {
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 13, h, m, 0).unwrap()
        };
        assert_eq!(blocks.len(), 4);
        let expected = [
            (utc(8, 0), 0.25 + 0.1),
            (utc(8, 15), 0.5 + 0.25 + 0.1),
            (utc(8, 30), 0.1),
            (utc(8, 45), 0.1),
        ];
        for ((start, kwh), (exp_start, exp_kwh)) in blocks.iter().zip(expected.iter()) {
            assert_eq!(start, exp_start);
            assert!((kwh - exp_kwh).abs() < 1e-5, "{start}: {kwh} != {exp_kwh}");
        }
    }

    #[test]
    fn test_matches_sensor_pattern_rejects_empty_suffix() {
        // sensor.energy_production_today_ should not match (underscore but no digits)
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today_",
            "sensor.energy_production_today"
        ));
    }

    fn row(entity_id: &str, platform: &str) -> RegistryEntry {
        RegistryEntry {
            entity_id: entity_id.to_string(),
            platform: platform.to_string(),
            unique_id: entity_id.to_string(),
            config_entry_id: None,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const OPEN_METEO: &str = "open_meteo_solar_forecast";

    #[test]
    fn provider_keeps_only_open_meteo_when_forecast_solar_is_present() {
        // (a) Forecast.Solar sensor and Open-Meteo sensor both match the pattern
        let registry = [
            row("sensor.energy_production_today", "forecast_solar"),
            row("sensor.energy_production_today_2", OPEN_METEO),
        ];
        let pattern = [
            ids(&[
                "sensor.energy_production_today",
                "sensor.energy_production_today_2",
            ]),
            Vec::new(),
            Vec::new(),
        ];
        let selection = select_provider_sensors(pattern, Some(&registry), OPEN_METEO);
        assert_eq!(selection.source, ProviderSource::Provider);
        assert_eq!(
            selection.sets[0],
            ids(&["sensor.energy_production_today_2"])
        );
    }

    #[test]
    fn provider_keeps_all_three_open_meteo_roof_planes() {
        // (b) three Open-Meteo entries, one per roof plane, all keep summing
        let registry = [
            row("sensor.energy_production_today_4", OPEN_METEO),
            row("sensor.energy_production_today_5", OPEN_METEO),
            row("sensor.energy_production_today_6", OPEN_METEO),
        ];
        let total = ids(&[
            "sensor.energy_production_today_4",
            "sensor.energy_production_today_5",
            "sensor.energy_production_today_6",
        ]);
        let selection = select_provider_sensors(
            [total.clone(), Vec::new(), Vec::new()],
            Some(&registry),
            OPEN_METEO,
        );
        assert_eq!(selection.source, ProviderSource::Provider);
        assert_eq!(selection.sets[0], total);
    }

    #[test]
    fn no_open_meteo_falls_back_to_pattern_match() {
        // (c) no Open-Meteo sensor: pattern-only sets with NoProviderMatch (WARN), and an
        // unreachable registry also falls back
        let registry = [row("sensor.energy_production_today", "forecast_solar")];
        let total = ids(&["sensor.energy_production_today"]);
        let pattern = [
            total.clone(),
            Vec::new(),
            ids(&["sensor.energy_production_tomorrow"]),
        ];
        let selection = select_provider_sensors(pattern.clone(), Some(&registry), OPEN_METEO);
        assert_eq!(selection.source, ProviderSource::NoProviderMatch);
        assert_eq!(selection.sets, pattern);

        let selection = select_provider_sensors(pattern.clone(), None, OPEN_METEO);
        assert_eq!(selection.source, ProviderSource::RegistryUnavailable);
        assert_eq!(selection.sets, pattern);
    }

    #[test]
    fn empty_provider_keeps_the_old_pattern_behaviour() {
        // (d) provider "" ignores the registry and keeps every pattern match
        let registry = [row("sensor.energy_production_today", "forecast_solar")];
        let pattern = [
            ids(&[
                "sensor.energy_production_today",
                "sensor.energy_production_today_2",
            ]),
            Vec::new(),
            Vec::new(),
        ];
        let selection = select_provider_sensors(pattern.clone(), Some(&registry), "");
        assert_eq!(selection.source, ProviderSource::PatternOnly);
        assert_eq!(selection.sets, pattern);
        let selection = select_provider_sensors(pattern.clone(), None, "");
        assert_eq!(selection.sets, pattern);
    }

    #[test]
    fn actual_entities_default_to_each_inverter_total_pv_power() {
        assert_eq!(
            actual_pv_entities(&[], &["solax"]),
            vec!["sensor.solax_pv_power_total".to_string()]
        );
        assert_eq!(
            actual_pv_entities(&["sensor.custom_pv".to_string()], &[]),
            vec!["sensor.custom_pv".to_string()]
        );
    }

    fn sensor_state(entity_id: &str, state: &str) -> HaEntityState {
        HaEntityState {
            entity_id: entity_id.to_string(),
            state: state.to_string(),
            attributes: serde_json::Value::Null,
            last_changed: String::new(),
            last_updated: String::new(),
        }
    }

    #[test]
    fn discovery_retries_only_in_a_fallback_and_every_five_minutes() {
        let now = Instant::now();
        let recent = now - Duration::from_secs(60);
        let old = now - DISCOVERY_RETRY - Duration::from_secs(1);
        assert!(discovery_retry_due(
            Some(ProviderSource::RegistryUnavailable),
            None,
            now
        ));
        assert!(discovery_retry_due(
            Some(ProviderSource::NoProviderMatch),
            Some(old),
            now
        ));
        assert!(!discovery_retry_due(
            Some(ProviderSource::NoProviderMatch),
            Some(recent),
            now
        ));
        assert!(!discovery_retry_due(
            Some(ProviderSource::Provider),
            Some(old),
            now
        ));
        assert!(!discovery_retry_due(
            Some(ProviderSource::PatternOnly),
            Some(old),
            now
        ));
        assert!(!discovery_retry_due(None, None, now));
    }

    #[test]
    fn fallback_warns_once_per_episode() {
        use ProviderSource::*;
        // Starts a fallback: warn
        assert!(fallback_warn_due(None, RegistryUnavailable));
        assert!(fallback_warn_due(Some(Provider), NoProviderMatch));
        // Still in the same fallback on a retry: no warn
        assert!(!fallback_warn_due(
            Some(RegistryUnavailable),
            RegistryUnavailable
        ));
        assert!(!fallback_warn_due(
            Some(NoProviderMatch),
            RegistryUnavailable
        ));
        // Provider found: no warn, and a later fallback is a new episode
        assert!(!fallback_warn_due(Some(NoProviderMatch), Provider));
        assert!(fallback_warn_due(Some(Provider), RegistryUnavailable));
    }

    #[test]
    fn forecast_sum_is_complete_only_when_every_sensor_gave_a_number() {
        let ids = ids(&[
            "sensor.energy_production_today_4",
            "sensor.energy_production_today_5",
        ]);
        let both = [
            sensor_state("sensor.energy_production_today_4", "3.5"),
            sensor_state("sensor.energy_production_today_5", "1.5"),
        ];
        let sum = sum_sensor_states(&ids, &both);
        assert!(sum.complete);
        assert!((sum.total - 5.0).abs() < 1e-4);

        // One plane unavailable: the sum is partial and must not become a forecast
        let one_unavailable = [
            sensor_state("sensor.energy_production_today_4", "3.5"),
            sensor_state("sensor.energy_production_today_5", "unavailable"),
        ];
        assert!(!sum_sensor_states(&ids, &one_unavailable).complete);

        // One plane missing from the answer
        let one_missing = [sensor_state("sensor.energy_production_today_4", "3.5")];
        assert!(!sum_sensor_states(&ids, &one_missing).complete);

        // No sensors at all
        assert!(!sum_sensor_states(&[], &[]).complete);
    }
}
