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

mod config;
mod heartbeat_client;
mod version;

use anyhow::Result;
use bevy_app::{ScheduleRunnerPlugin, TaskPoolPlugin, prelude::*};
use std::{sync::Arc, time::Duration};
use tracing::{info, warn};
use tracing_subscriber::FmtSubscriber;

use fluxion_adapters::{
    CzSpotPriceAdapter, HaClientResource, HaPlugin, HomeAssistantClient,
    HomeAssistantInverterAdapter, PriceAdapterTimezoneHandle,
};
use fluxion_core::{
    ConfigUpdateSender, FluxionCorePlugin, PluginManagerResource, SystemConfig, TimezoneConfig,
    UserControlPersistence, UserControlResource, UserControlUpdateSender, WebQuerySender,
    plugin_adapters::create_plugin_manager,
};
use fluxion_i18n::I18n;
use fluxion_web::{PluginApiState, RemoteAccessApiState, UserControlApiState};
use parking_lot::RwLock;

fn main() -> Result<()> {
    // Handle command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "--help" | "-h" => {
                println!("FluxION - PV Plant Automation (MVP)");
                println!("Version: {}", version::VERSION);
                println!();
                println!("Usage: fluxion [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -h, --help    Print this help message");
                println!("  -v, --version Print version");
                return Ok(());
            }
            "--version" | "-v" => {
                println!("{}", version::VERSION);
                return Ok(());
            }
            _ => {
                // Continue to normal execution for other args or no args
                // If we want to be strict about unknown args, we could handle that here
            }
        }
    }

    // Create tokio runtime for async HTTP operations
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    // Run Bevy app in a blocking task so tokio can keep running async tasks
    runtime.block_on(async {
        tokio::task::spawn_blocking(initialize_and_run)
            .await
            .expect("Bevy task panicked")
    })
}

/// What the backtest reads: the live entity ids (the controlled inverter, never a slave; PV adds
/// `pv_extra_prefixes`), prices, control and Winter Reserve settings. `Err` says what is missing.
fn backtest_site(
    config: &config::AppConfig,
    system_config: &SystemConfig,
) -> std::result::Result<fluxion_core::backtest::BacktestSite, String> {
    let timezone: chrono_tz::Tz = config
        .system
        .timezone
        .as_deref()
        .and_then(|tz| tz.parse().ok())
        .ok_or("the Home Assistant timezone is not known, so local days cannot be built")?;
    let master = config
        .inverters
        .iter()
        .find(|inv| inv.topology != "slave")
        .or(config.inverters.first())
        .ok_or("no inverter is configured")?;
    let mapper = fluxion_adapters::create_entity_mapper(master.inverter_type);
    let mut pv = vec![mapper.get_pv_power_entity(&master.id)];
    pv.extend(
        config::AppConfig::pv_extra_prefixes(master)
            .iter()
            .map(|prefix| mapper.get_pv_power_entity(prefix)),
    );
    let non_empty = |entity: &Option<String>| entity.clone().filter(|e| !e.trim().is_empty());
    let pricing = &system_config.pricing_config;
    Ok(fluxion_core::backtest::BacktestSite {
        timezone,
        entities: fluxion_core::backtest::BacktestEntities {
            soc: mapper.get_battery_soc_entity(&master.id),
            house_load: non_empty(&system_config.history.house_load_power_entity)
                .or_else(|| mapper.get_house_load_entity(&master.id))
                .ok_or("no house load sensor is known for this inverter")?,
            pv,
            grid_import: mapper
                .get_grid_import_power_entity(&master.id)
                .ok_or("no grid import power sensor is known for this inverter")?,
            grid_export: mapper
                .get_grid_export_power_entity(&master.id)
                .ok_or("no grid export power sensor is known for this inverter")?,
            spot_price: pricing.spot_price_entity.clone(),
            // Live, the buy sensor is used only when buying at spot
            buy_price: non_empty(&pricing.buy_price_entity)
                .filter(|_| pricing.use_spot_prices_to_buy),
            // The HDO sensor key the live fetcher reads (resolved by content at load, as live)
            hdo: non_empty(&Some(
                system_config
                    .strategies_config
                    .winter_adaptive_v3
                    .hdo_sensor_entity
                    .clone(),
            )),
        },
        pricing: pricing.clone(),
        control: system_config.control_config.clone(),
        winter_reserve: system_config.strategies_config.winter_reserve.clone(),
    })
}

