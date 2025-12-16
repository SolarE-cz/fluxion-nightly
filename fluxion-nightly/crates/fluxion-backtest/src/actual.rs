// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Analysis of actual (historical) plant data.
//!
//! This module calculates energy totals, costs, and battery value
//! from recorded historical data.

use anyhow::Result;
use chrono::NaiveDate;

use crate::db::{DataSource, find_price_at_timestamp};
use crate::types::{DayAnalysis, HourlyDataPoint, PriceRecord};

/// Default grid export price as a fraction of import price (sell-back rate)
const DEFAULT_EXPORT_PRICE_RATIO: f32 = 0.8;

/// Analyze actual historical data for a given day
///
/// This function:
/// - Loads historical plant data from the data source
/// - Calculates energy totals (PV, grid import/export, battery charge/discharge)
/// - Calculates financial metrics (costs, revenue, battery value)
/// - Returns a `DayAnalysis` with all metrics and time series data
pub fn analyze_actual_day<D: DataSource>(data_source: &D, date: NaiveDate) -> Result<DayAnalysis> {
    let records = data_source.get_day_data(date)?;
    let prices = data_source.get_prices(date)?;

    // If no prices for this specific day, get all prices and use time-of-day matching
    let prices = if prices.is_empty() {
        data_source.get_all_prices()?
    } else {
        prices
    };

    analyze_records_with_prices(date, &records, &prices, true)
}

/// Analyze a set of records with corresponding prices
///
/// This is the core analysis function used by both actual and simulated data.
pub fn analyze_records_with_prices(
    date: NaiveDate,
    records: &[crate::types::HistoricalRecord],
    prices: &[PriceRecord],
    is_actual: bool,
) -> Result<DayAnalysis> {
    if records.is_empty() {
        return Ok(DayAnalysis {
            date,
            strategy: if is_actual { "Actual" } else { "Simulated" }.to_owned(),
            is_actual,
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
        });
    }

    // Assume 5-minute intervals between records
    let interval_hours = 5.0 / 60.0;

    let mut pv_generation_kwh = 0.0;
    let mut grid_import_kwh = 0.0;
    let mut grid_export_kwh = 0.0;
    let mut battery_charge_kwh = 0.0;
    let mut battery_discharge_kwh = 0.0;
    let mut consumption_kwh = 0.0;

    let mut grid_import_cost_czk = 0.0;
    let mut grid_export_revenue_czk = 0.0;
    let mut battery_value_czk = 0.0;

    let mut hourly_data = Vec::with_capacity(records.len());

    for record in records {
        let price = find_price_at_timestamp(prices, record.timestamp);
        let export_price = price * DEFAULT_EXPORT_PRICE_RATIO;

        // Energy calculations (convert W to kWh)
        let pv_kwh = f64::from(record.pv_power_w) / 1000.0 * interval_hours;
        let consumption_interval_kwh = f64::from(record.house_load_w) / 1000.0 * interval_hours;

        pv_generation_kwh += pv_kwh;
        consumption_kwh += consumption_interval_kwh;

        // Grid power: positive = import, negative = export
        let grid_import_w = record.grid_power_w.max(0.0);
        let grid_export_w = (-record.grid_power_w).max(0.0);

        let grid_import_interval_kwh = f64::from(grid_import_w) / 1000.0 * interval_hours;
        let grid_export_interval_kwh = f64::from(grid_export_w) / 1000.0 * interval_hours;

        grid_import_kwh += grid_import_interval_kwh;
        grid_export_kwh += grid_export_interval_kwh;

        // Financial: grid costs
        grid_import_cost_czk += grid_import_interval_kwh * f64::from(price);
        grid_export_revenue_czk += grid_export_interval_kwh * f64::from(export_price);

        // Battery power: positive = discharge, negative = charge
        let battery_discharge_w = record.battery_power_w.max(0.0);
        let battery_charge_w = (-record.battery_power_w).max(0.0);

        let discharge_interval_kwh = f64::from(battery_discharge_w) / 1000.0 * interval_hours;
        let charge_interval_kwh = f64::from(battery_charge_w) / 1000.0 * interval_hours;

        battery_discharge_kwh += discharge_interval_kwh;
        battery_charge_kwh += charge_interval_kwh;

        // Battery value: energy discharged × price at discharge time
        // This represents the money saved by using battery instead of grid
        battery_value_czk += discharge_interval_kwh * f64::from(price);

        // Determine mode based on battery power
        let mode = if record.battery_power_w < -100.0 {
            "Charging"
        } else if record.battery_power_w > 100.0 {
            "Discharging"
        } else {
            "SelfUse"
        }
        .to_owned();

        hourly_data.push(HourlyDataPoint {
            timestamp: record.timestamp,
            price_czk: f64::from(price),
            mode,
            soc_percent: f64::from(record.battery_soc),
            grid_import_w: f64::from(grid_import_w),
            grid_export_w: f64::from(grid_export_w),
            pv_power_w: f64::from(record.pv_power_w),
            battery_power_w: f64::from(record.battery_power_w),
            house_load_w: f64::from(record.house_load_w),
        });
    }

    let net_cost_czk = grid_import_cost_czk - grid_export_revenue_czk;

    Ok(DayAnalysis {
        date,
        strategy: if is_actual { "Actual" } else { "Simulated" }.to_owned(),
        is_actual,
        pv_generation_kwh,
        grid_import_kwh,
        grid_export_kwh,
        battery_charge_kwh,
        battery_discharge_kwh,
        consumption_kwh,
        grid_import_cost_czk,
        grid_export_revenue_czk,
        battery_value_czk,
        net_cost_czk,
        hourly_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_analyze_empty_records() {
        let date = NaiveDate::from_ymd_opt(2024, 12, 14).unwrap();
        let result = analyze_records_with_prices(date, &[], &[], true).unwrap();

        assert_eq!(result.date, date);
        assert!(result.is_actual);
        assert!((result.pv_generation_kwh - 0.0).abs() < f64::EPSILON);
        assert!(result.hourly_data.is_empty());
    }

    #[test]
    fn test_battery_value_calculation() {
        use crate::types::HistoricalRecord;

        let date = NaiveDate::from_ymd_opt(2024, 12, 14).unwrap();
        let timestamp = Utc.with_ymd_and_hms(2024, 12, 14, 12, 0, 0).unwrap();

        // Record with battery discharging 1kW for 5 minutes at 3 CZK/kWh price
        let records = vec![HistoricalRecord {
            timestamp,
            battery_soc: 50.0,
            pv_power_w: 0.0,
            battery_power_w: 1000.0, // 1kW discharge
            grid_power_w: 0.0,
            house_load_w: 1000.0,
        }];

        let prices = vec![PriceRecord {
            timestamp,
            price_czk_per_kwh: 3.0,
        }];

        let result = analyze_records_with_prices(date, &records, &prices, true).unwrap();

        // 1kW * (5/60)h * 3 CZK/kWh = 0.25 CZK
        let expected_value = 1.0 * (5.0 / 60.0) * 3.0;
        assert!(
            (result.battery_value_czk - expected_value).abs() < 0.001,
            "Expected {expected_value}, got {}",
            result.battery_value_czk
        );
    }
}
