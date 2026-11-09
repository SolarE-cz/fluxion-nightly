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
    ControlConfig, Currency, ExportSelection, FixedPriceArbitrageConfigCore, InverterConfig,
    InverterTopology, NotificationsConfigCore, PriceSchedule, PricingConfig,
    RemoteAccessConfigCore, SolarAwareChargingConfigCore, SolarForecastConfigCore,
    StrategiesConfigCore, StrategyEnabledConfigCore, SystemConfig, SystemSettingsConfig,
    WinterAdaptiveConfigCore, WinterAdaptiveV2ConfigCore, WinterAdaptiveV3ConfigCore,
    WinterAdaptiveV4ConfigCore, WinterAdaptiveV5ConfigCore, WinterAdaptiveV7ConfigCore,
    WinterAdaptiveV8ConfigCore, WinterAdaptiveV9ConfigCore, WinterAdaptiveV10ConfigCore,
    WinterAdaptiveV20ConfigCore, WinterPeakDischargeConfigCore, WinterReserveConfigCore,
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

    /// Local timezone used to match HDO windows (the site timezone)
    pub fn timezone(&self) -> chrono_tz::Tz {
        self.0.timezone()
    }

    /// Set the local timezone used to match HDO windows
    pub fn set_timezone(&self, tz: chrono_tz::Tz) {
        self.0.set_timezone(tz)
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
#[derive(Resource, Clone)]
pub struct AsyncInverterWriter {
    source: Arc<dyn crate::traits::InverterDataSource>,
    /// Backup minimum SOC (%) last confirmed by the adapter, per inverter. Set only when a
    /// `SetBackupMode` returns Ok; cleared by any other write to that inverter, success or not.
    backup_confirmed: Arc<parking_lot::Mutex<std::collections::HashMap<String, u8>>>,
    /// Shutdown: queued VPP writes without a completion signal (keep-alives) are dropped, so
    /// the shutdown mode 0 goes right after the write in flight
    vpp_flush: Arc<std::sync::atomic::AtomicBool>,
    /// Ordered direct VPP write queue per inverter (created on the first VPP command)
    vpp_queues: Arc<parking_lot::Mutex<std::collections::HashMap<String, Arc<VppQueue>>>>,
    /// Inverters whose last direct VPP command failed, taken by the VPP system (re-send)
    vpp_failed: Arc<parking_lot::Mutex<std::collections::BTreeSet<String>>>,
    /// Result of the last `EnsureVppTargetSetType` per inverter, taken by the VPP system
    target_checks: TargetChecks,
}

type TargetChecks =
    Arc<parking_lot::Mutex<std::collections::HashMap<String, crate::vpp::TargetCheck>>>;

/// A queued direct VPP write, with an optional signal sent once it was executed
type VppQueued = (
    crate::components::InverterCommand,
    Option<tokio::sync::oneshot::Sender<()>>,
);

/// One inverter's ordered write queue and the wake-up of its worker
#[derive(Default)]
struct VppQueue {
    items: parking_lot::Mutex<std::collections::VecDeque<VppQueued>>,
    wake: tokio::sync::Notify,
}

/// A non-zero VPP command without a completion signal: superseded by a newer one (coalesced)
fn vpp_replaceable(item: &VppQueued) -> bool {
    matches!(
        item,
        (crate::components::InverterCommand::SetVpp(command), None)
            if *command != crate::vpp::VppCommand::Exit
    )
}

impl AsyncInverterWriter {
    /// Create a new async inverter writer
    pub fn new(source: Arc<dyn crate::traits::InverterDataSource>) -> Self {
        Self {
            source,
            backup_confirmed: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            vpp_flush: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            vpp_queues: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            vpp_failed: Arc::new(parking_lot::Mutex::new(std::collections::BTreeSet::new())),
            target_checks: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Result of the last target set type check of this inverter, once it ran (cleared)
    pub fn take_target_check(&self, inverter_id: &str) -> Option<crate::vpp::TargetCheck> {
        self.target_checks.lock().remove(inverter_id)
    }

    /// Whether a direct VPP command of this inverter failed since the last call (cleared)
    pub fn take_vpp_failure(&self, inverter_id: &str) -> bool {
        self.vpp_failed.lock().remove(inverter_id)
    }

    /// Backup minimum SOC the adapter last confirmed for this inverter (brief H19)
    pub fn confirmed_backup_min_soc(&self, inverter_id: &str) -> Option<u8> {
        self.backup_confirmed.lock().get(inverter_id).copied()
    }

    /// Record a confirmed backup minimum SOC (used by tests and by the success path)
    pub fn set_confirmed_backup_min_soc(&self, inverter_id: &str, min_soc_pct: u8) {
        self.backup_confirmed
            .lock()
            .insert(inverter_id.to_string(), min_soc_pct);
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
    /// Use this for mode changes where you don't need immediate confirmation.
    /// Direct VPP commands go through one ordered queue per inverter, so a mode 0 never lands
    /// before an earlier keep-alive.
    pub fn write_command_async(
        &self,
        inverter_id: String,
        command: crate::components::InverterCommand,
    ) {
        let source = self.source.clone();
        let backup_confirmed = self.backup_confirmed.clone();

        if matches!(command, crate::components::InverterCommand::SetVpp(_)) {
            self.queue_vpp(inverter_id, command, None);
            return;
        }

        tokio::spawn(async move {
            execute_command(&source, &backup_confirmed, inverter_id, command).await;
        });
    }
}

impl AsyncInverterWriter {
    /// Shutdown mode 0: queued keep-alives of every inverter are dropped, then `command` runs
    /// right after the inverter's write in flight (never before it); the returned receiver
    /// fires once it was executed (success or not).
    pub fn write_vpp_and_wait(
        &self,
        inverter_id: String,
        command: crate::components::InverterCommand,
    ) -> tokio::sync::oneshot::Receiver<()> {
        self.vpp_flush
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (done, wait) = tokio::sync::oneshot::channel();
        self.queue_vpp(inverter_id, command, Some(done));
        wait
    }

    /// Queue any command behind the inverter's VPP writes (e.g. the kill switch's basic work
    /// mode after its mode 0), fire-and-forget.
    pub fn write_command_queued(
        &self,
        inverter_id: String,
        command: crate::components::InverterCommand,
    ) {
        self.queue_vpp(inverter_id, command, None);
    }

    /// Queue a command in the inverter's ordered queue. Only the latest pending non-zero VPP
    /// command is kept (a newer one replaces it, so a slow link never builds a backlog); a mode 0,
    /// the shutdown write and other commands are never dropped.
    fn queue_vpp(
        &self,
        inverter_id: String,
        command: crate::components::InverterCommand,
        done: Option<tokio::sync::oneshot::Sender<()>>,
    ) {
        let queue = self
            .vpp_queues
            .lock()
            .entry(inverter_id.clone())
            .or_insert_with(|| {
                let queue = Arc::new(VppQueue::default());
                let worker = queue.clone();
                let source = self.source.clone();
                let backup_confirmed = self.backup_confirmed.clone();
                let failed = self.vpp_failed.clone();
                let flush = self.vpp_flush.clone();
                let checks = self.target_checks.clone();
                let id = inverter_id.clone();
                tokio::spawn(async move {
                    loop {
                        let next = worker.items.lock().pop_front();
                        let Some((command, done)) = next else {
                            worker.wake.notified().await;
                            continue;
                        };
                        if done.is_none() && flush.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::debug!("Shutdown: dropping queued {:?} for {}", command, id);
                            continue;
                        }
                        if command == crate::components::InverterCommand::EnsureVppTargetSetType {
                            let result = match source.write_command(&id, &command).await {
                                Ok(()) => crate::vpp::TargetCheck::Set,
                                Err(e) if e.is::<crate::vpp::TargetTypeRefused>() => {
                                    crate::vpp::TargetCheck::Refused(format!("{e:#}"))
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "⚠️ Target set type check of {} failed: {:#}",
                                        id,
                                        e
                                    );
                                    crate::vpp::TargetCheck::Failed(format!("{e:#}"))
                                }
                            };
                            checks.lock().insert(id.clone(), result);
                            if let Some(done) = done {
                                let _ = done.send(());
                            }
                            continue;
                        }
                        let vpp = matches!(command, crate::components::InverterCommand::SetVpp(_));
                        let ok =
                            execute_command(&source, &backup_confirmed, id.clone(), command).await;
                        if vpp {
                            let mut failed = failed.lock();
                            if ok {
                                failed.remove(&id);
                            } else {
                                failed.insert(id.clone());
                            }
                        }
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                });
                queue
            })
            .clone();
        let item = (command, done);
        {
            let mut items = queue.items.lock();
            if vpp_replaceable(&item) {
                items.retain(|pending| !vpp_replaceable(pending));
            }
            items.push_back(item);
        }
        queue.wake.notify_one();
    }
}

/// Write one command and record the Back Up minimum confirmation (see `AsyncInverterWriter`).
async fn execute_command(
    source: &Arc<dyn crate::traits::InverterDataSource>,
    backup_confirmed: &parking_lot::Mutex<std::collections::HashMap<String, u8>>,
    inverter_id: String,
    command: crate::components::InverterCommand,
) -> bool {
    use crate::components::InverterCommand;
    match source.write_command(&inverter_id, &command).await {
        Ok(_) => {
            if matches!(command, InverterCommand::SetVpp(_)) {
                tracing::debug!(
                    "✅ Async command succeeded for {}: {:?}",
                    inverter_id,
                    command
                );
            } else {
                tracing::info!(
                    "✅ Async command succeeded for {}: {:?}",
                    inverter_id,
                    command
                );
            }
            match command {
                InverterCommand::SetBackupMode { min_soc_pct }
                | InverterCommand::SetBackupMinSoc { min_soc_pct } => {
                    backup_confirmed.lock().insert(inverter_id, min_soc_pct);
                }
                // A volatile VPP command leaves the basic work mode and its minimum alone
                InverterCommand::SetVpp(_) => {}
                _ => {
                    backup_confirmed.lock().remove(&inverter_id);
                }
            }
            true
        }
        Err(e) => {
            // A failed VPP command does not touch the basic work mode or its minimum
            if !matches!(command, InverterCommand::SetVpp(_)) {
                backup_confirmed.lock().remove(&inverter_id);
            }
            tracing::error!(
                "❌ Async command failed for {}: {:?} - {}",
                inverter_id,
                command,
                e
            );
            false
        }
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

/// Wall clock of the state reader and the executor. Absent means `Utc::now`; tests insert one
/// to step time at the real 5 s read cadence.
#[derive(Resource, Clone)]
pub struct ClockResource(pub Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>);

impl ClockResource {
    pub fn now(&self) -> chrono::DateTime<chrono::Utc> {
        (self.0)()
    }
}

/// The time from `clock`, or `Utc::now` without one
pub fn clock_now(clock: Option<&ClockResource>) -> chrono::DateTime<chrono::Utc> {
    clock.map_or_else(chrono::Utc::now, ClockResource::now)
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

// ============= User Control Resource =============

pub use fluxion_types::{FixedTimeSlot, UserControlState};

/// Lever values synced from the HA helpers (see `docs/legacy-levers/CONTRACT.md`).
/// `available` is false until the first successful sync; consumers then apply no lever rules.
#[derive(Resource, Debug, Clone, Default)]
pub struct UserLeversResource {
    pub levers: fluxion_types::UserLevers,
    pub available: bool,
    /// True when the lever sync is enabled and running. Execution waits for `available` (max 120 s)
    /// before writing any mode, so a kill switch that is on is seen before the planner writes.
    pub sync_expected: bool,
}

/// Resource for user control state.
///
/// This resource holds the runtime state for user control features:
/// - Enable/disable FluxION mode changes
/// - Disallow charge/discharge modes
/// - Fixed time slots that override the schedule
#[derive(Resource, Debug, Clone, Default)]
pub struct UserControlResource {
    pub state: UserControlState,
}

impl UserControlResource {
    /// Create a new resource with the given state.
    pub fn new(state: UserControlState) -> Self {
        Self { state }
    }

    /// Check if FluxION is enabled.
    pub fn is_enabled(&self) -> bool {
        self.state.enabled
    }

    /// Check if a mode is allowed given current restrictions.
    pub fn is_mode_allowed(&self, mode: fluxion_types::InverterOperationMode) -> bool {
        self.state.is_mode_allowed(mode)
    }

    /// Get the fixed slot for a specific time, if any.
    pub fn get_fixed_slot_at(&self, time: DateTime<Utc>) -> Option<&FixedTimeSlot> {
        self.state.get_fixed_slot_at(time)
    }
}
