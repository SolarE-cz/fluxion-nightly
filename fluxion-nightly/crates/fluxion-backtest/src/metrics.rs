// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Metrics calculation for backtest comparisons.
//!
//! This module provides functions to compare two day analyses
//! and calculate the difference between them.

use serde::{Deserialize, Serialize};

use crate::types::DayAnalysis;

/// Comparison difference between two day analyses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonDiff {
    /// Net cost difference (right - left) in CZK
    /// Positive means right is more expensive
    pub cost_diff_czk: f64,

    /// Savings percentage: ((left.net_cost - right.net_cost) / left.net_cost) * 100
    /// Positive means right saved money compared to left
    pub savings_percent: f64,

    /// Battery value difference (right - left) in CZK
    pub battery_value_diff_czk: f64,

    /// Grid import difference (right - left) in kWh
    pub grid_import_diff_kwh: f64,

    /// Grid export difference (right - left) in kWh
    pub grid_export_diff_kwh: f64,

    /// Battery charge difference (right - left) in kWh
    pub battery_charge_diff_kwh: f64,

    /// Battery discharge difference (right - left) in kWh
    pub battery_discharge_diff_kwh: f64,
}

/// Calculate the comparison between two day analyses
///
/// The comparison is done from the perspective of the right panel:
/// - Positive `cost_diff_czk` means right is more expensive
/// - Positive `savings_percent` means right saved money
#[must_use]
pub fn calculate_comparison(left: &DayAnalysis, right: &DayAnalysis) -> ComparisonDiff {
    let cost_diff_czk = right.net_cost_czk - left.net_cost_czk;

    // Calculate savings percentage (positive means right saved money)
    let savings_percent = if left.net_cost_czk.abs() > 0.001 {
        ((left.net_cost_czk - right.net_cost_czk) / left.net_cost_czk) * 100.0
    } else {
        0.0
    };

    ComparisonDiff {
        cost_diff_czk,
        savings_percent,
        battery_value_diff_czk: right.battery_value_czk - left.battery_value_czk,
        grid_import_diff_kwh: right.grid_import_kwh - left.grid_import_kwh,
        grid_export_diff_kwh: right.grid_export_kwh - left.grid_export_kwh,
        battery_charge_diff_kwh: right.battery_charge_kwh - left.battery_charge_kwh,
        battery_discharge_diff_kwh: right.battery_discharge_kwh - left.battery_discharge_kwh,
    }
}

/// Format a savings percentage for display
#[must_use]
pub fn format_savings(savings_percent: f64) -> String {
    if savings_percent > 0.0 {
        format!("-{savings_percent:.1}%")
    } else if savings_percent < 0.0 {
        format!("+{:.1}%", savings_percent.abs())
    } else {
        "0%".to_owned()
    }
}

/// Format a cost difference for display
#[must_use]
pub fn format_cost_diff(cost_diff_czk: f64) -> String {
    if cost_diff_czk > 0.0 {
        format!("+{cost_diff_czk:.0} CZK")
    } else if cost_diff_czk < 0.0 {
        format!("{cost_diff_czk:.0} CZK")
    } else {
        "0 CZK".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn mock_day_analysis(net_cost: f64, battery_value: f64) -> DayAnalysis {
        DayAnalysis {
            date: NaiveDate::from_ymd_opt(2024, 12, 14).unwrap(),
            strategy: "Test".to_owned(),
            is_actual: false,
            pv_generation_kwh: 10.0,
            grid_import_kwh: 5.0,
            grid_export_kwh: 2.0,
            battery_charge_kwh: 6.0,
            battery_discharge_kwh: 4.0,
            consumption_kwh: 12.0,
            grid_import_cost_czk: net_cost + 10.0,
            grid_export_revenue_czk: 10.0,
            battery_value_czk: battery_value,
            net_cost_czk: net_cost,
            hourly_data: vec![],
        }
    }

    #[test]
    fn test_comparison_right_cheaper() {
        let left = mock_day_analysis(100.0, 50.0);
        let right = mock_day_analysis(80.0, 60.0);

        let diff = calculate_comparison(&left, &right);

        assert!((diff.cost_diff_czk - (-20.0)).abs() < 0.001);
        assert!((diff.savings_percent - 20.0).abs() < 0.001);
        assert!((diff.battery_value_diff_czk - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_comparison_right_more_expensive() {
        let left = mock_day_analysis(80.0, 60.0);
        let right = mock_day_analysis(100.0, 50.0);

        let diff = calculate_comparison(&left, &right);

        assert!((diff.cost_diff_czk - 20.0).abs() < 0.001);
        assert!((diff.savings_percent - (-25.0)).abs() < 0.001);
    }

    #[test]
    fn test_format_savings() {
        assert_eq!(format_savings(15.5), "-15.5%");
        assert_eq!(format_savings(-10.0), "+10.0%");
        assert_eq!(format_savings(0.0), "0%");
    }

    #[test]
    fn test_format_cost_diff() {
        assert_eq!(format_cost_diff(25.0), "+25 CZK");
        assert_eq!(format_cost_diff(-15.0), "-15 CZK");
        assert_eq!(format_cost_diff(0.0), "0 CZK");
    }
}
