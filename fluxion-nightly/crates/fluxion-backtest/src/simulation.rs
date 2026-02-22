// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Strategy simulation engine.
//!
//! This module simulates how different strategies would perform
//! against historical solar/consumption data.

use anyhow::Result;
use chrono::{NaiveDate, Timelike};

use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;

use crate::db::{DataSource, find_price_at_timestamp};
use crate::types::{
    DayAnalysis, HistoricalRecord, HourlyDataPoint, PriceRecord, StrategyChoice,
    StrategyConfigOverrides,
};

/// Default grid export price as a fraction of import price
const DEFAULT_EXPORT_PRICE_RATIO: f32 = 0.8;

/// Default battery capacity (kWh)
const DEFAULT_BATTERY_CAPACITY_KWH: f32 = 10.0;

/// Default max charge/discharge rate (kW)
const DEFAULT_MAX_BATTERY_RATE_KW: f32 = 3.0;

/// Simulate a day using the specified strategy
pub fn simulate_day<D: DataSource>(
    data_source: &D,
    date: NaiveDate,
    strategy: &StrategyChoice,
    config_overrides: Option<&StrategyConfigOverrides>,
) -> Result<DayAnalysis> {
    // For actual data, delegate to the actual analysis module
    if *strategy == StrategyChoice::Actual {
        return crate::actual::analyze_actual_day(data_source, date);
    }

    let records = data_source.get_day_data(date)?;
    let prices = data_source.get_prices(date)?;

    // If no prices for this specific day, get all prices
    let prices = if prices.is_empty() {
        data_source.get_all_prices()?
    } else {
        prices
    };

    match strategy {
        StrategyChoice::Actual => unreachable!(), // Handled above
        StrategyChoice::SelfUse => simulate_self_use(date, &records, &prices),
        StrategyChoice::WinterAdaptive => {
            simulate_winter_adaptive(date, &records, &prices, config_overrides)
        }
    }
}

/// Simulate with simple self-use logic (no optimization)
#[expect(clippy::unnecessary_wraps)]
fn simulate_self_use(
    date: NaiveDate,
    records: &[HistoricalRecord],
    prices: &[PriceRecord],
) -> Result<DayAnalysis> {
    if records.is_empty() {
        return Ok(empty_day_analysis(date, "Self-Use"));
    }

    let interval_hours = 5.0 / 60.0;
    let battery_capacity = DEFAULT_BATTERY_CAPACITY_KWH;

    // Start with the first recorded SOC
    let mut soc = records.first().map_or(50.0, |r| r.battery_soc);

    let mut totals = EnergyTotals::default();
    let mut hourly_data = Vec::with_capacity(records.len());

    for record in records {
        let price = find_price_at_timestamp(prices, record.timestamp);
        let export_price = price * DEFAULT_EXPORT_PRICE_RATIO;

        let load_kw = record.house_load_w / 1000.0;
        let pv_kw = record.pv_power_w / 1000.0;
        let net_load = load_kw - pv_kw;

        let (grid_kw, bat_kw, mode) = if net_load > 0.0 {
            // Deficit: need to discharge battery or import from grid
            if soc > 10.0 {
                // Discharge battery to cover load
                (0.0, net_load, "Discharging")
            } else {
                // Import from grid
                (net_load, 0.0, "Importing")
            }
        } else {
            // Surplus: can charge battery or export
            let surplus = -net_load;
            if soc < 100.0 {
                // Charge battery
                (0.0, -surplus, "Charging")
            } else {
                // Export to grid
                (-surplus, 0.0, "Exporting")
            }
        };

        // Update SOC
        let energy_change_kwh = -bat_kw * interval_hours;
        let soc_change = (energy_change_kwh / battery_capacity) * 100.0;
        soc = (soc + soc_change).clamp(0.0, 100.0);

        // Calculate energy flows
        let grid_import_kwh = grid_kw.max(0.0) * interval_hours;
        let grid_export_kwh = (-grid_kw).max(0.0) * interval_hours;
        let battery_discharge_kwh = bat_kw.max(0.0) * interval_hours;
        let battery_charge_kwh = (-bat_kw).max(0.0) * interval_hours;

        // Update totals
        totals.pv_generation_kwh += f64::from(pv_kw * interval_hours);
        totals.consumption_kwh += f64::from(load_kw * interval_hours);
        totals.grid_import_kwh += f64::from(grid_import_kwh);
        totals.grid_export_kwh += f64::from(grid_export_kwh);
        totals.battery_charge_kwh += f64::from(battery_charge_kwh);
        totals.battery_discharge_kwh += f64::from(battery_discharge_kwh);

        // Financial calculations
        totals.grid_import_cost_czk += f64::from(grid_import_kwh * price);
        totals.grid_export_revenue_czk += f64::from(grid_export_kwh * export_price);
        totals.battery_value_czk += f64::from(battery_discharge_kwh * price);

        hourly_data.push(HourlyDataPoint {
            timestamp: record.timestamp,
            price_czk: f64::from(price),
            mode: mode.to_owned(),
            soc_percent: f64::from(soc),
            grid_import_w: f64::from(grid_kw.max(0.0) * 1000.0),
            grid_export_w: f64::from((-grid_kw).max(0.0) * 1000.0),
            pv_power_w: f64::from(record.pv_power_w),
            battery_power_w: f64::from(bat_kw * 1000.0),
            house_load_w: f64::from(record.house_load_w),
        });
    }

    Ok(totals.into_day_analysis(date, "Self-Use", hourly_data))
}

