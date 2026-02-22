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

use bevy_ecs::prelude::Resource;
use fluxion_i18n::Language;
use serde::{Deserialize, Serialize};

use crate::history::ConsumptionHistoryConfig;
use crate::inverter::{InverterOperationMode, InverterType};

// ============= System Configuration =============

/// Central configuration resource for the FluxION system
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub inverters: Vec<InverterConfig>,
    #[serde(rename = "pricing")]
    pub pricing_config: PricingConfig,
    #[serde(rename = "control")]
    pub control_config: ControlConfig,
    #[serde(rename = "system")]
    pub system_config: SystemSettingsConfig,
    #[serde(default, rename = "strategies")]
    pub strategies_config: StrategiesConfigCore,
    #[serde(default, rename = "history")]
    pub history: ConsumptionHistoryConfig,
    #[serde(default, rename = "solar_forecast")]
    pub solar_forecast: SolarForecastConfigCore,
    #[serde(default, rename = "remote_access")]
    pub remote_access: RemoteAccessConfigCore,
}

/// Configuration for a single inverter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterConfig {
    pub id: String,
    pub inverter_type: InverterType,
    pub entity_prefix: String,
    pub topology: InverterTopology,
}

/// Inverter topology for multi-inverter setups
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InverterTopology {
    Independent,
    Master { slave_ids: Vec<String> },
    Slave { master_id: String },
}

/// Schedule for fixed prices (flat or hourly)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PriceSchedule {
    Flat(f32),
    Hourly(Vec<f32>),
}

impl PriceSchedule {
    /// Get price for a specific hour (0-23)
    pub fn get_price(&self, hour: usize) -> f32 {
        match self {
            PriceSchedule::Flat(price) => *price,
            PriceSchedule::Hourly(prices) => {
                if prices.is_empty() {
                    return 0.0;
                }
                // Handle wrapping or clamping if needed, but generally expect 24 items
                // If less than 24, cycle or clamp? Let's cycle for safety.
                prices[hour % prices.len()]
            }
        }
    }
}

impl Default for PriceSchedule {
    fn default() -> Self {
        PriceSchedule::Flat(0.0)
    }
}

/// Pricing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingConfig {
    pub spot_price_entity: String,
    /// Optional separate sensor for tomorrow's prices
    #[serde(default)]
    pub tomorrow_price_entity: Option<String>,
    pub use_spot_prices_to_buy: bool,
    pub use_spot_prices_to_sell: bool,
    pub fixed_buy_price_czk: PriceSchedule,
    pub fixed_sell_price_czk: PriceSchedule,

    // Spot market fees
    #[serde(default = "default_spot_buy_fee")]
    pub spot_buy_fee_czk: f32,
    #[serde(default = "default_spot_sell_fee")]
    pub spot_sell_fee_czk: f32,

    // ============= HDO Tariff Configuration (Czech Grid Fees) =============
    /// Home Assistant entity for HDO tariff schedule (e.g., "sensor.cez_hdo_raw_data")
    /// This sensor provides low/high tariff time periods for accurate grid fee calculation
    #[serde(default = "default_hdo_sensor_entity")]
    pub hdo_sensor_entity: String,

    /// Grid fee during HDO low tariff periods (CZK/kWh)
    /// This is added to spot prices during low tariff hours to get effective buy price
    #[serde(default = "default_hdo_low_tariff_czk")]
    pub hdo_low_tariff_czk: f32,

    /// Grid fee during HDO high tariff periods (CZK/kWh)
    /// This is added to spot prices during high tariff hours to get effective buy price
    #[serde(default = "default_hdo_high_tariff_czk")]
    pub hdo_high_tariff_czk: f32,
}

/// Control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    pub force_charge_hours: usize,
    pub force_discharge_hours: usize,
    pub min_battery_soc: f32,
    pub max_battery_soc: f32,
    pub maximum_export_power_w: u32,
    pub battery_capacity_kwh: f32,
    pub battery_wear_cost_czk_per_kwh: f32,
    pub battery_efficiency: f32,
    pub min_mode_change_interval_secs: u64,
    /// Average household consumption (kW) used for battery SOC predictions
    /// This is used as a fallback when actual load data is not available
    pub average_household_load_kw: f32,
    /// Hardware minimum battery SOC enforced by inverter firmware
    /// This is the absolute floor that predictions should use
    #[serde(default = "default_hardware_min_soc")]
    pub hardware_min_battery_soc: f32,

    /// Fixed grid export fee in CZK/kWh (what you get paid for selling to grid)
    /// This is typically much lower than import price and often a fixed rate
    /// Default: 0.5 CZK/kWh (typical Czech market rate)
    #[serde(default = "default_grid_export_fee")]
    pub grid_export_fee_czk_per_kwh: f32,

    // ============= Charge Time Planning Parameters =============
    /// Maximum battery charge rate in kW (determines minimum charge time)
    /// Typical values: 5-15 kW depending on battery/inverter specifications
    /// Default: 10.0 kW
    #[serde(default = "default_charge_rate")]
    pub max_battery_charge_rate_kw: f32,

    /// Target SOC (%) to reach before evening peak
    /// Scheduler will reserve enough cheap blocks to reach this SOC
    /// Default: 90% (leaves 10% room for solar top-up)
    #[serde(default = "default_evening_target_soc")]
    pub evening_target_soc: f32,

    /// Evening peak start hour (24h format, 0-23)
    /// Scheduler ensures battery is charged before this hour
    /// Default: 17 (5:00 PM)
    #[serde(default = "default_evening_peak_hour")]
    pub evening_peak_start_hour: u32,

    /// Minimum number of consecutive 15-minute blocks required for force-charge/discharge operations
    /// Single-block force operations can cause excessive inverter EEPROM writes.
    /// Default: 2 blocks (30 minutes minimum duration)
    #[serde(default = "default_min_consecutive_force_blocks")]
    pub min_consecutive_force_blocks: usize,

    /// Default battery operation mode when not force charging/discharging
    /// Default: SelfUse (normal self-consumption mode)
    #[serde(default = "default_battery_operation_mode")]
    pub default_battery_mode: InverterOperationMode,
}

