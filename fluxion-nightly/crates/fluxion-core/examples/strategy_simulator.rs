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
use fluxion_core::components::TimeBlockPrice;
use fluxion_core::resources::{ControlConfig, PriceSchedule, PricingConfig};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
};
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Raw data row from CSV
#[derive(Debug)]
struct InverterDataRow {
    timestamp: DateTime<Utc>,
    total_pv_yield_kwh: f32,
    total_battery_discharge_kwh: f32,
    total_battery_charge_kwh: f32,
    total_exported_kwh: f32,
    total_imported_kwh: f32,
    battery_soc: f32,
}

/// Aggregated 15-minute block data
#[derive(Debug)]
struct AggregatedBlock {
    start_time: DateTime<Utc>,
    pv_generation_kwh: f32,
    consumption_kwh: f32,
    _avg_soc: f32,
    start_soc: f32,
}

fn parse_csv<P: AsRef<Path>>(path: P) -> Result<Vec<InverterDataRow>, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut rows = Vec::new();
    let mut lines = reader.lines();

    // Skip header
    lines.next();

    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() < 17 {
            continue;
        }

        // Parse timestamp "2025-10-01 00:00:02"
        let time_str = parts[0];
        // Naive parsing, assuming UTC for simplicity or local time treated as UTC
        let naive = chrono::NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S")?;
        let timestamp = Utc.from_utc_datetime(&naive);

        // Parse cumulative counters
        let total_pv = parts[3].parse::<f32>().unwrap_or(0.0);
        let total_dis = parts[9].parse::<f32>().unwrap_or(0.0);
        let total_chg = parts[11].parse::<f32>().unwrap_or(0.0);
        let total_exp = parts[13].parse::<f32>().unwrap_or(0.0);
        let total_imp = parts[15].parse::<f32>().unwrap_or(0.0);
        let soc = parts[16].parse::<f32>().unwrap_or(0.0);

        rows.push(InverterDataRow {
            timestamp,
            total_pv_yield_kwh: total_pv,
            total_battery_discharge_kwh: total_dis,
            total_battery_charge_kwh: total_chg,
            total_exported_kwh: total_exp,
            total_imported_kwh: total_imp,
            battery_soc: soc,
        });
    }

    // Sort by time just in case
    rows.sort_by_key(|r| r.timestamp);
    Ok(rows)
}

fn aggregate_to_blocks(rows: &[InverterDataRow]) -> Vec<AggregatedBlock> {
    if rows.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut current_block_start = rows[0]
        .timestamp
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    // Align to 15 min
    let min = current_block_start.minute();
    let aligned_min = (min / 15) * 15;
    current_block_start = current_block_start.with_minute(aligned_min).unwrap();

    let mut block_rows = Vec::new();

    for row in rows {
        if row.timestamp >= current_block_start + chrono::Duration::minutes(15) {
            // Process previous block
            if let Some(block) = process_block(&block_rows, current_block_start) {
                blocks.push(block);
            }
            block_rows.clear();

            // Advance block start
            while row.timestamp >= current_block_start + chrono::Duration::minutes(15) {
                current_block_start += chrono::Duration::minutes(15);
            }
        }
        block_rows.push(row);
    }

    // Last block
    if let Some(block) = process_block(&block_rows, current_block_start) {
        blocks.push(block);
    }

    blocks
}

fn process_block(rows: &[&InverterDataRow], start_time: DateTime<Utc>) -> Option<AggregatedBlock> {
    if rows.len() < 2 {
        return None;
    }

    let first = rows.first().unwrap();
    let last = rows.last().unwrap();

    // Calculate deltas
    let delta_pv = last.total_pv_yield_kwh - first.total_pv_yield_kwh;
    let delta_dis = last.total_battery_discharge_kwh - first.total_battery_discharge_kwh;
    let delta_chg = last.total_battery_charge_kwh - first.total_battery_charge_kwh;
    let delta_imp = last.total_imported_kwh - first.total_imported_kwh;
    let delta_exp = last.total_exported_kwh - first.total_exported_kwh;

    // Load calculation: PV + Discharge - Charge + Import - Export
    let net_grid = delta_imp - delta_exp;
    let net_battery = delta_dis - delta_chg;
    let load = delta_pv + net_battery + net_grid;

    // Average SOC
    let avg_soc = rows.iter().map(|r| r.battery_soc).sum::<f32>() / rows.len() as f32;

    Some(AggregatedBlock {
        start_time,
        pv_generation_kwh: delta_pv.max(0.0),
        consumption_kwh: load.max(0.0), // Load shouldn't be negative
        _avg_soc: avg_soc,
        start_soc: first.battery_soc,
    })
}

