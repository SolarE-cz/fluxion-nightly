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

use chrono::{DateTime, TimeZone, Timelike, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
};
use fluxion_types::{
    config::ControlConfig, inverter::InverterOperationMode, pricing::TimeBlockPrice,
};
use rusqlite::Connection;

#[derive(Debug)]
struct HistoricalData {
    timestamp: DateTime<Utc>,
    battery_soc: f32,
    pv_power_w: f32,
    #[allow(dead_code)]
    battery_power_w: f32,
    #[allow(dead_code)]
    grid_power_w: f32,
    house_load_w: f32,
}

#[derive(Debug)]
struct TestData {
    prices: Vec<TimeBlockPrice>,
    history: Vec<HistoricalData>,
}

fn load_data() -> TestData {
    let conn = Connection::open("solax_data.db").expect("Failed to open DB");

    let mut stmt = conn
        .prepare("SELECT ts, price FROM prices ORDER BY ts ASC")
        .unwrap();
    let price_iter = stmt
        .query_map([], |row| {
            let ts: i64 = row.get(0)?;
            let price: f64 = row.get(1)?;
            Ok(TimeBlockPrice {
                block_start: Utc.timestamp_opt(ts, 0).unwrap(),
                price_czk_per_kwh: price as f32,
                duration_minutes: 15, // Default assumption
            })
        })
        .unwrap();

    let prices: Vec<TimeBlockPrice> = price_iter.map(|r| r.unwrap()).collect();

    let mut stmt = conn.prepare("SELECT timestamp, battery_soc, pv_power_w, battery_power_w, grid_power_w, house_load_w FROM historical_plant_data ORDER BY timestamp ASC").unwrap();
    let hist_iter = stmt
        .query_map([], |row| {
            let ts: i64 = row.get(0)?;
            Ok(HistoricalData {
                timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
                battery_soc: row.get::<_, f64>(1)? as f32,
                pv_power_w: row.get::<_, f64>(2)? as f32,
                battery_power_w: row.get::<_, f64>(3)? as f32,
                grid_power_w: row.get::<_, f64>(4)? as f32,
                house_load_w: row.get::<_, f64>(5)? as f32,
            })
        })
        .unwrap();

    let history: Vec<HistoricalData> = hist_iter.map(|r| r.unwrap()).collect();

    TestData { prices, history }
}

// Mock Self-Use Strategy
struct SelfUseStrategy;

impl SelfUseStrategy {
    fn evaluate(
        &self,
        consumption_kw: f32,
        pv_kw: f32,
        battery_soc: f32,
        _capacity_kwh: f32,
    ) -> (f32, f32) {
        // Simple logic:
        // If PV > Consumption: Charge battery or Export
        // If PV < Consumption: Discharge battery or Import

        let net_load = consumption_kw - pv_kw;

        if net_load > 0.0 {
            // Deficit
            if battery_soc > 10.0 {
                // Discharge to cover load
                // Return (grid_import, battery_discharge)
                // Discharge is positive
                (0.0, net_load)
            } else {
                // Import
                (net_load, 0.0)
            }
        } else {
            // Surplus (net_load is negative)
            if battery_soc < 100.0 {
                // Charge
                // Discharge is negative for charging
                (0.0, net_load)
            } else {
                // Export
                // Import is negative for export
                (net_load, 0.0)
            }
        }
    }
}

