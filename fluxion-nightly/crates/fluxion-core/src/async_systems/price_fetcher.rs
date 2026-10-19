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

use bevy_ecs::prelude::*;
// futures_timer::Delay no longer needed - removed channel polling
// std::time::Duration removed - no longer needed for Delay
use tracing::{debug, error, info, trace, warn};

use crate::{
    PluginManagerResource, PriceDataSourceResource,
    components::*,
    pricing::analyze_prices,
    resources::SystemConfig,
    scheduling::{ScheduleConfig, active_user_levers, generate_schedule_with_optimizer},
    strategy::winter_reserve::planner_levers,
};
use fluxion_types::UserLevers;
use fluxion_types::config::ControlConfig;

use super::BackupDischargeMinSoc;

/// Generates consumption forecast for scheduling using priority-based data sources
/// Priority: 1) Historical data (EMA), 2) Configured fallback. The instantaneous house load is
/// not used: on a parallel master it includes the slave's battery and PV flows (17 kW while the
/// slave charges, negative while it exports), and a flat plan load from it fed back into grid charging.
fn generate_consumption_forecast(
    consumption_history: &ConsumptionHistory,
    control_config: &ControlConfig,
    num_blocks: usize,
) -> Option<Vec<f32>> {
    // Calculate daily consumption estimate
    let daily_consumption_kwh = if !consumption_history.summaries().is_empty() {
        // Priority 1: Use historical EMA data (most accurate)
        let history: Vec<f32> = consumption_history.consumption_values();

        if let Some(ema) = calculate_ema(&history, 7) {
            trace!("📊 Using historical EMA consumption: {:.2} kWh/day", ema);
            ema
        } else {
            control_config.average_household_load_kw * 24.0
        }
    } else {
        // Priority 2: Use configured fallback
        trace!(
            "📊 Using fallback consumption: {:.2} kWh/day",
            control_config.average_household_load_kw * 24.0
        );
        control_config.average_household_load_kw * 24.0
    };

    // Convert daily to per-block (15-minute blocks)
    let consumption_per_block_kwh = daily_consumption_kwh / 96.0; // 96 blocks per day

    // Generate forecast array
    let forecast = vec![consumption_per_block_kwh; num_blocks];

    trace!(
        "📊 Generated consumption forecast: {:.3} kWh per 15-min block for {} blocks",
        consumption_per_block_kwh, num_blocks
    );

    Some(forecast)
}

/// Helper function to calculate Exponential Moving Average (from fluxion-strategy crate)
fn calculate_ema(values: &[f32], period: usize) -> Option<f32> {
    if values.is_empty() || period == 0 {
        return None;
    }

    let alpha = 2.0 / (period as f32 + 1.0);
    let mut ema = values[0];

    for &value in values.iter().skip(1) {
        ema = alpha * value + (1.0 - alpha) * ema;
    }

    Some(ema)
}

/// Initialize price cache resource with configured data source
/// Replaces the complex channel spawning with a simple resource
pub fn setup_price_cache(mut commands: Commands, price_source: Res<PriceDataSourceResource>) {
    let price_cache = crate::resources::PriceCache::new(
        price_source.0.clone(),
        3, // 3 seconds fetch interval (price cache refresh; the buy sensor has its own limit)
    );

    commands.insert_resource(price_cache);
    info!("✅ Price cache initialized with 3-second fetch interval");
}

/// Max wait at startup for the HDO tariff before the first schedule is built (brief H29).
const HDO_STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max wait at startup for the house-load profile before the first schedule is built (brief H31).
/// The history fetch reads three entities in turn, each HA history request with a 60 s timeout:
/// a normal fetch takes about 20 s (2026-10-18 restart, 7 days on a 96 kWh site). 90 s covers a
/// slow HA after its own restart and is a small part of a 15-minute block. The executor's 120 s
/// lever-sync wait ends as soon as the levers sync (normally within seconds), so it does not hide
/// this wait: with the history slow or down, the first mode write moves from about 30 s (HDO
/// bound, H29) to about 90 s after start, during which the inverter keeps its pre-restart mode.
const PROFILE_STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// One planner input the first schedule after a restart waits for.
struct StartupInput {
    bound: std::time::Duration,
    /// What is awaited, as in "Waiting for the {name}"
    name: &'static str,
    /// What the schedule is built with instead after the bound
    fallback: &'static str,
}