// Default value functions for serde
fn default_charge_rate() -> f32 {
    10.0
}
fn default_evening_target_soc() -> f32 {
    90.0
}
fn default_evening_peak_hour() -> u32 {
    17
}
fn default_hardware_min_soc() -> f32 {
    10.0
}

fn default_grid_export_fee() -> f32 {
    0.5
}

fn default_min_consecutive_force_blocks() -> usize {
    2
}
fn default_battery_operation_mode() -> InverterOperationMode {
    InverterOperationMode::SelfUse
}
fn default_spot_buy_fee() -> f32 {
    0.5
}
fn default_spot_sell_fee() -> f32 {
    0.5
}

fn default_hdo_sensor_entity() -> String {
    "sensor.cez_hdo_raw_data".to_string()
}

fn default_hdo_low_tariff_czk() -> f32 {
    0.50
}

fn default_hdo_high_tariff_czk() -> f32 {
    1.80
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            force_charge_hours: 4,
            force_discharge_hours: 2,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            maximum_export_power_w: 10000,
            battery_capacity_kwh: 20.0,
            battery_wear_cost_czk_per_kwh: 0.125,
            battery_efficiency: 0.95,
            min_mode_change_interval_secs: 300,
            average_household_load_kw: 1.0,
            hardware_min_battery_soc: 10.0,
            grid_export_fee_czk_per_kwh: 0.5,
            max_battery_charge_rate_kw: 10.0,
            evening_target_soc: 90.0,
            evening_peak_start_hour: 17,
            min_consecutive_force_blocks: 2,
            default_battery_mode: InverterOperationMode::SelfUse,
        }
    }
}

/// System settings configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSettingsConfig {
    pub update_interval_secs: u64,
    pub debug_mode: bool,
    pub display_currency: Currency,
    #[serde(default)]
    pub language: Language,
    #[serde(skip)]
    pub timezone: Option<String>, // Home Assistant timezone (fetched at runtime)
}

/// Strategies configuration for core module
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StrategiesConfigCore {
    #[serde(default)]
    pub winter_adaptive: WinterAdaptiveConfigCore,
    #[serde(default)]
    pub winter_adaptive_v2: WinterAdaptiveV2ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v3: WinterAdaptiveV3ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v4: WinterAdaptiveV4ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v5: WinterAdaptiveV5ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v7: WinterAdaptiveV7ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v8: WinterAdaptiveV8ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v9: WinterAdaptiveV9ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v10: WinterAdaptiveV10ConfigCore,
    #[serde(default)]
    pub winter_adaptive_v20: WinterAdaptiveV20ConfigCore,
    #[serde(default)]
    pub winter_peak_discharge: WinterPeakDischargeConfigCore,
    #[serde(default)]
    pub solar_aware_charging: SolarAwareChargingConfigCore,
    #[serde(default)]
    pub morning_precharge: StrategyEnabledConfigCore,
    #[serde(default)]
    pub day_ahead_planning: StrategyEnabledConfigCore,
    #[serde(default)]
    pub time_aware_charge: StrategyEnabledConfigCore,
    #[serde(default)]
    pub price_arbitrage: StrategyEnabledConfigCore,
    #[serde(default)]
    pub solar_first: StrategyEnabledConfigCore,
    #[serde(default)]
    pub self_use: StrategyEnabledConfigCore,
    #[serde(default)]
    pub fixed_price_arbitrage: FixedPriceArbitrageConfigCore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterPeakDischargeConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_strategy_priority")]
    pub priority: u8,
    pub min_spread_czk: f32,
    pub min_soc_to_start: f32,
    pub min_soc_target: f32,
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub min_hours_to_solar: u32,
}

impl Default for WinterPeakDischargeConfigCore {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 80, // High priority for peak discharge
            min_spread_czk: 3.0,
            min_soc_to_start: 70.0,
            min_soc_target: 50.0,
            solar_window_start_hour: 10,
            solar_window_end_hour: 14,
            min_hours_to_solar: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarAwareChargingConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_strategy_priority")]
    pub priority: u8,
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub midday_max_soc: f32,
    pub min_solar_forecast_kwh: f32,
}

impl Default for SolarAwareChargingConfigCore {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 70, // Medium-high priority for solar awareness
            solar_window_start_hour: 10,
            solar_window_end_hour: 14,
            midday_max_soc: 90.0,
            min_solar_forecast_kwh: 2.0,
        }
    }
}

/// Default strategy priority (used when strategies conflict)
fn default_strategy_priority() -> u8 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEnabledConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_strategy_priority")]
    pub priority: u8,
}

impl Default for StrategyEnabledConfigCore {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_strategy_priority")]
    pub priority: u8,
    pub ema_period_days: usize,
    pub min_solar_percentage: f32,
    pub daily_charging_target_soc: f32,
    pub conservation_threshold_soc: f32,
    pub top_expensive_blocks: usize,
    #[serde(default = "default_tomorrow_preservation_threshold")]
    pub tomorrow_preservation_threshold: f32,
    #[serde(default = "default_grid_export_price_threshold")]
    pub grid_export_price_threshold: f32,
    #[serde(default = "default_min_soc_for_export")]
    pub min_soc_for_export: f32,
    #[serde(default = "default_export_trigger_multiplier")]
    pub export_trigger_multiplier: f32,
    #[serde(default = "default_negative_price_handling_enabled")]
    pub negative_price_handling_enabled: bool,
    #[serde(default = "default_charge_on_negative_even_if_full")]
    pub charge_on_negative_even_if_full: bool,
    #[serde(skip)]
    pub historical_daily_consumption: Vec<f32>,
}