/// Price record from OTE CSV
#[derive(Debug, Clone)]
struct PriceData {
    _timestamp: DateTime<Utc>,
    _price_eur: f32,
    price_czk: f32,
}

use std::collections::HashMap;

/// Load real OTE prices from CSV
fn load_price_csv<P: AsRef<Path>>(
    path: P,
) -> Result<HashMap<DateTime<Utc>, PriceData>, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut prices = HashMap::new();
    let mut lines = reader.lines();

    // Skip header
    lines.next();

    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 3 {
            continue;
        }

        // Parse datetime "2025-10-01 00:00:00"
        let time_str = parts[0];
        let naive = chrono::NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S")?;
        let timestamp = Utc.from_utc_datetime(&naive);

        let price_eur = parts[1].parse::<f32>().unwrap_or(0.0);
        let price_czk = parts[2].parse::<f32>().unwrap_or(0.0);

        prices.insert(
            timestamp,
            PriceData {
                _timestamp: timestamp,
                _price_eur: price_eur,
                price_czk,
            },
        );
    }

    Ok(prices)
}

/// Get price for a given time, with fallback
fn get_price_czk(time: DateTime<Utc>, prices: &HashMap<DateTime<Utc>, PriceData>) -> f32 {
    // Try exact match first
    if let Some(price) = prices.get(&time) {
        return price.price_czk / 1000.0; // Convert from CZK/MWh to CZK/kWh
    }

    // Fallback: find nearest price within +/- 15 minutes
    for offset_minutes in [0, 15, -15, 30, -30] {
        let adjusted_time = time + chrono::Duration::minutes(offset_minutes);
        if let Some(price) = prices.get(&adjusted_time) {
            return price.price_czk / 1000.0;
        }
    }

    // Final fallback: use average day/night price
    let hour = time.hour();
    if (8..20).contains(&hour) {
        5.0 // High price CZK (fallback)
    } else {
        2.0 // Low price CZK (fallback)
    }
}

/// State for a single strategy simulation
struct StrategySimState {
    name: String,
    strategy: WinterAdaptiveStrategy,
    current_soc: f32,
    cumulative_profit_czk: f32,
    total_imported_kwh: f32,
    total_exported_kwh: f32,
    total_charged_kwh: f32,
    total_discharged_kwh: f32,
}