/// Simulate with Winter Adaptive strategy
#[expect(clippy::unnecessary_wraps, clippy::too_many_lines)]
fn simulate_winter_adaptive(
    date: NaiveDate,
    records: &[HistoricalRecord],
    prices: &[PriceRecord],
    config_overrides: Option<&StrategyConfigOverrides>,
) -> Result<DayAnalysis> {
    if records.is_empty() {
        return Ok(empty_day_analysis(date, "Winter Adaptive"));
    }

    // Build configuration with overrides
    let mut config = WinterAdaptiveConfig::default();
    if let Some(overrides) = config_overrides {
        if let Some(v) = overrides.daily_charging_target_soc {
            config.daily_charging_target_soc = v;
        }
        if let Some(v) = overrides.conservation_threshold_soc {
            config.conservation_threshold_soc = v;
        }
        if let Some(v) = overrides.top_expensive_blocks {
            config.top_expensive_blocks = v;
        }
        if let Some(v) = overrides.charge_safety_multiplier {
            config.charge_safety_multiplier = v;
        }
    }

    let strategy = WinterAdaptiveStrategy::new(config);

    let control_config = ControlConfig {
        battery_capacity_kwh: DEFAULT_BATTERY_CAPACITY_KWH,
        max_battery_charge_rate_kw: DEFAULT_MAX_BATTERY_RATE_KW,
        battery_efficiency: 0.95,
        ..Default::default()
    };

    let interval_hours = 5.0 / 60.0;

    // Convert prices to TimeBlockPrice format
    let time_block_prices: Vec<TimeBlockPrice> = prices
        .iter()
        .map(|p| TimeBlockPrice {
            block_start: p.timestamp,
            price_czk_per_kwh: p.price_czk_per_kwh,
            effective_price_czk_per_kwh: p.price_czk_per_kwh,
            duration_minutes: 15,
            spot_sell_price_czk_per_kwh: None,
        })
        .collect();

    // Start with the first recorded SOC
    let mut soc = records.first().map_or(50.0, |r| r.battery_soc);

    let mut totals = EnergyTotals::default();
    let mut hourly_data = Vec::with_capacity(records.len());
    let mut cumulative_consumption_kwh: f32 = 0.0;
    let mut current_day = records.first().map(|r| r.timestamp.date_naive());

    for record in records {
        // Reset consumption tracking on new day
        let record_date = record.timestamp.date_naive();
        if Some(record_date) != current_day {
            cumulative_consumption_kwh = 0.0;
            current_day = Some(record_date);
        }

        let price = find_price_at_timestamp(prices, record.timestamp);
        let export_price = price * DEFAULT_EXPORT_PRICE_RATIO;

        let load_kw = record.house_load_w / 1000.0;
        let pv_kw = record.pv_power_w / 1000.0;

        cumulative_consumption_kwh += load_kw * interval_hours;

        // Find the current price block for strategy evaluation
        let minute_of_day = record.timestamp.hour() * 60 + record.timestamp.minute();
        let total_minutes: u32 = 24 * 60;
        #[expect(clippy::integer_division)]
        let price_idx = (minute_of_day as usize * time_block_prices.len()) / total_minutes as usize;
        let fallback_price_block = TimeBlockPrice {
            block_start: record.timestamp,
            price_czk_per_kwh: price,
            effective_price_czk_per_kwh: price,
            duration_minutes: 15,
            spot_sell_price_czk_per_kwh: None,
        };
        let price_block = time_block_prices
            .get(price_idx % time_block_prices.len().max(1))
            .unwrap_or(&fallback_price_block);

        // Evaluate strategy
        let context = EvaluationContext {
            price_block,
            control_config: &control_config,
            current_battery_soc: soc,
            solar_forecast_kwh: pv_kw * interval_hours,
            consumption_forecast_kwh: load_kw * interval_hours,
            grid_export_price_czk_per_kwh: export_price,
            all_price_blocks: Some(&time_block_prices),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: Some(cumulative_consumption_kwh),
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let evaluation = strategy.evaluate(&context);

        // Apply the strategy decision
        let (grid_kw, bat_kw, mode_str) = match evaluation.mode {
            InverterOperationMode::ForceCharge => {
                let max_charge = control_config.max_battery_charge_rate_kw;
                let bat_kw = -max_charge;
                let grid_kw = load_kw - pv_kw - bat_kw;
                (grid_kw, bat_kw, "ForceCharge")
            }
            InverterOperationMode::ForceDischarge => {
                let max_discharge = control_config.max_battery_charge_rate_kw;
                let bat_kw = max_discharge;
                let grid_kw = load_kw - pv_kw - bat_kw;
                (grid_kw, bat_kw, "ForceDischarge")
            }
            InverterOperationMode::SelfUse
            | InverterOperationMode::BackUpMode
            | InverterOperationMode::NoChargeNoDischarge => {
                // Self-use logic for other modes
                let net_load = load_kw - pv_kw;
                if net_load > 0.0 {
                    if soc > 10.0 {
                        (0.0, net_load, "SelfUse")
                    } else {
                        (net_load, 0.0, "SelfUse")
                    }
                } else {
                    let surplus = -net_load;
                    if soc < 100.0 {
                        (0.0, -surplus, "SelfUse")
                    } else {
                        (-surplus, 0.0, "SelfUse")
                    }
                }
            }
        };

        // Update SOC
        let energy_change_kwh = -bat_kw * interval_hours;
        let soc_change = (energy_change_kwh / control_config.battery_capacity_kwh) * 100.0;
        soc = (soc + soc_change).clamp(0.0, 100.0);

        // Calculate energy flows
        let grid_import_kwh = grid_kw.max(0.0) * interval_hours;
        let grid_export_kwh = (-grid_kw).max(0.0) * interval_hours;
        let battery_discharge_kwh = bat_kw.max(0.0) * interval_hours;
        let battery_charge_kwh = (-bat_kw).max(0.0) * interval_hours;

        // Update totals
        totals.pv_generation_kwh += f64::from(pv_kw * interval_hours);
        totals.consumption_kwh += f64::from(load_kw * interval_hours);
        totals.grid_import_kwh += f64::from(grid_import_kwh);
        totals.grid_export_kwh += f64::from(grid_export_kwh);
        totals.battery_charge_kwh += f64::from(battery_charge_kwh);
        totals.battery_discharge_kwh += f64::from(battery_discharge_kwh);

        // Financial calculations
        totals.grid_import_cost_czk += f64::from(grid_import_kwh * price);
        totals.grid_export_revenue_czk += f64::from(grid_export_kwh * export_price);
        totals.battery_value_czk += f64::from(battery_discharge_kwh * price);

        hourly_data.push(HourlyDataPoint {
            timestamp: record.timestamp,
            price_czk: f64::from(price),
            mode: mode_str.to_owned(),
            soc_percent: f64::from(soc),
            grid_import_w: f64::from(grid_kw.max(0.0) * 1000.0),
            grid_export_w: f64::from((-grid_kw).max(0.0) * 1000.0),
            pv_power_w: f64::from(record.pv_power_w),
            battery_power_w: f64::from(bat_kw * 1000.0),
            house_load_w: f64::from(record.house_load_w),
        });
    }

    Ok(totals.into_day_analysis(date, "Winter Adaptive", hourly_data))
}

/// Helper struct to accumulate energy totals
#[derive(Default)]
struct EnergyTotals {
    pv_generation_kwh: f64,
    grid_import_kwh: f64,
    grid_export_kwh: f64,
    battery_charge_kwh: f64,
    battery_discharge_kwh: f64,
    consumption_kwh: f64,
    grid_import_cost_czk: f64,
    grid_export_revenue_czk: f64,
    battery_value_czk: f64,
}

impl EnergyTotals {
    fn into_day_analysis(
        self,
        date: NaiveDate,
        strategy: &str,
        hourly_data: Vec<HourlyDataPoint>,
    ) -> DayAnalysis {
        let net_cost_czk = self.grid_import_cost_czk - self.grid_export_revenue_czk;

        DayAnalysis {
            date,
            strategy: strategy.to_owned(),
            is_actual: false,
            pv_generation_kwh: self.pv_generation_kwh,
            grid_import_kwh: self.grid_import_kwh,
            grid_export_kwh: self.grid_export_kwh,
            battery_charge_kwh: self.battery_charge_kwh,
            battery_discharge_kwh: self.battery_discharge_kwh,
            consumption_kwh: self.consumption_kwh,
            grid_import_cost_czk: self.grid_import_cost_czk,
            grid_export_revenue_czk: self.grid_export_revenue_czk,
            battery_value_czk: self.battery_value_czk,
            net_cost_czk,
            hourly_data,
        }
    }
}

/// Create an empty day analysis for days with no data
fn empty_day_analysis(date: NaiveDate, strategy: &str) -> DayAnalysis {
    DayAnalysis {
        date,
        strategy: strategy.to_owned(),
        is_actual: false,
        pv_generation_kwh: 0.0,
        grid_import_kwh: 0.0,
        grid_export_kwh: 0.0,
        battery_charge_kwh: 0.0,
        battery_discharge_kwh: 0.0,
        consumption_kwh: 0.0,
        grid_import_cost_czk: 0.0,
        grid_export_revenue_czk: 0.0,
        battery_value_czk: 0.0,
        net_cost_czk: 0.0,
        hourly_data: vec![],
    }
}
