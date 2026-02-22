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
    let inverter_source: Arc<dyn fluxion_core::InverterDataSource> =
        Arc::new(HomeAssistantInverterAdapter::new(ha_client.clone(), mapper));
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

    // Wrap spot adapter in configurable source that respects use_spot_prices_to_buy/sell flags
    let price_source: Arc<dyn fluxion_core::PriceDataSource> =
        Arc::new(fluxion_adapters::ConfigurablePriceDataSource::new(
            Arc::new(spot_adapter),
            config.pricing.use_spot_prices_to_buy,
            config.pricing.use_spot_prices_to_sell,
            config.pricing.fixed_buy_prices.clone(),
            config.pricing.fixed_sell_prices.clone(),
            config.pricing.spot_sell_fee,
        ));
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
    let user_control_state = match user_control_persistence.load() {
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
    );

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
            Some(std::path::PathBuf::from("/home/daniel/Repositories/solare/fluxion/fluxion/crates/fluxion-integration-tests/solax_data.db")), // Backtest DB path - set to enable backtest feature
            Some(plugin_api_state), // Plugin API with shared PluginManager
            Some(fluxion_web::ScheduledExportConfig::default()), // Daily export at 23:55 for debugging
            Some(user_control_api_state), // User control API state
            Some(remote_access_state), // Remote access pairing API
        )
        .await
        {
            tracing::error!("❌ Web server failed: {}", e);
        }
    });

    // Create Bevy app with full configuration
    info!("🎮 Starting ECS application...");

    let mut app = App::new();
    app
        // Add TaskPoolPlugin to initialize async task pools
        .add_plugins(TaskPoolPlugin::default())
        // Add ScheduleRunnerPlugin for headless operation
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_millis(100)))
        .add_plugins((FluxionCorePlugin, HaPlugin))
        .insert_resource(config)
        .insert_resource(system_config)
        .insert_resource(debug_config)
        .insert_resource(execution_config)
        .insert_resource(query_channel)
        .insert_resource(config_update_channel)
        .insert_resource(timezone_config)
        .insert_resource(HaClientResource(ha_client))
        .insert_resource(price_adapter_tz_handle)
        .insert_resource(fluxion_core::InverterDataSourceResource(inverter_source))
        .insert_resource(fluxion_core::PriceDataSourceResource(price_source))
        .insert_resource(fluxion_core::ConsumptionHistoryDataSourceResource(
            history_source,
        ))
        .insert_resource(PluginManagerResource(plugin_manager))
        .insert_resource(UserControlResource::new(user_control_state))
        .insert_resource(user_control_update_channel)
        .init_resource::<fluxion_core::async_systems::BackupDischargeMinSoc>()
        .init_resource::<fluxion_core::async_systems::HdoScheduleData>();

    info!("✅ Starting main loop...");

    // Run the app with Bevy's built-in runner
    // This properly handles all schedules (Startup, Update, etc.)
    app.run();

    Ok(())
}
