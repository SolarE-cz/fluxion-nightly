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
    fixed_price_arbitrage::{FixedPriceArbitrageConfig, FixedPriceArbitrageStrategy},
    winter_adaptive::{WinterAdaptiveConfig, WinterAdaptiveStrategy},
    winter_adaptive_v2::{WinterAdaptiveV2Config, WinterAdaptiveV2Strategy},
    winter_adaptive_v3::{WinterAdaptiveV3Config, WinterAdaptiveV3Strategy},
    winter_adaptive_v4::{WinterAdaptiveV4Config, WinterAdaptiveV4Strategy},
    winter_adaptive_v5::{WinterAdaptiveV5Config, WinterAdaptiveV5Strategy},
    winter_adaptive_v7::{WinterAdaptiveV7Config, WinterAdaptiveV7Strategy},
    winter_adaptive_v8::{WinterAdaptiveV8Config, WinterAdaptiveV8Strategy},
    winter_adaptive_v9::{WinterAdaptiveV9Config, WinterAdaptiveV9Strategy},
    winter_adaptive_v10::{WinterAdaptiveV10Config, WinterAdaptiveV10Strategy},
    winter_adaptive_v20::{WinterAdaptiveV20Config, WinterAdaptiveV20Strategy},
};
use fluxion_plugins::{BlockDecision, EvaluationRequest, Plugin, PluginManager};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use std::sync::Arc;

/// Generic adapter that wraps any EconomicStrategy as a Plugin.
pub struct StrategyPlugin<S: EconomicStrategy> {
    strategy: S,
    priority: u8,
    control_config: ControlConfig,
}

impl<S: EconomicStrategy> std::fmt::Debug for StrategyPlugin<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyPlugin")
            .field("name", &self.strategy.name())
            .field("priority", &self.priority)
            .finish()
    }
}

impl<S: EconomicStrategy> StrategyPlugin<S> {
    pub fn new(strategy: S, priority: u8, control_config: ControlConfig) -> Self {
        Self {
            strategy,
            priority,
            control_config,
        }
    }
}

