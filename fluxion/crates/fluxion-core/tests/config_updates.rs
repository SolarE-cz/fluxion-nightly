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
    ConfigSection, ConfigUpdateEvent, ConfigUpdateSender, OperationSchedule, SpotPriceData,
    SystemConfig, TimeBlockPrice,
};
use fluxion_i18n::Language;
use std::collections::HashSet;

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
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            grid_distribution_fee_czk: 1.2,
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
        },
        strategies_config: Default::default(),
    };

    // Create config update channel
    let (config_sender, config_channel) = ConfigUpdateSender::new();

    // Insert resources
    app.insert_resource(initial_config.clone());
    app.insert_resource(config_channel);

    // Create some dummy price data
    let price_data = SpotPriceData {
        time_block_prices: vec![
            TimeBlockPrice {
                block_start: chrono::Utc::now(),
                duration_minutes: 15,
                price_czk_per_kwh: 3.0,
            },
            TimeBlockPrice {
                block_start: chrono::Utc::now() + chrono::Duration::minutes(15),
                duration_minutes: 15,
                price_czk_per_kwh: 5.0,
            },
        ],
        block_duration_minutes: 15,
        fetched_at: chrono::Utc::now(),
        ha_last_updated: chrono::Utc::now(),
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
            use_spot_prices_to_buy: true,
            use_spot_prices_to_sell: true,
            fixed_buy_price_czk: fluxion_core::PriceSchedule::Flat(4.0),
            fixed_sell_price_czk: fluxion_core::PriceSchedule::Flat(2.0),
            spot_buy_fee_czk: 0.5,
            spot_sell_fee_czk: 0.5,
            grid_distribution_fee_czk: 1.2,
        },
        control_config: Default::default(),
        system_config: fluxion_core::SystemSettingsConfig {
            update_interval_secs: 60,
            debug_mode: true,
            display_currency: fluxion_core::Currency::CZK,
            language: Language::English,
            timezone: None,
        },
        strategies_config: Default::default(),
    };

    // Create config update channel
    let (config_sender, config_channel) = ConfigUpdateSender::new();

    // Insert resources
    app.insert_resource(initial_config.clone());
    app.insert_resource(config_channel);

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