/// What the PV forecast correction reads (brief H34): the same live PV entities as the backtest
/// (master plus `pv_extra_prefixes`), the master's SOC, grid export and export limit, and the
/// export cap and curtailment settings. `None` when the site cannot be built.
fn pv_correction_site(
    config: &config::AppConfig,
    system_config: &SystemConfig,
) -> Option<fluxion_core::pv_correction::CorrectionSite> {
    let timezone: chrono_tz::Tz = config.system.timezone.as_deref()?.parse().ok()?;
    let master = config
        .inverters
        .iter()
        .find(|inv| inv.topology != "slave")
        .or(config.inverters.first())?;
    let mapper = fluxion_adapters::create_entity_mapper(master.inverter_type);
    let mut pv = vec![mapper.get_pv_power_entity(&master.id)];
    pv.extend(
        config::AppConfig::pv_extra_prefixes(master)
            .iter()
            .map(|prefix| mapper.get_pv_power_entity(prefix)),
    );
    let control = &system_config.control_config;
    let export_cap_w = control
        .export_limit_hard_cap_w
        .map_or(control.maximum_export_power_w, |cap| {
            cap.min(control.maximum_export_power_w)
        });
    Some(fluxion_core::pv_correction::CorrectionSite {
        timezone,
        pv,
        soc: mapper.get_battery_soc_entity(&master.id),
        grid_export: mapper.get_grid_export_power_entity(&master.id)?,
        export_limit: Some(mapper.get_export_limit_entity(&master.id)),
        limits: fluxion_core::pv_correction::CurtailLimits {
            max_soc_pct: control.max_battery_soc,
            export_cap_w,
            export_off_w: control
                .export_limit_min_w
                .max(fluxion_core::pv_correction::EXPORT_OFF_MIN_W),
            export_curtailment_enabled: control.export_curtailment_enabled,
        },
    })
}

