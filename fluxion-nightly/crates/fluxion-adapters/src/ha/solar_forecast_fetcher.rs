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
use crate::ha::solar_forecast_accuracy::{AccuracyMonitor, BIAS_STATE_PATH};
use crate::ha::types::HaEntityState;
use crate::ha::ws::websocket_url;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use fluxion_core::async_systems::{
    SolarForecastChannel, SolarForecastData, SolarForecastSender, SolarForecastUpdate,
};
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
    let prefixes: Vec<&str> = system_config
        .inverters
        .iter()
        .map(|inv| inv.entity_prefix.as_str())
        .collect();
    let actual_entities = actual_pv_entities(&config.actual_pv_power_entities, &prefixes);
    let mut accuracy = AccuracyMonitor::new(PathBuf::from(BIAS_STATE_PATH), tz, actual_entities);

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
        let forecast_total = fetch_and_send_solar_forecast(&client, &discovered, &sender).await;
        accuracy.tick(&client, Utc::now(), forecast_total).await;

        // Poll at configured interval (default 60 seconds)
        loop {
            tokio::time::sleep(Duration::from_secs(config.fetch_interval_seconds)).await;
            retry_discovery_if_due(&client, &config, &discovered).await;
            tracing::debug!("📊 Polling solar forecast from HA");
            let forecast_total = fetch_and_send_solar_forecast(&client, &discovered, &sender).await;
            accuracy.tick(&client, Utc::now(), forecast_total).await;
        }
    });
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

/// Fetch and send one solar forecast update. Returns the total-today forecast (kWh) only when
/// every total-today sensor gave a number; `None` otherwise (not discovered, or a sensor missing).
async fn fetch_and_send_solar_forecast(
    client: &HomeAssistantClient,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
    sender: &crossbeam_channel::Sender<SolarForecastUpdate>,
) -> Option<f32> {
    let cache = discovered.read().await;

    if !cache.initialized {
        tracing::warn!("⚠️ Solar forecast sensors not yet discovered, skipping fetch");
        return None;
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
    let block_kwh = fetch_block_kwh(client, &forecast_sensor_ids).await;

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
    today.complete.then_some(total_today)
}

/// Fetch per-block PV forecast (UTC block start, kWh) summed across the given sensors
async fn fetch_block_kwh(
    client: &HomeAssistantClient,
    entity_ids: &[String],
) -> Vec<(DateTime<Utc>, f32)> {
    if entity_ids.is_empty() {
        return Vec::new();
    }

    match client.get_states(entity_ids).await {
        Ok(states) => block_kwh_from_states(&states),
        Err(e) => {
            tracing::warn!("⚠️ Failed to fetch solar forecast attributes: {}", e);
            Vec::new()
        }
    }
}

/// Sum per-block PV kWh of all sensors, keyed by UTC block start
fn block_kwh_from_states(states: &[HaEntityState]) -> Vec<(DateTime<Utc>, f32)> {
    let mut per_block: BTreeMap<DateTime<Utc>, f32> = BTreeMap::new();
    for state in states {
        for (block_start, kwh) in sensor_block_kwh(&state.attributes) {
            *per_block.entry(block_start).or_insert(0.0) += kwh;
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

#[cfg(test)]
mod tests {
    use super::*;

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