#[test]
fn compare_strategies() {
    let data = load_data();
    println!("Loaded {} price blocks", data.prices.len());
    println!("Loaded {} historical records", data.history.len());

    if data.prices.is_empty() {
        eprintln!("No price data found. Run fetch_data first.");
        return;
    }
    if data.history.is_empty() {
        eprintln!("No historical data found. Run import_csv first.");
        return;
    }

    // Setup Winter Adaptive Strategy
    let winter_config = WinterAdaptiveConfig {
        target_battery_soc: 90.0,
        ..Default::default()
    };
    // winter_config.enable_grid_charge = true; // Field does not exist, assuming enabled by default or logic
    let winter_strategy = WinterAdaptiveStrategy::new(winter_config);

    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0, // Assuming 10kWh battery from CSV (approx)
        max_battery_charge_rate_kw: 3.0,
        // max_battery_discharge_rate_kw: 3.0, // Field does not exist
        // inverter_power_limit_kw: 10.0, // Field does not exist
        battery_efficiency: 0.95,
        ..Default::default()
    };

    // Simulation Loop
    let mut self_use_cost = 0.0;
    let mut winter_cost = 0.0;

    // Start SOC from the first record
    let start_soc = data.history[0].battery_soc;
    let mut self_use_soc = start_soc;
    let mut winter_soc = start_soc;

    println!("Starting simulation with SOC: {:.1}%", start_soc);

    // Assuming 5 min blocks in history
    let duration_h = 5.0 / 60.0;

    for record in data.history.iter() {
        // Find matching price based on time of day
        // We assume 'prices' contains a 24h profile or similar.
        // We map record.timestamp to the same time in the price list.
        // If price list is absolute timestamps, we might need modulo logic.
        // Let's assume prices are just a list we cycle through if they don't match date.

        // Better approach: Find price block that covers this time of day.
        // Assuming prices are 15 min blocks? Or 1 hour?
        // Let's just pick the price index = (hour * 60 + minute) / (24*60 / prices.len())
        let minute_of_day = record.timestamp.hour() * 60 + record.timestamp.minute();
        let total_minutes = 24 * 60;
        let price_idx = (minute_of_day as usize * data.prices.len()) / total_minutes as usize;
        let price_block = &data.prices[price_idx % data.prices.len()];

        let price_buy = price_block.price_czk_per_kwh;
        let price_sell = price_buy * 0.8; // Simplified sell price

        let load_kw = record.house_load_w / 1000.0;
        let pv_kw = record.pv_power_w / 1000.0;

        // --- Self-Use Simulation ---
        let (su_grid_kw, su_bat_kw) = SelfUseStrategy.evaluate(
            load_kw,
            pv_kw,
            self_use_soc,
            control_config.battery_capacity_kwh,
        );

        // Update Self-Use SOC
        // su_bat_kw > 0 means discharge
        let su_energy_change_kwh = -su_bat_kw * duration_h;
        let su_soc_change = (su_energy_change_kwh / control_config.battery_capacity_kwh) * 100.0;
        self_use_soc = (self_use_soc + su_soc_change).clamp(0.0, 100.0);

        // Calculate Self-Use Cost
        if su_grid_kw > 0.0 {
            self_use_cost += su_grid_kw * duration_h * price_buy;
        } else {
            self_use_cost += su_grid_kw * duration_h * price_sell; // su_grid_kw is negative
        }

        // --- Winter Adaptive Simulation ---
        // Construct Context
        let context = EvaluationContext {
            price_block,
            control_config: &control_config,
            current_battery_soc: winter_soc,
            solar_forecast_kwh: pv_kw * duration_h, // Simple forecast = current
            consumption_forecast_kwh: load_kw * duration_h,
            grid_export_price_czk_per_kwh: price_sell,
            all_price_blocks: Some(&data.prices), // Pass all prices for lookahead
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
        };

        let evaluation = winter_strategy.evaluate(&context);

        // Apply Winter Strategy Decision
        // The strategy returns a mode (Charge, Discharge, etc.)
        // We need to simulate what actually happens in that mode given the physical constraints (PV, Load).

        let (wa_grid_kw, wa_bat_kw);

        match evaluation.mode {
            InverterOperationMode::ForceCharge => {
                // Charge from Grid + PV
                // Target is max charge rate
                let max_charge = control_config.max_battery_charge_rate_kw;
                let bat_kw = -max_charge; // Charge
                // Grid = Load - PV - Bat (where Bat is negative)
                // Grid = Load - PV + Charge
                let grid_kw = load_kw - pv_kw - bat_kw;
                (wa_grid_kw, wa_bat_kw) = (grid_kw, bat_kw);
            }
            InverterOperationMode::ForceDischarge => {
                // Discharge to Grid
                let max_discharge = control_config.max_battery_charge_rate_kw; // Assume discharge = charge rate
                let bat_kw = max_discharge;
                let grid_kw = load_kw - pv_kw - bat_kw;
                (wa_grid_kw, wa_bat_kw) = (grid_kw, bat_kw);
            }
            InverterOperationMode::SelfUse => {
                // Same as Self-Use logic
                let (g, b) = SelfUseStrategy.evaluate(
                    load_kw,
                    pv_kw,
                    winter_soc,
                    control_config.battery_capacity_kwh,
                );
                (wa_grid_kw, wa_bat_kw) = (g, b);
            }
            // Handle other modes
            _ => {
                // Default to Self Use
                let (g, b) = SelfUseStrategy.evaluate(
                    load_kw,
                    pv_kw,
                    winter_soc,
                    control_config.battery_capacity_kwh,
                );
                (wa_grid_kw, wa_bat_kw) = (g, b);
            }
        }

        // Update Winter SOC
        let wa_energy_change_kwh = -wa_bat_kw * duration_h;
        let wa_soc_change = (wa_energy_change_kwh / control_config.battery_capacity_kwh) * 100.0;
        winter_soc = (winter_soc + wa_soc_change).clamp(0.0, 100.0);

        // Calculate Winter Cost
        if wa_grid_kw > 0.0 {
            winter_cost += wa_grid_kw * duration_h * price_buy;
        } else {
            winter_cost += wa_grid_kw * duration_h * price_sell;
        }
    }

    println!("Simulation complete.");
    println!("Total Records: {}", data.history.len());
    println!("Self-Use Cost:      {:.2} CZK", self_use_cost);
    println!("Winter Adaptive Cost: {:.2} CZK", winter_cost);
    println!("Difference:         {:.2} CZK", self_use_cost - winter_cost);

    if winter_cost < self_use_cost {
        println!("SUCCESS: Winter Adaptive Strategy saved money!");
    } else {
        println!(
            "RESULT: Winter Adaptive Strategy cost more or same (might be expected depending on prices)."
        );
    }
}