fn default_tomorrow_preservation_threshold() -> f32 {
    1.2
}
fn default_grid_export_price_threshold() -> f32 {
    8.0
}
fn default_min_soc_for_export() -> f32 {
    50.0
}
fn default_export_trigger_multiplier() -> f32 {
    2.5
}
fn default_negative_price_handling_enabled() -> bool {
    true
}
fn default_charge_on_negative_even_if_full() -> bool {
    false
}

impl Default for WinterAdaptiveConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V4 is the default strategy
            priority: 100,  // Highest priority by default (main strategy)
            ema_period_days: 7,
            min_solar_percentage: 0.10,
            daily_charging_target_soc: 90.0,
            conservation_threshold_soc: 75.0,
            top_expensive_blocks: 12,
            tomorrow_preservation_threshold: 1.2,
            grid_export_price_threshold: 8.0,
            min_soc_for_export: 50.0,
            export_trigger_multiplier: 2.5,
            negative_price_handling_enabled: true,
            charge_on_negative_even_if_full: false,
            historical_daily_consumption: Vec::new(),
        }
    }
}

/// Winter Adaptive V2 strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV2ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v2_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 90%)
    #[serde(default = "default_daily_charging_target_soc")]
    pub daily_charging_target_soc: f32,
}

fn default_daily_charging_target_soc() -> f32 {
    90.0
}

fn default_winter_adaptive_v2_priority() -> u8 {
    100
}

impl Default for WinterAdaptiveV2ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V4 is the default strategy
            priority: 100,
            daily_charging_target_soc: 90.0,
        }
    }
}

/// Winter Adaptive V3 strategy configuration
/// Simplified strategy with HDO tariff integration for accurate grid fee calculation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV3ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v3_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 90%)
    #[serde(default = "default_v3_daily_charging_target_soc")]
    pub daily_charging_target_soc: f32,
    /// Home Assistant entity for HDO tariff schedule
    #[serde(default = "default_hdo_sensor_entity")]
    pub hdo_sensor_entity: String,
    /// Grid fee during HDO low tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_low_tariff_czk")]
    pub hdo_low_tariff_czk: f32,
    /// Grid fee during HDO high tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_high_tariff_czk")]
    pub hdo_high_tariff_czk: f32,
    /// Minimum SOC for winter discharge (default: 50%)
    #[serde(default = "default_winter_discharge_min_soc")]
    pub winter_discharge_min_soc: f32,
    /// Number of top expensive blocks per day to allow discharge (default: 4)
    #[serde(default = "default_top_discharge_blocks_per_day")]
    pub top_discharge_blocks_per_day: usize,
    /// Minimum arbitrage buffer above median+high_grid_fee for discharge to be worthwhile
    /// Default: 1.0 CZK (use 0.05 for EUR)
    #[serde(default = "default_discharge_arbitrage_buffer")]
    pub discharge_arbitrage_buffer: f32,
}

fn default_winter_adaptive_v3_priority() -> u8 {
    100
}

fn default_v3_daily_charging_target_soc() -> f32 {
    90.0
}

fn default_winter_discharge_min_soc() -> f32 {
    50.0
}

fn default_top_discharge_blocks_per_day() -> usize {
    4
}

fn default_discharge_arbitrage_buffer() -> f32 {
    1.0 // 1.0 CZK, use 0.05 for EUR
}

impl Default for WinterAdaptiveV3ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V3 is deprecated, V4 is the default
            priority: 100,
            daily_charging_target_soc: 90.0,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
            winter_discharge_min_soc: 50.0,
            top_discharge_blocks_per_day: 4,
            discharge_arbitrage_buffer: 1.0,
        }
    }
}

/// Winter Adaptive V4 Configuration - Global Price Optimization
///
/// V4 uses true global optimization: it ranks ALL blocks by price and selects
/// the globally cheapest for charging and globally most expensive for discharge.
/// This fixes the V3 bug where it would charge at 3.73 CZK when 2.31 CZK blocks
/// were available later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV4ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v4_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 100%)
    #[serde(default = "default_v4_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Home Assistant entity for HDO tariff schedule
    #[serde(default = "default_hdo_sensor_entity")]
    pub hdo_sensor_entity: String,
    /// Grid fee during HDO low tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_low_tariff_czk")]
    pub hdo_low_tariff_czk: f32,
    /// Grid fee during HDO high tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_high_tariff_czk")]
    pub hdo_high_tariff_czk: f32,
    /// Number of top expensive blocks per day for discharge (default: 4)
    #[serde(default = "default_v4_discharge_blocks_per_day")]
    pub discharge_blocks_per_day: usize,
    /// Minimum price spread for discharge to be worthwhile (CZK)
    #[serde(default = "default_v4_min_discharge_spread")]
    pub min_discharge_spread_czk: f32,
}

fn default_winter_adaptive_v4_priority() -> u8 {
    100
}

fn default_v4_target_battery_soc() -> f32 {
    100.0
}

fn default_v4_discharge_blocks_per_day() -> usize {
    4
}

fn default_v4_min_discharge_spread() -> f32 {
    0.50
}

impl Default for WinterAdaptiveV4ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V5 is now the default strategy
            priority: 100,
            target_battery_soc: 100.0,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
            discharge_blocks_per_day: 4,
            min_discharge_spread_czk: 0.50,
        }
    }
}

/// Configuration for Winter Adaptive V5 strategy
/// V5 combines the best logic from V2, V3, and V4 to maximize cost savings:
/// - Global price ranking (from V4)
/// - Reserve SOC protection (from V3)
/// - Grid avoidance during expensive blocks (NEW)
/// - Aggressive charging during cheap blocks (NEW)
/// - Safety margins (from V2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV5ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v5_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 100%)
    #[serde(default = "default_v5_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 40%)
    #[serde(default = "default_v5_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Home Assistant entity for HDO tariff schedule
    #[serde(default = "default_hdo_sensor_entity")]
    pub hdo_sensor_entity: String,
    /// Grid fee during HDO low tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_low_tariff_czk")]
    pub hdo_low_tariff_czk: f32,
    /// Grid fee during HDO high tariff periods (CZK/kWh)
    #[serde(default = "default_hdo_high_tariff_czk")]
    pub hdo_high_tariff_czk: f32,
    /// Percentile threshold for "cheap" blocks (default: 30%)
    #[serde(default = "default_v5_cheap_block_percentile")]
    pub cheap_block_percentile: f32,
    /// Percentile threshold for "expensive" blocks (default: 70%)
    #[serde(default = "default_v5_expensive_block_percentile")]
    pub expensive_block_percentile: f32,
    /// Minimum price spread for discharge (CZK)
    #[serde(default = "default_v5_min_discharge_spread")]
    pub min_discharge_spread_czk: f32,
    /// Safety margin for energy needs calculation (default: 0.15 = 15%)
    #[serde(default = "default_v5_safety_margin")]
    pub safety_margin_pct: f32,
}

