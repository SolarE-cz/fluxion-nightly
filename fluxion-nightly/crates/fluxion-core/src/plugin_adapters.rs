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

//! Plugin adapters for wrapping Fluxion strategies as plugins.

use crate::strategy::{
    EconomicStrategy, EvaluationContext,
    winter_adaptive::{WinterAdaptiveConfig, WinterAdaptiveStrategy},
    winter_adaptive_v2::{WinterAdaptiveV2Config, WinterAdaptiveV2Strategy},
};
use fluxion_plugins::{
    BlockDecision, EvaluationRequest, Plugin, PluginManager,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::pricing::TimeBlockPrice;
use std::sync::Arc;

/// Adapter that wraps WinterAdaptiveStrategy as a Plugin
pub struct WinterAdaptivePlugin {
    strategy: WinterAdaptiveStrategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptivePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptivePlugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptivePlugin {
    /// Create a new Winter Adaptive V1 plugin
    pub fn new(config: WinterAdaptiveConfig, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveStrategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptivePlugin {
    fn name(&self) -> &str {
        self.strategy.name()
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn is_enabled(&self) -> bool {
        self.strategy.is_enabled()
    }

    fn evaluate(&self, request: &EvaluationRequest) -> anyhow::Result<BlockDecision> {
        let (price_block, all_blocks) = convert_request(request);

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &self.control_config,
            current_battery_soc: request.battery.current_soc_percent,
            solar_forecast_kwh: request.forecast.solar_kwh,
            consumption_forecast_kwh: request.forecast.consumption_kwh,
            grid_export_price_czk_per_kwh: request.forecast.grid_export_price_czk_per_kwh,
            all_price_blocks: Some(&all_blocks),
            backup_discharge_min_soc: request.backup_discharge_min_soc,
            grid_import_today_kwh: request.historical.grid_import_today_kwh,
            consumption_today_kwh: request.historical.consumption_today_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(eval.net_profit_czk),
            decision_uid: eval.decision_uid,
        })
    }
}

/// Adapter that wraps WinterAdaptiveV2Strategy as a Plugin
pub struct WinterAdaptiveV2Plugin {
    strategy: WinterAdaptiveV2Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV2Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV2Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV2Plugin {
    /// Create a new Winter Adaptive V2 plugin
    pub fn new(config: WinterAdaptiveV2Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV2Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV2Plugin {
    fn name(&self) -> &str {
        self.strategy.name()
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn is_enabled(&self) -> bool {
        self.strategy.is_enabled()
    }

    fn evaluate(&self, request: &EvaluationRequest) -> anyhow::Result<BlockDecision> {
        let (price_block, all_blocks) = convert_request(request);

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &self.control_config,
            current_battery_soc: request.battery.current_soc_percent,
            solar_forecast_kwh: request.forecast.solar_kwh,
            consumption_forecast_kwh: request.forecast.consumption_kwh,
            grid_export_price_czk_per_kwh: request.forecast.grid_export_price_czk_per_kwh,
            all_price_blocks: Some(&all_blocks),
            backup_discharge_min_soc: request.backup_discharge_min_soc,
            grid_import_today_kwh: request.historical.grid_import_today_kwh,
            consumption_today_kwh: request.historical.consumption_today_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(eval.net_profit_czk),
            decision_uid: eval.decision_uid,
        })
    }
}

/// Convert an EvaluationRequest to the types needed by strategies
fn convert_request(request: &EvaluationRequest) -> (TimeBlockPrice, Vec<TimeBlockPrice>) {
    let price_block = TimeBlockPrice {
        block_start: request.block.block_start,
        duration_minutes: request.block.duration_minutes,
        price_czk_per_kwh: request.block.price_czk_per_kwh,
    };

    let all_blocks: Vec<TimeBlockPrice> = request
        .all_blocks
        .iter()
        .map(|b| TimeBlockPrice {
            block_start: b.block_start,
            duration_minutes: b.duration_minutes,
            price_czk_per_kwh: b.price_czk_per_kwh,
        })
        .collect();

    (price_block, all_blocks)
}

/// Initialize a PluginManager with the default built-in strategies.
///
/// This function registers the built-in Rust strategies (Winter Adaptive V1, V2)
/// into an existing PluginManager. Use this when you need to initialize a shared
/// manager that may also receive external plugin registrations.
///
/// # Arguments
/// * `manager` - The PluginManager to initialize with built-in strategies
/// * `strategies_config` - Optional strategies configuration (uses defaults if None)
/// * `control_config` - Control configuration for battery parameters
pub fn init_plugin_manager(
    manager: &mut PluginManager,
    strategies_config: Option<&fluxion_types::config::StrategiesConfigCore>,
    control_config: &ControlConfig,
) {
    // Create Winter Adaptive V1 config from core config or defaults
    let mut v1_config = WinterAdaptiveConfig::default();
    if let Some(sc) = strategies_config {
        v1_config.enabled = sc.winter_adaptive.enabled;
        v1_config.priority = sc.winter_adaptive.priority;
        v1_config.ema_period_days = sc.winter_adaptive.ema_period_days;
        v1_config.min_solar_percentage = sc.winter_adaptive.min_solar_percentage;
        v1_config.daily_charging_target_soc = sc.winter_adaptive.daily_charging_target_soc;
        v1_config.conservation_threshold_soc = sc.winter_adaptive.conservation_threshold_soc;
        v1_config.top_expensive_blocks = sc.winter_adaptive.top_expensive_blocks;
        v1_config.tomorrow_preservation_threshold = sc.winter_adaptive.tomorrow_preservation_threshold;
        v1_config.grid_export_price_threshold = sc.winter_adaptive.grid_export_price_threshold;
        v1_config.min_soc_for_export = sc.winter_adaptive.min_soc_for_export;
        v1_config.export_trigger_multiplier = sc.winter_adaptive.export_trigger_multiplier;
        v1_config.negative_price_handling_enabled = sc.winter_adaptive.negative_price_handling_enabled;
        v1_config.charge_on_negative_even_if_full = sc.winter_adaptive.charge_on_negative_even_if_full;
    }

    // Create Winter Adaptive V2 config from core config or defaults
    let mut v2_config = WinterAdaptiveV2Config::default();
    if let Some(sc) = strategies_config {
        v2_config.enabled = sc.winter_adaptive_v2.enabled;
        v2_config.priority = sc.winter_adaptive_v2.priority;
    }

    // Register Winter Adaptive V1
    let v1_plugin = WinterAdaptivePlugin::new(v1_config, control_config.clone());
    manager.register(Arc::new(v1_plugin));

    // Register Winter Adaptive V2
    let v2_plugin = WinterAdaptiveV2Plugin::new(v2_config, control_config.clone());
    manager.register(Arc::new(v2_plugin));
}

/// Create a PluginManager with the default strategies registered.
///
/// This is a convenience function that creates a new PluginManager and initializes
/// it with the built-in strategies. Use this for standalone/testing scenarios.
/// For shared plugin managers, use `init_plugin_manager` instead.
///
/// # Arguments
/// * `strategies_config` - Optional strategies configuration (uses defaults if None)
/// * `control_config` - Control configuration for battery parameters
///
/// # Returns
/// A new PluginManager with built-in strategies registered
pub fn create_plugin_manager(
    strategies_config: Option<&fluxion_types::config::StrategiesConfigCore>,
    control_config: &ControlConfig,
) -> PluginManager {
    let mut manager = PluginManager::new();
    init_plugin_manager(&mut manager, strategies_config, control_config);
    manager
}
