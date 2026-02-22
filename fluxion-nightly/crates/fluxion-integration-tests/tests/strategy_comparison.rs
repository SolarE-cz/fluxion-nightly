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
                effective_price_czk_per_kwh: price as f32,
                duration_minutes: 15, // Default assumption
                spot_sell_price_czk_per_kwh: None,
            })
        })
        .unwrap();

    let prices: Vec<TimeBlockPrice> = price_iter.map(|r| r.unwrap()).collect();

    let mut stmt = conn.prepare("SELECT timestamp, battery_soc, pv_power_w, house_load_w FROM historical_plant_data ORDER BY timestamp ASC").unwrap();
    let hist_iter = stmt
        .query_map([], |row| {
            let ts: i64 = row.get(0)?;
            Ok(HistoricalData {
                timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
                battery_soc: row.get::<_, f64>(1)? as f32,
                pv_power_w: row.get::<_, f64>(2)? as f32,
                house_load_w: row.get::<_, f64>(3)? as f32,
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
        daily_charging_target_soc: 60.0,
        charge_safety_multiplier: 1.1,
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

    // --- Database Setup for Results ---
    let mut conn = Connection::open("solax_data.db").expect("Failed to open DB for writing");
    conn.execute("DROP TABLE IF EXISTS simulation_winter_adaptive", [])
        .expect("Failed to drop table");
    conn.execute(
        "CREATE TABLE simulation_winter_adaptive (
            timestamp INTEGER PRIMARY KEY,
            soc REAL NOT NULL,
            import_w REAL NOT NULL,
            export_w REAL NOT NULL,
            battery_power_w REAL NOT NULL,
            pv_power_w REAL NOT NULL,
            house_load_w REAL NOT NULL,
            mode TEXT NOT NULL
        )",
        [],
    )
    .expect("Failed to create table");

    let tx = conn.transaction().expect("Failed to start transaction");

    // Simulation Loop
    let mut self_use_cost = 0.0;
    let mut winter_cost = 0.0;

    // Start SOC from the first record
    let start_soc = data.history[0].battery_soc;
    let mut self_use_soc = start_soc;
    let mut winter_soc = start_soc;

    // Consumption tracking
    let mut cumulative_consumption_kwh = 0.0;
    let mut current_day = data.history[0].timestamp.date_naive();

    println!("Starting simulation with SOC: {:.1}%", start_soc);

    // Assuming 5 min blocks in history
    let duration_h = 5.0 / 60.0;

    for record in data.history.iter() {
        // Reset consumption tracking on new day
        let record_date = record.timestamp.date_naive();
        if record_date != current_day {
            cumulative_consumption_kwh = 0.0;
            current_day = record_date;
        }

        // Find matching price based on time of day
        let minute_of_day = record.timestamp.hour() * 60 + record.timestamp.minute();
        let total_minutes = 24 * 60;
        let price_idx = (minute_of_day as usize * data.prices.len()) / total_minutes as usize;
        let price_block = &data.prices[price_idx % data.prices.len()];

        let price_buy = price_block.price_czk_per_kwh;
        let price_sell = price_buy * 0.8; // Simplified sell price

        let load_kw = record.house_load_w / 1000.0;
        let pv_kw = record.pv_power_w / 1000.0;

        // Update cumulative consumption BEFORE evaluation (so far today)
        // Note: This is "consumed so far", so we add current block AFTER or BEFORE?
        // The strategy expects "today_consumed_so_far".
        // If we are at 10:00, we want consumption from 00:00 to 10:00.
        // Let's add current block to it, assuming we know it (perfect foresight in sim)
        cumulative_consumption_kwh += load_kw * duration_h;

        // --- Self-Use Simulation ---
        let (su_grid_kw, su_bat_kw) = SelfUseStrategy.evaluate(
            load_kw,
            pv_kw,
            self_use_soc,
            control_config.battery_capacity_kwh,
        );

        // Update Self-Use SOC
        let su_energy_change_kwh = -su_bat_kw * duration_h;
        let su_soc_change = (su_energy_change_kwh / control_config.battery_capacity_kwh) * 100.0;
        self_use_soc = (self_use_soc + su_soc_change).clamp(0.0, 100.0);

        // Calculate Self-Use Cost
        if su_grid_kw > 0.0 {
            self_use_cost += su_grid_kw * duration_h * price_buy;
        } else {
            self_use_cost += su_grid_kw * duration_h * price_sell;
        }

        // --- Winter Adaptive Simulation ---
        let context = EvaluationContext {
            price_block,
            control_config: &control_config,
            current_battery_soc: winter_soc,
            solar_forecast_kwh: pv_kw * duration_h,
            consumption_forecast_kwh: load_kw * duration_h,
            grid_export_price_czk_per_kwh: price_sell,
            all_price_blocks: Some(&data.prices),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: Some(cumulative_consumption_kwh),
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let evaluation = winter_strategy.evaluate(&context);

        let (wa_grid_kw, wa_bat_kw);

        match evaluation.mode {
            InverterOperationMode::ForceCharge => {
                let max_charge = control_config.max_battery_charge_rate_kw;
                let bat_kw = -max_charge;
                let grid_kw = load_kw - pv_kw - bat_kw;
                (wa_grid_kw, wa_bat_kw) = (grid_kw, bat_kw);
            }
            InverterOperationMode::ForceDischarge => {
                let max_discharge = control_config.max_battery_charge_rate_kw;
                let bat_kw = max_discharge;
                let grid_kw = load_kw - pv_kw - bat_kw;
                (wa_grid_kw, wa_bat_kw) = (grid_kw, bat_kw);
            }
            _ => {
                // Self Use logic for other modes
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

        // Write to DB (convert kW back to W for consistency with DB schema)
        tx.execute(
            "INSERT INTO simulation_winter_adaptive (timestamp, soc, import_w, export_w, battery_power_w, pv_power_w, house_load_w, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                record.timestamp.timestamp(),
                winter_soc,
                if wa_grid_kw > 0.0 { wa_grid_kw * 1000.0 } else { 0.0 },
                if wa_grid_kw < 0.0 { -wa_grid_kw * 1000.0 } else { 0.0 },
                -wa_bat_kw * 1000.0, // Positive = Charge to match DB convention
                record.pv_power_w,
                record.house_load_w,
                format!("{:?}", evaluation.mode),
            ),
        )
        .unwrap();
    }

    tx.commit().expect("Failed to commit transaction");

    println!("Simulation complete.");
    println!("Total Records: {}", data.history.len());
    println!("Self-Use Cost:      {:.2} CZK", self_use_cost);
    println!("Winter Adaptive Cost: {:.2} CZK", winter_cost);
    println!("Difference:         {:.2} CZK", self_use_cost - winter_cost);

    if winter_cost < self_use_cost {
        println!("SUCCESS: Winter Adaptive Strategy saved money!");
    } else {
        println!("RESULT: Winter Adaptive Strategy cost more or same.");
    }
}
