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

mod persistence;
mod validation;

pub use persistence::load_config_with_fallback;
pub use validation::ValidationResult;

use anyhow::{Context, Result};
use bevy_ecs::prelude::*;
use fluxion_i18n::Language;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

/// Main application configuration - FluxION MVP
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Inverter configurations (one or more)
    pub inverters: Vec<InverterConfig>,

    /// Pricing configuration
    pub pricing: PricingConfig,

    /// Control configuration
    pub control: ControlConfig,

    /// System configuration
    pub system: SystemConfig,

    /// Strategies configuration (seasonal and specialized strategies)
    #[serde(default)]
    pub strategies: StrategiesConfig,

    /// Consumption history configuration
    #[serde(default)]
    pub history: ConsumptionHistoryConfig,
}

/// Configuration for a single inverter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterConfig {
    /// Unique ID for this inverter
    pub id: String,

    /// Inverter type (enum defining supported inverter models)
    /// Accepts both "inverter_type" (config.toml) and "vendor" (HA addon options.json)
    #[serde(alias = "vendor")]
    pub inverter_type: fluxion_core::InverterType,

    /// Entity prefix in HA (e.g., "solax", "solax_<ip>")
    pub entity_prefix: String,

    /// Control topology: independent, master, slave
    pub topology: String,

    /// Slave inverter IDs (if topology = master)
    /// Accepts both "slaves" (config.toml) and "slave_ids" (HA addon options.json)
    #[serde(default, alias = "slave_ids")]
    pub slaves: Option<Vec<String>>,

    /// Master inverter ID (if topology = slave)
    /// Accepts both "master" (config.toml) and "master_id" (HA addon options.json)
    #[serde(alias = "master_id")]
    pub master: Option<String>,
}

/// Pricing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingConfig {
    /// HA entity ID for spot price data
    pub spot_price_entity: String,

    /// Optional separate sensor for tomorrow's prices
    #[serde(default)]
    pub tomorrow_price_entity: Option<String>,

    /// Use spot prices for buying decisions
    pub use_spot_prices_to_buy: bool,

    /// Use spot prices for selling decisions
    pub use_spot_prices_to_sell: bool,

    /// Fixed hourly buy prices (CZK/kWh) - fallback when spot disabled
    /// Can be 24 values (hourly) - will be expanded to 96 (15-min blocks)
    pub fixed_buy_prices: Vec<f32>,

    /// Fixed hourly sell prices (CZK/kWh) - fallback when spot disabled
    /// Can be 24 values (hourly) - will be expanded to 96 (15-min blocks)
    pub fixed_sell_prices: Vec<f32>,

    /// Spot market buy fee (CZK/kWh)
    #[serde(default = "default_spot_buy_fee")]
    pub spot_buy_fee: f32,

    /// Spot market sell fee (CZK/kWh)
    #[serde(default = "default_spot_sell_fee")]
    pub spot_sell_fee: f32,

    /// Grid distribution fee (CZK/kWh) - added to import price
    #[serde(default = "default_grid_distribution_fee")]
    pub spot_grid_fee: f32,
}

/// Control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    /// Maximum export power limit (watts)
    pub maximum_export_power_w: u32,

    /// Number of cheapest hours to force-charge
    pub force_charge_hours: usize,

    /// Number of most expensive hours to force-discharge
    pub force_discharge_hours: usize,

    /// Minimum battery state of charge for strategy decisions (percent)
    /// This is the target minimum SOC for strategies (e.g., when to stop discharge)
    pub min_battery_soc: f32,

    /// Maximum battery state of charge (percent)
    pub max_battery_soc: f32,

    /// Hardware minimum battery SOC limit enforced by inverter (percent)
    /// This is the absolute minimum SOC that the inverter firmware will allow.
    /// Typically lower than min_battery_soc. For Solax: from number.solax_selfuse_discharge_min_soc
    /// Default: 10.0%
    #[serde(default = "default_hardware_min_soc")]
    pub hardware_min_battery_soc: f32,

    /// Battery capacity in kWh (user-configurable)
    #[serde(default = "default_battery_capacity")]
    pub battery_capacity_kwh: f32,

    /// Maximum battery charge rate in kW used for planning how many
    /// 15-minute charge blocks are required to reach the target SOC.
    /// This should match the inverter's actual sustained charge power.
    /// Typical values: 3-10 kW depending on system size.
    #[serde(default = "default_max_battery_charge_rate_kw")]
    pub max_battery_charge_rate_kw: f32,

    /// Battery wear cost per kWh cycled (CZK/kWh)
    /// This represents the cost of battery degradation
    /// Example: 23 kWh battery with 6000 cycles and 115,000 CZK cost
    /// = 115000 / (23 * 6000) = 0.833 CZK per full cycle = ~0.036 CZK/kWh per half-cycle
    /// Conservative estimate: 0.125 CZK/kWh
    #[serde(default = "default_battery_wear_cost")]
    pub battery_wear_cost_czk_per_kwh: f32,

    /// Battery round-trip efficiency (0.0 to 1.0)
    /// Typical lithium-ion batteries: 0.90-0.95
    #[serde(default = "default_battery_efficiency")]
    pub battery_efficiency: f32,

    /// Minimum time between mode changes (seconds) to prevent rapid switching
    /// Default: 300 seconds (5 minutes)
    /// Minimum allowed: 60 seconds (1 minute)
    #[serde(default = "default_min_mode_change_interval")]
    pub min_mode_change_interval_secs: u64,

    /// Average household consumption (kW) used for battery SOC predictions
    /// Used as fallback when actual load data is not available
    /// Typical values: 0.3-1.0 kW depending on household size
    #[serde(default = "default_average_household_load")]
    pub average_household_load_kw: f32,

    /// Minimum number of consecutive 15-minute blocks required for force-charge/discharge operations
    /// Single-block force operations can cause excessive inverter EEPROM writes.
    /// Default: 2 blocks (30 minutes minimum duration)
    /// Set to 1 to allow single-block operations (not recommended)
    /// Set to 4 for 1-hour minimum duration
    #[serde(default = "default_min_consecutive_force_blocks")]
    pub min_consecutive_force_blocks: usize,

    /// Default battery operation mode when not force charging/discharging
    /// Options: "SelfUse" (default) or "BackUpMode" (Solax-specific)
    /// - SelfUse: Normal self-consumption, battery used to minimize grid import
    /// - BackUpMode: Prioritize battery reserve for power outages (Solax only)
    #[serde(default = "default_battery_mode")]
    pub default_battery_mode: String,
}

