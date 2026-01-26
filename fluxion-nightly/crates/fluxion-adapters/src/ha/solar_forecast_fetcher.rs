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
use crate::ha::types::HaEntityState;
use bevy_ecs::prelude::*;
use fluxion_core::SystemConfig;
use fluxion_core::async_systems::{
    SolarForecastChannel, SolarForecastData, SolarForecastSender, SolarForecastUpdate,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Cached discovered sensor entity IDs for each pattern
#[derive(Debug, Default)]
struct DiscoveredSensors {
    total_today: Vec<String>,
    remaining_today: Vec<String>,
    tomorrow: Vec<String>,
    initialized: bool,
}

/// Startup system: spawn async worker to fetch solar forecast from HA
pub fn spawn_solar_forecast_fetcher(
    ha_client: Option<Res<HaClientResource>>,
    sender: Option<Res<SolarForecastSender>>,
    system_config: Res<SystemConfig>,
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
        fetch_and_send_solar_forecast(&client, &discovered, &sender).await;

        // Poll at configured interval (default 60 seconds)
        loop {
            tokio::time::sleep(Duration::from_secs(config.fetch_interval_seconds)).await;
            tracing::debug!("📊 Polling solar forecast from HA");
            fetch_and_send_solar_forecast(&client, &discovered, &sender).await;
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

    // Find matching sensors for each pattern
    let total_today = find_matching_sensor_ids(&all_states, &config.sensor_total_today_pattern);
    let remaining_today =
        find_matching_sensor_ids(&all_states, &config.sensor_remaining_today_pattern);
    let tomorrow = find_matching_sensor_ids(&all_states, &config.sensor_tomorrow_pattern);

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

    Ok(())
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

/// Helper function to fetch solar forecast data and send through channel
async fn fetch_and_send_solar_forecast(
    client: &HomeAssistantClient,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
    sender: &crossbeam_channel::Sender<SolarForecastUpdate>,
) {
    let cache = discovered.read().await;

    if !cache.initialized {
        tracing::warn!("⚠️ Solar forecast sensors not yet discovered, skipping fetch");
        return;
    }

    // Fetch states for discovered sensors
    let total_today = fetch_and_sum_sensors(client, &cache.total_today).await;
    let remaining_today = fetch_and_sum_sensors(client, &cache.remaining_today).await;
    let tomorrow = fetch_and_sum_sensors(client, &cache.tomorrow).await;

    // Send update
    let update = SolarForecastUpdate {
        total_today_kwh: total_today,
        remaining_today_kwh: remaining_today,
        tomorrow_kwh: tomorrow,
    };

    tracing::debug!(
        "📊 Solar forecast: today={:.1} kWh, remaining={:.1} kWh, tomorrow={:.1} kWh",
        total_today,
        remaining_today,
        tomorrow
    );

    let _ = sender.send(update);
}

/// Fetch states for specific entity IDs and sum their values
async fn fetch_and_sum_sensors(client: &HomeAssistantClient, entity_ids: &[String]) -> f32 {
    if entity_ids.is_empty() {
        return 0.0;
    }

    let states = match client.get_states(entity_ids).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("⚠️ Failed to fetch sensor states: {}", e);
            return 0.0;
        }
    };

    let mut sum = 0.0;
    for state in &states {
        match state.state.parse::<f32>() {
            Ok(value) => {
                sum += value;
                tracing::trace!("   {} = {:.2} kWh", state.entity_id, value);
            }
            Err(_) => {
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

    sum
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
    fn test_matches_sensor_pattern_rejects_empty_suffix() {
        // sensor.energy_production_today_ should not match (underscore but no digits)
        assert!(!matches_sensor_pattern(
            "sensor.energy_production_today_",
            "sensor.energy_production_today"
        ));
    }
}
