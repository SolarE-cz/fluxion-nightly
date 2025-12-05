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
    #[serde(default = "default_grid_distribution_fee")]
    pub grid_distribution_fee_czk: f32,
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
fn default_grid_distribution_fee() -> f32 {
    1.2
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterPeakDischargeConfigCore {
    pub enabled: bool,
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
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub midday_max_soc: f32,
    pub min_solar_forecast_kwh: f32,
}

impl Default for SolarAwareChargingConfigCore {
    fn default() -> Self {
        Self {
            enabled: true,
            solar_window_start_hour: 10,
            solar_window_end_hour: 14,
            midday_max_soc: 90.0,
            min_solar_forecast_kwh: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEnabledConfigCore {
    pub enabled: bool,
}

impl Default for StrategyEnabledConfigCore {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterAdaptiveConfigCore {
    pub enabled: bool,
    pub ema_period_days: usize,
    pub min_solar_percentage: f32,
    pub target_battery_soc: f32,
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
            enabled: true,
            ema_period_days: 7,
            min_solar_percentage: 0.10,
            target_battery_soc: 90.0,
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
