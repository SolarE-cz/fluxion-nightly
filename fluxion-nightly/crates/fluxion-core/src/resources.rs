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

use anyhow::Result;
use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fluxion_i18n::{I18n, I18nError, Language};
use std::sync::Arc;
use std::time::Duration;

// ============= System Configuration (Imported from fluxion-types) =============
pub use fluxion_types::config::{
    ControlConfig, Currency, InverterConfig, InverterTopology, PriceSchedule, PricingConfig,
    SolarAwareChargingConfigCore, StrategiesConfigCore, StrategyEnabledConfigCore, SystemConfig,
    SystemSettingsConfig, WinterAdaptiveConfigCore, WinterAdaptiveV2ConfigCore,
    WinterAdaptiveV3ConfigCore, WinterAdaptiveV4ConfigCore, WinterAdaptiveV5ConfigCore,
    WinterAdaptiveV6ConfigCore, WinterAdaptiveV7ConfigCore, WinterPeakDischargeConfigCore,
};
pub use fluxion_types::history::ConsumptionHistoryConfig;

// ============= Timezone Configuration =============

/// Resource for Home Assistant timezone configuration
/// This is synchronized periodically with Home Assistant to detect timezone changes
#[derive(Resource, Debug, Clone)]
pub struct TimezoneConfig {
    /// Current timezone string from Home Assistant (e.g., "Europe/Prague", "America/New_York")
    pub timezone: Option<String>,

    /// Parsed timezone for efficient time conversions
    /// Cached to avoid re-parsing on every use
    pub tz: Option<Tz>,

    /// Last time we checked HA for timezone updates
    pub last_check: DateTime<Utc>,

    /// How often to check HA for timezone changes (default: 5 minutes)
    pub check_interval: Duration,
}

impl Default for TimezoneConfig {
    fn default() -> Self {
        Self {
            timezone: None,
            tz: None,
            last_check: Utc::now(),
            check_interval: Duration::from_secs(300), // 5 minutes
        }
    }
}

impl TimezoneConfig {
    /// Create a new `TimezoneConfig` with a specific timezone
    #[must_use]
    pub fn new(timezone: Option<String>) -> Self {
        let tz = timezone
            .as_ref()
            .and_then(|tz_str| tz_str.parse::<Tz>().ok());
        Self {
            timezone,
            tz,
            last_check: Utc::now(),
            check_interval: Duration::from_secs(300),
        }
    }

    /// Update the timezone (re-parses the timezone string)
    pub fn update_timezone(&mut self, timezone: Option<String>) {
        if self.timezone != timezone {
            tracing::info!("🌍 Timezone changed: {:?} -> {:?}", self.timezone, timezone);
            self.tz = timezone
                .as_ref()
                .and_then(|tz_str| tz_str.parse::<Tz>().ok());
            self.timezone = timezone;
        }
        self.last_check = Utc::now();
    }

    /// Check if it's time to sync timezone with HA
    #[must_use]
    pub fn should_check(&self) -> bool {
        Utc::now()
            .signed_duration_since(self.last_check)
            .to_std()
            .unwrap_or_default()
            >= self.check_interval
    }
}

// ============= Internationalization =============

/// Resource for internationalization (i18n)
#[derive(Resource, Clone)]
pub struct I18nResource(pub Arc<I18n>);

impl I18nResource {
    /// Create a new `I18nResource` from the system configuration
    ///
    /// # Errors
    ///
    /// Returns `I18nError` if the i18n system fails to initialize.
    pub fn from_config(config: &SystemConfig) -> Result<Self, I18nError> {
        let language = config.system_config.language;
        Ok(Self(Arc::new(I18n::new(language)?)))
    }

    /// Create a new `I18nResource` with a specific language
    ///
    /// # Errors
    ///
    /// Returns `I18nError` if the i18n system fails to initialize.
    pub fn new(language: Language) -> Result<Self, I18nError> {
        Ok(Self(Arc::new(I18n::new(language)?)))
    }

    /// Get a reference to the underlying `I18n` instance
    #[must_use]
    pub fn inner(&self) -> &I18n {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_price_schedule_flat() {
        let schedule = PriceSchedule::Flat(5.0);
        assert_eq!(schedule.get_price(0), 5.0);
        assert_eq!(schedule.get_price(12), 5.0);
        assert_eq!(schedule.get_price(23), 5.0);
    }

    #[test]
    fn test_price_schedule_hourly() {
        let prices: Vec<f32> = (0..24).map(|i| i as f32).collect();
        let schedule = PriceSchedule::Hourly(prices);
        assert_eq!(schedule.get_price(0), 0.0);
        assert_eq!(schedule.get_price(12), 12.0);
        assert_eq!(schedule.get_price(23), 23.0);

        // Test wrapping
        assert_eq!(schedule.get_price(24), 0.0);
    }

    #[test]
    fn test_price_schedule_hourly_incomplete() {
        let prices = vec![10.0, 20.0];
        let schedule = PriceSchedule::Hourly(prices);
        assert_eq!(schedule.get_price(0), 10.0);
        assert_eq!(schedule.get_price(1), 20.0);
        assert_eq!(schedule.get_price(2), 10.0); // Wraps
    }
}

/// Wrapper resource for the consumption history data source
#[derive(Resource)]
pub struct ConsumptionHistoryDataSourceResource(
    pub Arc<dyn crate::traits::ConsumptionHistoryDataSource>,
);

// ============= HDO (Czech Grid Tariff) Cache Resource =============

/// Global HDO cache resource for centralized grid fee calculation
/// This cache is shared by all strategies to ensure consistent pricing
#[derive(Resource)]
pub struct GlobalHdoCache(pub crate::strategy::pricing::HdoCache);

impl GlobalHdoCache {
    /// Create a new global HDO cache with specified TTL in seconds
    pub fn new(ttl_secs: u64) -> Self {
        Self(crate::strategy::pricing::HdoCache::new(ttl_secs))
    }