fn initialize_and_run() -> Result<()> {
    // Initialize tracing with env filter support
    // Respects RUST_LOG environment variable
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Load configuration with web UI fallback
    let config = config::load_config_with_fallback()?;

    info!("🚀 Starting FluxION - PV Plant Automation (MVP)");
    info!("📋 Configuration Summary:");
    info!("   Inverters: {}", config.inverters.len());
    for inv in &config.inverters {
        info!(
            "     - {} ({}) - {} [{}]",
            inv.id, inv.inverter_type, inv.entity_prefix, inv.topology
        );
    }
    info!("   Spot price entity: {}", config.pricing.spot_price_entity);
    info!(
        "   Use spot prices: buy={}, sell={}",
        config.pricing.use_spot_prices_to_buy, config.pricing.use_spot_prices_to_sell
    );
    info!(
        "   Control: charge {}h, discharge {}h",
        config.control.force_charge_hours, config.control.force_discharge_hours
    );
    info!(
        "   Battery SOC: {}%-{}%",
        config.control.min_battery_soc, config.control.max_battery_soc
    );
    info!("   Max export: {}W", config.control.maximum_export_power_w);
    info!(
        "   Update interval: {}s",
        config.system.update_interval_secs
    );
    info!("   Debug mode: {}", config.system.debug_mode);

    // Initialize Home Assistant client
    let ha_client = if std::env::var("SUPERVISOR_TOKEN").is_ok() {
        info!("🏠 Initializing HA client using Supervisor API...");
        Arc::new(HomeAssistantClient::from_supervisor()?)
    } else {
        info!("🏠 Initializing HA client from configuration...");
        Arc::new(HomeAssistantClient::from_config(
            config.system.ha_base_url.clone(),
            config.system.ha_token.clone(),
        )?)
    };

    // Fetch timezone from Home Assistant and create TimezoneConfig
    let mut config = config;
    let runtime_handle = tokio::runtime::Handle::current();
    let timezone_config =
        if let Ok(timezone) = runtime_handle.block_on(async { ha_client.get_timezone().await }) {
            config.system.timezone = Some(timezone.clone());
            info!("🌍 Using Home Assistant timezone: {}", timezone);
            TimezoneConfig::new(Some(timezone))
        } else {
            warn!("⚠️ Failed to fetch timezone from HA, times will be displayed in UTC");
            TimezoneConfig::default()
        };

    // Detect SolaX vs SolaX Ultra per inverter from HA, before any mapper or history default is
    // built from the configured type (brief H12)
    for inv in config.inverters.iter_mut() {
        inv.inverter_type = runtime_handle.block_on(fluxion_adapters::solax::detect_inverter_type(
            &ha_client,
            &inv.id,
            inv.inverter_type,
        ));
    }

    // Create vendor-specific entity mapper based on config
    // Use inverter type from first inverter (for multi-inverter setups, all should use same type)
    let inverter_type = config
        .inverters
        .first()
        .map(|inv| inv.inverter_type)
        .expect("No inverters configured");
    let mapper = fluxion_adapters::create_entity_mapper(inverter_type);
    info!(
        "📦 Using {} entity mapper",
        mapper.vendor_name().display_name()
    );

    // Create data sources
    let actuation_guard = fluxion_adapters::ha::actuation::ActuationGuardConfig {
        auto_unlock: config.control.auto_unlock_inverter,
        ..Default::default()
    };
    // Read-only PV of parallel units, added to their inverter's PV (never written to)
    let pv_extra_prefixes: std::collections::HashMap<String, Vec<String>> = config
        .inverters
        .iter()
        .map(|inv| (inv.id.clone(), config::AppConfig::pv_extra_prefixes(inv)))
        .filter(|(_, extras)| !extras.is_empty())
        .collect();
    for (id, extras) in &pv_extra_prefixes {
        info!(
            "☀️ Inverter {} PV adds read-only parallel units: {:?}",
            id, extras
        );
    }
    let inverter_adapter = HomeAssistantInverterAdapter::new(ha_client.clone(), mapper)
        .with_guard(actuation_guard)
        .with_pv_extra_prefixes(pv_extra_prefixes);
    // Shared with the pre-wake system, so both use one awaken rate limit
    let actuation_state =
        fluxion_adapters::ha::actuation::ActuationStateResource(inverter_adapter.actuation_state());
    let inverter_source: Arc<dyn fluxion_core::InverterDataSource> = Arc::new(inverter_adapter);
    info!("🔌 Inverter data source: {}", inverter_source.name());

    // Create spot price adapter (always create, but may not be used)
    let spot_adapter = if let Some(tomorrow_entity) = &config.pricing.tomorrow_price_entity {
        info!("💰 Using separate tomorrow sensor: {}", tomorrow_entity);
        CzSpotPriceAdapter::with_tomorrow_sensor(
            ha_client.clone(),
            config.pricing.spot_price_entity.clone(),
            tomorrow_entity.clone(),
        )
    } else {
        CzSpotPriceAdapter::new(ha_client.clone(), config.pricing.spot_price_entity.clone())
    };

    // Get the timezone handle from the spot adapter for later synchronization
    // This allows the timezone_sync_system to update the adapter's timezone
    // when the Home Assistant timezone changes
    let price_adapter_tz_handle = PriceAdapterTimezoneHandle::new(spot_adapter.timezone_handle());
    info!("🌍 Price adapter timezone handle created for HA timezone sync");

    // Price feed health check and OTE fallback (brief H35): status for the dashboard, fallback
    // fetches and the HA notification in a background task, off the scheduling loop
    let price_feed_status = fluxion_core::price_feed::PriceFeedStatusResource::default();
    let price_fallback = {
        let check_time = fluxion_adapters::price_fallback::parse_check_time(
            &config.pricing.tomorrow_prices_check_time,
        )
        .unwrap_or_else(|| {
            tracing::warn!(
                "⚠️ pricing.tomorrow_prices_check_time '{}' is not HH:MM, using {}",
                config.pricing.tomorrow_prices_check_time,
                fluxion_adapters::price_fallback::DEFAULT_CHECK_TIME
            );
            fluxion_adapters::price_fallback::parse_check_time(
                fluxion_adapters::price_fallback::DEFAULT_CHECK_TIME,
            )
            .unwrap_or_default()
        });
        let mut fallback_config = fluxion_adapters::PriceFallbackConfig::new(
            config.pricing.spot_price_entity.clone(),
            check_time,
            config.pricing.ote_fallback_enabled,
            config.pricing.price_alert_notification,
        );
        // The last validation survives restarts (second day of an outage)
        if std::path::Path::new("/data").is_dir() {
            fallback_config.validation_path =
                Some(fluxion_adapters::price_fallback::DEFAULT_VALIDATION_PATH.into());
        }
        match fluxion_adapters::PriceFallback::new(fallback_config, price_feed_status.0.clone()) {
            Ok(fallback) => {
                let fallback = Arc::new(fallback);
                info!(
                    "💰 Price feed check after {} Europe/Prague; OTE fallback {}, HA notification {}",
                    check_time.format("%H:%M"),
                    if config.pricing.ote_fallback_enabled {
                        "on"
                    } else {
                        "off"
                    },
                    if config.pricing.price_alert_notification {
                        "on"
                    } else {
                        "off"
                    }
                );
                fallback.clone().spawn(Some(ha_client.clone()));
                Some(fallback)
            }
            Err(e) => {
                tracing::error!("❌ Price feed check not started: {e:#}");
                None
            }
        }
    };

    // Wrap spot adapter in configurable source that respects use_spot_prices_to_buy/sell flags
    let price_source: Arc<dyn fluxion_core::PriceDataSource> = Arc::new(
        fluxion_adapters::ConfigurablePriceDataSource::new(
            Arc::new(spot_adapter),
            config.pricing.use_spot_prices_to_buy,
            config.pricing.use_spot_prices_to_sell,
            config.pricing.fixed_buy_prices.clone(),
            config.pricing.fixed_sell_prices.clone(),
            config.pricing.spot_sell_fee,
        )
        .with_buy_price_entity(config.pricing.buy_price_entity.clone())
        .with_price_fallback(price_fallback),
    );
    info!("💰 Price data source: {}", price_source.name());

    let history_source: Arc<dyn fluxion_core::traits::ConsumptionHistoryDataSource> = Arc::new(
        fluxion_adapters::HaConsumptionHistoryAdapter::new(ha_client.clone()),
    );
    info!("📊 History data source: {}", history_source.name());

    // Convert AppConfig to SystemConfig for ECS
    let system_config = SystemConfig::from(config.clone());

    // Create shared plugin manager with built-in strategies
    let plugin_manager = create_plugin_manager(
        Some(&system_config.strategies_config),
        &system_config.control_config,
    );
    let plugin_manager = Arc::new(RwLock::new(plugin_manager));
    info!("🔌 Plugin manager initialized with built-in strategies");

    // Configure debug mode
    let debug_config = if config.system.debug_mode {
        fluxion_core::DebugModeConfig::enabled()
    } else {
        fluxion_core::DebugModeConfig::disabled()
    };

    // Configure execution settings (mode change debounce)
    let execution_config =
        fluxion_core::ExecutionConfig::new(config.control.min_mode_change_interval_secs);

    // Create message passing channel for web queries
    let (query_sender, query_channel) = WebQuerySender::new();

    // Create message passing channel for config updates
    let (config_update_sender, config_update_channel) = ConfigUpdateSender::new();

    // Load user control state from persistence
    let user_control_persistence = UserControlPersistence::default_production();
    let mut user_control_state = match user_control_persistence.load() {
        Ok(state) => {
            info!(
                "🎛️ Loaded user control state: enabled={}, disallow_charge={}, disallow_discharge={}, {} fixed slots",
                state.enabled,
                state.disallow_charge,
                state.disallow_discharge,
                state.fixed_time_slots.len()
            );
            state
        }
        Err(e) => {
            warn!(
                "⚠️ Failed to load user control state, using defaults: {}",
                e
            );
            Default::default()
        }
    };
    // The config's default_battery_mode seeds the dashboard only while the user has never set it
    if user_control_state.seed_default_mode_from_config(
        system_config.control_config.default_battery_mode,
        system_config.control_config.min_battery_soc,
    ) {
        info!(
            "🎛️ Default mode seeded from config: backup, minimum SOC {}%",
            user_control_state.backup_min_soc_pct
        );
    }

    // Create channel for user control updates from web to ECS
    let (user_control_update_sender, user_control_update_channel) = UserControlUpdateSender::new();

    // Create user control API state for web server
    let user_control_api_state = UserControlApiState::new(
        user_control_state.clone(),
        user_control_persistence
            .path()
            .to_string_lossy()
            .to_string(),
        Some(user_control_update_sender),
    )
    .with_max_battery_soc(system_config.control_config.max_battery_soc);

    // Initialize i18n with configured language from config
    let language = config.system.language;
    let i18n = Arc::new(I18n::new(language).expect("Failed to initialize i18n"));
    info!(
        "🌍 Initialized i18n with language: {}",
        language.display_name()
    );

    // Spawn heartbeat client if enabled
    if config.server_heartbeat.enabled {
        heartbeat_client::spawn_heartbeat_task(
            config.server_heartbeat.clone(),
            query_sender.clone(),
        );
    }

    // Spawn web server on tokio runtime
    info!("🌐 Starting web server on port 8099...");
    let i18n_for_server = i18n.clone();
    // Serialize SystemConfig (not AppConfig) for the web API
    // This ensures consistency with what the ECS expects
    let config_json = serde_json::to_value(&system_config).unwrap_or_else(|e| {
        warn!("Failed to serialize config to JSON: {e}");
        serde_json::json!({})
    });
    let config_sender_for_web = config_update_sender.clone();
    // Backtest on this site's own HA history (read-only GETs on the web runtime, brief H28)
    let pv_correction_site = fluxion_core::pv_correction::PvCorrectionSiteResource(
        pv_correction_site(&config, &system_config),
    );
    let backtest_state = fluxion_web::BacktestState::new(
        Arc::new(fluxion_adapters::ha::HaBacktestHistory::new(
            ha_client.clone(),
        )),
        backtest_site(&config, &system_config),
        i18n.clone(),
    )
    .with_user_control(Arc::clone(&user_control_api_state.state));
    let plugin_api_state = PluginApiState::new(plugin_manager.clone());
    let remote_access_state =
        RemoteAccessApiState::new(std::path::Path::new("./data"), 8099, "FluxION".to_string());
    tokio::spawn(async move {
        if let Err(e) = fluxion_web::start_web_server(
            query_sender,
            i18n_for_server,
            8099,
            config_json,
            Some(config_sender_for_web),
            Some(backtest_state),   // Backtest on HA history
            Some(plugin_api_state), // Plugin API with shared PluginManager
            Some(fluxion_web::ScheduledExportConfig::default()), // Daily export at 23:55 for debugging
            Some(user_control_api_state),                        // User control API state
            Some(remote_access_state),                           // Remote access pairing API
        )
        .await
        {
            tracing::error!("❌ Web server failed: {}", e);
        }
    });

    // Create Bevy app with full configuration
    info!("🎮 Starting ECS application...");

    let levers_sync_config = fluxion_adapters::ha::levers::LeversSyncConfig {
        enabled: config.levers.enabled,
        ..Default::default()
    };

    // Dashboard entities, the lock guard and the manual-mode poll use one HA prefix:
    // the first inverter FluxION controls (never a slave), not the "solax" default
    let ha_config = fluxion_adapters::ha::plugin::HaConfig {
        inverter_prefix: config
            .inverters
            .iter()
            .find(|inv| inv.topology != "slave")
            .or(config.inverters.first())
            .map_or_else(|| "solax".to_string(), |inv| inv.entity_prefix.clone()),
        ..Default::default()
    };
    info!("   HA entity prefix: {}", ha_config.inverter_prefix);

    let mut app = App::new();
    app
        // Add TaskPoolPlugin to initialize async task pools
        .add_plugins(TaskPoolPlugin::default())
        // Add ScheduleRunnerPlugin for headless operation
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_millis(100)))
        .add_plugins((FluxionCorePlugin, HaPlugin))
        .insert_resource(ha_config)
        .insert_resource(levers_sync_config)
        .insert_resource(config)
        .insert_resource(system_config)
        .insert_resource(debug_config)
        .insert_resource(execution_config)
        .insert_resource(query_channel)
        .insert_resource(config_update_channel)
        .insert_resource(timezone_config)
        .insert_resource(pv_correction_site)
        .insert_resource(HaClientResource(ha_client))
        .insert_resource(actuation_state)
        .insert_resource(price_adapter_tz_handle)
        .insert_resource(fluxion_core::InverterDataSourceResource(inverter_source))
        .insert_resource(fluxion_core::PriceDataSourceResource(price_source))
        .insert_resource(price_feed_status)
        .insert_resource(fluxion_core::ConsumptionHistoryDataSourceResource(
            history_source,
        ))
        .insert_resource(PluginManagerResource(plugin_manager))
        .insert_resource(UserControlResource::new(user_control_state))
        .insert_resource(user_control_update_channel)
        .init_resource::<fluxion_core::async_systems::BackupDischargeMinSoc>()
        .init_resource::<fluxion_core::async_systems::HdoScheduleData>()
        .init_resource::<fluxion_core::resources::GlobalHdoCache>();

    info!("✅ Starting main loop...");

    // Run the app with Bevy's built-in runner
    // This properly handles all schedules (Startup, Update, etc.)
    app.run();

    Ok(())
}