const HDO_INPUT: StartupInput = StartupInput {
    bound: HDO_STARTUP_WAIT,
    name: "HDO tariff",
    fallback: "high grid fee",
};

const PROFILE_INPUT: StartupInput = StartupInput {
    bound: PROFILE_STARTUP_WAIT,
    name: "house-load profile",
    fallback: "load forecast without the profile",
};

/// Startup state of the wait for one planner input: when the first run happened, and whether the
/// wait is over (input arrived, timed out, or not configured).
#[derive(Default)]
pub struct StartupWait {
    started: Option<std::time::Instant>,
    done: bool,
    logged: bool,
    timed_out: bool,
}

/// Startup waits of the first schedule, one per input (kept in one `Local`).
#[derive(Default)]
pub struct StartupWaits {
    hdo: StartupWait,
    profile: StartupWait,
}

/// True while the first schedule waits for `input` (max `input.bound` from the first run). Only
/// the first schedule waits; no wait when the input is not configured.
fn startup_input_wait(
    input: &StartupInput,
    configured: bool,
    ready: bool,
    startup: &mut StartupWait,
    now: std::time::Instant,
) -> bool {
    if startup.done {
        return false;
    }
    let started = *startup.started.get_or_insert(now);
    if !configured || ready {
        startup.done = true;
        return false;
    }
    if now.saturating_duration_since(started) >= input.bound {
        warn!(
            "{} not received {} s after startup, building the schedule without it ({})",
            input.name,
            input.bound.as_secs(),
            input.fallback
        );
        startup.done = true;
        startup.timed_out = true;
        return false;
    }
    if !startup.logged {
        info!(
            "⏳ Waiting for the {} before the first schedule (max {} s)",
            input.name,
            input.bound.as_secs()
        );
        startup.logged = true;
    }
    true
}

/// True while the first schedule waits for the HDO tariff (max `HDO_STARTUP_WAIT`). Without it
/// every block is priced at the high grid fee, and the executor's initial sync applies that plan
/// (2026-10-17 restart). Only the first schedule waits; no wait without an HDO sensor.
pub fn hdo_startup_wait(
    hdo_configured: bool,
    hdo_ready: bool,
    startup: &mut StartupWait,
    now: std::time::Instant,
) -> bool {
    startup_input_wait(&HDO_INPUT, hdo_configured, hdo_ready, startup, now)
}

/// True while the first schedule waits for the house-load profile (max `PROFILE_STARTUP_WAIT`).
/// Without it the planner falls back to a flat load forecast (31 kWh instead of 86 on the
/// 2026-10-18 restart), and the executor's initial sync applies that plan. Ready = the profile is
/// there, or the history fetch delivered its result without one (waiting longer gains nothing).
/// Only the first schedule waits; no wait without a history source.
pub fn profile_startup_wait(
    profile_configured: bool,
    profile_ready: bool,
    startup: &mut StartupWait,
    now: std::time::Instant,
) -> bool {
    startup_input_wait(
        &PROFILE_INPUT,
        profile_configured,
        profile_ready,
        startup,
        now,
    )
}

/// Whether the first schedule still waits for an input. Both waits run from the same first run
/// (both are evaluated every time), so the combined worst case is the longer bound, not the sum.
fn first_schedule_waits(
    (hdo_configured, hdo_ready): (bool, bool),
    (profile_configured, profile_ready): (bool, bool),
    waits: &mut StartupWaits,
    now: std::time::Instant,
) -> bool {
    let hdo = hdo_startup_wait(hdo_configured, hdo_ready, &mut waits.hdo, now);
    let profile = profile_startup_wait(profile_configured, profile_ready, &mut waits.profile, now);
    hdo || profile
}