    /// Check if cache needs refresh based on TTL
    pub fn needs_refresh(&self) -> bool {
        self.0.needs_refresh()
    }

    /// Update cache with new HDO schedules
    pub fn update(&self, schedules: Vec<crate::strategy::pricing::HdoDaySchedule>) {
        self.0.update(schedules)
    }

    /// Check if a given time is in low tariff period
    pub fn is_low_tariff(&self, dt: chrono::DateTime<chrono::Utc>) -> Option<bool> {
        self.0.is_low_tariff(dt)
    }
}

impl Default for GlobalHdoCache {
    fn default() -> Self {
        Self::new(3600) // 1 hour default TTL
    }
}

// ============= Async Cache Resources =============

/// Cached price data source that fetches prices on demand
/// Replaces channel-based async price fetcher with direct async calls
#[derive(Resource)]
pub struct PriceCache {
    data: parking_lot::Mutex<Option<crate::components::SpotPriceData>>,
    last_fetch: parking_lot::Mutex<std::time::Instant>,
    last_error: parking_lot::Mutex<Option<String>>,
    source: Arc<dyn crate::traits::PriceDataSource>,
    fetch_interval: Duration,
}

impl PriceCache {
    /// Create a new price cache with the specified fetch interval
    pub fn new(source: Arc<dyn crate::traits::PriceDataSource>, interval_secs: u64) -> Self {
        Self {
            data: parking_lot::Mutex::new(None),
            last_fetch: parking_lot::Mutex::new(
                std::time::Instant::now() - Duration::from_secs(1000),
            ),
            last_error: parking_lot::Mutex::new(None),
            source,
            fetch_interval: Duration::from_secs(interval_secs),
        }
    }

    /// Get cached data or fetch fresh data if stale
    /// This is the main replacement for channel-based price updates
    pub fn get_or_fetch(&self) -> Result<crate::components::SpotPriceData> {
        // Check if we have fresh data
        let last = *self.last_fetch.lock();
        if last.elapsed() < self.fetch_interval
            && let Some(data) = self.data.lock().clone()
        {
            return Ok(data);
        }

        // Fetch fresh data using tokio runtime handle
        let handle = tokio::runtime::Handle::current();
        let prices = handle.block_on(async { self.source.read_prices().await })?;

        // Update cache
        *self.data.lock() = Some(prices.clone());
        *self.last_fetch.lock() = std::time::Instant::now();
        *self.last_error.lock() = None;

        Ok(prices)
    }

    /// Check if cached data is stale and needs refreshing
    pub fn is_stale(&self) -> bool {
        self.last_fetch.lock().elapsed() > self.fetch_interval
    }

    /// Get the last error that occurred during fetching
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    /// Get cached data without fetching (returns None if no cache or stale)
    pub fn get_cached(&self) -> Option<crate::components::SpotPriceData> {
        let last = *self.last_fetch.lock();
        if last.elapsed() < self.fetch_interval {
            self.data.lock().clone()
        } else {
            None
        }
    }
}

/// Direct async inverter writer that replaces channel-based command execution
/// Provides both synchronous and fire-and-forget async command writing
#[derive(Resource)]
pub struct AsyncInverterWriter {
    source: Arc<dyn crate::traits::InverterDataSource>,
}

impl AsyncInverterWriter {
    /// Create a new async inverter writer
    pub fn new(source: Arc<dyn crate::traits::InverterDataSource>) -> Self {
        Self { source }
    }

    /// Write a command synchronously (blocks until completion)
    /// Use this when you need immediate confirmation of success/failure
    pub fn write_command(
        &self,
        inverter_id: &str,
        command: &crate::components::InverterCommand,
    ) -> Result<()> {
        let source = self.source.clone();
        let inverter_id = inverter_id.to_string();
        let command = command.clone();

        let handle = tokio::runtime::Handle::current();
        handle.block_on(async move { source.write_command(&inverter_id, &command).await })
    }

