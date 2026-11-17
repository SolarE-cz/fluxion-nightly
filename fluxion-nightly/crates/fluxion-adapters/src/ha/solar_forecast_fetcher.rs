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
use crate::ha::pv_forecast_correction::{
    CautiousPvOutput, CorrectionWorker, SharedForecastStore, SharedProviderCount, SharedSensorList,
};
use crate::ha::solar_forecast_accuracy::{AccuracyMonitor, BIAS_STATE_PATH, SharedCautiousPv};
use crate::ha::types::HaEntityState;
use crate::ha::ws::websocket_url;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use fluxion_core::async_systems::{
    CautiousPvChannel, PvCorrectionChannel, SolarForecastChannel, SolarForecastData,
    SolarForecastSender, SolarForecastUpdate, correction_to_apply,
};
use fluxion_core::backtest::local_midnight_utc;
use fluxion_core::pv_correction::{ForecastStore, PvCorrectionSiteResource, SNAPSHOT_PATH};
use fluxion_core::{SystemConfig, TimezoneConfig};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// The sensors of each selected provider, averaged when there are several
    providers: Vec<ProviderSensors>,
    /// Named providers skipped in the last discovery (no total-today sensor)
    skipped_providers: Vec<String>,
    initialized: bool,
    /// How the provider filter was applied in the last discovery
    source: Option<ProviderSource>,
    /// When discovery last ran successfully
    last_attempt: Option<Instant>,
    /// The averaging INFO line as last logged
    averaged_info: Option<AveragedInfo>,
    /// The selection of the last discovery that found the named providers (kept while the
    /// registry is unavailable or matches nothing)
    good_providers: Vec<ProviderSensors>,
    /// Whether the averaged forecast currently has no fresh provider
    no_fresh: NoFreshEpisode,
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
    cautious_pv: Option<Res<CautiousPvChannel>>,
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
    // Cautious PV for Winter Reserve: the PV forecast correction computes the forecast error and
    // keeps the hysteresis state in its store; the bias sensor shows it. The threshold is read
    // here, at startup (an options change restarts the add-on).
    let cautious_view: SharedCautiousPv = Arc::new(std::sync::Mutex::new(None));
    accuracy = accuracy.with_cautious_pv(cautious_view.clone());
    let cautious_sender = cautious_pv.map(|channel| channel.sender.clone());
    let cautious_thresholds =
        cautious_pv_thresholds(&system_config.strategies_config.winter_reserve);

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
        let fetched = fetch_and_send_solar_forecast(&client, &discovered, &sender, tz).await;
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
                providers: setup.providers.clone(),
                sender: setup.sender.clone(),
                cautious_pv: cautious_sender.map(|sender| CautiousPvOutput {
                    thresholds: cautious_thresholds,
                    sender,
                    view: cautious_view,
                }),
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
            let fetched = fetch_and_send_solar_forecast(&client, &discovered, &sender, tz).await;
            record_snapshot(correction.as_ref(), &fetched);
            accuracy
                .tick(&client, Utc::now(), fetched.total_today)
                .await;
        }
    });
}

/// Cautious-PV thresholds the correction worker uses, from `strategies.winter_reserve`
fn cautious_pv_thresholds(
    wr: &fluxion_types::config::WinterReserveConfigCore,
) -> fluxion_core::strategy::winter_reserve::CautiousPvThresholds {
    fluxion_core::strategy::winter_reserve::CautiousPvThresholds {
        on_pct: wr.cautious_pv_mae_pct,
        off_pct: wr.cautious_pv_off_pct,
    }
}

/// What the PV forecast correction needs in the fetcher task
struct CorrectionSetup {
    site: fluxion_core::pv_correction::CorrectionSite,
    store: SharedForecastStore,
    sender: crossbeam_channel::Sender<fluxion_core::pv_correction::CorrectionResult>,
    /// Discovered total-today sensors, for the worker's bootstrap
    sensors: SharedSensorList,
    /// Providers averaged in the last discovery
    providers: SharedProviderCount,
}