fn default_winter_adaptive_v5_priority() -> u8 {
    95
}

fn default_v5_target_battery_soc() -> f32 {
    100.0
}

fn default_v5_min_discharge_soc() -> f32 {
    40.0
}

fn default_v5_cheap_block_percentile() -> f32 {
    30.0
}

fn default_v5_expensive_block_percentile() -> f32 {
    70.0
}

fn default_v5_min_discharge_spread() -> f32 {
    0.50
}

fn default_v5_safety_margin() -> f32 {
    0.15
}

impl Default for WinterAdaptiveV5ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V7 is now the default strategy
            priority: 95,
            target_battery_soc: 100.0,
            min_discharge_soc: 40.0,
            hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
            cheap_block_percentile: 30.0,
            expensive_block_percentile: 70.0,
            min_discharge_spread_czk: 0.50,
            safety_margin_pct: 0.15,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Winter Adaptive V7 configuration - Unconstrained Multi-Cycle Arbitrage Optimizer
/// V7 removes all artificial limitations and uses pure economic decision-making:
/// - No "top N blocks" limits
/// - No "below median only" constraints
/// - Multiple charge/discharge cycles per day
/// - 3 CZK minimum spread for profitability
/// - Home-first export policy (SOC >50% after export)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV7ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v7_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 95%)
    #[serde(default = "default_v7_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 10%)
    #[serde(default = "default_v7_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Minimum profit per cycle (CZK) - discharge_value - charge_cost >= this
    #[serde(default = "default_v7_min_cycle_profit_czk")]
    pub min_cycle_profit_czk: f32,
    /// Valley detection threshold (std devs below mean)
    #[serde(default = "default_v7_valley_threshold")]
    pub valley_threshold_std_dev: f32,
    /// Peak detection threshold (std devs above mean)
    #[serde(default = "default_v7_peak_threshold")]
    pub peak_threshold_std_dev: f32,
    /// Minimum spread for grid export (CZK)
    #[serde(default = "default_v7_min_export_spread")]
    pub min_export_spread_czk: f32,
    /// Minimum SOC after export (%)
    #[serde(default = "default_v7_min_soc_after_export")]
    pub min_soc_after_export: f32,
    /// Average consumption per block (kWh)
    #[serde(default = "default_v7_avg_consumption")]
    pub avg_consumption_per_block_kwh: f32,
    /// Enable negative price handling
    #[serde(default = "default_true")]
    pub negative_price_handling_enabled: bool,
    /// Round-trip battery efficiency
    #[serde(default = "default_v7_efficiency")]
    pub battery_round_trip_efficiency: f32,
    /// Enable solar-aware charge reduction
    #[serde(default = "default_true")]
    pub solar_aware_charging_enabled: bool,
    /// Minimum grid charge blocks to schedule (safety margin)
    #[serde(default = "default_v7_min_grid_charge_blocks")]
    pub min_grid_charge_blocks: usize,
    /// Price threshold (CZK/kWh) below which we always charge
    #[serde(default = "default_v7_opportunistic_threshold")]
    pub opportunistic_charge_threshold_czk: f32,
}

fn default_v7_min_grid_charge_blocks() -> usize {
    2
}
fn default_v7_opportunistic_threshold() -> f32 {
    1.5
}

fn default_winter_adaptive_v7_priority() -> u8 {
    100
}
fn default_v7_target_battery_soc() -> f32 {
    95.0
}
fn default_v7_min_discharge_soc() -> f32 {
    10.0
}
fn default_v7_min_cycle_profit_czk() -> f32 {
    3.0
}
fn default_v7_valley_threshold() -> f32 {
    0.5
}
fn default_v7_peak_threshold() -> f32 {
    0.5
}
fn default_v7_min_export_spread() -> f32 {
    5.0
}
fn default_v7_min_soc_after_export() -> f32 {
    50.0
}
fn default_v7_avg_consumption() -> f32 {
    0.25
}
fn default_v7_efficiency() -> f32 {
    0.90
}

impl Default for WinterAdaptiveV7ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V9 is now the default strategy
            priority: 100,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            min_cycle_profit_czk: 3.0,
            valley_threshold_std_dev: 0.5,
            peak_threshold_std_dev: 0.5,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            avg_consumption_per_block_kwh: 0.25,
            negative_price_handling_enabled: true,
            battery_round_trip_efficiency: 0.90,
            solar_aware_charging_enabled: true,
            min_grid_charge_blocks: 2,
            opportunistic_charge_threshold_czk: 1.5,
        }
    }
}