impl<S: EconomicStrategy + 'static> Plugin for StrategyPlugin<S> {
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
            hourly_consumption_profile: request
                .historical
                .hourly_consumption_profile
                .as_ref()
                .and_then(|v| <&[f32; 24]>::try_from(v.as_slice()).ok()),
        };

        let eval = self.strategy.evaluate(&context);
        let strategy_name = self.strategy.name().to_owned();

        // Calculate net profit from energy flows (centralized cost calculation)
        let net_profit = calculate_net_profit(
            &eval,
            price_block.effective_price_czk_per_kwh,
            request.forecast.grid_export_price_czk_per_kwh,
            request.battery_avg_charge_price_czk_per_kwh,
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
        spot_sell_price_czk_per_kwh: request.block.spot_sell_price_czk_per_kwh,
    };

    let all_blocks: Vec<TimeBlockPrice> = request
        .all_blocks
        .iter()
        .map(|b| TimeBlockPrice {
            block_start: b.block_start,
            duration_minutes: b.duration_minutes,
            price_czk_per_kwh: b.price_czk_per_kwh,
            effective_price_czk_per_kwh: b.effective_price_czk_per_kwh,
            spot_sell_price_czk_per_kwh: b.spot_sell_price_czk_per_kwh,
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
/// For **Self-Use mode**: includes battery discharge as savings (avoided grid purchase).
/// This makes the dashboard show meaningful cost/savings for Self-Use blocks:
/// - Positive profit = battery covers consumption (money saved)
/// - Negative profit = grid import needed (money spent)
/// - Net of both for partial coverage
///
/// For **other modes**: profit = export revenue - import cost
///
/// # Arguments
/// * `eval` - The block evaluation containing energy flows and mode
/// * `import_price_czk_per_kwh` - Grid import effective price for this block (spot + grid_fee + buy_fee)
/// * `export_price_czk_per_kwh` - Grid export price for this block
/// * `avg_charge_price_czk_per_kwh` - Average cost basis of energy currently in the battery
///
/// # Returns
/// Net profit in CZK (positive = profit/savings, negative = cost)
fn calculate_net_profit(
    eval: &crate::strategy::BlockEvaluation,
    import_price_czk_per_kwh: f32,
    export_price_czk_per_kwh: f32,
    avg_charge_price_czk_per_kwh: f32,
) -> f32 {
    let import_cost = eval.energy_flows.grid_import_kwh * import_price_czk_per_kwh;
    let export_revenue = eval.energy_flows.grid_export_kwh * export_price_czk_per_kwh;

    // For Self-Use: battery discharge saves money by avoiding grid import,
    // but the real savings is only the delta between current grid price and
    // what the stored energy cost to charge.
    let battery_savings = if matches!(eval.mode, InverterOperationMode::SelfUse) {
        eval.energy_flows.battery_discharge_kwh
            * (import_price_czk_per_kwh - avg_charge_price_czk_per_kwh)
    } else {
        0.0
    };

    battery_savings + export_revenue - import_cost
}

/// Initialize a PluginManager with the default built-in strategies.
///
/// This function registers the built-in Rust strategies (Winter Adaptive V1–V10, V20,
/// Fixed Price Arbitrage) into an existing PluginManager. Use this when you need to
/// initialize a shared manager that may also receive external plugin registrations.
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

    // Register V1–V5
    let priority = v1_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveStrategy::new(v1_config),
        priority,
        control_config.clone(),
    )));

    let priority = v2_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV2Strategy::new(v2_config),
        priority,
        control_config.clone(),
    )));

    let priority = v3_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV3Strategy::new(v3_config),
        priority,
        control_config.clone(),
    )));

    let priority = v4_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV4Strategy::new(v4_config),
        priority,
        control_config.clone(),
    )));

    let priority = v5_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV5Strategy::new(v5_config),
        priority,
        control_config.clone(),
    )));

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

    let priority = v7_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV7Strategy::new(v7_config),
        priority,
        control_config.clone(),
    )));

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

    let priority = v8_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV8Strategy::new(v8_config),
        priority,
        control_config.clone(),
    )));

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

    let priority = v9_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV9Strategy::new(v9_config),
        priority,
        control_config.clone(),
    )));

    // Create Winter Adaptive V10 config from core config or defaults
    let mut v10_config = WinterAdaptiveV10Config::default();
    if let Some(sc) = strategies_config {
        v10_config.enabled = sc.winter_adaptive_v10.enabled;
        v10_config.priority = sc.winter_adaptive_v10.priority;
        v10_config.target_battery_soc = sc.winter_adaptive_v10.target_battery_soc;
        v10_config.min_discharge_soc = sc.winter_adaptive_v10.min_discharge_soc;
        v10_config.battery_round_trip_efficiency =
            sc.winter_adaptive_v10.battery_round_trip_efficiency;
        v10_config.negative_price_handling_enabled =
            sc.winter_adaptive_v10.negative_price_handling_enabled;
        v10_config.opportunistic_charge_threshold_czk =
            sc.winter_adaptive_v10.opportunistic_charge_threshold_czk;
        v10_config.min_export_spread_czk = sc.winter_adaptive_v10.min_export_spread_czk;
        v10_config.min_soc_after_export = sc.winter_adaptive_v10.min_soc_after_export;
        v10_config.solar_threshold_kwh = sc.winter_adaptive_v10.solar_threshold_kwh;
        v10_config.solar_confidence_factor = sc.winter_adaptive_v10.solar_confidence_factor;
        v10_config.min_savings_threshold_czk = sc.winter_adaptive_v10.min_savings_threshold_czk;
    }

    let priority = v10_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV10Strategy::new(v10_config),
        priority,
        control_config.clone(),
    )));

    // Create Winter Adaptive V20 config from core config or defaults
    let mut v20_config = WinterAdaptiveV20Config::default();
    if let Some(sc) = strategies_config {
        v20_config.enabled = sc.winter_adaptive_v20.enabled;
        v20_config.priority = sc.winter_adaptive_v20.priority;
        v20_config.target_battery_soc = sc.winter_adaptive_v20.target_battery_soc;
        v20_config.min_discharge_soc = sc.winter_adaptive_v20.min_discharge_soc;
        v20_config.battery_round_trip_efficiency =
            sc.winter_adaptive_v20.battery_round_trip_efficiency;
        v20_config.negative_price_handling_enabled =
            sc.winter_adaptive_v20.negative_price_handling_enabled;
        v20_config.opportunistic_charge_threshold_czk =
            sc.winter_adaptive_v20.opportunistic_charge_threshold_czk;
        v20_config.min_export_spread_czk = sc.winter_adaptive_v20.min_export_spread_czk;
        v20_config.min_soc_after_export = sc.winter_adaptive_v20.min_soc_after_export;
        v20_config.solar_threshold_kwh = sc.winter_adaptive_v20.solar_threshold_kwh;
        v20_config.solar_confidence_factor = sc.winter_adaptive_v20.solar_confidence_factor;
        v20_config.min_savings_threshold_czk = sc.winter_adaptive_v20.min_savings_threshold_czk;
        v20_config.volatile_cv_threshold = sc.winter_adaptive_v20.volatile_cv_threshold;
        v20_config.expensive_level_threshold = sc.winter_adaptive_v20.expensive_level_threshold;
        v20_config.high_solar_ratio_threshold = sc.winter_adaptive_v20.high_solar_ratio_threshold;
        v20_config.low_solar_ratio_threshold = sc.winter_adaptive_v20.low_solar_ratio_threshold;
        v20_config.tomorrow_expensive_ratio = sc.winter_adaptive_v20.tomorrow_expensive_ratio;
        v20_config.tomorrow_cheap_ratio = sc.winter_adaptive_v20.tomorrow_cheap_ratio;
        v20_config.negative_price_fraction_threshold =
            sc.winter_adaptive_v20.negative_price_fraction_threshold;
    }

    let priority = v20_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        WinterAdaptiveV20Strategy::new(v20_config),
        priority,
        control_config.clone(),
    )));

    // Create Fixed Price Arbitrage config from core config or defaults
    let mut fpa_config = FixedPriceArbitrageConfig::default();
    if let Some(sc) = strategies_config {
        fpa_config.enabled = sc.fixed_price_arbitrage.enabled;
        fpa_config.priority = sc.fixed_price_arbitrage.priority;
        fpa_config.min_profit_threshold_czk = sc.fixed_price_arbitrage.min_profit_threshold_czk;
    }

    let priority = fpa_config.priority;
    manager.register(Arc::new(StrategyPlugin::new(
        FixedPriceArbitrageStrategy::new(fpa_config),
        priority,
        control_config.clone(),
    )));
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