impl StrategySimState {
    fn new(name: &str, strategy: WinterAdaptiveStrategy, initial_soc: f32) -> Self {
        Self {
            name: name.to_string(),
            strategy,
            current_soc: initial_soc,
            cumulative_profit_czk: 0.0,
            total_imported_kwh: 0.0,
            total_exported_kwh: 0.0,
            total_charged_kwh: 0.0,
            total_discharged_kwh: 0.0,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Load Data
    // Note: Paths are hardcoded for the example, might need adjustment or env vars
    let data_path = Path::new("simulation_data/inverter_data.csv");
    if !data_path.exists() {
        println!(
            "Simulation data not found at {:?}. Skipping simulation.",
            data_path
        );
        return Ok(());
    }

    let rows = parse_csv(data_path)?;
    println!("Loaded {} rows of inverter data", rows.len());

    // 1.5. Load Real OTE Prices
    let price_csv_path = Path::new("crates/fluxion-core/data/prices_2025_10.csv");
    let prices = if price_csv_path.exists() {
        match load_price_csv(price_csv_path) {
            Ok(p) => {
                println!(
                    "Loaded {} real OTE price records from {:?}",
                    p.len(),
                    price_csv_path
                );
                p
            }
            Err(e) => {
                println!(
                    "Warning: Could not load real prices: {}. Using synthetic prices as fallback.",
                    e
                );
                HashMap::new()
            }
        }
    } else {
        println!(
            "Price data not found at {:?}. Using synthetic prices.",
            price_csv_path
        );
        HashMap::new()
    };

    // 2. Aggregate
    let blocks = aggregate_to_blocks(&rows);
    println!("Aggregated into {} 15-minute blocks", blocks.len());

    if blocks.is_empty() {
        println!("No data blocks to simulate.");
        return Ok(());
    }

    // 3. Setup Strategies
    let initial_soc = blocks[0].start_soc;

    // Strategy 1: Default Winter Adaptive
    let default_config = WinterAdaptiveConfig::default();
    let default_strategy = WinterAdaptiveStrategy::new(default_config);

    // Strategy 2: Aggressive Winter Adaptive (more expensive blocks avoided)
    let aggressive_config = WinterAdaptiveConfig {
        top_expensive_blocks: 16, // Avoid top 16 blocks instead of 12
        ..WinterAdaptiveConfig::default()
    };
    let aggressive_strategy = WinterAdaptiveStrategy::new(aggressive_config);

    // Create simulation states
    let mut sim_states = vec![
        StrategySimState::new("Default Winter Adaptive", default_strategy, initial_soc),
        StrategySimState::new(
            "Aggressive Winter Adaptive",
            aggressive_strategy,
            initial_soc,
        ),
    ];

    let config = ControlConfig {
        battery_capacity_kwh: 23.0,
        max_battery_charge_rate_kw: 10.0,
        battery_efficiency: 0.95,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..Default::default()
    };

    // Pricing configuration with fees
    let pricing_config = PricingConfig {
        spot_price_entity: "sensor.spot_price".to_string(),
        tomorrow_price_entity: None,
        use_spot_prices_to_buy: true,
        use_spot_prices_to_sell: true,
        fixed_buy_price_czk: PriceSchedule::Flat(0.0),
        fixed_sell_price_czk: PriceSchedule::Flat(0.0),
        spot_buy_fee_czk: 0.5,
        spot_sell_fee_czk: 0.5,
        hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_string(),
        hdo_low_tariff_czk: 0.50,
        hdo_high_tariff_czk: 1.80,
    };

    // Buy fee added to import (grid fees are now handled via HDO tariffs)
    let buy_fee_czk = pricing_config.spot_buy_fee_czk;

    // Create price blocks for the whole period for lookahead
    let all_price_blocks: Vec<TimeBlockPrice> = blocks
        .iter()
        .map(|b| {
            let spot_price = get_price_czk(b.start_time, &prices);
            TimeBlockPrice {
                block_start: b.start_time,
                duration_minutes: 15,
                price_czk_per_kwh: spot_price + buy_fee_czk, // Import price = Spot + Buy Fee
                effective_price_czk_per_kwh: spot_price + buy_fee_czk,
            }
        })
        .collect();

    println!("Starting simulation...");

    // 4. Run Simulation
    for (i, block) in blocks.iter().enumerate() {
        let price_block = &all_price_blocks[i];

        // For each strategy, we need a separate context because SOC is different
        for state in &mut sim_states {
            // Calculate spot price for export (no grid fee on export)
            let spot_price = get_price_czk(block.start_time, &prices);

            let context = EvaluationContext {
                price_block,
                control_config: &config,
                current_battery_soc: state.current_soc, // Use SIMULATED SOC
                solar_forecast_kwh: block.pv_generation_kwh,
                consumption_forecast_kwh: block.consumption_kwh,
                grid_export_price_czk_per_kwh: spot_price, // Export at spot price (no grid fee)
                all_price_blocks: Some(&all_price_blocks),
                backup_discharge_min_soc: config.hardware_min_battery_soc,
                grid_import_today_kwh: None, // Not tracked in simulation
                consumption_today_kwh: None, // Not tracked in simulation
                solar_forecast_total_today_kwh: 0.0,
                solar_forecast_remaining_today_kwh: 0.0,
                solar_forecast_tomorrow_kwh: 0.0,
                battery_avg_charge_price_czk_per_kwh: 0.0,
            };

            let evaluation = state.strategy.evaluate(&context);

            // Update cumulative stats
            state.cumulative_profit_czk += evaluation.net_profit_czk;
            state.total_imported_kwh += evaluation.energy_flows.grid_import_kwh;
            state.total_exported_kwh += evaluation.energy_flows.grid_export_kwh;
            state.total_charged_kwh += evaluation.energy_flows.battery_charge_kwh;
            state.total_discharged_kwh += evaluation.energy_flows.battery_discharge_kwh;

            // Update SOC
            let energy_in = evaluation.energy_flows.battery_charge_kwh * config.battery_efficiency;
            let energy_out =
                evaluation.energy_flows.battery_discharge_kwh / config.battery_efficiency;
            let net_energy_change = energy_in - energy_out;

            let soc_change = (net_energy_change / config.battery_capacity_kwh) * 100.0;
            state.current_soc = (state.current_soc + soc_change).clamp(0.0, 100.0);

            if state.cumulative_profit_czk.is_infinite() {
                println!(
                    "WARNING: Infinite profit detected for {} at block {}",
                    state.name, i
                );
            }
        }
    }

    // 5. Report
    println!(
        "\n{:<30} | {:<15} | {:<10} | {:<10} | {:<10} | {:<10}",
        "Strategy", "Profit (CZK)", "Imp (kWh)", "Exp (kWh)", "Disch(kWh)", "Final SOC"
    );
    println!("{}", "-".repeat(100));

    for state in sim_states {
        println!(
            "{:<30} | {:<15.2} | {:<10.2} | {:<10.2} | {:<10.2} | {:<5.1}%",
            state.name,
            state.cumulative_profit_czk,
            state.total_imported_kwh,
            state.total_exported_kwh,
            state.total_discharged_kwh,
            state.current_soc
        );
    }

    Ok(())
}