/// Winter Adaptive V8 configuration - Top-N Peak Discharge Optimizer
/// V8 focuses on aggressive discharge during the absolute highest price peaks:
/// - User-configurable number of top price blocks for discharge (default: 8 blocks = 2 hours)
/// - Predictive battery management ensures capacity during peak hours
/// - 3 CZK minimum spread requirement
/// - Prevents early battery depletion before afternoon/evening peaks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV8ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v8_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 95%)
    #[serde(default = "default_v8_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 10%)
    #[serde(default = "default_v8_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Number of top price blocks to discharge in (default: 8 = 2 hours)
    #[serde(default = "default_v8_top_discharge_blocks")]
    pub top_discharge_blocks_count: usize,
    /// Minimum price spread (CZK) for discharge (default: 3.0)
    #[serde(default = "default_v8_min_discharge_spread")]
    pub min_discharge_spread_czk: f32,
    /// Round-trip battery efficiency (default: 0.90)
    #[serde(default = "default_v8_efficiency")]
    pub battery_round_trip_efficiency: f32,
    /// Percentile for cheap charging blocks (default: 0.25 = bottom 25%)
    #[serde(default = "default_v8_cheap_percentile")]
    pub cheap_block_percentile: f32,
    /// Average consumption per block (kWh) (default: 0.25)
    #[serde(default = "default_v8_avg_consumption")]
    pub avg_consumption_per_block_kwh: f32,
    /// Minimum spread for grid export (CZK) (default: 5.0)
    #[serde(default = "default_v8_min_export_spread")]
    pub min_export_spread_czk: f32,
    /// Minimum SOC after export (%) (default: 50%)
    #[serde(default = "default_v8_min_soc_after_export")]
    pub min_soc_after_export: f32,
    /// Enable negative price handling (default: true)
    #[serde(default = "default_true")]
    pub negative_price_handling_enabled: bool,
    // === Solar-Aware Charging ===
    /// Enable solar-aware charge reduction (default: true)
    #[serde(default = "default_true")]
    pub solar_aware_charging_enabled: bool,
    /// Minimum grid charge blocks as safety margin (default: 2)
    #[serde(default = "default_v8_min_grid_charge_blocks")]
    pub min_grid_charge_blocks: usize,
    /// Price threshold for opportunistic charging (CZK/kWh) (default: 1.5)
    #[serde(default = "default_v8_opportunistic_charge_threshold")]
    pub opportunistic_charge_threshold_czk: f32,
    /// Factor for solar capacity reservation (0.0-1.0) (default: 0.7)
    #[serde(default = "default_v8_solar_capacity_factor")]
    pub solar_capacity_reservation_factor: f32,
    /// Minimum solar forecast (kWh) to trigger charge reduction (default: 2.0)
    #[serde(default = "default_v8_min_solar_for_reduction")]
    pub min_solar_for_reduction_kwh: f32,
}

fn default_winter_adaptive_v8_priority() -> u8 {
    100
}
fn default_v8_target_battery_soc() -> f32 {
    95.0
}
fn default_v8_min_discharge_soc() -> f32 {
    10.0
}
fn default_v8_top_discharge_blocks() -> usize {
    8 // 2 hours
}
fn default_v8_min_discharge_spread() -> f32 {
    3.0
}
fn default_v8_efficiency() -> f32 {
    0.90
}
fn default_v8_cheap_percentile() -> f32 {
    0.25
}
fn default_v8_avg_consumption() -> f32 {
    0.25
}
fn default_v8_min_export_spread() -> f32 {
    5.0
}
fn default_v8_min_soc_after_export() -> f32 {
    50.0
}
fn default_v8_min_grid_charge_blocks() -> usize {
    2
}
fn default_v8_opportunistic_charge_threshold() -> f32 {
    1.5
}
fn default_v8_solar_capacity_factor() -> f32 {
    0.7
}
fn default_v8_min_solar_for_reduction() -> f32 {
    2.0
}

impl Default for WinterAdaptiveV8ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V7 is still default
            priority: 100,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            top_discharge_blocks_count: 8,
            min_discharge_spread_czk: 3.0,
            battery_round_trip_efficiency: 0.90,
            cheap_block_percentile: 0.25,
            avg_consumption_per_block_kwh: 0.25,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            negative_price_handling_enabled: true,
            // Solar-aware charging defaults
            solar_aware_charging_enabled: true,
            min_grid_charge_blocks: 2,
            opportunistic_charge_threshold_czk: 1.5,
            solar_capacity_reservation_factor: 0.7,
            min_solar_for_reduction_kwh: 2.0,
        }
    }
}

/// Winter Adaptive V9 configuration - Solar-Aware Morning Peak Optimizer
/// V9 maximizes solar utilization while ensuring morning peak coverage:
/// - High solar days: Minimal grid charging, only cover morning peak
/// - Low solar days: Full arbitrage mode like V7
/// - Target ~20% SOC by end of morning peak (leaves room for solar)
/// - 3 CZK minimum spread for arbitrage opportunities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV9ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v9_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 95%)
    #[serde(default = "default_v9_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 10%)
    #[serde(default = "default_v9_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Morning peak start hour (default: 6)
    #[serde(default = "default_v9_morning_peak_start")]
    pub morning_peak_start_hour: u8,
    /// Morning peak end hour (default: 9)
    #[serde(default = "default_v9_morning_peak_end")]
    pub morning_peak_end_hour: u8,
    /// Target SOC (%) at end of morning peak (default: 20%)
    #[serde(default = "default_v9_target_soc_after_peak")]
    pub target_soc_after_morning_peak: f32,
    /// Average consumption per block during morning peak (kWh) (default: 0.5)
    #[serde(default = "default_v9_morning_consumption")]
    pub morning_peak_consumption_per_block_kwh: f32,
    /// Minimum solar forecast (kWh) to trigger solar-first mode (default: 5.0)
    #[serde(default = "default_v9_solar_threshold")]
    pub solar_threshold_kwh: f32,
    /// Factor to apply to solar forecast for conservative planning (default: 0.7)
    #[serde(default = "default_v9_solar_confidence")]
    pub solar_confidence_factor: f32,
    /// Minimum price spread (CZK) for arbitrage (default: 3.0)
    #[serde(default = "default_v9_min_arbitrage_spread")]
    pub min_arbitrage_spread_czk: f32,
    /// Percentile for cheap blocks (0.0-1.0) (default: 0.25)
    #[serde(default = "default_v9_cheap_percentile")]
    pub cheap_block_percentile: f32,
    /// Number of top expensive blocks for arbitrage discharge (default: 8)
    #[serde(default = "default_v9_top_discharge_blocks")]
    pub top_discharge_blocks_count: usize,
    /// Minimum spread for grid export (CZK) (default: 5.0)
    #[serde(default = "default_v9_min_export_spread")]
    pub min_export_spread_czk: f32,
    /// Minimum SOC after export (%) (default: 50%)
    #[serde(default = "default_v9_min_soc_after_export")]
    pub min_soc_after_export: f32,
    /// Round-trip battery efficiency (default: 0.90)
    #[serde(default = "default_v9_efficiency")]
    pub battery_round_trip_efficiency: f32,
    /// Enable negative price handling (default: true)
    #[serde(default = "default_true")]
    pub negative_price_handling_enabled: bool,
    /// Minimum overnight charge blocks (safety margin) (default: 4)
    #[serde(default = "default_v9_min_overnight_blocks")]
    pub min_overnight_charge_blocks: usize,
    /// Price threshold for opportunistic charging (CZK/kWh) (default: 1.5)
    #[serde(default = "default_v9_opportunistic_threshold")]
    pub opportunistic_charge_threshold_czk: f32,
}

