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

use crate::config_api::ValidationIssue;
use fluxion_core::resources::SystemConfig;

/// Merge two JSON values recursively
/// `target` is modified in place with values from `source`
pub fn merge_json(target: &mut serde_json::Value, source: serde_json::Value) {
    match (target, source) {
        (serde_json::Value::Object(target_obj), serde_json::Value::Object(source_obj)) => {
            for (k, v) in source_obj {
                merge_json(target_obj.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (target_val, source_val) => {
            *target_val = source_val;
        }
    }
}

/// Validate the system configuration
/// Returns a tuple of (errors, warnings)
#[expect(clippy::too_many_lines)]
pub fn validate_config(config: &SystemConfig) -> (Vec<ValidationIssue>, Vec<ValidationIssue>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // ============= System Settings =============
    if config.system_config.update_interval_secs < 5 {
        errors.push(ValidationIssue {
            field: "system.update_interval_secs".to_owned(),
            message: "Update interval must be at least 5 seconds".to_owned(),
            severity: "error".to_owned(),
        });
    } else if config.system_config.update_interval_secs < 30 {
        warnings.push(ValidationIssue {
            field: "system.update_interval_secs".to_owned(),
            message: "Short update interval may cause high CPU usage".to_owned(),
            severity: "warning".to_owned(),
        });
    }

    // Display timezone: optional IANA name, empty means not set
    if let Some(tz) = config
        .system_config
        .display_timezone
        .as_deref()
        .map(str::trim)
        .filter(|tz| !tz.is_empty())
        && tz.parse::<chrono_tz::Tz>().is_err()
    {
        errors.push(ValidationIssue {
            field: "system.display_timezone".to_owned(),
            message: format!("Unknown timezone '{tz}', use an IANA name such as Europe/Prague"),
            severity: "error".to_owned(),
        });
    }

    // ============= Control Settings =============
    let control = &config.control_config;

    // Battery SOC limits
    if control.min_battery_soc < 0.0 || control.min_battery_soc > 100.0 {
        errors.push(ValidationIssue {
            field: "control.min_battery_soc".to_owned(),
            message: "Minimum SOC must be between 0% and 100%".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if control.max_battery_soc < 0.0 || control.max_battery_soc > 100.0 {
        errors.push(ValidationIssue {
            field: "control.max_battery_soc".to_owned(),
            message: "Maximum SOC must be between 0% and 100%".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if control.min_battery_soc > control.max_battery_soc {
        errors.push(ValidationIssue {
            field: "control.min_battery_soc".to_owned(),
            message: "Minimum SOC cannot be higher than maximum SOC".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // Hardware limits
    if control.hardware_min_battery_soc < 0.0 || control.hardware_min_battery_soc > 20.0 {
        warnings.push(ValidationIssue {
            field: "control.hardware_min_battery_soc".to_owned(),
            message: "Hardware minimum SOC is unusually high (>20%) or invalid".to_owned(),
            severity: "warning".to_owned(),
        });
    }

    if control.min_battery_soc < control.hardware_min_battery_soc {
        errors.push(ValidationIssue {
            field: "control.min_battery_soc".to_owned(),
            message: format!(
                "Minimum SOC cannot be lower than hardware limit ({}%)",
                control.hardware_min_battery_soc
            ),
            severity: "error".to_owned(),
        });
    }

    // Force hours
    if control.force_charge_hours > 24 {
        errors.push(ValidationIssue {
            field: "control.force_charge_hours".to_owned(),
            message: "Force charge hours cannot exceed 24".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if control.force_discharge_hours > 24 {
        errors.push(ValidationIssue {
            field: "control.force_discharge_hours".to_owned(),
            message: "Force discharge hours cannot exceed 24".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if control.force_charge_hours + control.force_discharge_hours > 24 {
        errors.push(ValidationIssue {
            field: "control.force_charge_hours".to_owned(),
            message: "Total force hours cannot exceed 24".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // Peak hours
    if control.evening_peak_start_hour > 23 {
        errors.push(ValidationIssue {
            field: "control.evening_peak_start_hour".to_owned(),
            message: "Evening peak hour must be between 0 and 23".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // Battery Params
    if control.battery_capacity_kwh <= 0.0 {
        errors.push(ValidationIssue {
            field: "control.battery_capacity_kwh".to_owned(),
            message: "Battery capacity must be positive".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if control.battery_efficiency <= 0.0 || control.battery_efficiency > 1.0 {
        errors.push(ValidationIssue {
            field: "control.battery_efficiency".to_owned(),
            message: "Battery efficiency must be between 0 and 1.0".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // ============= Pricing Settings =============
    let pricing = &config.pricing_config;

    if pricing.spot_buy_fee_czk < 0.0 {
        errors.push(ValidationIssue {
            field: "pricing.spot_buy_fee_czk".to_owned(),
            message: "Buy fee cannot be negative".to_owned(),
            severity: "error".to_owned(),
        });
    }

    if pricing.spot_sell_fee_czk < 0.0 {
        errors.push(ValidationIssue {
            field: "pricing.spot_sell_fee_czk".to_owned(),
            message: "Sell fee cannot be negative".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // ============= Strategies Settings =============
    let strategies = &config.strategies_config;

    // Winter Peak Discharge
    if strategies.winter_peak_discharge.enabled {
        if strategies.winter_peak_discharge.min_spread_czk < 0.0 {
            errors.push(ValidationIssue {
                field: "strategies.winter_peak_discharge.min_spread_czk".to_owned(),
                message: "Minimum spread cannot be negative".to_owned(),
                severity: "error".to_owned(),
            });
        }

        if strategies.winter_peak_discharge.solar_window_start_hour
            >= strategies.winter_peak_discharge.solar_window_end_hour
        {
            errors.push(ValidationIssue {
                field: "strategies.winter_peak_discharge.solar_window_start_hour".to_owned(),
                message: "Solar window start must be before end".to_owned(),
                severity: "error".to_owned(),
            });
        }
    }

    // Winter Adaptive
    if strategies.winter_adaptive.enabled && strategies.winter_adaptive.ema_period_days < 1 {
        errors.push(ValidationIssue {
            field: "strategies.winter_adaptive.ema_period_days".to_owned(),
            message: "EMA period must be at least 1 day".to_owned(),
            severity: "error".to_owned(),
        });
    }

    // Winter Reserve
    let wr = &strategies.winter_reserve;
    if !(wr.cheap_percentile > 0.0 && wr.cheap_percentile <= 0.5) {
        errors.push(ValidationIssue {
            field: "strategies.winter_reserve.cheap_percentile".to_owned(),
            message: "Cheap percentile must be in (0, 0.5]".to_owned(),
            severity: "error".to_owned(),
        });
    }
    for (field, value) in [
        ("insane_price_multiplier", wr.insane_price_multiplier),
        ("load_safety_factor", wr.load_safety_factor),
        ("pv_confidence", wr.pv_confidence),
    ] {
        if value <= 0.0 {
            errors.push(ValidationIssue {
                field: format!("strategies.winter_reserve.{field}"),
                message: "Must be greater than 0".to_owned(),
                severity: "error".to_owned(),
            });
        }
    }
    if !(0.1..=5.0).contains(&wr.soc_resolution_pct) {
        errors.push(ValidationIssue {
            field: "strategies.winter_reserve.soc_resolution_pct".to_owned(),
            message: "SOC resolution must be between 0.1 and 5 percent".to_owned(),
            severity: "error".to_owned(),
        });
    }
    if wr.min_arbitrage_profit_czk < 0.0 {
        errors.push(ValidationIssue {
            field: "strategies.winter_reserve.min_arbitrage_profit_czk".to_owned(),
            message: "Minimum arbitrage profit must not be negative".to_owned(),
            severity: "error".to_owned(),
        });
    }

    (errors, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxion_core::resources::{
        ConsumptionHistoryConfig, ControlConfig, PricingConfig, StrategiesConfigCore, SystemConfig,
        SystemSettingsConfig,
    };
    use fluxion_types::config::RemoteAccessConfigCore;

    fn default_config() -> SystemConfig {
        SystemConfig {
            inverters: vec![],
            pricing_config: PricingConfig {
                spot_price_entity: "sensor.spot_price".to_owned(),
                tomorrow_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_price_czk: fluxion_core::resources::PriceSchedule::Flat(5.0),
                fixed_sell_price_czk: fluxion_core::resources::PriceSchedule::Flat(1.0),
                spot_buy_fee_czk: 0.5,
                spot_sell_fee_czk: 0.5,
                hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_owned(),
                hdo_low_tariff_czk: 0.50,
                hdo_high_tariff_czk: 1.80,
            },
            control_config: ControlConfig::default(),
            system_config: SystemSettingsConfig {
                update_interval_secs: 60,
                debug_mode: true,
                display_currency: fluxion_core::resources::Currency::CZK,
                language: fluxion_i18n::Language::English,
                timezone: None,
                display_timezone: None,
            },
            strategies_config: StrategiesConfigCore::default(),
            history: ConsumptionHistoryConfig::default(),
            solar_forecast: fluxion_core::resources::SolarForecastConfigCore::default(),
            remote_access: RemoteAccessConfigCore::default(),
        }
    }

    #[test]
    fn test_valid_config() {
        let config = default_config();
        let (errors, warnings) = validate_config(&config);
        assert!(errors.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_display_timezone_validation() {
        let mut config = default_config();

        config.system_config.display_timezone = Some("Mars/Olympus".to_owned());
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|e| e.field == "system.display_timezone"));

        config.system_config.display_timezone = Some("Europe/Prague".to_owned());
        let (errors, _) = validate_config(&config);
        assert!(!errors.iter().any(|e| e.field == "system.display_timezone"));

        config.system_config.display_timezone = Some(String::new());
        let (errors, _) = validate_config(&config);
        assert!(!errors.iter().any(|e| e.field == "system.display_timezone"));
    }

    #[test]
    fn test_display_timezone_survives_config_update() {
        // Same flow as update_config_handler: merge the partial update into the
        // current config JSON, then deserialize it back into SystemConfig.
        let mut current = serde_json::to_value(default_config()).unwrap();
        let update = serde_json::json!({ "system": { "display_timezone": "Europe/Prague" } });
        merge_json(&mut current, update);

        let config: SystemConfig = serde_json::from_value(current).unwrap();
        assert_eq!(
            config.system_config.display_timezone.as_deref(),
            Some("Europe/Prague")
        );
    }

    #[test]
    fn test_invalid_soc() {
        let mut config = default_config();
        config.control_config.min_battery_soc = -10.0;
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|e| e.field == "control.min_battery_soc"));

        config.control_config.min_battery_soc = 110.0;
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|e| e.field == "control.min_battery_soc"));
    }

    #[test]
    fn test_min_soc_higher_than_max() {
        let mut config = default_config();
        config.control_config.min_battery_soc = 80.0;
        config.control_config.max_battery_soc = 50.0;
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|e| e.field == "control.min_battery_soc"));
    }

    #[test]
    fn test_force_hours_limit() {
        let mut config = default_config();
        config.control_config.force_charge_hours = 25;
        let (errors, _) = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "control.force_charge_hours")
        );

        config.control_config.force_charge_hours = 10;
        config.control_config.force_discharge_hours = 15; // Total 25
        let (errors, _) = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "control.force_charge_hours")
        );
    }

    #[test]
    fn test_update_interval_warning() {
        let mut config = default_config();
        config.system_config.update_interval_secs = 10;
        let (errors, warnings) = validate_config(&config);
        assert!(errors.is_empty());
        assert!(
            warnings
                .iter()
                .any(|w| w.field == "system.update_interval_secs")
        );
    }

    #[test]
    fn test_strategy_validation() {
        let mut config = default_config();
        config.strategies_config.winter_peak_discharge.enabled = true;
        config
            .strategies_config
            .winter_peak_discharge
            .min_spread_czk = -5.0;

        let (errors, _) = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "strategies.winter_peak_discharge.min_spread_czk")
        );
    }

    #[test]
    fn test_winter_reserve_min_arbitrage_profit_must_not_be_negative() {
        let mut config = default_config();
        config
            .strategies_config
            .winter_reserve
            .min_arbitrage_profit_czk = -0.5;
        let (errors, _) = validate_config(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "strategies.winter_reserve.min_arbitrage_profit_czk")
        );

        config
            .strategies_config
            .winter_reserve
            .min_arbitrage_profit_czk = 0.0;
        let (errors, _) = validate_config(&config);
        assert!(
            !errors
                .iter()
                .any(|e| e.field == "strategies.winter_reserve.min_arbitrage_profit_czk")
        );
    }
}