/// Publish the current discovered total-today sensors and provider count to the correction worker
async fn share_sensors(setup: &CorrectionSetup, discovered: &Arc<RwLock<DiscoveredSensors>>) {
    let (current, providers) = {
        let cache = discovered.read().await;
        (cache.total_today.clone(), cache.providers.len())
    };
    let mut shared = setup.sensors.lock().unwrap_or_else(|e| e.into_inner());
    if *shared != current {
        *shared = current;
    }
    *setup.providers.lock().unwrap_or_else(|e| e.into_inner()) = providers;
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
        providers: Arc::new(std::sync::Mutex::new(1)),
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

    // Find matching sensors for each pattern, then keep the configured providers' sensors
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
    let several = provider_names(&config.provider).len() > 1;
    let mut selection =
        select_provider_sensors(pattern_sets, registry.as_deref(), &config.provider);
    let (previous, previous_skipped, good) = {
        let cache = discovered.read().await;
        (
            cache.source,
            cache.skipped_providers.clone(),
            cache.good_providers.clone(),
        )
    };
    let kept = apply_last_good(&mut selection, several, &good);
    for name in selection
        .skipped
        .iter()
        .filter(|name| !previous_skipped.contains(*name))
    {
        tracing::warn!(
            "⚠️ Solar forecast provider '{}' has no total-today sensor, skipped; retrying every 5 min",
            name
        );
    }
    for provider in selection
        .providers
        .iter()
        .filter(|p| previous_skipped.contains(&p.name))
    {
        tracing::info!(
            "☀️ Solar forecast provider '{}' has sensors now, included",
            provider.name
        );
    }
    match selection.source {
        ProviderSource::PatternOnly => {}
        ProviderSource::Provider => {
            if previous != Some(ProviderSource::Provider) {
                match selection.providers.as_slice() {
                    [only] => tracing::info!(
                        "☀️ Solar forecast provider '{}': using only its sensors",
                        only.name
                    ),
                    several => {
                        let names: Vec<String> =
                            several.iter().map(|p| format!("'{}'", p.name)).collect();
                        tracing::info!(
                            "☀️ Solar forecast providers {}: averaged",
                            names.join(", ")
                        );
                    }
                }
            }
        }
        ProviderSource::RegistryUnavailable | ProviderSource::NoProviderMatch if kept => {
            if fallback_warn_due(previous, selection.source) {
                tracing::warn!(
                    "⚠️ Solar forecast providers '{}' not all checked (registry unavailable or no match), keeping the last good selection; retrying every 5 min",
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
                if several {
                    tracing::warn!(
                        "⚠️ Several solar forecast providers named with no last good selection: their totals may be summed across providers"
                    );
                }
            }
        }
        ProviderSource::NoProviderMatch => {
            if fallback_warn_due(previous, selection.source) {
                tracing::warn!(
                    "⚠️ No sensors of solar forecast provider '{}' match, using pattern match only; retrying every 5 min",
                    config.provider
                );
                if several {
                    tracing::warn!(
                        "⚠️ Several solar forecast providers named with no last good selection: their totals may be summed across providers"
                    );
                }
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
    if selection.source == ProviderSource::Provider {
        cache.good_providers = selection.providers.clone();
    }
    cache.providers = selection.providers;
    cache.skipped_providers = selection.skipped;
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

/// Retry discovery while a named provider is skipped (its sensors may appear), at most every
/// `DISCOVERY_RETRY`
fn skipped_retry_due(skipped: bool, last_attempt: Option<Instant>, now: Instant) -> bool {
    skipped && last_attempt.is_none_or(|t| now.duration_since(t) >= DISCOVERY_RETRY)
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
    let (source, last_attempt, skipped) = {
        let cache = discovered.read().await;
        (
            cache.source,
            cache.last_attempt,
            !cache.skipped_providers.is_empty(),
        )
    };
    let now = Instant::now();
    if !discovery_retry_due(source, last_attempt, now)
        && !skipped_retry_due(skipped, last_attempt, now)
    {
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

/// The sensors of one provider (registry platform), in the three sets
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProviderSensors {
    name: String,
    total_today: Vec<String>,
    remaining_today: Vec<String>,
    tomorrow: Vec<String>,
}

/// Sensor sets for [total today, remaining today, tomorrow] after the provider filter, and the
/// sensors of each selected provider
#[derive(Debug, PartialEq, Eq)]
struct ProviderSelection {
    sets: [Vec<String>; 3],
    source: ProviderSource,
    /// Selected providers, one entry for the pattern match when there is no provider filter
    providers: Vec<ProviderSensors>,
    /// Named providers skipped for having no total-today sensor
    skipped: Vec<String>,
}

/// The pattern-matched sets as one unfiltered provider
fn pattern_only(sets: [Vec<String>; 3], source: ProviderSource) -> ProviderSelection {
    let providers = vec![ProviderSensors {
        name: String::new(),
        total_today: sets[0].clone(),
        remaining_today: sets[1].clone(),
        tomorrow: sets[2].clone(),
    }];
    ProviderSelection {
        sets,
        source,
        providers,
        skipped: Vec::new(),
    }
}

/// Keep only the pattern-matched sensors whose registry platform is one of the `provider` names
/// (comma-separated). Each named provider keeps its own sensors, so they can be averaged.
/// Falls back to the pattern-only sets when there is no provider, no registry, or no named
/// provider with a sensor for total today.
fn select_provider_sensors(
    pattern_sets: [Vec<String>; 3],
    registry: Option<&[RegistryEntry]>,
    provider: &str,
) -> ProviderSelection {
    let names = provider_names(provider);
    if names.is_empty() {
        return pattern_only(pattern_sets, ProviderSource::PatternOnly);
    }
    let Some(registry) = registry else {
        return pattern_only(pattern_sets, ProviderSource::RegistryUnavailable);
    };
    let platforms: HashMap<&str, &str> = registry
        .iter()
        .map(|e| (e.entity_id.as_str(), e.platform.as_str()))
        .collect();
    let mut providers: Vec<ProviderSensors> = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        if providers.iter().any(|p| p.name == name) {
            continue;
        }
        let keep = |ids: &[String]| -> Vec<String> {
            ids.iter()
                .filter(|id| platforms.get(id.as_str()) == Some(&name))
                .cloned()
                .collect()
        };
        let [total_today, remaining_today, tomorrow] = pattern_sets.each_ref().map(|ids| keep(ids));
        if total_today.is_empty() {
            skipped.push(name.to_string());
            continue;
        }
        providers.push(ProviderSensors {
            name: name.to_string(),
            total_today,
            remaining_today,
            tomorrow,
        });
    }
    if providers.is_empty() {
        let mut selection = pattern_only(pattern_sets, ProviderSource::NoProviderMatch);
        selection.skipped = skipped;
        return selection;
    }
    ProviderSelection {
        sets: flat_sets(&providers),
        source: ProviderSource::Provider,
        providers,
        skipped,
    }
}

/// The provider names of the option: comma-separated, trimmed, empty parts dropped
fn provider_names(provider: &str) -> Vec<&str> {
    provider
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The sensors of the providers, each set flattened over the providers
fn flat_sets(providers: &[ProviderSensors]) -> [Vec<String>; 3] {
    [
        providers
            .iter()
            .flat_map(|p| p.total_today.iter().cloned())
            .collect(),
        providers
            .iter()
            .flat_map(|p| p.remaining_today.iter().cloned())
            .collect(),
        providers
            .iter()
            .flat_map(|p| p.tomorrow.iter().cloned())
            .collect(),
    ]
}

/// Several providers named, and the registry is unavailable or matches none of them: keep the
/// last good per-provider selection when there is one. Returns whether it was kept; otherwise the
/// selection stays the pattern-only fallback.
fn apply_last_good(
    selection: &mut ProviderSelection,
    several: bool,
    good: &[ProviderSensors],
) -> bool {
    if !several || good.is_empty() || !is_fallback(selection.source) {
        return false;
    }
    selection.sets = flat_sets(good);
    selection.providers = good.to_vec();
    true
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

/// One provider's fetched sums and per-block forecast
struct ProviderFetch {
    name: String,
    total_today: SensorSum,
    remaining_today: SensorSum,
    tomorrow: SensorSum,
    /// Every sensor of the provider was updated since local midnight
    fresh: bool,
    /// Per-block forecast of each of the provider's today and tomorrow sensors that answered
    blocks: Vec<Vec<(DateTime<Utc>, f32)>>,
}

/// The providers' forecast, averaged
#[derive(Debug, Default, PartialEq)]
struct Averaged {
    total_today: f32,
    remaining_today: f32,
    tomorrow: f32,
    /// Every selected provider gave a complete total for today and, when several are averaged,
    /// was updated since local midnight. Only then the total and the snapshot are used.
    answered: bool,
    /// Providers in the mean
    used: usize,
    block_kwh: Vec<(DateTime<Utc>, f32)>,
}

/// Average the providers. One provider gives its own sums and blocks unchanged. With several, a
/// provider not updated since local midnight is left out for this fetch: Forecast.Solar updates
/// its state hourly, so after midnight its `_today` still holds yesterday's total.
fn average_providers(
    providers: &[ProviderFetch],
    today: chrono::NaiveDate,
    tz: chrono_tz::Tz,
) -> Averaged {
    let several = providers.len() > 1;
    let used: Vec<&ProviderFetch> = providers.iter().filter(|p| !several || p.fresh).collect();
    let tomorrow_date = today + chrono::Duration::days(1);
    let per_provider: Vec<Vec<(DateTime<Utc>, f32)>> = used
        .iter()
        .map(|p| merge_sensor_blocks(&p.blocks))
        .collect();
    let total_today = averaged_sum(&used, |p| p.total_today);
    let tomorrow = averaged_sum(&used, |p| p.tomorrow);
    // Accepted choice: a single provider in use is not scaled to its own total
    let block_kwh = if used.len() > 1 {
        averaged_blocks(
            &per_provider,
            [(today, total_today), (tomorrow_date, tomorrow)],
            tz,
        )
    } else {
        per_provider.into_iter().next().unwrap_or_default()
    };
    Averaged {
        total_today,
        remaining_today: averaged_sum(&used, |p| p.remaining_today),
        tomorrow,
        answered: providers
            .iter()
            .all(|p| p.total_today.complete && (!several || p.fresh)),
        used: used.len(),
        block_kwh,
    }
}

/// Mean of the used providers' sums whose sum is complete (an incomplete provider is left out,
/// not counted as 0). Accepted choice: when none is complete, the mean of the partial sums, which
/// is what one provider gives on its own.
fn averaged_sum(providers: &[&ProviderFetch], pick: fn(&ProviderFetch) -> SensorSum) -> f32 {
    let sums: Vec<SensorSum> = providers.iter().map(|p| pick(p)).collect();
    let complete: Vec<f32> = sums
        .iter()
        .filter(|s| s.complete)
        .map(|s| s.total)
        .collect();
    if complete.is_empty() {
        let partial: Vec<f32> = sums.iter().map(|s| s.total).collect();
        mean(&partial)
    } else {
        mean(&complete)
    }
}

fn mean(values: &[f32]) -> f32 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f32>() / values.len() as f32
    }
}

/// Local day of a UTC block start
fn local_day(block_start: DateTime<Utc>, tz: chrono_tz::Tz) -> chrono::NaiveDate {
    block_start.with_timezone(&tz).date_naive()
}

/// Per-block shape of the providers, per local day: the mean over the providers that have blocks
/// on that day. Each day is then scaled so its total is the averaged total for today or tomorrow
/// (a day with a zero shape total stays zero).
fn averaged_blocks(
    per_provider: &[Vec<(DateTime<Utc>, f32)>],
    totals: [(chrono::NaiveDate, f32); 2],
    tz: chrono_tz::Tz,
) -> Vec<(DateTime<Utc>, f32)> {
    let days: BTreeSet<chrono::NaiveDate> = per_provider
        .iter()
        .flatten()
        .map(|(start, _)| local_day(*start, tz))
        .collect();
    let mut out = Vec::new();
    for day in days {
        let mut per_block: BTreeMap<DateTime<Utc>, f32> = BTreeMap::new();
        let mut with_day = 0usize;
        for blocks in per_provider {
            let on_day: Vec<_> = blocks
                .iter()
                .filter(|(start, _)| local_day(*start, tz) == day)
                .collect();
            if on_day.is_empty() {
                continue;
            }
            with_day += 1;
            for (start, kwh) in on_day {
                *per_block.entry(*start).or_insert(0.0) += kwh;
            }
        }
        let shape: Vec<(DateTime<Utc>, f32)> = per_block
            .into_iter()
            .map(|(start, kwh)| (start, kwh / with_day as f32))
            .collect();
        let shape_total: f32 = shape.iter().map(|(_, kwh)| kwh).sum();
        let target = totals
            .iter()
            .find(|(date, _)| *date == day)
            .map(|(_, total)| *total);
        // Accepted choice: a day other than today and tomorrow has no averaged total, so it stays
        // unscaled (the mean shape as the providers give it)
        let factor = match target {
            Some(total) if shape_total > 0.0 => total / shape_total,
            _ => 1.0,
        };
        out.extend(shape.into_iter().map(|(start, kwh)| (start, kwh * factor)));
    }
    out
}

/// The averaging as one log line: each provider's today and tomorrow, then the means
fn averaged_log_line(providers: &[ProviderFetch], averaged: &Averaged) -> String {
    let each: Vec<String> = providers
        .iter()
        .map(|p| {
            format!(
                "{} today={:.1} tomorrow={:.1}",
                p.name, p.total_today.total, p.tomorrow.total
            )
        })
        .collect();
    format!(
        "{}; mean today={:.1} tomorrow={:.1}",
        each.join("; "),
        averaged.total_today,
        averaged.tomorrow
    )
}

/// The averaging INFO line as last logged
#[derive(Debug, Clone, Copy)]
struct AveragedInfo {
    at: Instant,
    today: f32,
    tomorrow: f32,
}

/// The averaging INFO line is logged when the first averaged fetch comes, when either mean moved
/// by 0.5 kWh or more, and otherwise at most once an hour, when the means changed
fn averaged_info_due(last: Option<AveragedInfo>, today: f32, tomorrow: f32, now: Instant) -> bool {
    let Some(last) = last else {
        return true;
    };
    let moved = (today - last.today).abs() >= 0.5 || (tomorrow - last.tomorrow).abs() >= 0.5;
    let changed = today != last.today || tomorrow != last.tomorrow;
    moved || (changed && now.duration_since(last.at) >= Duration::from_secs(60 * 60))
}

/// Whether a state was updated at or after `since` (a state that did not update still holds
/// yesterday's value)
fn state_updated_since(state: &HaEntityState, since: DateTime<Utc>) -> bool {
    parse_forecast_timestamp(&state.last_updated).is_some_and(|at| at >= since)
}

/// Every selected sensor once, for one state call
fn all_sensor_ids(providers: &[ProviderSensors]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for provider in providers {
        for id in provider
            .total_today
            .iter()
            .chain(&provider.remaining_today)
            .chain(&provider.tomorrow)
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    ids
}

/// The sums of one sensor set from the states of all selected sensors: its own states, summed with
/// the completeness check, and whether all of them answered and were updated since `since`
fn sums_from_states(
    entity_ids: &[String],
    states: &[HaEntityState],
    since: DateTime<Utc>,
) -> (SensorSum, bool) {
    if entity_ids.is_empty() {
        return (sum_sensor_states(entity_ids, &[]), true);
    }
    let own: Vec<HaEntityState> = states
        .iter()
        .filter(|state| entity_ids.contains(&state.entity_id))
        .cloned()
        .collect();
    let fresh = own.len() == entity_ids.len() && own.iter().all(|s| state_updated_since(s, since));
    (sum_sensor_states(entity_ids, &own), fresh)
}

/// One provider from the states of all selected sensors: its sums, whether they are fresh, and
/// its per-block forecast
fn provider_from_states(
    sensors: &ProviderSensors,
    states: &[HaEntityState],
    since: DateTime<Utc>,
) -> ProviderFetch {
    let (total_today, fresh_total) = sums_from_states(&sensors.total_today, states, since);
    let (remaining_today, fresh_remaining) =
        sums_from_states(&sensors.remaining_today, states, since);
    let (tomorrow, fresh_tomorrow) = sums_from_states(&sensors.tomorrow, states, since);
    let blocks = states
        .iter()
        .filter(|s| {
            sensors.total_today.contains(&s.entity_id) || sensors.tomorrow.contains(&s.entity_id)
        })
        .map(|s| sensor_block_kwh(&s.attributes))
        .collect();
    ProviderFetch {
        name: sensors.name.clone(),
        total_today,
        remaining_today,
        tomorrow,
        fresh: fresh_total && fresh_remaining && fresh_tomorrow,
        blocks,
    }
}

/// Averaged mode: the states of all selected sensors in one call, split per provider in memory
async fn fetch_providers_batched(
    client: &HomeAssistantClient,
    providers: &[ProviderSensors],
    since: DateTime<Utc>,
) -> Vec<ProviderFetch> {
    let ids = all_sensor_ids(providers);
    let states = if ids.is_empty() {
        Vec::new()
    } else {
        match client.get_states(&ids).await {
            Ok(states) => states,
            Err(e) => {
                tracing::warn!("⚠️ Failed to fetch sensor states: {}", e);
                Vec::new()
            }
        }
    };
    providers
        .iter()
        .map(|sensors| provider_from_states(sensors, &states, since))
        .collect()
}

/// Single-provider mode: the calls of before, one for the blocks and one per sensor set
async fn fetch_provider_per_call(
    client: &HomeAssistantClient,
    sensors: &ProviderSensors,
    since: DateTime<Utc>,
) -> ProviderFetch {
    let forecast_sensor_ids: Vec<String> = sensors
        .total_today
        .iter()
        .chain(sensors.tomorrow.iter())
        .cloned()
        .collect();
    let per_sensor_blocks = fetch_block_kwh(client, &forecast_sensor_ids).await;
    let (total_today, fresh_total) = fetch_sum_checked(client, &sensors.total_today, since).await;
    let (remaining_today, fresh_remaining) =
        fetch_sum_checked(client, &sensors.remaining_today, since).await;
    let (tomorrow, fresh_tomorrow) = fetch_sum_checked(client, &sensors.tomorrow, since).await;
    ProviderFetch {
        name: sensors.name.clone(),
        total_today,
        remaining_today,
        tomorrow,
        fresh: fresh_total && fresh_remaining && fresh_tomorrow,
        blocks: per_sensor_blocks
            .into_iter()
            .map(|(_, blocks)| blocks)
            .collect(),
    }
}

/// The no-fresh-provider episode: one WARN when it starts, one INFO when a provider is fresh again
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NoFreshEpisode {
    active: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum NoFreshEvent {
    Started,
    Recovered,
}

impl NoFreshEpisode {
    /// Record one averaged fetch; returns the event to log, if any
    fn observe(&mut self, any_fresh: bool) -> Option<NoFreshEvent> {
        match (self.active, any_fresh) {
            (false, false) => {
                self.active = true;
                Some(NoFreshEvent::Started)
            }
            (true, true) => {
                self.active = false;
                Some(NoFreshEvent::Recovered)
            }
            _ => None,
        }
    }
}

/// Fetch and send one solar forecast update
async fn fetch_and_send_solar_forecast(
    client: &HomeAssistantClient,
    discovered: &Arc<RwLock<DiscoveredSensors>>,
    sender: &crossbeam_channel::Sender<SolarForecastUpdate>,
    tz: chrono_tz::Tz,
) -> Fetched {
    let cache = discovered.read().await;

    if !cache.initialized {
        tracing::warn!("⚠️ Solar forecast sensors not yet discovered, skipping fetch");
        return Fetched::default();
    }

    let today = Utc::now().with_timezone(&tz).date_naive();
    let midnight = local_midnight_utc(today, tz);
    let several = cache.providers.len() > 1;

    // Each provider's sums (and whether its sensors were updated since midnight), with its blocks
    let providers = if several {
        fetch_providers_batched(client, &cache.providers, midnight).await
    } else {
        let mut providers = Vec::with_capacity(cache.providers.len());
        for sensors in &cache.providers {
            providers.push(fetch_provider_per_call(client, sensors, midnight).await);
        }
        providers
    };
    let averaged = average_providers(&providers, today, tz);
    for stale in providers.iter().filter(|p| several && !p.fresh) {
        tracing::debug!(
            "📊 Solar forecast provider '{}' left out: its sensors are not updated since local midnight",
            stale.name
        );
    }
    let any_fresh = averaged.used > 0;
    let total_sensors = cache.total_today.len();
    let last_info = cache.averaged_info;
    drop(cache);

    if several {
        let event = discovered.write().await.no_fresh.observe(any_fresh);
        match event {
            Some(NoFreshEvent::Started) => tracing::warn!(
                "⚠️ PV forecast: no provider fresh for today, keeping the last forecast"
            ),
            Some(NoFreshEvent::Recovered) => {
                tracing::info!("☀️ PV forecast: a provider is fresh again")
            }
            None => {}
        }
    }
    if several && !any_fresh {
        tracing::debug!(
            "📊 Solar forecast: no provider updated since local midnight, nothing sent"
        );
        return Fetched::default();
    }

    // Send update
    let update = SolarForecastUpdate {
        total_today_kwh: averaged.total_today,
        remaining_today_kwh: averaged.remaining_today,
        tomorrow_kwh: averaged.tomorrow,
        block_kwh: averaged.block_kwh.clone(),
    };

    tracing::debug!(
        "📊 Solar forecast: today={:.1} kWh, remaining={:.1} kWh, tomorrow={:.1} kWh",
        averaged.total_today,
        averaged.remaining_today,
        averaged.tomorrow
    );

    // The snapshot records one series: the averaged blocks, when every provider answered
    let (snapshot_blocks, sensors) = if several {
        let answered = if averaged.answered {
            vec![averaged.block_kwh.clone()]
        } else {
            Vec::new()
        };
        (answered, 1)
    } else {
        (
            providers
                .first()
                .map(|p| p.blocks.clone())
                .unwrap_or_default(),
            total_sensors,
        )
    };
    // Accepted choice: the accuracy monitor gets the averaged total only when it is answered
    // (every provider complete and, when averaged, updated since midnight)
    let total_today = averaged.answered.then_some(averaged.total_today);

    let log_line = several.then(|| averaged_log_line(&providers, &averaged));
    let info_due = log_line.is_some()
        && averaged_info_due(
            last_info,
            averaged.total_today,
            averaged.tomorrow,
            Instant::now(),
        );
    if let Some(line) = log_line {
        tracing::debug!("☀️ Solar forecast providers averaged: {}", line);
        if info_due {
            tracing::info!("☀️ Solar forecast providers averaged: {}", line);
            discovered.write().await.averaged_info = Some(AveragedInfo {
                at: Instant::now(),
                today: averaged.total_today,
                tomorrow: averaged.tomorrow,
            });
        }
    }

    let _ = sender.send(update);
    Fetched {
        total_today,
        per_sensor_blocks: snapshot_blocks,
        sensors,
    }
}

/// Fetch the per-block PV forecast (UTC block start, kWh) of each sensor that answered, by entity
async fn fetch_block_kwh(
    client: &HomeAssistantClient,
    entity_ids: &[String],
) -> Vec<(String, Vec<(DateTime<Utc>, f32)>)> {
    if entity_ids.is_empty() {
        return Vec::new();
    }

    match client.get_states(entity_ids).await {
        Ok(states) => states
            .iter()
            .map(|state| (state.entity_id.clone(), sensor_block_kwh(&state.attributes)))
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

/// Fetch the states of the entity IDs and sum them, with the completeness check. Also whether every
/// state was updated at or after `since` (a state that did not update still holds yesterday's value).
async fn fetch_sum_checked(
    client: &HomeAssistantClient,
    entity_ids: &[String],
    since: DateTime<Utc>,
) -> (SensorSum, bool) {
    if entity_ids.is_empty() {
        return (sum_sensor_states(entity_ids, &[]), true);
    }
    match client.get_states(entity_ids).await {
        Ok(states) => {
            let fresh = states.len() == entity_ids.len()
                && states.iter().all(|state| state_updated_since(state, since));
            (sum_sensor_states(entity_ids, &states), fresh)
        }
        Err(e) => {
            tracing::warn!("⚠️ Failed to fetch sensor states: {}", e);
            (sum_sensor_states(entity_ids, &[]), false)
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

/// Update system: store the newest cautious-PV signal from the accuracy monitor
pub fn poll_cautious_pv_channel(
    channel: Option<Res<CautiousPvChannel>>,
    mut forecast_data: Option<ResMut<SolarForecastData>>,
) {
    let Some(channel) = channel else {
        return;
    };
    while let Ok(signal) = channel.receiver.try_recv() {
        if let Some(data) = forecast_data.as_mut()
            && data.cautious_pv != signal
        {
            data.cautious_pv = signal;
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
    #[test]
    fn custom_cautious_pv_thresholds_reach_the_worker() {
        let wr = fluxion_types::config::WinterReserveConfigCore {
            cautious_pv_mae_pct: 30.0,
            cautious_pv_off_pct: 18.0,
            ..Default::default()
        };
        assert_eq!(
            cautious_pv_thresholds(&wr),
            fluxion_core::strategy::winter_reserve::CautiousPvThresholds {
                on_pct: 30.0,
                off_pct: 18.0,
            }
        );
    }

    use super::*;
    use fluxion_core::pv_correction::CorrectionResult;

    fn correction(factor: f32) -> CorrectionResult {
        CorrectionResult {
            factor,
            days_used: 7,
            measured_kwh: 117.0,
            forecast_kwh: 100.0,
            forecast_error_pct: None,
        }
    }

    #[test]
    fn cautious_pv_signal_reaches_the_forecast_data() {
        use bevy_ecs::system::RunSystemOnce;
        let mut world = World::new();
        let (tx, rx) = crossbeam_channel::bounded(4);
        world.insert_resource(CautiousPvChannel {
            sender: tx.clone(),
            receiver: rx,
        });
        world.init_resource::<SolarForecastData>();
        let signal = fluxion_types::CautiousPvSignal {
            mae_pct: 39.0,
            cautious: true,
        };
        tx.send(Some(signal)).unwrap();
        world.run_system_once(poll_cautious_pv_channel).unwrap();
        assert_eq!(
            world.resource::<SolarForecastData>().cautious_pv,
            Some(signal)
        );
        // Too few days again: back to the fallback months
        tx.send(None).unwrap();
        world.run_system_once(poll_cautious_pv_channel).unwrap();
        assert_eq!(world.resource::<SolarForecastData>().cautious_pv, None);
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

    fn complete_sum(total: f32) -> SensorSum {
        SensorSum {
            total,
            complete: true,
        }
    }

    fn provider_fetch(
        name: &str,
        total_today: SensorSum,
        blocks: Vec<Vec<(DateTime<Utc>, f32)>>,
    ) -> ProviderFetch {
        ProviderFetch {
            name: name.to_string(),
            total_today,
            remaining_today: SensorSum {
                total: 0.0,
                complete: false,
            },
            tomorrow: SensorSum {
                total: 0.0,
                complete: false,
            },
            fresh: true,
            blocks,
        }
    }

    /// Open-Meteo: 10 kWh over the 32 daylight blocks of `day`, with per-block attributes
    fn open_meteo(day: chrono::NaiveDate) -> ProviderFetch {
        provider_fetch(
            OPEN_METEO,
            complete_sum(10.0),
            vec![sensor_day(day, 10.0 / 32.0)],
        )
    }

    /// Forecast.Solar: three plane sensors summing 2 + 4 + 1 kWh, without per-block attributes
    fn forecast_solar(total: SensorSum) -> ProviderFetch {
        provider_fetch(
            "forecast_solar",
            total,
            vec![Vec::new(), Vec::new(), Vec::new()],
        )
    }

    #[test]
    fn two_providers_average_the_day_total_and_keep_one_shape() {
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let averaged = average_providers(
            &[open_meteo(day), forecast_solar(complete_sum(7.0))],
            day,
            tz,
        );
        assert!((averaged.total_today - 8.5).abs() < 1e-5);
        assert!(averaged.answered);
        // Open-Meteo's 32 daylight blocks, scaled from 10 to the averaged 8.5 kWh
        let daylight: Vec<f32> = averaged
            .block_kwh
            .iter()
            .map(|(_, kwh)| *kwh)
            .filter(|kwh| *kwh > 0.0)
            .collect();
        assert_eq!(daylight.len(), 32);
        let total: f32 = daylight.iter().sum();
        assert!((total - 8.5).abs() < 1e-4, "{total}");
        for kwh in &daylight {
            assert!((kwh - 0.85 * 10.0 / 32.0).abs() < 1e-5, "{kwh}");
        }
    }

    #[test]
    fn unavailable_provider_is_left_out_of_the_mean() {
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let alone = average_providers(&[open_meteo(day)], day, tz);
        let unavailable = SensorSum {
            total: 0.0,
            complete: false,
        };
        let both = average_providers(&[open_meteo(day), forecast_solar(unavailable)], day, tz);
        assert_eq!(both.total_today, alone.total_today);
        assert_eq!(both.total_today, 10.0);
        assert!(!both.answered);
        assert_eq!(both.block_kwh.len(), alone.block_kwh.len());
        for ((start, kwh), (alone_start, alone_kwh)) in both.block_kwh.iter().zip(&alone.block_kwh)
        {
            assert_eq!(start, alone_start);
            assert!(
                (kwh - alone_kwh).abs() < 1e-5,
                "{start}: {kwh} != {alone_kwh}"
            );
        }
    }

    #[test]
    fn one_provider_keeps_its_own_total_and_blocks_unscaled() {
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        // The blocks sum to 9 while the day total is 10: a single provider is not scaled
        let single = provider_fetch(
            OPEN_METEO,
            complete_sum(10.0),
            vec![sensor_day(day, 9.0 / 32.0)],
        );
        let expected = merge_sensor_blocks(&single.blocks);
        let averaged = average_providers(&[single], day, tz);
        assert_eq!(averaged.total_today, 10.0);
        assert!(averaged.answered);
        assert_eq!(averaged.block_kwh, expected);
    }

    #[test]
    fn averaged_snapshot_sums_to_the_averaged_total() {
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let averaged = average_providers(
            &[open_meteo(day), forecast_solar(complete_sum(7.0))],
            day,
            tz,
        );
        let at = chrono::TimeZone::with_ymd_and_hms(&tz, 2026, 10, 19, 0, 5, 0)
            .unwrap()
            .with_timezone(&Utc);
        let mut store = ForecastStore::default();
        assert!(store.record_snapshot(at, tz, &[averaged.block_kwh], 1));
        let kwh: f32 = store.day(day).unwrap().hours.iter().map(|h| h.kwh).sum();
        assert!((kwh - 8.5).abs() < 1e-3, "{kwh}");
    }

    #[test]
    fn named_providers_keep_their_own_sensors_and_unknown_names_are_skipped() {
        let registry = [
            row("sensor.energy_production_today_4", OPEN_METEO),
            row("sensor.energy_production_today_5", "forecast_solar"),
            row("sensor.energy_production_today_6", "forecast_solar"),
            row("sensor.energy_production_today_7", "other_platform"),
        ];
        let pattern = [
            ids(&[
                "sensor.energy_production_today_4",
                "sensor.energy_production_today_5",
                "sensor.energy_production_today_6",
                "sensor.energy_production_today_7",
            ]),
            Vec::new(),
            Vec::new(),
        ];
        let selection = select_provider_sensors(
            pattern.clone(),
            Some(&registry),
            "open_meteo_solar_forecast, forecast_solar",
        );
        assert_eq!(selection.source, ProviderSource::Provider);
        assert_eq!(selection.providers.len(), 2);
        assert_eq!(selection.providers[0].name, OPEN_METEO);
        assert_eq!(selection.providers[1].name, "forecast_solar");
        assert_eq!(
            selection.providers[1].total_today,
            ids(&[
                "sensor.energy_production_today_5",
                "sensor.energy_production_today_6"
            ])
        );
        assert_eq!(
            selection.sets[0],
            ids(&[
                "sensor.energy_production_today_4",
                "sensor.energy_production_today_5",
                "sensor.energy_production_today_6"
            ])
        );
        assert!(selection.skipped.is_empty());

        let selection = select_provider_sensors(
            pattern,
            Some(&registry),
            "open_meteo_solar_forecast, no_such_platform",
        );
        assert_eq!(selection.providers.len(), 1);
        assert_eq!(selection.skipped, vec!["no_such_platform".to_string()]);
        assert_eq!(selection.source, ProviderSource::Provider);
    }

    #[test]
    fn stale_provider_is_left_out_until_it_refreshes() {
        // Forecast.Solar still holds yesterday's total after midnight: its 10 kWh is not counted
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let stale_fs = ProviderFetch {
            fresh: false,
            ..forecast_solar(complete_sum(10.0))
        };
        let before = average_providers(&[open_meteo(day), stale_fs], day, tz);
        assert_eq!(before.total_today, 10.0, "Open-Meteo alone");
        assert_eq!(before.used, 1);
        assert!(!before.answered, "no snapshot while a provider is stale");
        let alone = average_providers(&[open_meteo(day)], day, tz);
        assert_eq!(before.block_kwh.len(), alone.block_kwh.len());
        for ((_, kwh), (_, alone_kwh)) in before.block_kwh.iter().zip(&alone.block_kwh) {
            assert!((kwh - alone_kwh).abs() < 1e-5, "{kwh} != {alone_kwh}");
        }

        // After its refresh: the mean of both, and the snapshot is taken
        let after = average_providers(
            &[open_meteo(day), forecast_solar(complete_sum(2.0))],
            day,
            tz,
        );
        assert!(
            (after.total_today - 6.0).abs() < 1e-5,
            "{}",
            after.total_today
        );
        assert_eq!(after.used, 2);
        assert!(after.answered);
    }

    #[test]
    fn single_provider_is_not_left_out_when_stale() {
        // Single-provider mode is unchanged: its freshness is not checked
        let tz = chrono_tz::Europe::Prague;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let stale = ProviderFetch {
            fresh: false,
            ..open_meteo(day)
        };
        let averaged = average_providers(&[stale], day, tz);
        assert_eq!(averaged.used, 1);
        assert_eq!(averaged.total_today, 10.0);
        assert!(averaged.answered);
    }

    #[test]
    fn retry_while_a_named_provider_is_skipped_every_five_minutes() {
        let now = Instant::now();
        let recent = now - Duration::from_secs(60);
        let old = now - DISCOVERY_RETRY - Duration::from_secs(1);
        assert!(skipped_retry_due(true, None, now));
        assert!(skipped_retry_due(true, Some(old), now));
        assert!(!skipped_retry_due(true, Some(recent), now));
        assert!(!skipped_retry_due(false, Some(old), now));
    }

    #[test]
    fn several_providers_keep_the_last_good_selection_when_the_registry_fails() {
        let good = vec![
            ProviderSensors {
                name: OPEN_METEO.to_string(),
                total_today: ids(&["sensor.energy_production_today_4"]),
                remaining_today: Vec::new(),
                tomorrow: Vec::new(),
            },
            ProviderSensors {
                name: "forecast_solar".to_string(),
                total_today: ids(&["sensor.energy_production_today_5"]),
                remaining_today: Vec::new(),
                tomorrow: Vec::new(),
            },
        ];
        let pattern = [
            ids(&[
                "sensor.energy_production_today_4",
                "sensor.energy_production_today_9",
            ]),
            Vec::new(),
            Vec::new(),
        ];
        let mut selection = select_provider_sensors(
            pattern.clone(),
            None,
            "open_meteo_solar_forecast,forecast_solar",
        );
        assert_eq!(selection.source, ProviderSource::RegistryUnavailable);
        assert!(apply_last_good(&mut selection, true, &good));
        assert_eq!(selection.providers, good);
        assert_eq!(
            selection.sets[0],
            ids(&[
                "sensor.energy_production_today_4",
                "sensor.energy_production_today_5"
            ])
        );
        // The source stays a fallback, so discovery keeps retrying
        assert_eq!(selection.source, ProviderSource::RegistryUnavailable);

        // No previous selection, or one provider only: the pattern-only sets stay
        let mut selection = select_provider_sensors(
            pattern.clone(),
            None,
            "open_meteo_solar_forecast,forecast_solar",
        );
        assert!(!apply_last_good(&mut selection, true, &[]));
        assert_eq!(selection.sets, pattern);
        let mut selection =
            select_provider_sensors(pattern.clone(), None, "open_meteo_solar_forecast");
        assert!(!apply_last_good(&mut selection, false, &good));
        assert_eq!(selection.sets, pattern);
    }

    #[test]
    fn averaging_info_logs_on_a_half_kwh_move_and_otherwise_hourly() {
        let now = Instant::now();
        let last = AveragedInfo {
            at: now - Duration::from_secs(10 * 60),
            today: 8.0,
            tomorrow: 9.0,
        };
        assert!(averaged_info_due(None, 8.0, 9.0, now), "the first line");
        // Within the hour: a small move is not logged, a half kWh move is
        assert!(!averaged_info_due(Some(last), 8.2, 9.2, now));
        assert!(averaged_info_due(Some(last), 8.5, 9.0, now));
        assert!(averaged_info_due(Some(last), 8.0, 8.5, now));
        // After an hour: a change is logged, no change is not
        let hour_ago = AveragedInfo {
            at: now - Duration::from_secs(61 * 60),
            ..last
        };
        assert!(averaged_info_due(Some(hour_ago), 8.2, 9.0, now));
        assert!(!averaged_info_due(Some(hour_ago), 8.0, 9.0, now));
    }

    fn timed_state(
        entity_id: &str,
        state: &str,
        updated: &str,
        attributes: serde_json::Value,
    ) -> HaEntityState {
        HaEntityState {
            entity_id: entity_id.to_string(),
            state: state.to_string(),
            attributes,
            last_changed: updated.to_string(),
            last_updated: updated.to_string(),
        }
    }

    #[test]
    fn batched_split_gives_each_provider_the_totals_of_its_own_states() {
        let tz = chrono_tz::Europe::Prague;
        let since = chrono::TimeZone::with_ymd_and_hms(&tz, 2026, 10, 19, 0, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let updated = "2026-10-19T09:00:00+00:00";
        let om_block = serde_json::json!({"watts": {"2026-10-19T10:00:00+02:00": 1000}});
        let states = vec![
            timed_state("sensor.om_today", "10.0", updated, om_block),
            timed_state("sensor.fs_today_5", "2.0", updated, serde_json::Value::Null),
            timed_state("sensor.fs_today_6", "4.0", updated, serde_json::Value::Null),
            timed_state("sensor.fs_today_7", "1.0", updated, serde_json::Value::Null),
            timed_state(
                "sensor.other_tomorrow",
                "9.0",
                updated,
                serde_json::Value::Null,
            ),
        ];
        let om = ProviderSensors {
            name: OPEN_METEO.to_string(),
            total_today: ids(&["sensor.om_today"]),
            remaining_today: Vec::new(),
            tomorrow: Vec::new(),
        };
        let fs = ProviderSensors {
            name: "forecast_solar".to_string(),
            total_today: ids(&[
                "sensor.fs_today_5",
                "sensor.fs_today_6",
                "sensor.fs_today_7",
            ]),
            remaining_today: Vec::new(),
            tomorrow: Vec::new(),
        };
        // The per-provider path asks for each set alone: its answer is the states of its sensors
        let split_om = provider_from_states(&om, &states, since);
        assert_eq!(
            split_om.total_today,
            sum_sensor_states(&om.total_today, &states[0..1])
        );
        assert!(split_om.fresh);
        assert_eq!(split_om.blocks.len(), 1);
        let split_fs = provider_from_states(&fs, &states, since);
        assert_eq!(
            split_fs.total_today,
            sum_sensor_states(&fs.total_today, &states[1..4])
        );
        assert_eq!(split_fs.total_today.total, 7.0);
        assert!(split_fs.fresh);
        assert_eq!(split_fs.blocks.len(), 3);

        // A sensor not updated since midnight, or missing from the answer: not fresh
        let mut stale = states.clone();
        stale[1].last_updated = "2026-10-18T20:00:00+00:00".to_string();
        assert!(!provider_from_states(&fs, &stale, since).fresh);
        assert!(!provider_from_states(&fs, &states[..3], since).fresh);
        assert!(!provider_from_states(&fs, &[], since).fresh);
    }

    #[test]
    fn one_batch_covers_every_selected_sensor_once() {
        let a = ProviderSensors {
            name: "a".to_string(),
            total_today: ids(&["x.1", "x.2"]),
            remaining_today: ids(&["x.r"]),
            tomorrow: ids(&["x.t"]),
        };
        let b = ProviderSensors {
            name: "b".to_string(),
            total_today: ids(&["x.2", "y.1"]),
            remaining_today: Vec::new(),
            tomorrow: Vec::new(),
        };
        assert_eq!(
            all_sensor_ids(&[a, b]),
            ids(&["x.1", "x.2", "x.r", "x.t", "y.1"])
        );
    }

    #[test]
    fn no_fresh_episode_warns_once_and_rearms_after_recovery() {
        let mut episode = NoFreshEpisode::default();
        assert_eq!(episode.observe(false), Some(NoFreshEvent::Started));
        assert_eq!(episode.observe(false), None);
        assert_eq!(episode.observe(false), None);
        assert_eq!(episode.observe(true), Some(NoFreshEvent::Recovered));
        assert_eq!(episode.observe(true), None);
        assert_eq!(episode.observe(false), Some(NoFreshEvent::Started));
    }
}