fn default_winter_adaptive_v9_priority() -> u8 {
    100
}
fn default_v9_target_battery_soc() -> f32 {
    95.0
}
fn default_v9_min_discharge_soc() -> f32 {
    10.0
}
fn default_v9_morning_peak_start() -> u8 {
    6
}
fn default_v9_morning_peak_end() -> u8 {
    9
}
fn default_v9_target_soc_after_peak() -> f32 {
    20.0
}
fn default_v9_morning_consumption() -> f32 {
    0.5
}
fn default_v9_solar_threshold() -> f32 {
    5.0
}
fn default_v9_solar_confidence() -> f32 {
    0.7
}
fn default_v9_min_arbitrage_spread() -> f32 {
    3.0
}
fn default_v9_cheap_percentile() -> f32 {
    0.25
}
fn default_v9_top_discharge_blocks() -> usize {
    8
}
fn default_v9_min_export_spread() -> f32 {
    5.0
}
fn default_v9_min_soc_after_export() -> f32 {
    50.0
}
fn default_v9_efficiency() -> f32 {
    0.90
}
fn default_v9_min_overnight_blocks() -> usize {
    4
}
fn default_v9_opportunistic_threshold() -> f32 {
    1.5
}

impl Default for WinterAdaptiveV9ConfigCore {
    fn default() -> Self {
        Self {
            enabled: true, // V9 is the default strategy
            priority: 100,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            morning_peak_start_hour: 6,
            morning_peak_end_hour: 9,
            target_soc_after_morning_peak: 20.0,
            morning_peak_consumption_per_block_kwh: 0.5,
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            min_arbitrage_spread_czk: 3.0,
            cheap_block_percentile: 0.25,
            top_discharge_blocks_count: 8,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 50.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            min_overnight_charge_blocks: 4,
            opportunistic_charge_threshold_czk: 1.5,
        }
    }
}

/// Winter Adaptive V10 configuration - Dynamic Battery Budget Allocation
/// V10 uses unified budget allocation instead of mode-based planning:
/// - Ranks all blocks by effective price
/// - Allocates finite battery budget to most expensive blocks first
/// - Cheap blocks get GridPowered (NoChargeNoDischarge) to preserve battery
/// - Solar excess blocks stay SelfUse for natural charging
/// - No hardcoded time windows - everything driven by economics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV10ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v10_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 95%)
    #[serde(default = "default_v10_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 10%)
    #[serde(default = "default_v10_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Round-trip battery efficiency (default: 0.90)
    #[serde(default = "default_v10_efficiency")]
    pub battery_round_trip_efficiency: f32,
    /// Enable negative price handling (default: true)
    #[serde(default = "default_true")]
    pub negative_price_handling_enabled: bool,
    /// Price threshold (CZK/kWh) below which we always charge (default: 1.5)
    #[serde(default = "default_v10_opportunistic_threshold")]
    pub opportunistic_charge_threshold_czk: f32,
    /// Minimum spread for grid export (CZK) (default: 5.0)
    #[serde(default = "default_v10_min_export_spread")]
    pub min_export_spread_czk: f32,
    /// Minimum SOC after export (%) (default: 50%)
    #[serde(default = "default_v10_min_soc_after_export")]
    pub min_soc_after_export: f32,
    /// Minimum solar forecast (kWh) to consider solar excess (default: 5.0)
    #[serde(default = "default_v10_solar_threshold")]
    pub solar_threshold_kwh: f32,
    /// Factor to apply to solar forecast for conservative planning (default: 0.7)
    #[serde(default = "default_v10_solar_confidence")]
    pub solar_confidence_factor: f32,
    /// Minimum savings per kWh (block_price - avg_charge_price) to justify battery use (default: 0.5)
    #[serde(default = "default_v10_min_savings_threshold")]
    pub min_savings_threshold_czk: f32,
}

fn default_winter_adaptive_v10_priority() -> u8 {
    100
}
fn default_v10_target_battery_soc() -> f32 {
    95.0
}
fn default_v10_min_discharge_soc() -> f32 {
    10.0
}
fn default_v10_efficiency() -> f32 {
    0.90
}
fn default_v10_opportunistic_threshold() -> f32 {
    1.5
}
fn default_v10_min_export_spread() -> f32 {
    5.0
}
fn default_v10_min_soc_after_export() -> f32 {
    35.0
}
fn default_v10_solar_threshold() -> f32 {
    5.0
}
fn default_v10_solar_confidence() -> f32 {
    0.7
}
fn default_v10_min_savings_threshold() -> f32 {
    0.5
}

impl Default for WinterAdaptiveV10ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false, // V9 remains the default strategy
            priority: 100,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 5.0,
            min_soc_after_export: 35.0,
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
        }
    }
}

