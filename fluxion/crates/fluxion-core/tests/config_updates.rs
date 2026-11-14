// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided \"AS IS\", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

//! Integration tests for configuration update events
//!
//! This tests the full flow: Web UI -> ConfigUpdateEvent -> ECS -> SystemConfig update -> Schedule recalculation

use bevy_app::App;
use bevy_ecs::system::RunSystemOnce;
use fluxion_core::{
    ConfigSection, ConfigUpdateEvent, ConfigUpdateSender, DebugModeConfig, OperationSchedule,
    PluginManagerResource, SpotPriceData, SystemConfig, TimeBlockPrice,
    components::ConsumptionHistory, plugin_adapters::create_plugin_manager,
};
use fluxion_i18n::Language;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;

#[test]
fn test_config_update_flow() {
    // Create a minimal Bevy app for testing
    let mut app = App::new();

    // Create initial system config
    let mut initial_config = SystemConfig {
        inverters: vec![],
        pricing_config: fluxion_core::PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            buy_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        },
        control_config: fluxion_core::ControlConfig {
            force_charge_hours: 2,
            force_discharge_hours: 1,
            min_battery_soc: 10.0,
            max_battery_soc: 90.0,
            ..Default::default()
        },
        system_config: fluxion_core::SystemSettingsConfig {
            update_interval_secs: 60,
            debug_mode: true,
            display_currency: fluxion_core::Currency::CZK,
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

    // Create config update channel
    let (config_sender, config_channel) = ConfigUpdateSender::new();

    // Create plugin manager for scheduling
    let plugin_manager = create_plugin_manager(
        Some(&initial_config.strategies_config),
        &initial_config.control_config,
    );
    let plugin_manager_res = PluginManagerResource(Arc::new(RwLock::new(plugin_manager)));

    // Insert resources
    app.insert_resource(initial_config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(plugin_manager_res);

    // Create some dummy price data
    let price_data = SpotPriceData {
        time_block_prices: vec![
            TimeBlockPrice {
                block_start: chrono::Utc::now(),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
                effective_price_czk_per_kwh: 3.0,
                spot_sell_price_czk_per_kwh: None,
                buy_price_czk_per_kwh: None,
            },
            TimeBlockPrice {
                block_start: chrono::Utc::now() + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
                effective_price_czk_per_kwh: 5.0,
                spot_sell_price_czk_per_kwh: None,
                buy_price_czk_per_kwh: None,
            },
        ],
        block_duration_minutes: 15,
        fetched_at: chrono::Utc::now(),
        ha_last_updated: chrono::Utc::now(),
        fallback_block_starts: Vec::new(),
    };
    app.world_mut().spawn(price_data);

    // Verify initial config
    let config_before = app.world().resource::<SystemConfig>();
    assert_eq!(config_before.control_config.force_charge_hours, 2);
    assert_eq!(config_before.control_config.max_battery_soc, 90.0);

    // Modify config
    initial_config.control_config.force_charge_hours = 4;
    initial_config.control_config.max_battery_soc = 100.0;

    // Send config update event
    let config_json = serde_json::to_value(&initial_config).expect("Failed to serialize config");
    let mut changed_sections = HashSet::new();
    changed_sections.insert(ConfigSection::Control);

    let event = ConfigUpdateEvent::new(config_json, changed_sections);
    config_sender
        .send_update(event)
        .expect("Failed to send config update");

    // Run the config event handler system
    app.world_mut()
        .run_system_once(fluxion_core::async_systems::config_event_handler)
        .expect("Failed to run config event handler");

    // Verify config was updated
    let config_after = app.world().resource::<SystemConfig>();
    assert_eq!(config_after.control_config.force_charge_hours, 4);
    assert_eq!(config_after.control_config.max_battery_soc, 100.0);

    // Verify schedule was created/updated (if price data exists)
    let mut schedule_query = app.world_mut().query::<&OperationSchedule>();
    let schedule_count = schedule_query.iter(app.world()).count();

    // Schedule should exist after config update with Control section changed
    assert!(
        schedule_count > 0,
        "Schedule should be created after config update"
    );
}

#[test]
fn test_config_update_no_schedule_recalc_when_not_needed() {
    // Create a minimal Bevy app for testing
    let mut app = App::new();

    // Create initial system config
    let mut initial_config = SystemConfig {
        inverters: vec![],
        pricing_config: fluxion_core::PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            buy_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        },
        control_config: Default::default(),
        system_config: fluxion_core::SystemSettingsConfig {
            update_interval_secs: 60,
            debug_mode: true,
            display_currency: fluxion_core::Currency::CZK,
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

    // Create config update channel
    let (config_sender, config_channel) = ConfigUpdateSender::new();

    // Create plugin manager for scheduling
    let plugin_manager = create_plugin_manager(
        Some(&initial_config.strategies_config),
        &initial_config.control_config,
    );
    let plugin_manager_res = PluginManagerResource(Arc::new(RwLock::new(plugin_manager)));

    // Insert resources
    app.insert_resource(initial_config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(plugin_manager_res);

    // Modify only system settings (not Control/Pricing/Strategies)
    initial_config.system_config.debug_mode = false;
    initial_config.system_config.language = Language::Czech;

    // Send config update event with only System section changed
    let config_json = serde_json::to_value(&initial_config).expect("Failed to serialize config");
    let mut changed_sections = HashSet::new();
    changed_sections.insert(ConfigSection::System);

    let event = ConfigUpdateEvent::new(config_json, changed_sections);
    config_sender
        .send_update(event)
        .expect("Failed to send config update");

    // Run the config event handler system
    app.world_mut()
        .run_system_once(fluxion_core::async_systems::config_event_handler)
        .expect("Failed to run config event handler");

    // Verify config was updated
    let config_after = app.world().resource::<SystemConfig>();
    assert!(!config_after.system_config.debug_mode);
    assert_eq!(config_after.system_config.language, Language::Czech);

    // Verify schedule was NOT created (no price data and System section doesn't trigger recalc)
    let mut schedule_query = app.world_mut().query::<&OperationSchedule>();
    let schedule_count = schedule_query.iter(app.world()).count();

    assert_eq!(
        schedule_count, 0,
        "Schedule should not be created for System config changes"
    );
}

/// Brief H37: a config update cannot change `control.mode_control_enabled` or
/// `control.vpp_direct_enabled` at runtime (the inverter adapter took them at startup); the rest of
/// the update still applies
#[test]
fn test_config_update_keeps_running_mode_control() {
    let mut app = App::new();
    let mut config = SystemConfig {
        inverters: vec![],
        pricing_config: fluxion_core::PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            buy_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        },
        control_config: fluxion_core::ControlConfig {
            max_battery_soc: 90.0,
            mode_control_enabled: false,
            ..Default::default()
        },
        system_config: fluxion_core::SystemSettingsConfig {
            update_interval_secs: 60,
            debug_mode: true,
            display_currency: fluxion_core::Currency::CZK,
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
    let (config_sender, config_channel) = ConfigUpdateSender::new();
    let plugin_manager =
        create_plugin_manager(Some(&config.strategies_config), &config.control_config);
    app.insert_resource(config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(PluginManagerResource(Arc::new(RwLock::new(plugin_manager))));

    // Both ways: off stays off, and after a restart with it on, on stays on
    for (running, requested) in [(false, true), (true, false)] {
        app.world_mut()
            .resource_mut::<SystemConfig>()
            .control_config
            .mode_control_enabled = running;
        config.control_config.mode_control_enabled = requested;
        config.control_config.max_battery_soc = if running { 95.0 } else { 100.0 };
        let mut changed_sections = HashSet::new();
        changed_sections.insert(ConfigSection::Control);
        config_sender
            .send_update(ConfigUpdateEvent::new(
                serde_json::to_value(&config).unwrap(),
                changed_sections,
            ))
            .unwrap();
        app.world_mut()
            .run_system_once(fluxion_core::async_systems::config_event_handler)
            .unwrap();

        let after = app.world().resource::<SystemConfig>();
        assert_eq!(after.control_config.mode_control_enabled, running);
        assert_eq!(
            after.control_config.max_battery_soc,
            config.control_config.max_battery_soc
        );
    }

    // The same for `control.vpp_direct_enabled` (brief H37 pattern, VPP direct control)
    for (running, requested) in [(false, true), (true, false)] {
        app.world_mut()
            .resource_mut::<SystemConfig>()
            .control_config
            .vpp_direct_enabled = running;
        config.control_config.vpp_direct_enabled = requested;
        config.control_config.max_battery_soc = if running { 95.0 } else { 100.0 };
        let mut changed_sections = HashSet::new();
        changed_sections.insert(ConfigSection::Control);
        config_sender
            .send_update(ConfigUpdateEvent::new(
                serde_json::to_value(&config).unwrap(),
                changed_sections,
            ))
            .unwrap();
        app.world_mut()
            .run_system_once(fluxion_core::async_systems::config_event_handler)
            .unwrap();

        let after = app.world().resource::<SystemConfig>();
        assert_eq!(after.control_config.vpp_direct_enabled, running);
        assert_eq!(
            after.control_config.max_battery_soc,
            config.control_config.max_battery_soc
        );
    }
}

/// A config with the given control section and the defaults for everything else
fn rebuild_test_config(control_config: fluxion_core::ControlConfig) -> SystemConfig {
    SystemConfig {
        inverters: vec![],
        pricing_config: fluxion_core::PricingConfig {
            spot_price_entity: "sensor.spot_price".to_string(),
            tomorrow_price_entity: None,
            buy_price_entity: None,
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        },
        control_config,
        system_config: fluxion_core::SystemSettingsConfig {
            update_interval_secs: 60,
            debug_mode: true,
            display_currency: fluxion_core::Currency::CZK,
            language: Language::English,
            timezone: None,
            display_timezone: None,
        },
        strategies_config: Default::default(),
        history: Default::default(),
        solar_forecast: Default::default(),
        remote_access: Default::default(),
        notifications: Default::default(),
    }
}

/// Sends one config update for `sections` and runs the handler
fn send_config_update(
    app: &mut App,
    sender: &ConfigUpdateSender,
    config: &SystemConfig,
    sections: &[ConfigSection],
) {
    let changed_sections: HashSet<ConfigSection> = sections.iter().copied().collect();
    sender
        .send_update(ConfigUpdateEvent::new(
            serde_json::to_value(config).unwrap(),
            changed_sections,
        ))
        .unwrap();
    app.world_mut()
        .run_system_once(fluxion_core::async_systems::config_event_handler)
        .unwrap();
}

/// A block of the given start, 15 minutes long, with one price
fn rebuild_test_block(start: chrono::DateTime<chrono::Utc>, price: f32) -> serde_json::Value {
    serde_json::json!({
        "block_start": start,
        "duration_minutes": 15,
        "price_czk_per_kwh": price,
        "effective_price_czk_per_kwh": price,
    })
}

/// An evaluation request for the planner: a day of cheap then dear blocks, battery at 50 %
fn rebuild_test_request() -> fluxion_plugins::EvaluationRequest {
    let now = chrono::Utc::now();
    let blocks: Vec<serde_json::Value> = (0..96)
        .map(|i| {
            let price = if i < 48 { 1.0 } else { 6.0 };
            rebuild_test_block(now + chrono::Duration::minutes(15 * i), price)
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "block": blocks[0],
        "battery": {
            "current_soc_percent": 50.0, "capacity_kwh": 18.0, "max_charge_rate_kw": 10.0,
            "min_soc_percent": 10.0, "max_soc_percent": 100.0, "efficiency": 0.95,
            "wear_cost_czk_per_kwh": 0.1,
        },
        "forecast": {"solar_kwh": 0.0, "consumption_kwh": 0.5, "grid_export_price_czk_per_kwh": 2.0},
        "all_blocks": blocks,
        "historical": {"grid_import_today_kwh": null, "consumption_today_kwh": null},
        "backup_discharge_min_soc": 10.0,
    }))
    .unwrap()
}

/// Test A: a Control update reaches the built-in planner, which decides from the new capacity
#[test]
fn test_config_update_rebuilds_builtin_planner_config() {
    let mut app = App::new();
    let mut config = rebuild_test_config(fluxion_core::ControlConfig {
        battery_capacity_kwh: 18.0,
        ..Default::default()
    });
    let (config_sender, config_channel) = ConfigUpdateSender::new();
    let plugin_manager =
        create_plugin_manager(Some(&config.strategies_config), &config.control_config);
    app.insert_resource(config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(PluginManagerResource(Arc::new(RwLock::new(plugin_manager))));

    let request = rebuild_test_request();
    let decide = |app: &App| {
        format!(
            "{:?}",
            app.world()
                .resource::<PluginManagerResource>()
                .0
                .read()
                .evaluate(&request)
        )
    };
    let before = decide(&app);

    config.control_config.battery_capacity_kwh = 12.0;
    send_config_update(&mut app, &config_sender, &config, &[ConfigSection::Control]);

    let fresh = format!(
        "{:?}",
        create_plugin_manager(Some(&config.strategies_config), &config.control_config)
            .evaluate(&request)
    );
    assert_ne!(
        before, fresh,
        "the test must discriminate: the capacity change must change the decision"
    );
    assert_eq!(decide(&app), fresh);
}

/// Test B: an external plugin registered before a Control update is still listed after it
#[test]
fn test_config_update_keeps_external_plugins() {
    struct ExternalPlugin;

    impl fluxion_plugins::Plugin for ExternalPlugin {
        fn name(&self) -> &str {
            "External-Test-Plugin"
        }

        fn priority(&self) -> u8 {
            50
        }

        fn is_enabled(&self) -> bool {
            true
        }

        fn evaluate(
            &self,
            _request: &fluxion_plugins::EvaluationRequest,
        ) -> anyhow::Result<fluxion_plugins::BlockDecision> {
            anyhow::bail!("not evaluated in this test")
        }
    }

    let mut app = App::new();
    let config = rebuild_test_config(Default::default());
    let (config_sender, config_channel) = ConfigUpdateSender::new();
    let mut plugin_manager =
        create_plugin_manager(Some(&config.strategies_config), &config.control_config);
    plugin_manager.register(Arc::new(ExternalPlugin));
    app.insert_resource(config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(PluginManagerResource(Arc::new(RwLock::new(plugin_manager))));

    let names_of = |app: &App| -> Vec<String> {
        app.world()
            .resource::<PluginManagerResource>()
            .0
            .read()
            .list_plugins()
            .iter()
            .map(|(name, _, _)| name.to_string())
            .collect()
    };
    let before = names_of(&app);

    send_config_update(&mut app, &config_sender, &config, &[ConfigSection::Control]);

    let after = names_of(&app);
    assert!(after.iter().any(|n| n == "External-Test-Plugin"));
    assert_eq!(
        after.len(),
        before.len(),
        "built-ins are replaced, not added"
    );
}

/// Test C: a Pricing-only update does not rebuild the built-ins, so a web UI override survives it
#[test]
fn test_pricing_update_does_not_rebuild_builtins() {
    let mut app = App::new();
    let config = rebuild_test_config(Default::default());
    let (config_sender, config_channel) = ConfigUpdateSender::new();
    let plugin_manager =
        create_plugin_manager(Some(&config.strategies_config), &config.control_config);
    app.insert_resource(config.clone());
    app.insert_resource(config_channel);
    app.insert_resource(DebugModeConfig::default());
    app.insert_resource(ConsumptionHistory::default());
    app.insert_resource(PluginManagerResource(Arc::new(RwLock::new(plugin_manager))));

    let winter_reserve_enabled = |app: &App| -> bool {
        app.world()
            .resource::<PluginManagerResource>()
            .0
            .read()
            .list_plugins()
            .iter()
            .find(|(name, _, _)| *name == "Winter-Reserve")
            .expect("Winter-Reserve is registered")
            .2
    };
    assert!(winter_reserve_enabled(&app));
    app.world()
        .resource::<PluginManagerResource>()
        .0
        .write()
        .set_enabled("Winter-Reserve", false);

    send_config_update(&mut app, &config_sender, &config, &[ConfigSection::Pricing]);

    assert!(!winter_reserve_enabled(&app));
}