    /// Write a command asynchronously (fire-and-forget)
    /// Use this for mode changes where you don't need immediate confirmation
    pub fn write_command_async(
        &self,
        inverter_id: String,
        command: crate::components::InverterCommand,
    ) {
        let source = self.source.clone();

        tokio::spawn(async move {
            match source.write_command(&inverter_id, &command).await {
                Ok(_) => {
                    tracing::info!(
                        "✅ Async command succeeded for {}: {:?}",
                        inverter_id,
                        command
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "❌ Async command failed for {}: {:?} - {}",
                        inverter_id,
                        command,
                        e
                    );
                }
            }
        });
    }
}

/// On-demand health checker that replaces channel-based periodic checks
/// Provides cached health status with configurable check intervals
#[derive(Resource)]
pub struct HealthChecker {
    inverter_source: Option<Arc<dyn crate::traits::InverterDataSource>>,
    price_source: Option<Arc<dyn crate::traits::PriceDataSource>>,
    last_check: parking_lot::Mutex<std::collections::HashMap<String, (std::time::Instant, bool)>>,
    check_interval: Duration,
}

impl HealthChecker {
    /// Create a new health checker with the specified sources and interval
    pub fn new(
        inverter_source: Arc<dyn crate::traits::InverterDataSource>,
        price_source: Arc<dyn crate::traits::PriceDataSource>,
        check_interval_secs: u64,
    ) -> Self {
        Self {
            inverter_source: Some(inverter_source),
            price_source: Some(price_source),
            last_check: parking_lot::Mutex::new(std::collections::HashMap::new()),
            check_interval: Duration::from_secs(check_interval_secs),
        }
    }

    /// Check all sources and return their health status
    /// Uses caching to avoid excessive health checks
    pub fn check_all(&self) -> std::collections::HashMap<String, bool> {
        let mut results = std::collections::HashMap::new();
        let now = std::time::Instant::now();

        // Check cached results first
        let mut cache = self.last_check.lock();

        // Check inverter source
        if let Some(ref source) = self.inverter_source {
            let name = "inverter_source";
            let needs_check = cache
                .get(name)
                .is_none_or(|(last_time, _)| last_time.elapsed() >= self.check_interval);

            if needs_check {
                let handle = tokio::runtime::Handle::current();
                let is_healthy = handle.block_on(source.health_check()).unwrap_or(false);

                results.insert(name.to_string(), is_healthy);
                cache.insert(name.to_string(), (now, is_healthy));
            } else if let Some((_, cached_result)) = cache.get(name) {
                results.insert(name.to_string(), *cached_result);
            }
        }

        // Check price source
        if let Some(ref source) = self.price_source {
            let name = "price_source";
            let needs_check = cache
                .get(name)
                .is_none_or(|(last_time, _)| last_time.elapsed() >= self.check_interval);

            if needs_check {
                let handle = tokio::runtime::Handle::current();
                let is_healthy = handle.block_on(source.health_check()).unwrap_or(false);

                results.insert(name.to_string(), is_healthy);
                cache.insert(name.to_string(), (now, is_healthy));
            } else if let Some((_, cached_result)) = cache.get(name) {
                results.insert(name.to_string(), *cached_result);
            }
        }

        results
    }

    /// Check if any cached results are stale and need refreshing
    pub fn should_check(&self) -> bool {
        let cache = self.last_check.lock();
        cache
            .values()
            .any(|(time, _)| time.elapsed() >= self.check_interval)
            || cache.is_empty()
    }

    /// Get cached health status without performing fresh checks
    pub fn get_cached_status(&self) -> std::collections::HashMap<String, bool> {
        let cache = self.last_check.lock();
        let mut results = std::collections::HashMap::new();

        for (name, (time, healthy)) in cache.iter() {
            if time.elapsed() < self.check_interval {
                results.insert(name.clone(), *healthy);
            }
        }

        results
    }
}

/// Direct inverter state reader that replaces channel-based state polling
/// Provides on-demand state reading with timer-based control
#[derive(Resource)]
pub struct InverterStateReader {
    source: Arc<dyn crate::traits::InverterDataSource>,
}

impl InverterStateReader {
    /// Create a new inverter state reader
    pub fn new(source: Arc<dyn crate::traits::InverterDataSource>) -> Self {
        Self { source }
    }

    /// Read state for a specific inverter
    pub fn read_state(&self, inverter_id: &str) -> Result<crate::GenericInverterState> {
        let source = self.source.clone();
        let inverter_id = inverter_id.to_string();

        let handle = tokio::runtime::Handle::current();
        handle.block_on(async move { source.read_state(&inverter_id).await })
    }
}

/// Timer resource for controlling state read frequency
#[derive(Resource)]
pub struct StateReadTimer {
    last_read: parking_lot::Mutex<std::time::Instant>,
    read_interval: Duration,
}

impl StateReadTimer {
    /// Create a new state read timer with the specified interval
    pub fn new(interval_secs: u64) -> Self {
        Self {
            last_read: parking_lot::Mutex::new(
                std::time::Instant::now() - Duration::from_secs(interval_secs),
            ),
            read_interval: Duration::from_secs(interval_secs),
        }
    }

    /// Check if it's time to read states
    pub fn should_read(&self) -> bool {
        self.last_read.lock().elapsed() >= self.read_interval
    }

    /// Mark that a read has been performed
    pub fn mark_read(&self) {
        *self.last_read.lock() = std::time::Instant::now();
    }
}