fn default_battery_capacity() -> f32 {
    23.0 // Typical home battery capacity
}

fn default_battery_wear_cost() -> f32 {
    0.125 // CZK per kWh cycled (conservative estimate)
}

fn default_battery_efficiency() -> f32 {
    0.95 // 95% round-trip efficiency
}

fn default_min_mode_change_interval() -> u64 {
    300 // 5 minutes default debounce interval
}

fn default_average_household_load() -> f32 {
    0.5 // 500W typical household consumption
}

fn default_hardware_min_soc() -> f32 {
    10.0 // Typical hardware minimum SOC enforced by inverter firmware
}

fn default_max_battery_charge_rate_kw() -> f32 {
    5.0 // Conservative default; should be set to actual inverter charge power
}

fn default_min_consecutive_force_blocks() -> usize {
    2 // Require at least 2 consecutive blocks (30 minutes) for force operations
}

fn default_battery_mode() -> String {
    "SelfUse".to_string() // Default to self-use mode for backward compatibility
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

/// System configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    /// Debug mode (default: true for safety)
    pub debug_mode: bool,

    /// Update interval (seconds)
    pub update_interval_secs: u64,

    /// Log level (debug, info, warn, error)
    pub log_level: String,

    /// Home Assistant base URL (optional, defaults to supervisor)
    pub ha_base_url: Option<String>,

    /// Home Assistant token (optional, uses SUPERVISOR_TOKEN if not set)
    pub ha_token: Option<String>,

    /// Display currency (EUR, USD, or CZK)
    #[serde(default)]
    pub display_currency: String,

    /// UI language
    #[serde(default)]
    pub language: Language,

    /// Home Assistant timezone (fetched from HA at runtime)
    #[serde(skip)]
    pub timezone: Option<String>,
}