/// Winter Adaptive V20 configuration - Adaptive Budget Allocation
/// V20 = V10 algorithm + DayMetrics-driven parameter resolution:
/// - Volatile days: lower savings threshold, more bootstrap blocks, lower export spread
/// - Expensive days: higher savings threshold
/// - High solar: wider daylight window, higher solar confidence
/// - Low solar: tighter daylight window, lower solar confidence
/// - Tomorrow expensive: limit discharge blocks (save battery)
/// - Tomorrow cheap: reduce charge blocks (charge cheaper tomorrow)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveV20ConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_winter_adaptive_v20_priority")]
    pub priority: u8,
    /// Target battery SOC for charging (default: 95%)
    #[serde(default = "default_v20_target_battery_soc")]
    pub target_battery_soc: f32,
    /// Minimum SOC before allowing discharge (default: 10%)
    #[serde(default = "default_v20_min_discharge_soc")]
    pub min_discharge_soc: f32,
    /// Round-trip battery efficiency (default: 0.90)
    #[serde(default = "default_v20_efficiency")]
    pub battery_round_trip_efficiency: f32,
    /// Enable negative price handling (default: true)
    #[serde(default = "default_true")]
    pub negative_price_handling_enabled: bool,
    /// Price threshold (CZK/kWh) below which we always charge (default: 1.5)
    #[serde(default = "default_v20_opportunistic_threshold")]
    pub opportunistic_charge_threshold_czk: f32,
    /// Minimum spread for grid export (CZK) (default: 3.0)
    #[serde(default = "default_v20_min_export_spread")]
    pub min_export_spread_czk: f32,
    /// Minimum SOC after export (%) (default: 25%)
    #[serde(default = "default_v20_min_soc_after_export")]
    pub min_soc_after_export: f32,
    /// Minimum solar forecast (kWh) to consider solar excess (default: 5.0)
    #[serde(default = "default_v20_solar_threshold")]
    pub solar_threshold_kwh: f32,
    /// Factor to apply to solar forecast for conservative planning (default: 0.7)
    #[serde(default = "default_v20_solar_confidence")]
    pub solar_confidence_factor: f32,
    /// Minimum savings per kWh to justify battery use (default: 0.5)
    #[serde(default = "default_v20_min_savings_threshold")]
    pub min_savings_threshold_czk: f32,
    // === DayMetrics thresholds ===
    /// CV threshold above which day is considered volatile (default: 0.35)
    #[serde(default = "default_v20_volatile_cv_threshold")]
    pub volatile_cv_threshold: f32,
    /// Price level threshold above which day is considered expensive (default: 0.5)
    #[serde(default = "default_v20_expensive_level_threshold")]
    pub expensive_level_threshold: f32,
    /// Solar ratio above which solar is considered high (default: 1.1)
    #[serde(default = "default_v20_high_solar_ratio_threshold")]
    pub high_solar_ratio_threshold: f32,
    /// Solar ratio below which solar is considered low (default: 0.9)
    #[serde(default = "default_v20_low_solar_ratio_threshold")]
    pub low_solar_ratio_threshold: f32,
    /// Tomorrow price ratio above which tomorrow is expensive (default: 1.3)
    #[serde(default = "default_v20_tomorrow_expensive_ratio")]
    pub tomorrow_expensive_ratio: f32,
    /// Tomorrow price ratio below which tomorrow is cheap (default: 0.7)
    #[serde(default = "default_v20_tomorrow_cheap_ratio")]
    pub tomorrow_cheap_ratio: f32,
    /// Negative price fraction threshold for significant negative pricing (default: 0.0)
    #[serde(default = "default_v20_negative_price_fraction_threshold")]
    pub negative_price_fraction_threshold: f32,
}

fn default_winter_adaptive_v20_priority() -> u8 {
    100
}
fn default_v20_target_battery_soc() -> f32 {
    95.0
}
fn default_v20_min_discharge_soc() -> f32 {
    10.0
}
fn default_v20_efficiency() -> f32 {
    0.90
}
fn default_v20_opportunistic_threshold() -> f32 {
    1.5
}
fn default_v20_min_export_spread() -> f32 {
    3.0
}
fn default_v20_min_soc_after_export() -> f32 {
    25.0
}
fn default_v20_solar_threshold() -> f32 {
    5.0
}
fn default_v20_solar_confidence() -> f32 {
    0.7
}
fn default_v20_min_savings_threshold() -> f32 {
    0.5
}
fn default_v20_volatile_cv_threshold() -> f32 {
    0.35
}
fn default_v20_expensive_level_threshold() -> f32 {
    0.3
}
fn default_v20_high_solar_ratio_threshold() -> f32 {
    1.1
}
fn default_v20_low_solar_ratio_threshold() -> f32 {
    0.9
}
fn default_v20_tomorrow_expensive_ratio() -> f32 {
    1.3
}
fn default_v20_tomorrow_cheap_ratio() -> f32 {
    0.7
}
fn default_v20_negative_price_fraction_threshold() -> f32 {
    0.0
}

impl Default for WinterAdaptiveV20ConfigCore {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 100,
            target_battery_soc: 95.0,
            min_discharge_soc: 10.0,
            battery_round_trip_efficiency: 0.90,
            negative_price_handling_enabled: true,
            opportunistic_charge_threshold_czk: 1.5,
            min_export_spread_czk: 3.0,
            min_soc_after_export: 25.0,
            solar_threshold_kwh: 5.0,
            solar_confidence_factor: 0.7,
            min_savings_threshold_czk: 0.5,
            volatile_cv_threshold: 0.35,
            expensive_level_threshold: 0.3,
            high_solar_ratio_threshold: 1.1,
            low_solar_ratio_threshold: 0.9,
            tomorrow_expensive_ratio: 1.3,
            tomorrow_cheap_ratio: 0.7,
            negative_price_fraction_threshold: 0.0,
        }
    }
}

// ============================================================================
// Remote Access Configuration
// ============================================================================

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteAccessConfigCore {
    pub enabled: bool,
}

// ============================================================================
// Solar Forecast Configuration
// ============================================================================

fn default_total_today_pattern() -> String {
    "sensor.energy_production_today".to_string()
}