/// Simplified system that updates prices using direct cache access
/// Replaces the complex channel polling with on-demand fetching
#[allow(clippy::too_many_arguments)]
#[expect(clippy::type_complexity)]
pub fn update_prices_system(
    price_cache: Res<crate::resources::PriceCache>,
    mut commands: Commands,
    mut price_data_query: Query<(Entity, &mut SpotPriceData)>,
    mut price_analysis_query: Query<(Entity, &mut PriceAnalysis)>,
    mut schedule_query: Query<&mut OperationSchedule>,
    _battery_query: Query<&BatteryStatus>,
    config: Res<SystemConfig>,
    backup_soc: Option<Res<BackupDischargeMinSoc>>,
    hdo_data: Option<Res<super::HdoScheduleData>>,
    hdo_cache: Option<Res<crate::resources::GlobalHdoCache>>,
    consumption_history: Res<crate::components::ConsumptionHistory>,
    inverter_raw_state_query: Query<&RawInverterState>,
    plugin_manager_res: Res<PluginManagerResource>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
    solar_forecast: Option<Res<super::SolarForecastData>>,
    (timezone_config, user_levers, mut last_planner_levers, mut startup_waits): (
        Option<Res<crate::resources::TimezoneConfig>>,
        Option<Res<crate::resources::UserLeversResource>>,
        Local<Option<UserLevers>>,
        Local<StartupWaits>,
    ),
) {
    // A change of the planner levers regenerates the schedule even when prices are fresh
    let planner_levers_now = planner_levers(active_user_levers(user_levers.as_deref()));
    let levers_changed = planner_levers_now != *last_planner_levers;

    // Only fetch if cache is stale (non-blocking check)
    if !price_cache.is_stale() && !levers_changed {
        return;
    }

    // Startup: the first schedule waits for the inputs it would otherwise be built without, each
    // only when its source is configured:
    // - the HDO tariff (max 30 s, H29), from the sensor the HA HDO fetcher reads. Ready = the HDO
    //   cache knows the tariff now.
    // - the house-load profile (max 90 s, H31), from the history fetcher's entities. Ready = the
    //   profile is there, or the fetch delivered its result without one.
    let hdo_configured = !config
        .strategies_config
        .winter_adaptive_v3
        .hdo_sensor_entity
        .is_empty();
    let hdo_ready = hdo_cache
        .as_ref()
        .is_some_and(|cache| cache.is_low_tariff(chrono::Utc::now()).is_some());
    let profile_configured = !config.history.consumption_entity.trim().is_empty()
        || config
            .history
            .house_load_power_entity
            .as_deref()
            .is_some_and(|entity| !entity.trim().is_empty());
    let profile_ready =
        consumption_history.hourly_profile().is_some() || consumption_history.fetch_received();
    if first_schedule_waits(
        (hdo_configured, hdo_ready),
        (profile_configured, profile_ready),
        &mut startup_waits,
        std::time::Instant::now(),
    ) {
        return;
    }

    debug!("💰 Checking for updated price data...");

    // Fetch prices using the cache (will return cached data if fresh)
    let mut new_prices = match price_cache.get_or_fetch() {
        Ok(prices) => prices,
        Err(e) => {
            error!("❌ Failed to fetch prices: {}", e);
            return;
        }
    };

    // Calculate effective prices (spot + grid fees) using HDO tariff data
    crate::scheduling::calculate_effective_prices(
        &mut new_prices.time_block_prices,
        hdo_cache.as_deref(),
        &config.pricing_config,
    );

    let new_block_count = new_prices.time_block_prices.len();
    let new_hours = new_block_count as f32 / 4.0;

    // Detect if we got significantly more data (day-ahead prices arrived)
    let old_block_count = price_data_query
        .single()
        .ok()
        .map(|(_, data)| data.time_block_prices.len())
        .unwrap_or(0);

    let is_day_ahead_arrival = new_block_count > old_block_count + 10;

    if is_day_ahead_arrival {
        info!(
            "📊 Day-ahead prices arrived! Old: {} blocks ({:.1}h), New: {} blocks ({:.1}h). Will recalculate schedule.",
            old_block_count,
            old_block_count as f32 / 4.0,
            new_block_count,
            new_hours
        );
    } else {
        debug!(
            "📊 Received price data update: {} blocks ({:.1} hours)",
            new_block_count, new_hours
        );
    }

    // Update price data entity or create if doesn't exist
    if let Ok((_, mut price_data)) = price_data_query.single_mut() {
        *price_data = new_prices.clone();
    } else {
        commands.spawn(new_prices.clone());
    }

    debug!(
        "🔄 Regenerating schedule due to: {}",
        if is_day_ahead_arrival {
            "day-ahead prices arrival"
        } else {
            "price data update"
        }
    );

    // Regenerate schedule based on new prices
    let analysis = analyze_prices(
        &new_prices.time_block_prices,
        config.control_config.force_charge_hours,
        config.control_config.force_discharge_hours,
        config.pricing_config.use_spot_prices_to_buy,
        config.pricing_config.use_spot_prices_to_sell,
        config.control_config.min_consecutive_force_blocks,
    );

    let schedule_config = ScheduleConfig {
        min_battery_soc: config.control_config.min_battery_soc,
        max_battery_soc: config.control_config.max_battery_soc,
        target_inverters: config.inverters.iter().map(|i| i.id.clone()).collect(),
        display_currency: config.system_config.display_currency,
        default_battery_mode: config.control_config.default_battery_mode,
    };

    // Skip scheduling if no inverter state is available yet (startup race condition)
    if inverter_raw_state_query.iter().count() == 0 {
        debug!("⏳ Skipping schedule generation - waiting for inverter state data");
        return;
    }

    *last_planner_levers = planner_levers_now;

    // Get current battery SOC from raw inverter state (more reliable than BatteryStatus component)
    let current_soc = inverter_raw_state_query
        .iter()
        .map(|raw| raw.state.battery_soc)
        .sum::<f32>()
        / inverter_raw_state_query.iter().count().max(1) as f32;

    // Get backup_discharge_min_soc from HA sensor (via BackupDischargeMinSoc resource)
    let backup_discharge_min_soc = backup_soc
        .as_ref()
        .map(|s| s.value)
        .unwrap_or(config.control_config.hardware_min_battery_soc);

    // Generate consumption forecast from available data
    let consumption_forecast = generate_consumption_forecast(
        &consumption_history,
        &config.control_config,
        new_prices.time_block_prices.len(),
    );

    // Get today's grid import energy from inverter state (sensor.<prefix>_today_s_import_energy)
    // Fallback to consumption history if sensor is unavailable
    let grid_import_today_kwh = inverter_raw_state_query
        .iter()
        .next()
        .and_then(|raw_state| raw_state.state.grid_import_today_kwh)
        .or_else(|| {
            // Fallback: Get today's consumption from history (most recent day)
            consumption_history
                .summaries()
                .front()
                .filter(|s| {
                    // Only use if it's actually today's data (within last 24 hours)
                    let age = chrono::Utc::now().signed_duration_since(s.date);
                    age < chrono::Duration::hours(24)
                })
                .map(|s| s.grid_import_kwh)
        });

    // Use economic optimizer for schedule generation with shared plugin manager
    let plugin_manager = plugin_manager_res.0.read();
    let hdo_raw_data = hdo_data.as_ref().and_then(|h| h.raw_data.clone());
    let user_control_state = user_control.as_ref().map(|uc| &uc.state);
    // PV forecast per block (aligned with time_block_prices)
    let block_starts: Vec<_> = new_prices
        .time_block_prices
        .iter()
        .map(|b| b.block_start)
        .collect();
    let solar_forecast_per_block =
        super::solar_block_forecast(solar_forecast.as_deref(), &block_starts);

    let new_schedule = generate_schedule_with_optimizer(
        &new_prices.time_block_prices,
        &config.control_config,
        &schedule_config,
        current_soc,
        solar_forecast_per_block.as_deref(), // PV forecast per block
        consumption_forecast.as_deref(),     // Enhanced consumption forecast
        backup_discharge_min_soc,
        grid_import_today_kwh,
        &plugin_manager,
        hdo_raw_data,
        solar_forecast.as_ref().map_or(0.0, |s| s.total_today_kwh),
        solar_forecast
            .as_ref()
            .map_or(0.0, |s| s.remaining_today_kwh),
        solar_forecast.as_ref().map_or(0.0, |s| s.tomorrow_kwh),
        user_control_state,
        consumption_history
            .hourly_profile()
            .map(|p| &p.hourly_avg_kwh),
        consumption_history
            .hourly_profile()
            .and_then(|p| p.hourly_p75_kwh.as_ref()),
        timezone_config
            .as_ref()
            .and_then(|tz| tz.timezone.as_deref()),
        active_user_levers(user_levers.as_deref()),
    );

    // Update or create PriceAnalysis entity
    if let Ok((_, mut price_analysis)) = price_analysis_query.single_mut() {
        *price_analysis = analysis;
    } else {
        commands.spawn(analysis);
    }

    // Update schedule or create if doesn't exist
    if let Ok(mut schedule) = schedule_query.single_mut() {
        *schedule = new_schedule;
        debug!("✅ Schedule regenerated based on new price data");
    } else {
        commands.spawn(new_schedule);
        info!("✅ Initial schedule created");
    }
}