/// Strategies configuration root
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StrategiesConfig {
    #[serde(default)]
    pub winter_adaptive: WinterAdaptiveConfig,
    #[serde(default)]
    pub winter_peak_discharge: WinterPeakDischargeConfig,
    #[serde(default)]
    pub solar_aware_charging: SolarAwareChargingConfig,
    #[serde(default)]
    pub morning_precharge: StrategyEnabledConfig,
    #[serde(default)]
    pub day_ahead_planning: StrategyEnabledConfig,
    #[serde(default)]
    pub time_aware_charge: StrategyEnabledConfig,
    #[serde(default)]
    pub price_arbitrage: StrategyEnabledConfig,
    #[serde(default)]
    pub solar_first: StrategyEnabledConfig,
    #[serde(default)]
    pub self_use: StrategyEnabledConfig,
    #[serde(default)]
    pub seasonal: SeasonalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WinterAdaptiveConfig {
    pub enabled: bool,
    pub ema_period_days: usize,
    pub min_solar_percentage: f32,
    pub target_battery_soc: f32,
    pub top_expensive_blocks: usize,
    pub tomorrow_preservation_threshold: f32,
    pub grid_export_price_threshold: f32,
    pub min_soc_for_export: f32,
    pub export_trigger_multiplier: f32,
    pub negative_price_handling_enabled: bool,
    pub charge_on_negative_even_if_full: bool,
}

impl Default for WinterAdaptiveConfig {
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinterPeakDischargeConfig {
    pub enabled: bool,
    pub min_spread_czk: f32,
    pub min_soc_to_start: f32,
    pub min_soc_target: f32,
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub min_hours_to_solar: u32,
}

impl Default for WinterPeakDischargeConfig {
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
pub struct SolarAwareChargingConfig {
    pub enabled: bool,
    pub solar_window_start_hour: u32,
    pub solar_window_end_hour: u32,
    pub midday_max_soc: f32,
    pub min_solar_forecast_kwh: f32,
}

impl Default for SolarAwareChargingConfig {
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
pub struct StrategyEnabledConfig {
    pub enabled: bool,
}

impl Default for StrategyEnabledConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionHistoryConfig {
    /// Home Assistant entity ID for daily consumption (e.g., "sensor.solax_today_s_import_energy")
    #[serde(default = "default_consumption_entity")]
    pub consumption_entity: String,

    /// Home Assistant entity ID for daily solar production (e.g., "sensor.energy_production_today")
    #[serde(default = "default_solar_production_entity")]
    pub solar_production_entity: String,

    /// Number of days to track for EMA calculation
    #[serde(default = "default_ema_days")]
    pub ema_days: usize,

    /// Number of days to track for seasonal mode detection
    #[serde(default = "default_seasonal_detection_days")]
    pub seasonal_detection_days: usize,
}

impl Default for ConsumptionHistoryConfig {
    fn default() -> Self {
        Self {
            consumption_entity: default_consumption_entity(),
            solar_production_entity: default_solar_production_entity(),
            ema_days: default_ema_days(),
            seasonal_detection_days: default_seasonal_detection_days(),
        }
    }
}

fn default_consumption_entity() -> String {
    "sensor.solax_today_s_import_energy".to_string()
}

fn default_solar_production_entity() -> String {
    "sensor.energy_production_today".to_string()
}

fn default_ema_days() -> usize {
    7
}

fn default_seasonal_detection_days() -> usize {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SeasonalConfig {
    pub force_season: Option<String>,
}

impl Default for AppConfig {
    /// Default configuration for single Solax inverter
    fn default() -> Self {
        Self {
            inverters: vec![InverterConfig {
                id: "main_inverter".to_string(),
                inverter_type: fluxion_core::InverterType::Solax,
                entity_prefix: "solax".to_string(),
                topology: "independent".to_string(),
                slaves: None,
                master: None,
            }],
            pricing: PricingConfig {
                spot_price_entity: "sensor.current_spot_electricity_price_15min".to_string(),
                tomorrow_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_prices: vec![0.05; 24], // 24 hourly values
                fixed_sell_prices: vec![0.08; 24],
                spot_buy_fee: default_spot_buy_fee(),
                spot_sell_fee: default_spot_sell_fee(),
                spot_grid_fee: default_grid_distribution_fee(),
            },
            control: ControlConfig {
                maximum_export_power_w: 5000,
                force_charge_hours: 4,
                force_discharge_hours: 2,
                min_battery_soc: 10.0,
                max_battery_soc: 100.0,
                battery_capacity_kwh: default_battery_capacity(),
                max_battery_charge_rate_kw: default_max_battery_charge_rate_kw(),
                battery_wear_cost_czk_per_kwh: default_battery_wear_cost(),
                battery_efficiency: default_battery_efficiency(),
                min_mode_change_interval_secs: default_min_mode_change_interval(),
                average_household_load_kw: default_average_household_load(),
                hardware_min_battery_soc: default_hardware_min_soc(),
                min_consecutive_force_blocks: default_min_consecutive_force_blocks(),
                default_battery_mode: default_battery_mode(),
            },
            system: SystemConfig {
                debug_mode: true, // Safe default
                update_interval_secs: 60,
                log_level: "info".to_string(),
                ha_base_url: None,
                ha_token: None,
                display_currency: "EUR".to_string(),
                language: Language::English,
                timezone: None, // Will be fetched from HA at runtime
            },
            strategies: StrategiesConfig::default(),
            history: ConsumptionHistoryConfig::default(),
        }
    }
}

impl AppConfig {
    /// Load configuration from HA addon options or config file
    pub fn load() -> Result<Self> {
        // Try HA addon options first (/data/options.json)
        if let Ok(options_str) = std::fs::read_to_string("/data/options.json") {
            let config: AppConfig =
                serde_json::from_str(&options_str).context("Failed to parse HA addon options")?;
            info!("✅ Loaded configuration from HA addon options");
            config.validate()?;
            return Ok(config);
        }

        // Try config.toml for development
        if let Ok(config_str) = std::fs::read_to_string("config.toml") {
            let config: AppConfig =
                toml::from_str(&config_str).context("Failed to parse config.toml")?;
            info!("✅ Loaded configuration from config.toml");
            config.validate()?;
            return Ok(config);
        }

        // Try config.json for development
        if let Ok(config_str) = std::fs::read_to_string("config.json") {
            let config: AppConfig =
                serde_json::from_str(&config_str).context("Failed to parse config.json")?;
            info!("✅ Loaded configuration from config.json");
            config.validate()?;
            return Ok(config);
        }

        // Fall back to defaults with environment variable overrides
        warn!("No configuration file found, using defaults with environment overrides");
        let config = Self::from_env();
        config.validate()?;
        Ok(config)
    }

    /// Load from environment variables (development/testing)
    fn from_env() -> Self {
        let mut config = Self::default();

        // Override spot price entity
        if let Ok(entity) = std::env::var("SPOT_PRICE_ENTITY") {
            config.pricing.spot_price_entity = entity;
        }

        // Override debug mode
        if let Ok(debug_mode) = std::env::var("DEBUG_MODE")
            && let Ok(enabled) = debug_mode.parse::<bool>()
        {
            config.system.debug_mode = enabled;
        }

        // Override update interval
        if let Ok(interval) = std::env::var("UPDATE_INTERVAL_SECS")
            && let Ok(secs) = interval.parse::<u64>()
        {
            config.system.update_interval_secs = secs;
        }

        // Override HA connection
        if let Ok(url) = std::env::var("HA_BASE_URL") {
            config.system.ha_base_url = Some(url);
        }
        if let Ok(token) = std::env::var("HA_TOKEN") {
            config.system.ha_token = Some(token);
        }

        config
    }

    /// Validate configuration with detailed error reporting
    pub fn validate_detailed(&self) -> ValidationResult {
        let mut result = ValidationResult::success();

        // Validate inverters
        if self.inverters.is_empty() {
            result.add_error(
                "inverters",
                "Configuration must include at least one inverter",
            );
            return result; // Early return if no inverters
        }

        for (idx, inverter) in self.inverters.iter().enumerate() {
            let prefix = format!("inverters[{idx}]");

            if inverter.id.is_empty() {
                result.add_error(format!("{prefix}.id"), "Inverter ID cannot be empty");
            }
            if inverter.entity_prefix.is_empty() {
                result.add_error(
                    format!("{prefix}.entity_prefix"),
                    "Entity prefix cannot be empty",
                );
            }

            // Validate topology relationships
            match inverter.topology.as_str() {
                "master" => {
                    if inverter.slaves.is_none() || inverter.slaves.as_ref().unwrap().is_empty() {
                        result.add_error(
                            format!("{prefix}.slaves"),
                            "Master inverter must have at least one slave configured",
                        );
                    }
                }
                "slave" => {
                    if inverter.master.is_none() {
                        result.add_error(
                            format!("{prefix}.master"),
                            "Slave inverter must have a master configured",
                        );
                    }
                }
                "independent" => {}
                _ => {
                    result.add_error(
                        format!("{prefix}.topology"),
                        format!(
                            "Invalid topology '{}' (must be: independent, master, or slave)",
                            inverter.topology
                        ),
                    );
                }
            }
        }

        // Validate pricing
        if self.pricing.spot_price_entity.is_empty() {
            result.add_error(
                "pricing.spot_price_entity",
                "Spot price entity cannot be empty",
            );
        }

        // Validate fixed prices
        if !self.pricing.fixed_buy_prices.is_empty()
            && self.pricing.fixed_buy_prices.len() != 24
            && self.pricing.fixed_buy_prices.len() != 96
        {
            result.add_error(
                "pricing.fixed_buy_prices",
                format!(
                    "Must have 24 (hourly) or 96 (15-min) values, got {}",
                    self.pricing.fixed_buy_prices.len()
                ),
            );
        }
        if !self.pricing.fixed_sell_prices.is_empty()
            && self.pricing.fixed_sell_prices.len() != 24
            && self.pricing.fixed_sell_prices.len() != 96
        {
            result.add_error(
                "pricing.fixed_sell_prices",
                format!(
                    "Must have 24 (hourly) or 96 (15-min) values, got {}",
                    self.pricing.fixed_sell_prices.len()
                ),
            );
        }

        // Validate control parameters
        // CRITICAL: maximum_export_power_w must be set correctly to avoid grid penalties
        if self.control.maximum_export_power_w == 0 {
            result.add_error(
                "control.maximum_export_power_w",
                "CRITICAL: Maximum export power must be configured! Using wrong value can result in heavy penalties for exceeding your allowed grid export limit. Set this to your contracted maximum export power in watts.",
            );
        }

        if self.control.min_battery_soc < 0.0 || self.control.min_battery_soc > 100.0 {
            result.add_error("control.min_battery_soc", "Must be between 0 and 100");
        }
        if self.control.max_battery_soc < 0.0 || self.control.max_battery_soc > 100.0 {
            result.add_error("control.max_battery_soc", "Must be between 0 and 100");
        }
        if self.control.min_battery_soc >= self.control.max_battery_soc {
            result.add_error(
                "control.min_battery_soc",
                "Must be less than max_battery_soc",
            );
        }

        // Validate battery economic parameters
        if self.control.battery_capacity_kwh <= 0.0 {
            result.add_error("control.battery_capacity_kwh", "Must be positive");
        }
        if self.control.battery_wear_cost_czk_per_kwh < 0.0 {
            result.add_error(
                "control.battery_wear_cost_czk_per_kwh",
                "Must be non-negative",
            );
        }
        if self.control.battery_efficiency <= 0.0 || self.control.battery_efficiency > 1.0 {
            result.add_error("control.battery_efficiency", "Must be between 0.0 and 1.0");
        }

        // Validate mode change interval
        if self.control.min_mode_change_interval_secs < 60 {
            result.add_error(
                "control.min_mode_change_interval_secs",
                "Must be at least 60 seconds (1 minute)",
            );
        }
        if self.control.min_mode_change_interval_secs > 3600 {
            result.add_warning(
                "control.min_mode_change_interval_secs",
                format!(
                    "Value is very high ({}s), consider reducing",
                    self.control.min_mode_change_interval_secs
                ),
            );
        }

        // Validate force operation hours
        if self.control.force_charge_hours == 0 {
            result.add_warning(
                "control.force_charge_hours",
                "No charging will be scheduled",
            );
        }
        if self.control.force_discharge_hours == 0 {
            result.add_warning(
                "control.force_discharge_hours",
                "No discharging will be scheduled",
            );
        }

        // Validate system
        if self.system.update_interval_secs < 10 {
            result.add_error("system.update_interval_secs", "Must be at least 10 seconds");
        }
        if self.system.update_interval_secs > 600 {
            result.add_warning(
                "system.update_interval_secs",
                format!(
                    "Value is very high ({}s), consider reducing",
                    self.system.update_interval_secs
                ),
            );
        }

        result
    }

    /// Validate configuration (returns simple Result for backward compatibility)
    pub fn validate(&self) -> Result<()> {
        // Validate at least one inverter
        if self.inverters.is_empty() {
            anyhow::bail!("Configuration must include at least one inverter");
        }

        // Validate inverter configs
        for (idx, inverter) in self.inverters.iter().enumerate() {
            if inverter.id.is_empty() {
                anyhow::bail!("Inverter {} has empty ID", idx);
            }
            if inverter.entity_prefix.is_empty() {
                anyhow::bail!("Inverter '{}' has empty entity_prefix", inverter.id);
            }

            // Validate topology relationships
            match inverter.topology.as_str() {
                "master" => {
                    if inverter.slaves.is_none() || inverter.slaves.as_ref().unwrap().is_empty() {
                        anyhow::bail!(
                            "Inverter '{}' is configured as master but has no slaves",
                            inverter.id
                        );
                    }
                }
                "slave" => {
                    if inverter.master.is_none() {
                        anyhow::bail!(
                            "Inverter '{}' is configured as slave but has no master",
                            inverter.id
                        );
                    }
                }
                "independent" => {}
                _ => {
                    anyhow::bail!(
                        "Inverter '{}' has invalid topology: '{}' (must be: independent, master, or slave)",
                        inverter.id,
                        inverter.topology
                    );
                }
            }
        }

        // Validate pricing
        if self.pricing.spot_price_entity.is_empty() {
            anyhow::bail!("spot_price_entity cannot be empty");
        }

        // Validate fixed prices (must be 24 or 96 values)
        if !self.pricing.fixed_buy_prices.is_empty()
            && self.pricing.fixed_buy_prices.len() != 24
            && self.pricing.fixed_buy_prices.len() != 96
        {
            anyhow::bail!(
                "fixed_buy_prices must have 24 (hourly) or 96 (15-min) values, got {}",
                self.pricing.fixed_buy_prices.len()
            );
        }
        if !self.pricing.fixed_sell_prices.is_empty()
            && self.pricing.fixed_sell_prices.len() != 24
            && self.pricing.fixed_sell_prices.len() != 96
        {
            anyhow::bail!(
                "fixed_sell_prices must have 24 (hourly) or 96 (15-min) values, got {}",
                self.pricing.fixed_sell_prices.len()
            );
        }

        // Validate control parameters
        // CRITICAL: maximum_export_power_w must be set correctly to avoid grid penalties
        if self.control.maximum_export_power_w == 0 {
            anyhow::bail!(
                "\n\n🚨 CRITICAL CONFIGURATION ERROR 🚨\n\n\
                maximum_export_power_w is not configured!\n\n\
                You MUST set this value to your contracted maximum grid export power (in watts).\n\
                Using an incorrect value can result in HEAVY FINANCIAL PENALTIES for exceeding\n\
                your allowed export limit.\n\n\
                Example: If your contract allows 10 kW export, set maximum_export_power_w: 10000\n\n\
                Please configure this value in the Home Assistant addon options and restart.\n"
            );
        }

        if self.control.min_battery_soc < 0.0 || self.control.min_battery_soc > 100.0 {
            anyhow::bail!("min_battery_soc must be between 0 and 100");
        }
        if self.control.max_battery_soc < 0.0 || self.control.max_battery_soc > 100.0 {
            anyhow::bail!("max_battery_soc must be between 0 and 100");
        }
        if self.control.min_battery_soc >= self.control.max_battery_soc {
            anyhow::bail!("min_battery_soc must be less than max_battery_soc");
        }

        // Validate battery economic parameters
        if self.control.battery_capacity_kwh <= 0.0 {
            anyhow::bail!("battery_capacity_kwh must be positive");
        }
        if self.control.max_battery_charge_rate_kw <= 0.0 {
            anyhow::bail!("max_battery_charge_rate_kw must be positive");
        }
        if self.control.battery_wear_cost_czk_per_kwh < 0.0 {
            anyhow::bail!("battery_wear_cost_czk_per_kwh must be non-negative");
        }
        if self.control.battery_efficiency <= 0.0 || self.control.battery_efficiency > 1.0 {
            anyhow::bail!("battery_efficiency must be between 0.0 and 1.0");
        }

        // Validate mode change interval
        if self.control.min_mode_change_interval_secs < 60 {
            anyhow::bail!("min_mode_change_interval_secs must be at least 60 seconds (1 minute)");
        }
        if self.control.min_mode_change_interval_secs > 3600 {
            warn!(
                "min_mode_change_interval_secs is very high ({}s), consider reducing",
                self.control.min_mode_change_interval_secs
            );
        }

        if self.control.force_charge_hours == 0 {
            warn!("force_charge_hours is 0 - no charging will be scheduled");
        }
        if self.control.force_discharge_hours == 0 {
            warn!("force_discharge_hours is 0 - no discharging will be scheduled");
        }

        // Note: charge planning parameters (max_battery_charge_rate_kw, evening_target_soc, evening_peak_start_hour)
        // use serde defaults and are validated by the core scheduler

        // Validate system
        if self.system.update_interval_secs < 10 {
            anyhow::bail!("update_interval_secs must be at least 10 seconds");
        }
        if self.system.update_interval_secs > 600 {
            warn!(
                "update_interval_secs is very high ({}s), consider reducing",
                self.system.update_interval_secs
            );
        }

        Ok(())
    }

    /// Save current configuration to file
    ///
    /// Future use: Will enable runtime configuration changes via Web UI
    /// Currently used in tests to verify serialization/deserialization
    #[allow(dead_code)]
    pub fn save(&self, path: &str) -> Result<()> {
        let toml_str = toml::to_string_pretty(self)?;
        std::fs::write(path, toml_str)?;
        info!("Configuration saved to {}", path);
        Ok(())
    }

    /// Get update interval as Duration
    ///
    /// Helper method for converting update_interval_secs to Duration type
    /// Currently used in tests; may be used by future runtime config updates
    #[allow(dead_code)]
    pub fn update_interval(&self) -> Duration {
        Duration::from_secs(self.system.update_interval_secs)
    }

    /// Check if running in debug mode
    ///
    /// Helper method for debug mode status
    /// Currently used in tests; may be used by future conditional features
    #[allow(dead_code)]
    pub fn is_debug_mode(&self) -> bool {
        self.system.debug_mode
    }
}

/// Convert AppConfig to fluxion_core::SystemConfig
impl From<AppConfig> for fluxion_core::SystemConfig {
    fn from(app_config: AppConfig) -> Self {
        // Parse currency string, default to EUR if invalid
        let display_currency = match app_config.system.display_currency.to_uppercase().as_str() {
            "EUR" => fluxion_core::Currency::EUR,
            "USD" => fluxion_core::Currency::USD,
            "CZK" => fluxion_core::Currency::CZK,
            _ => {
                tracing::warn!(
                    "Invalid display_currency '{}', defaulting to EUR",
                    app_config.system.display_currency
                );
                fluxion_core::Currency::EUR
            }
        };

        fluxion_core::SystemConfig {
            inverters: app_config
                .inverters
                .iter()
                .map(|inv| fluxion_core::InverterConfig {
                    id: inv.id.clone(),
                    inverter_type: inv.inverter_type,
                    entity_prefix: inv.entity_prefix.clone(),
                    topology: match inv.topology.as_str() {
                        "master" => fluxion_core::InverterTopology::Master {
                            slave_ids: inv.slaves.clone().unwrap_or_default(),
                        },
                        "slave" => fluxion_core::InverterTopology::Slave {
                            master_id: inv.master.clone().unwrap_or_default(),
                        },
                        _ => fluxion_core::InverterTopology::Independent,
                    },
                })
                .collect(),
            pricing_config: fluxion_core::PricingConfig {
                spot_price_entity: app_config.pricing.spot_price_entity,
                tomorrow_price_entity: app_config.pricing.tomorrow_price_entity,
                use_spot_prices_to_buy: app_config.pricing.use_spot_prices_to_buy,
                use_spot_prices_to_sell: app_config.pricing.use_spot_prices_to_sell,
                fixed_buy_price_czk: if app_config.pricing.fixed_buy_prices.len() == 1 {
                    fluxion_core::PriceSchedule::Flat(app_config.pricing.fixed_buy_prices[0])
                } else if !app_config.pricing.fixed_buy_prices.is_empty() {
                    // If we have 96 values, we should probably downsample or handle it,
                    // but for now let's just take the first 24 if it's > 24 to match hourly expectation
                    // or just pass it all if PriceSchedule is robust.
                    // Given PriceSchedule expects hourly for now, let's take every 4th if len == 96
                    if app_config.pricing.fixed_buy_prices.len() == 96 {
                        let hourly: Vec<f32> = app_config
                            .pricing
                            .fixed_buy_prices
                            .iter()
                            .step_by(4)
                            .copied()
                            .collect();
                        fluxion_core::PriceSchedule::Hourly(hourly)
                    } else {
                        fluxion_core::PriceSchedule::Hourly(
                            app_config.pricing.fixed_buy_prices.clone(),
                        )
                    }
                } else {
                    fluxion_core::PriceSchedule::Flat(5.00)
                },
                fixed_sell_price_czk: if app_config.pricing.fixed_sell_prices.len() == 1 {
                    fluxion_core::PriceSchedule::Flat(app_config.pricing.fixed_sell_prices[0])
                } else if !app_config.pricing.fixed_sell_prices.is_empty() {
                    if app_config.pricing.fixed_sell_prices.len() == 96 {
                        let hourly: Vec<f32> = app_config
                            .pricing
                            .fixed_sell_prices
                            .iter()
                            .step_by(4)
                            .copied()
                            .collect();
                        fluxion_core::PriceSchedule::Hourly(hourly)
                    } else {
                        fluxion_core::PriceSchedule::Hourly(
                            app_config.pricing.fixed_sell_prices.clone(),
                        )
                    }
                } else {
                    fluxion_core::PriceSchedule::Flat(2.00)
                },
                spot_buy_fee_czk: app_config.pricing.spot_buy_fee,
                spot_sell_fee_czk: app_config.pricing.spot_sell_fee,
                grid_distribution_fee_czk: app_config.pricing.spot_grid_fee,
            },
            control_config: fluxion_core::ControlConfig {
                force_charge_hours: app_config.control.force_charge_hours,
                force_discharge_hours: app_config.control.force_discharge_hours,
                min_battery_soc: app_config.control.min_battery_soc,
                max_battery_soc: app_config.control.max_battery_soc,
                maximum_export_power_w: app_config.control.maximum_export_power_w,
                battery_capacity_kwh: app_config.control.battery_capacity_kwh,
                battery_wear_cost_czk_per_kwh: app_config.control.battery_wear_cost_czk_per_kwh,
                battery_efficiency: app_config.control.battery_efficiency,
                min_mode_change_interval_secs: app_config.control.min_mode_change_interval_secs,
                average_household_load_kw: app_config.control.average_household_load_kw,
                hardware_min_battery_soc: app_config.control.hardware_min_battery_soc,
                grid_export_fee_czk_per_kwh: 0.5, // Fixed export fee (TODO: make configurable)
                max_battery_charge_rate_kw: app_config.control.max_battery_charge_rate_kw,
                evening_target_soc: app_config.strategies.winter_adaptive.target_battery_soc,
                evening_peak_start_hour: 17, // Default value
                min_consecutive_force_blocks: app_config.control.min_consecutive_force_blocks,
                default_battery_mode: match app_config
                    .control
                    .default_battery_mode
                    .to_uppercase()
                    .as_str()
                {
                    "BACKUPMODE" | "BACKUP" | "BACK_UP_MODE" => {
                        fluxion_core::InverterOperationMode::BackUpMode
                    }
                    _ => fluxion_core::InverterOperationMode::SelfUse, // Default or "SELFUSE"
                },
            },
            system_config: fluxion_core::SystemSettingsConfig {
                update_interval_secs: app_config.system.update_interval_secs,
                debug_mode: app_config.system.debug_mode,
                display_currency,
                language: app_config.system.language,
                timezone: app_config.system.timezone,
            },
            strategies_config: fluxion_core::StrategiesConfigCore {
                winter_adaptive: fluxion_core::WinterAdaptiveConfigCore {
                    enabled: app_config.strategies.winter_adaptive.enabled,
                    ema_period_days: app_config.strategies.winter_adaptive.ema_period_days,
                    min_solar_percentage: app_config
                        .strategies
                        .winter_adaptive
                        .min_solar_percentage,
                    target_battery_soc: app_config.strategies.winter_adaptive.target_battery_soc,
                    top_expensive_blocks: app_config
                        .strategies
                        .winter_adaptive
                        .top_expensive_blocks,
                    tomorrow_preservation_threshold: app_config
                        .strategies
                        .winter_adaptive
                        .tomorrow_preservation_threshold,
                    grid_export_price_threshold: app_config
                        .strategies
                        .winter_adaptive
                        .grid_export_price_threshold,
                    min_soc_for_export: app_config.strategies.winter_adaptive.min_soc_for_export,
                    export_trigger_multiplier: app_config
                        .strategies
                        .winter_adaptive
                        .export_trigger_multiplier,
                    negative_price_handling_enabled: app_config
                        .strategies
                        .winter_adaptive
                        .negative_price_handling_enabled,
                    charge_on_negative_even_if_full: app_config
                        .strategies
                        .winter_adaptive
                        .charge_on_negative_even_if_full,
                    historical_daily_consumption: Vec::new(),
                },
                winter_peak_discharge: fluxion_core::WinterPeakDischargeConfigCore {
                    enabled: app_config.strategies.winter_peak_discharge.enabled,
                    min_spread_czk: app_config.strategies.winter_peak_discharge.min_spread_czk,
                    min_soc_to_start: app_config.strategies.winter_peak_discharge.min_soc_to_start,
                    min_soc_target: app_config.strategies.winter_peak_discharge.min_soc_target,
                    solar_window_start_hour: app_config
                        .strategies
                        .winter_peak_discharge
                        .solar_window_start_hour,
                    solar_window_end_hour: app_config
                        .strategies
                        .winter_peak_discharge
                        .solar_window_end_hour,
                    min_hours_to_solar: app_config
                        .strategies
                        .winter_peak_discharge
                        .min_hours_to_solar,
                },
                solar_aware_charging: fluxion_core::SolarAwareChargingConfigCore {
                    enabled: app_config.strategies.solar_aware_charging.enabled,
                    solar_window_start_hour: app_config
                        .strategies
                        .solar_aware_charging
                        .solar_window_start_hour,
                    solar_window_end_hour: app_config
                        .strategies
                        .solar_aware_charging
                        .solar_window_end_hour,
                    midday_max_soc: app_config.strategies.solar_aware_charging.midday_max_soc,
                    min_solar_forecast_kwh: app_config
                        .strategies
                        .solar_aware_charging
                        .min_solar_forecast_kwh,
                },
                morning_precharge: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.morning_precharge.enabled,
                },
                day_ahead_planning: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.day_ahead_planning.enabled,
                },
                time_aware_charge: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.time_aware_charge.enabled,
                },
                price_arbitrage: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.price_arbitrage.enabled,
                },
                solar_first: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.solar_first.enabled,
                },
                self_use: fluxion_core::StrategyEnabledConfigCore {
                    enabled: app_config.strategies.self_use.enabled,
                },
            },
            history: fluxion_core::ConsumptionHistoryConfig {
                consumption_entity: app_config.history.consumption_entity,
                solar_production_entity: app_config.history.solar_production_entity,
                ema_days: app_config.history.ema_days,
                seasonal_detection_days: app_config.history.seasonal_detection_days,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();

        assert_eq!(config.inverters.len(), 1);
        assert_eq!(config.inverters[0].id, "main_inverter");
        assert_eq!(
            config.inverters[0].inverter_type,
            fluxion_core::InverterType::Solax
        );
        assert_eq!(config.inverters[0].topology, "independent");

        assert!(config.system.debug_mode);
        assert_eq!(config.system.update_interval_secs, 60);

        // Validation should pass on default
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_inverters() {
        let mut config = AppConfig::default();
        config.inverters.clear();

        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least one inverter")
        );
    }

    #[test]
    fn test_validate_master_without_slaves() {
        let mut config = AppConfig::default();
        config.inverters[0].topology = "master".to_string();
        config.inverters[0].slaves = None;

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_slave_without_master() {
        let mut config = AppConfig::default();
        config.inverters[0].topology = "slave".to_string();
        config.inverters[0].master = None;

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_topology() {
        let mut config = AppConfig::default();
        config.inverters[0].topology = "invalid".to_string();

        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("invalid topology")
        );
    }

    #[test]
    fn test_validate_invalid_soc_range() {
        let mut config = AppConfig::default();
        config.control.min_battery_soc = 80.0;
        config.control.max_battery_soc = 70.0;

        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("min_battery_soc must be less than")
        );
    }

    #[test]
    fn test_validate_soc_out_of_range() {
        let mut config = AppConfig::default();
        config.control.min_battery_soc = -10.0;

        assert!(config.validate().is_err());

        config.control.min_battery_soc = 10.0;
        config.control.max_battery_soc = 110.0;

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_fixed_prices_wrong_length() {
        let mut config = AppConfig::default();
        config.pricing.fixed_buy_prices = vec![0.05; 12]; // Invalid: not 24 or 96

        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("24 (hourly) or 96 (15-min)")
        );
    }

    #[test]
    fn test_validate_fixed_prices_valid_lengths() {
        let mut config = AppConfig::default();

        // 24 values (hourly) - valid
        config.pricing.fixed_buy_prices = vec![0.05; 24];
        config.pricing.fixed_sell_prices = vec![0.08; 24];
        assert!(config.validate().is_ok());

        // 96 values (15-min blocks) - valid
        config.pricing.fixed_buy_prices = vec![0.05; 96];
        config.pricing.fixed_sell_prices = vec![0.08; 96];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_update_interval_too_low() {
        let mut config = AppConfig::default();
        config.system.update_interval_secs = 5;

        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least 10 seconds")
        );
    }

    #[test]
    fn test_update_interval_duration() {
        let config = AppConfig::default();
        let duration = config.update_interval();

        assert_eq!(duration, Duration::from_secs(60));
    }

    #[test]
    fn test_is_debug_mode() {
        let mut config = AppConfig::default();
        assert!(config.is_debug_mode());

        config.system.debug_mode = false;
        assert!(!config.is_debug_mode());
    }

    #[test]
    fn test_toml_serialization() {
        let config = AppConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();

        // Deserialize back
        let deserialized: AppConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.inverters[0].id, deserialized.inverters[0].id);
        assert_eq!(config.system.debug_mode, deserialized.system.debug_mode);
    }

    #[test]
    fn test_json_serialization() {
        let config = AppConfig::default();
        let json_str = serde_json::to_string_pretty(&config).unwrap();

        // Deserialize back
        let deserialized: AppConfig = serde_json::from_str(&json_str).unwrap();

        assert_eq!(config.inverters[0].id, deserialized.inverters[0].id);
        assert_eq!(config.system.debug_mode, deserialized.system.debug_mode);
    }

    #[test]
    fn test_multi_inverter_config() {
        let config = AppConfig {
            inverters: vec![
                InverterConfig {
                    id: "master".to_string(),
                    inverter_type: fluxion_core::InverterType::Solax,
                    entity_prefix: "solax_1".to_string(),
                    topology: "master".to_string(),
                    slaves: Some(vec!["slave_1".to_string()]),
                    master: None,
                },
                InverterConfig {
                    id: "slave_1".to_string(),
                    inverter_type: fluxion_core::InverterType::Solax,
                    entity_prefix: "solax_2".to_string(),
                    topology: "slave".to_string(),
                    slaves: None,
                    master: Some("master".to_string()),
                },
            ],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    /// Test that the HA addon options.json format can be correctly parsed into AppConfig.
    /// This test validates that the field names used in fluxion/config.yaml match our Rust structs.
    /// The HA addon uses slightly different field names (e.g., "vendor" instead of "inverter_type"),
    /// so we rely on serde aliases for compatibility.
    #[test]
    fn test_ha_addon_options_format() {
        // This JSON matches the structure of /data/options.json as defined in fluxion/config.yaml
        let ha_addon_json = r#"{
            "inverters": [
                {
                    "id": "solax",
                    "vendor": "solax",
                    "entity_prefix": "solax",
                    "topology": "independent"
                }
            ],
            "pricing": {
                "spot_price_entity": "sensor.current_spot_electricity_price_15min",
                "use_spot_prices_to_buy": true,
                "use_spot_prices_to_sell": true,
                "fixed_buy_prices": [],
                "fixed_sell_prices": []
            },
            "control": {
                "maximum_export_power_w": 10000,
                "force_charge_hours": 4,
                "force_discharge_hours": 2,
                "min_battery_soc": 10,
                "max_battery_soc": 100,
                "battery_capacity_kwh": 23.0,
                "max_battery_charge_rate_kw": 5.0,
                "average_household_load_kw": 0.5
            },
            "system": {
                "debug_mode": true,
                "log_level": "info",
                "update_interval_secs": 60
            },
            "strategies": {
                "winter_adaptive": {
                    "enabled": true,
                    "ema_period_days": 7,
                    "min_solar_percentage": 0.10,
                    "target_battery_soc": 90.0,
                    "top_expensive_blocks": 12,
                    "tomorrow_preservation_threshold": 1.2,
                    "grid_export_price_threshold": 8.0,
                    "min_soc_for_export": 50.0,
                    "export_trigger_multiplier": 2.5,
                    "negative_price_handling_enabled": true,
                    "charge_on_negative_even_if_full": false
                },
                "winter_peak_discharge": {
                    "enabled": true,
                    "min_spread_czk": 3.0,
                    "min_soc_to_start": 70.0,
                    "min_soc_target": 50.0,
                    "solar_window_start_hour": 10,
                    "solar_window_end_hour": 14,
                    "min_hours_to_solar": 4
                },
                "solar_aware_charging": {
                    "enabled": true,
                    "solar_window_start_hour": 10,
                    "solar_window_end_hour": 14,
                    "midday_max_soc": 90.0,
                    "min_solar_forecast_kwh": 2.0
                },
                "morning_precharge": { "enabled": true },
                "day_ahead_planning": { "enabled": true },
                "time_aware_charge": { "enabled": true },
                "price_arbitrage": { "enabled": true },
                "solar_first": { "enabled": true },
                "self_use": { "enabled": true }
            }
        }"#;

        let config: AppConfig = serde_json::from_str(ha_addon_json)
            .expect("Failed to parse HA addon options format - check field name compatibility!");

        // Verify critical fields were correctly parsed
        assert_eq!(config.inverters.len(), 1);
        assert_eq!(config.inverters[0].id, "solax");
        assert_eq!(
            config.inverters[0].inverter_type,
            fluxion_core::InverterType::Solax,
            "vendor field should map to inverter_type via serde alias"
        );
        assert_eq!(config.inverters[0].topology, "independent");

        // Verify pricing
        assert!(config.pricing.use_spot_prices_to_buy);
        assert!(config.pricing.use_spot_prices_to_sell);

        // Verify control
        assert_eq!(config.control.force_charge_hours, 4);
        assert_eq!(config.control.force_discharge_hours, 2);

        // Verify system
        assert!(config.system.debug_mode);

        // Verify strategies
        assert!(config.strategies.winter_adaptive.enabled);
        assert!(config.strategies.winter_peak_discharge.enabled);

        // Configuration should be valid
        assert!(
            config.validate().is_ok(),
            "HA addon options format should produce valid config"
        );
    }

    /// Test that master/slave topology works with HA addon field names (master_id, slave_ids)
    #[test]
    fn test_ha_addon_master_slave_topology() {
        let ha_addon_json = r#"{
            "inverters": [
                {
                    "id": "master_inv",
                    "vendor": "solax",
                    "entity_prefix": "solax_1",
                    "topology": "master",
                    "slave_ids": ["slave_inv"]
                },
                {
                    "id": "slave_inv",
                    "vendor": "solax",
                    "entity_prefix": "solax_2",
                    "topology": "slave",
                    "master_id": "master_inv"
                }
            ],
            "pricing": {
                "spot_price_entity": "sensor.price",
                "use_spot_prices_to_buy": true,
                "use_spot_prices_to_sell": true,
                "fixed_buy_prices": [],
                "fixed_sell_prices": []
            },
            "control": {
                "maximum_export_power_w": 5000,
                "force_charge_hours": 2,
                "force_discharge_hours": 1,
                "min_battery_soc": 10,
                "max_battery_soc": 100
            },
            "system": {
                "debug_mode": true,
                "log_level": "info",
                "update_interval_secs": 60
            }
        }"#;

        let config: AppConfig = serde_json::from_str(ha_addon_json)
            .expect("Failed to parse HA addon master/slave config");

        // Verify master configuration
        assert_eq!(config.inverters[0].topology, "master");
        assert_eq!(
            config.inverters[0].slaves,
            Some(vec!["slave_inv".to_string()]),
            "slave_ids should map to slaves via serde alias"
        );

        // Verify slave configuration
        assert_eq!(config.inverters[1].topology, "slave");
        assert_eq!(
            config.inverters[1].master,
            Some("master_inv".to_string()),
            "master_id should map to master via serde alias"
        );

        // Configuration should be valid
        assert!(
            config.validate().is_ok(),
            "HA addon master/slave config should be valid"
        );
    }
}
