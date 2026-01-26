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
    winter_adaptive_v3::{WinterAdaptiveV3Config, WinterAdaptiveV3Strategy},
    winter_adaptive_v4::{WinterAdaptiveV4Config, WinterAdaptiveV4Strategy},
    winter_adaptive_v5::{WinterAdaptiveV5Config, WinterAdaptiveV5Strategy},
    winter_adaptive_v7::{WinterAdaptiveV7Config, WinterAdaptiveV7Strategy},
    winter_adaptive_v8::{WinterAdaptiveV8Config, WinterAdaptiveV8Strategy},
    winter_adaptive_v9::{WinterAdaptiveV9Config, WinterAdaptiveV9Strategy},
};
use fluxion_plugins::{BlockDecision, EvaluationRequest, Plugin, PluginManager};
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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

/// Adapter that wraps WinterAdaptiveV3Strategy as a Plugin
pub struct WinterAdaptiveV3Plugin {
    strategy: WinterAdaptiveV3Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV3Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV3Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV3Plugin {
    /// Create a new Winter Adaptive V3 plugin
    pub fn new(config: WinterAdaptiveV3Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV3Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV3Plugin {
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
        // HDO cache is now managed centrally, no need to update per-strategy caches

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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

/// Adapter that wraps WinterAdaptiveV4Strategy as a Plugin
pub struct WinterAdaptiveV4Plugin {
    strategy: WinterAdaptiveV4Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV4Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV4Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV4Plugin {
    /// Create a new Winter Adaptive V4 plugin
    pub fn new(config: WinterAdaptiveV4Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV4Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV4Plugin {
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
        // HDO cache is now managed centrally, no need to update per-strategy caches

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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

// ============================================================================
// Winter Adaptive V5 Plugin
// ============================================================================

/// Adapter that wraps WinterAdaptiveV5Strategy as a Plugin
pub struct WinterAdaptiveV5Plugin {
    strategy: WinterAdaptiveV5Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV5Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV5Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV5Plugin {
    /// Create a new Winter Adaptive V5 plugin
    pub fn new(config: WinterAdaptiveV5Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV5Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV5Plugin {
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
        // HDO cache is now managed centrally, no need to update per-strategy caches

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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

// ============================================================================
// Winter Adaptive V7 Plugin
// ============================================================================

/// Adapter that wraps WinterAdaptiveV7Strategy as a Plugin
pub struct WinterAdaptiveV7Plugin {
    strategy: WinterAdaptiveV7Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV7Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV7Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV7Plugin {
    /// Create a new Winter Adaptive V7 plugin
    pub fn new(config: WinterAdaptiveV7Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV7Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV7Plugin {
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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

// ============================================================================
// Winter Adaptive V8 Plugin
// ============================================================================

/// Adapter that wraps WinterAdaptiveV8Strategy as a Plugin
pub struct WinterAdaptiveV8Plugin {
    strategy: WinterAdaptiveV8Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV8Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV8Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV8Plugin {
    /// Create a new Winter Adaptive V8 plugin
    pub fn new(config: WinterAdaptiveV8Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV8Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV8Plugin {
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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
            decision_uid: eval.decision_uid,
        })
    }
}

// ============================================================================
// Winter Adaptive V9 Plugin
// ============================================================================

/// Adapter that wraps WinterAdaptiveV9Strategy as a Plugin
pub struct WinterAdaptiveV9Plugin {
    strategy: WinterAdaptiveV9Strategy,
    priority: u8,
    control_config: ControlConfig,
}

impl std::fmt::Debug for WinterAdaptiveV9Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinterAdaptiveV9Plugin")
            .field("priority", &self.priority)
            .finish()
    }
}

impl WinterAdaptiveV9Plugin {
    /// Create a new Winter Adaptive V9 plugin
    pub fn new(config: WinterAdaptiveV9Config, control_config: ControlConfig) -> Self {
        Self {
            priority: config.priority,
            strategy: WinterAdaptiveV9Strategy::new(config),
            control_config,
        }
    }
}

impl Plugin for WinterAdaptiveV9Plugin {
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
            solar_forecast_total_today_kwh: request.solar_forecast_total_today_kwh,
            solar_forecast_remaining_today_kwh: request.solar_forecast_remaining_today_kwh,
            solar_forecast_tomorrow_kwh: request.solar_forecast_tomorrow_kwh,
            battery_avg_charge_price_czk_per_kwh: request.battery_avg_charge_price_czk_per_kwh,
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
        );

        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(strategy_name),
            confidence: None,
            expected_profit_czk: Some(net_profit),
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
        effective_price_czk_per_kwh: request.block.effective_price_czk_per_kwh,
    };

    let all_blocks: Vec<TimeBlockPrice> = request
        .all_blocks
        .iter()
        .map(|b| TimeBlockPrice {
            block_start: b.block_start,
            duration_minutes: b.duration_minutes,
            price_czk_per_kwh: b.price_czk_per_kwh,
            effective_price_czk_per_kwh: b.effective_price_czk_per_kwh,
        })
        .collect();

    (price_block, all_blocks)
}

/// Calculate net profit from energy flows and pricing (centralized cost calculation).
///
/// This ensures all strategies are evaluated with identical cost logic.
/// Only uses real measurable costs (grid import/export at spot/tariff prices).
/// Does NOT include battery degradation or other estimated costs.
///
/// Net profit = export revenue - import cost
///
/// # Arguments
/// * `eval` - The block evaluation containing energy flows
/// * `import_price_czk_per_kwh` - Grid import price for this block
/// * `export_price_czk_per_kwh` - Grid export price for this block
///
/// # Returns
/// Net profit in CZK (positive = profit, negative = cost)
fn calculate_net_profit(
    eval: &crate::strategy::BlockEvaluation,
    import_price_czk_per_kwh: f32,
    export_price_czk_per_kwh: f32,
) -> f32 {
    let import_cost = eval.energy_flows.grid_import_kwh * import_price_czk_per_kwh;
    let export_revenue = eval.energy_flows.grid_export_kwh * export_price_czk_per_kwh;
    export_revenue - import_cost
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
        v1_config.tomorrow_preservation_threshold =
            sc.winter_adaptive.tomorrow_preservation_threshold;
        v1_config.grid_export_price_threshold = sc.winter_adaptive.grid_export_price_threshold;
        v1_config.min_soc_for_export = sc.winter_adaptive.min_soc_for_export;
        v1_config.export_trigger_multiplier = sc.winter_adaptive.export_trigger_multiplier;
        v1_config.negative_price_handling_enabled =
            sc.winter_adaptive.negative_price_handling_enabled;
        v1_config.charge_on_negative_even_if_full =
            sc.winter_adaptive.charge_on_negative_even_if_full;
    }

    // Create Winter Adaptive V2 config from core config or defaults
    let mut v2_config = WinterAdaptiveV2Config::default();
    if let Some(sc) = strategies_config {
        v2_config.enabled = sc.winter_adaptive_v2.enabled;
        v2_config.priority = sc.winter_adaptive_v2.priority;
        v2_config.daily_charging_target_soc = sc.winter_adaptive_v2.daily_charging_target_soc;
    }

    // Create Winter Adaptive V3 config from core config or defaults
    let mut v3_config = WinterAdaptiveV3Config::default();
    if let Some(sc) = strategies_config {
        v3_config.enabled = sc.winter_adaptive_v3.enabled;
        v3_config.priority = sc.winter_adaptive_v3.priority;
        v3_config.daily_charging_target_soc = sc.winter_adaptive_v3.daily_charging_target_soc;
        // HDO configuration is now global in PricingConfig, not per-strategy
        v3_config.winter_discharge_min_soc = sc.winter_adaptive_v3.winter_discharge_min_soc;
        v3_config.top_discharge_blocks_per_day = sc.winter_adaptive_v3.top_discharge_blocks_per_day;
    }

    // Create Winter Adaptive V4 config from core config or defaults
    // HDO configuration is now global in PricingConfig, not per-strategy
    let mut v4_config = WinterAdaptiveV4Config::default();
    if let Some(sc) = strategies_config {
        v4_config.enabled = sc.winter_adaptive_v4.enabled;
        v4_config.priority = sc.winter_adaptive_v4.priority;
        v4_config.target_battery_soc = sc.winter_adaptive_v4.target_battery_soc;
        v4_config.discharge_blocks_per_day = sc.winter_adaptive_v4.discharge_blocks_per_day;
        v4_config.min_discharge_spread_czk = sc.winter_adaptive_v4.min_discharge_spread_czk;
    }

    // Create Winter Adaptive V5 config from core config or defaults
    // HDO configuration is now global in PricingConfig, not per-strategy
    let mut v5_config = WinterAdaptiveV5Config::default();
    if let Some(sc) = strategies_config {
        v5_config.enabled = sc.winter_adaptive_v5.enabled;
        v5_config.priority = sc.winter_adaptive_v5.priority;
        v5_config.target_battery_soc = sc.winter_adaptive_v5.target_battery_soc;
        v5_config.min_discharge_soc = sc.winter_adaptive_v5.min_discharge_soc;
        v5_config.cheap_block_percentile = sc.winter_adaptive_v5.cheap_block_percentile;
        v5_config.expensive_block_percentile = sc.winter_adaptive_v5.expensive_block_percentile;
        v5_config.min_discharge_spread_czk = sc.winter_adaptive_v5.min_discharge_spread_czk;
        v5_config.safety_margin_pct = sc.winter_adaptive_v5.safety_margin_pct;
    }

    // Register Winter Adaptive V1
    let v1_plugin = WinterAdaptivePlugin::new(v1_config, control_config.clone());
    manager.register(Arc::new(v1_plugin));

    // Register Winter Adaptive V2
    let v2_plugin = WinterAdaptiveV2Plugin::new(v2_config, control_config.clone());
    manager.register(Arc::new(v2_plugin));

    // Register Winter Adaptive V3
    let v3_plugin = WinterAdaptiveV3Plugin::new(v3_config, control_config.clone());
    manager.register(Arc::new(v3_plugin));

    // Register Winter Adaptive V4
    let v4_plugin = WinterAdaptiveV4Plugin::new(v4_config, control_config.clone());
    manager.register(Arc::new(v4_plugin));

    // Register Winter Adaptive V5
    let v5_plugin = WinterAdaptiveV5Plugin::new(v5_config, control_config.clone());
    manager.register(Arc::new(v5_plugin));

    // Create Winter Adaptive V7 config from core config or defaults
    let mut v7_config = WinterAdaptiveV7Config::default();
    if let Some(sc) = strategies_config {
        v7_config.enabled = sc.winter_adaptive_v7.enabled;
        v7_config.priority = sc.winter_adaptive_v7.priority;
        v7_config.target_battery_soc = sc.winter_adaptive_v7.target_battery_soc;
        v7_config.min_discharge_soc = sc.winter_adaptive_v7.min_discharge_soc;
        v7_config.min_cycle_profit_czk = sc.winter_adaptive_v7.min_cycle_profit_czk;
        v7_config.valley_threshold_std_dev = sc.winter_adaptive_v7.valley_threshold_std_dev;
        v7_config.peak_threshold_std_dev = sc.winter_adaptive_v7.peak_threshold_std_dev;
        v7_config.min_export_spread_czk = sc.winter_adaptive_v7.min_export_spread_czk;
        v7_config.min_soc_after_export = sc.winter_adaptive_v7.min_soc_after_export;
        v7_config.avg_consumption_per_block_kwh =
            sc.winter_adaptive_v7.avg_consumption_per_block_kwh;
        v7_config.negative_price_handling_enabled =
            sc.winter_adaptive_v7.negative_price_handling_enabled;
        v7_config.battery_round_trip_efficiency =
            sc.winter_adaptive_v7.battery_round_trip_efficiency;
        v7_config.solar_aware_charging_enabled = sc.winter_adaptive_v7.solar_aware_charging_enabled;
        v7_config.min_grid_charge_blocks = sc.winter_adaptive_v7.min_grid_charge_blocks;
        v7_config.opportunistic_charge_threshold_czk =
            sc.winter_adaptive_v7.opportunistic_charge_threshold_czk;
    }

    // Register Winter Adaptive V7
    let v7_plugin = WinterAdaptiveV7Plugin::new(v7_config, control_config.clone());
    manager.register(Arc::new(v7_plugin));

    // Create Winter Adaptive V8 config from core config or defaults
    let mut v8_config = WinterAdaptiveV8Config::default();
    if let Some(sc) = strategies_config {
        v8_config.enabled = sc.winter_adaptive_v8.enabled;
        v8_config.priority = sc.winter_adaptive_v8.priority;
        v8_config.target_battery_soc = sc.winter_adaptive_v8.target_battery_soc;
        v8_config.min_discharge_soc = sc.winter_adaptive_v8.min_discharge_soc;
        v8_config.top_discharge_blocks_count = sc.winter_adaptive_v8.top_discharge_blocks_count;
        v8_config.min_discharge_spread_czk = sc.winter_adaptive_v8.min_discharge_spread_czk;
        v8_config.battery_round_trip_efficiency =
            sc.winter_adaptive_v8.battery_round_trip_efficiency;
        v8_config.cheap_block_percentile = sc.winter_adaptive_v8.cheap_block_percentile;
        v8_config.avg_consumption_per_block_kwh =
            sc.winter_adaptive_v8.avg_consumption_per_block_kwh;
        v8_config.min_export_spread_czk = sc.winter_adaptive_v8.min_export_spread_czk;
        v8_config.min_soc_after_export = sc.winter_adaptive_v8.min_soc_after_export;
        v8_config.negative_price_handling_enabled =
            sc.winter_adaptive_v8.negative_price_handling_enabled;
        // Solar-aware charging config
        v8_config.solar_aware_charging_enabled = sc.winter_adaptive_v8.solar_aware_charging_enabled;
        v8_config.min_grid_charge_blocks = sc.winter_adaptive_v8.min_grid_charge_blocks;
        v8_config.opportunistic_charge_threshold_czk =
            sc.winter_adaptive_v8.opportunistic_charge_threshold_czk;
        v8_config.solar_capacity_reservation_factor =
            sc.winter_adaptive_v8.solar_capacity_reservation_factor;
        v8_config.min_solar_for_reduction_kwh = sc.winter_adaptive_v8.min_solar_for_reduction_kwh;
    }

    // Register Winter Adaptive V8
    let v8_plugin = WinterAdaptiveV8Plugin::new(v8_config, control_config.clone());
    manager.register(Arc::new(v8_plugin));

    // Create Winter Adaptive V9 config from core config or defaults
    let mut v9_config = WinterAdaptiveV9Config::default();
    if let Some(sc) = strategies_config {
        v9_config.enabled = sc.winter_adaptive_v9.enabled;
        v9_config.priority = sc.winter_adaptive_v9.priority;
        v9_config.target_battery_soc = sc.winter_adaptive_v9.target_battery_soc;
        v9_config.min_discharge_soc = sc.winter_adaptive_v9.min_discharge_soc;
        v9_config.morning_peak_start_hour = sc.winter_adaptive_v9.morning_peak_start_hour;
        v9_config.morning_peak_end_hour = sc.winter_adaptive_v9.morning_peak_end_hour;
        v9_config.target_soc_after_morning_peak =
            sc.winter_adaptive_v9.target_soc_after_morning_peak;
        v9_config.morning_peak_consumption_per_block_kwh =
            sc.winter_adaptive_v9.morning_peak_consumption_per_block_kwh;
        v9_config.solar_threshold_kwh = sc.winter_adaptive_v9.solar_threshold_kwh;
        v9_config.solar_confidence_factor = sc.winter_adaptive_v9.solar_confidence_factor;
        v9_config.min_arbitrage_spread_czk = sc.winter_adaptive_v9.min_arbitrage_spread_czk;
        v9_config.cheap_block_percentile = sc.winter_adaptive_v9.cheap_block_percentile;
        v9_config.top_discharge_blocks_count = sc.winter_adaptive_v9.top_discharge_blocks_count;
        v9_config.min_export_spread_czk = sc.winter_adaptive_v9.min_export_spread_czk;
        v9_config.min_soc_after_export = sc.winter_adaptive_v9.min_soc_after_export;
        v9_config.battery_round_trip_efficiency =
            sc.winter_adaptive_v9.battery_round_trip_efficiency;
        v9_config.negative_price_handling_enabled =
            sc.winter_adaptive_v9.negative_price_handling_enabled;
        v9_config.min_overnight_charge_blocks = sc.winter_adaptive_v9.min_overnight_charge_blocks;
        v9_config.opportunistic_charge_threshold_czk =
            sc.winter_adaptive_v9.opportunistic_charge_threshold_czk;
    }

    // Register Winter Adaptive V9
    let v9_plugin = WinterAdaptiveV9Plugin::new(v9_config, control_config.clone());
    manager.register(Arc::new(v9_plugin));
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