#[cfg(test)]
mod consumption_forecast_tests {
    use super::*;

    #[test]
    fn without_history_the_forecast_is_the_configured_average() {
        // Never the instantaneous house load: a parallel master reads ~17 kW while the slave charges.
        let control = ControlConfig {
            average_household_load_kw: 2.0,
            ..ControlConfig::default()
        };
        let forecast =
            generate_consumption_forecast(&ConsumptionHistory::new(7), &control, 4).unwrap();
        assert_eq!(forecast, vec![0.5; 4]);
    }
}

/// Briefs H29 and H31: the first schedule after a restart waits for the HDO tariff and the
/// house-load profile.
#[cfg(test)]
mod hdo_startup_tests {
    use super::*;
    use crate::async_systems::HdoScheduleData;
    use crate::resources::{GlobalHdoCache, PriceCache, TimezoneConfig};
    use crate::traits::{GenericInverterState, PriceDataSource};
    use async_trait::async_trait;
    use chrono::{DurationRound, Utc};
    use fluxion_i18n::Language;
    use fluxion_types::config::{Currency, PriceSchedule, PricingConfig, SystemSettingsConfig};
    use fluxion_types::pricing::TimeBlockPrice;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn first_schedule_waits_for_hdo_then_proceeds_when_it_arrives() {
        let mut startup = StartupWait::default();
        let t0 = Instant::now();
        assert!(hdo_startup_wait(true, false, &mut startup, t0));
        assert!(hdo_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(10)
        ));
        assert!(!hdo_startup_wait(
            true,
            true,
            &mut startup,
            t0 + Duration::from_secs(11)
        ));
        assert!(!startup.timed_out);
        // Only the first schedule waits: HDO missing later does not block replans
        assert!(!hdo_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(12)
        ));
    }

    #[test]
    fn first_schedule_proceeds_after_30_s_without_hdo() {
        let mut startup = StartupWait::default();
        let t0 = Instant::now();
        assert!(hdo_startup_wait(true, false, &mut startup, t0));
        assert!(hdo_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(29)
        ));
        assert!(!hdo_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(30)
        ));
        assert!(startup.timed_out);
    }

    #[test]
    fn no_hdo_sensor_no_wait() {
        let mut startup = StartupWait::default();
        assert!(!hdo_startup_wait(
            false,
            false,
            &mut startup,
            Instant::now()
        ));
        assert!(!startup.timed_out);
    }

    struct FlatPrices;

    #[async_trait]
    impl PriceDataSource for FlatPrices {
        async fn read_prices(&self) -> anyhow::Result<SpotPriceData> {
            let start = Utc::now()
                .duration_trunc(chrono::Duration::minutes(15))
                .unwrap();
            Ok(SpotPriceData {
                time_block_prices: (0..96)
                    .map(|i| TimeBlockPrice {
                        block_start: start + chrono::Duration::minutes(15 * i),
                        duration_minutes: 15,
                        price_czk_per_kwh: 3.0,
                        effective_price_czk_per_kwh: 3.0,
                        spot_sell_price_czk_per_kwh: Some(2.0),
                        buy_price_czk_per_kwh: None,
                    })
                    .collect(),
                block_duration_minutes: 15,
                fetched_at: Utc::now(),
                ha_last_updated: Utc::now(),
            })
        }

        async fn health_check(&self) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn name(&self) -> &str {
            "flat"
        }
    }

    /// `history_entity`: the house-load entity of the history fetcher; empty = no history source.
    fn config(hdo_sensor: &str, history_entity: &str) -> SystemConfig {
        let mut config = SystemConfig {
            inverters: vec![],
            pricing_config: PricingConfig {
                spot_price_entity: "sensor.spot_price".to_string(),
                tomorrow_price_entity: None,
                buy_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_price_czk: PriceSchedule::Flat(4.0),
                fixed_sell_price_czk: PriceSchedule::Flat(2.0),
                spot_buy_fee_czk: 0.5,
                spot_sell_fee_czk: 0.5,
                hdo_sensor_entity: hdo_sensor.to_string(),
                hdo_low_tariff_czk: 0.36,
                hdo_high_tariff_czk: 1.0,
            },
            control_config: ControlConfig::default(),
            system_config: SystemSettingsConfig {
                update_interval_secs: 60,
                debug_mode: true,
                display_currency: Currency::CZK,
                language: Language::English,
                timezone: None,
                display_timezone: None,
            },
            strategies_config: Default::default(),
            history: Default::default(),
            solar_forecast: Default::default(),
            remote_access: Default::default(),
            notifications: Default::default(),
        };
        config
            .strategies_config
            .winter_adaptive_v3
            .hdo_sensor_entity = hdo_sensor.to_string();
        config.history.consumption_entity = String::new();
        config.history.solar_production_entity = String::new();
        config.history.house_load_power_entity =
            (!history_entity.is_empty()).then(|| history_entity.to_string());
        config
    }

    /// The price/schedule systems of a restart: HDO cache sync and the history channel poll, then
    /// the price update. The sender plays the history fetcher.
    fn world(
        hdo_sensor: &str,
        history_entity: &str,
    ) -> (
        World,
        Schedule,
        crossbeam_channel::Sender<crate::async_tasks::ConsumptionHistoryUpdate>,
    ) {
        let config = config(hdo_sensor, history_entity);
        let manager = crate::plugin_adapters::create_plugin_manager(
            Some(&config.strategies_config),
            &config.control_config,
        );
        let mut world = World::new();
        world.insert_resource(PriceCache::new(Arc::new(FlatPrices), 3));
        world.insert_resource(PluginManagerResource(Arc::new(RwLock::new(manager))));
        world.insert_resource(config);
        world.insert_resource(ConsumptionHistory::default());
        world.insert_resource(HdoScheduleData::default());
        world.insert_resource(GlobalHdoCache::default());
        world.insert_resource(TimezoneConfig::new(Some("Europe/Prague".to_string())));
        world.spawn(RawInverterState {
            state: GenericInverterState {
                battery_soc: 50.0,
                ..Default::default()
            },
            last_updated: Utc::now(),
        });
        let (history_tx, history_rx) = crossbeam_channel::unbounded();
        world.spawn(crate::async_tasks::ConsumptionHistoryChannel {
            receiver: history_rx,
        });
        let mut schedule = Schedule::default();
        schedule.add_systems(
            (
                crate::continuous_systems::sync_hdo_cache_system,
                crate::async_systems::poll_consumption_history_channel,
                update_prices_system,
            )
                .chain(),
        );
        (world, schedule, history_tx)
    }

    /// History fetch result with the 7-day house-load profile (or without one).
    fn history_update(with_profile: bool) -> crate::async_tasks::ConsumptionHistoryUpdate {
        crate::async_tasks::ConsumptionHistoryUpdate {
            daily_summaries: Vec::new(),
            hourly_profile: with_profile.then_some(HourlyConsumptionProfile {
                hourly_avg_kwh: [3.5; 24],
                days_averaged: 7,
                hourly_p75_kwh: None,
            }),
        }
    }

    /// HDO sensor data for today and tomorrow (Prague): low tariff all day.
    fn hdo_all_low() -> String {
        let today = Utc::now()
            .with_timezone(&chrono_tz::Europe::Prague)
            .date_naive();
        let days: Vec<String> = [today, today.succ_opt().unwrap()]
            .iter()
            .map(|d| {
                format!(
                    r#"{{"signal": "EVV1", "datum": "{}", "casy": "00:00-24:00"}}"#,
                    d.format("%d.%m.%Y")
                )
            })
            .collect();
        format!(r#"{{"data": {{"signals": [{}]}}}}"#, days.join(","))
    }

    fn has_schedule(world: &mut World) -> bool {
        world.query::<&OperationSchedule>().iter(world).count() > 0
    }

    #[test]
    fn restart_builds_the_first_schedule_only_after_hdo_arrives() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (mut world, mut schedule, _history) = world("sensor.cez_hdo_raw_data", "");
        // HDO not received yet: no schedule (2026-10-17 16:13:20 built one at the high fee)
        schedule.run(&mut world);
        assert!(!has_schedule(&mut world));
        // HDO arrives (poll_hdo_channel): the next run builds the schedule at the low fee
        world.resource_mut::<HdoScheduleData>().raw_data = Some(hdo_all_low());
        schedule.run(&mut world);
        assert!(has_schedule(&mut world));
        let prices = world
            .query::<&SpotPriceData>()
            .single(&world)
            .unwrap()
            .time_block_prices[0]
            .effective_price_czk_per_kwh;
        // spot 3.0 + low HDO fee 0.36 + buy fee 0.5
        assert!((prices - 3.86).abs() < 1e-3, "effective price {prices}");
    }

    #[test]
    fn restart_without_hdo_sensor_builds_the_schedule_at_once() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (mut world, mut schedule, _history) = world("", "");
        schedule.run(&mut world);
        assert!(has_schedule(&mut world));
    }

    // ---- H31: the house-load profile ----

    #[test]
    fn first_schedule_waits_for_the_profile_then_proceeds_when_it_arrives() {
        let mut startup = StartupWait::default();
        let t0 = Instant::now();
        assert!(profile_startup_wait(true, false, &mut startup, t0));
        assert!(profile_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(60)
        ));
        assert!(!profile_startup_wait(
            true,
            true,
            &mut startup,
            t0 + Duration::from_secs(61)
        ));
        assert!(!startup.timed_out);
        // Only the first schedule waits: a later failed history refresh does not block replans
        assert!(!profile_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(3600)
        ));
    }

    #[test]
    fn first_schedule_proceeds_after_90_s_without_the_profile() {
        let mut startup = StartupWait::default();
        let t0 = Instant::now();
        assert!(profile_startup_wait(true, false, &mut startup, t0));
        assert!(profile_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(89)
        ));
        // The WARN path
        assert!(!profile_startup_wait(
            true,
            false,
            &mut startup,
            t0 + Duration::from_secs(90)
        ));
        assert!(startup.timed_out);
    }

    #[test]
    fn no_history_source_no_profile_wait() {
        let mut startup = StartupWait::default();
        assert!(!profile_startup_wait(
            false,
            false,
            &mut startup,
            Instant::now()
        ));
        assert!(!startup.timed_out);
    }

    #[test]
    fn hdo_and_profile_waits_run_together_bounded_by_the_longer() {
        let mut waits = StartupWaits::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        // Both missing
        assert!(first_schedule_waits(
            (true, false),
            (true, false),
            &mut waits,
            at(0)
        ));
        // HDO arrives, profile still missing: still waiting
        assert!(first_schedule_waits(
            (true, true),
            (true, false),
            &mut waits,
            at(5)
        ));
        // Profile arrives: the first schedule is built
        assert!(!first_schedule_waits(
            (true, true),
            (true, true),
            &mut waits,
            at(20)
        ));

        // Neither ever arrives: HDO gives up at 30 s, the profile at 90 s (measured from the same
        // first run), so the combined worst case is 90 s, not 120 s
        let mut waits = StartupWaits::default();
        assert!(first_schedule_waits(
            (true, false),
            (true, false),
            &mut waits,
            at(0)
        ));
        assert!(first_schedule_waits(
            (true, false),
            (true, false),
            &mut waits,
            at(30)
        ));
        assert!(waits.hdo.timed_out);
        assert!(first_schedule_waits(
            (true, false),
            (true, false),
            &mut waits,
            at(89)
        ));
        assert!(!first_schedule_waits(
            (true, false),
            (true, false),
            &mut waits,
            at(90)
        ));
        assert!(waits.profile.timed_out);
    }

    #[test]
    fn restart_builds_the_first_schedule_only_after_the_profile_arrives() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (mut world, mut schedule, history) = world("", "sensor.house_load");
        // Profile not loaded yet: no prices, no schedule (2026-10-18 14:53:11 built one with a
        // flat 31 kWh load instead of 86 kWh, and the initial sync applied it)
        schedule.run(&mut world);
        schedule.run(&mut world);
        assert!(!has_schedule(&mut world));
        assert_eq!(world.query::<&SpotPriceData>().iter(&world).count(), 0);
        // The history fetch delivers the profile: the next run builds the schedule with it
        history.send(history_update(true)).unwrap();
        schedule.run(&mut world);
        assert!(has_schedule(&mut world));
        assert!(
            world
                .resource::<ConsumptionHistory>()
                .hourly_profile()
                .is_some()
        );
    }

    #[test]
    fn restart_proceeds_when_the_history_fetch_returns_no_profile() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (mut world, mut schedule, history) = world("", "sensor.house_load");
        schedule.run(&mut world);
        assert!(!has_schedule(&mut world));
        // The fetch finished without a profile (too little data, or HA failed): no point waiting
        history.send(history_update(false)).unwrap();
        schedule.run(&mut world);
        assert!(has_schedule(&mut world));
    }

    #[test]
    fn restart_waits_for_both_hdo_and_profile() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (mut world, mut schedule, history) =
            world("sensor.cez_hdo_raw_data", "sensor.house_load");
        schedule.run(&mut world);
        assert!(!has_schedule(&mut world));
        // HDO first: the profile is still missing
        world.resource_mut::<HdoScheduleData>().raw_data = Some(hdo_all_low());
        schedule.run(&mut world);
        assert!(!has_schedule(&mut world));
        // Then the profile: the schedule is built, at the low HDO fee
        history.send(history_update(true)).unwrap();
        schedule.run(&mut world);
        assert!(has_schedule(&mut world));
        let price = world
            .query::<&SpotPriceData>()
            .single(&world)
            .unwrap()
            .time_block_prices[0]
            .effective_price_czk_per_kwh;
        assert!((price - 3.86).abs() < 1e-3, "effective price {price}");

        // Profile first, then HDO: same, the schedule waits for the last one
        let (mut world2, mut schedule2, history2) =
            self::world("sensor.cez_hdo_raw_data", "sensor.house_load");
        history2.send(history_update(true)).unwrap();
        schedule2.run(&mut world2);
        assert!(!has_schedule(&mut world2));
        world2.resource_mut::<HdoScheduleData>().raw_data = Some(hdo_all_low());
        schedule2.run(&mut world2);
        assert!(has_schedule(&mut world2));
    }
}