fn default_remaining_today_pattern() -> String {
    "sensor.energy_production_today_remaining".to_string()
}

fn default_tomorrow_pattern() -> String {
    "sensor.energy_production_tomorrow".to_string()
}

fn default_fetch_interval() -> u64 {
    60
}

/// Configuration for solar forecast data fetching from Home Assistant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarForecastConfigCore {
    /// Enable solar forecast fetching
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Sensor pattern for total today forecast
    #[serde(default = "default_total_today_pattern")]
    pub sensor_total_today_pattern: String,

    /// Sensor pattern for remaining today forecast
    #[serde(default = "default_remaining_today_pattern")]
    pub sensor_remaining_today_pattern: String,

    /// Sensor pattern for tomorrow forecast
    #[serde(default = "default_tomorrow_pattern")]
    pub sensor_tomorrow_pattern: String,

    /// Fetch interval in seconds (default: 60)
    #[serde(default = "default_fetch_interval")]
    pub fetch_interval_seconds: u64,
}

impl Default for SolarForecastConfigCore {
    fn default() -> Self {
        Self {
            enabled: true,
            sensor_total_today_pattern: "sensor.energy_production_today".to_string(),
            sensor_remaining_today_pattern: "sensor.energy_production_today_remaining".to_string(),
            sensor_tomorrow_pattern: "sensor.energy_production_tomorrow".to_string(),
            fetch_interval_seconds: 60,
        }
    }
}

/// Fixed Price Arbitrage strategy configuration
/// For users with fixed-price energy contracts who can sell at spot prices.
/// Charges at fixed price, discharges to grid when spot sell price spikes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedPriceArbitrageConfigCore {
    pub enabled: bool,
    /// Priority for conflict resolution (0-100, higher wins)
    #[serde(default = "default_fixed_price_arbitrage_priority")]
    pub priority: u8,
    /// Minimum spread (sell - buy) in CZK/kWh to trigger arbitrage
    #[serde(default = "default_fpa_min_profit_threshold")]
    pub min_profit_threshold_czk: f32,
}

fn default_fixed_price_arbitrage_priority() -> u8 {
    85
}

fn default_fpa_min_profit_threshold() -> f32 {
    3.0
}

impl Default for FixedPriceArbitrageConfigCore {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 85,
            min_profit_threshold_czk: 3.0,
        }
    }
}

/// All available strategy types - add new strategies here to ensure they're tracked
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StrategyType {
    WinterAdaptive,
    WinterAdaptiveV2,
    WinterAdaptiveV3,
    WinterAdaptiveV4,
    WinterAdaptiveV5,
    WinterAdaptiveV7,
    WinterAdaptiveV8,
    WinterAdaptiveV9,
    WinterAdaptiveV10,
    WinterAdaptiveV20,
    WinterPeakDischarge,
    SolarAwareCharging,
    MorningPrecharge,
    DayAheadPlanning,
    TimeAwareCharge,
    PriceArbitrage,
    SolarFirst,
    SelfUse,
    FixedPriceArbitrage,
}

impl StrategyType {
    /// Get all strategy types
    pub fn all() -> &'static [StrategyType] {
        &[
            StrategyType::WinterAdaptive,
            StrategyType::WinterAdaptiveV2,
            StrategyType::WinterAdaptiveV3,
            StrategyType::WinterAdaptiveV4,
            StrategyType::WinterAdaptiveV5,
            StrategyType::WinterAdaptiveV7,
            StrategyType::WinterAdaptiveV8,
            StrategyType::WinterAdaptiveV9,
            StrategyType::WinterAdaptiveV10,
            StrategyType::WinterAdaptiveV20,
            StrategyType::WinterPeakDischarge,
            StrategyType::SolarAwareCharging,
            StrategyType::MorningPrecharge,
            StrategyType::DayAheadPlanning,
            StrategyType::TimeAwareCharge,
            StrategyType::PriceArbitrage,
            StrategyType::SolarFirst,
            StrategyType::SelfUse,
            StrategyType::FixedPriceArbitrage,
        ]
    }

    /// Get display name for the strategy
    pub fn display_name(&self) -> &'static str {
        match self {
            StrategyType::WinterAdaptive => "Winter Adaptive",
            StrategyType::WinterAdaptiveV2 => "Winter Adaptive V2",
            StrategyType::WinterAdaptiveV3 => "Winter Adaptive V3",
            StrategyType::WinterAdaptiveV4 => "Winter Adaptive V4",
            StrategyType::WinterAdaptiveV5 => "Winter Adaptive V5",
            StrategyType::WinterAdaptiveV7 => "Winter Adaptive V7",
            StrategyType::WinterAdaptiveV8 => "Winter Adaptive V8",
            StrategyType::WinterAdaptiveV9 => "Winter Adaptive V9",
            StrategyType::WinterAdaptiveV10 => "Winter Adaptive V10",
            StrategyType::WinterAdaptiveV20 => "Winter Adaptive V20",
            StrategyType::WinterPeakDischarge => "Winter Peak Discharge",
            StrategyType::SolarAwareCharging => "Solar Aware Charging",
            StrategyType::MorningPrecharge => "Morning Precharge",
            StrategyType::DayAheadPlanning => "Day Ahead Planning",
            StrategyType::TimeAwareCharge => "Time Aware Charge",
            StrategyType::PriceArbitrage => "Price Arbitrage",
            StrategyType::SolarFirst => "Solar First",
            StrategyType::SelfUse => "Self Use",
            StrategyType::FixedPriceArbitrage => "Fixed Price Arbitrage",
        }
    }
}

/// Currency display option
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Currency {
    #[serde(rename = "EUR")]
    #[default]
    EUR,
    #[serde(rename = "USD")]
    USD,
    #[serde(rename = "CZK")]
    CZK,
}

impl Currency {
    pub fn symbol(&self) -> &'static str {
        match self {
            Currency::EUR => "€",
            Currency::USD => "$",
            Currency::CZK => "Kč",
        }
    }
}
