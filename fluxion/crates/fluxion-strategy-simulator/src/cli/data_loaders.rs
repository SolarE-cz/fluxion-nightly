// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Data loaders for converting various sources to SyntheticDay format.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;

use crate::synthetic_data::{
    SyntheticBlock, SyntheticDay, SyntheticDayConfig, SyntheticDayGenerator,
};

/// Trait for loading data from various sources into SyntheticDay format
pub trait DataLoader {
    /// Load data for the specified date (if applicable)
    fn load(&self, date: Option<NaiveDate>) -> Result<SyntheticDay>;
}

/// Loader for synthetic data using built-in scenarios
pub struct SyntheticLoader {
    pub config: SyntheticDayConfig,
}

impl DataLoader for SyntheticLoader {
    fn load(&self, _date: Option<NaiveDate>) -> Result<SyntheticDay> {
        SyntheticDayGenerator::generate(&self.config)
    }
}

/// Loader for SQLite database (solax_data.db)
pub struct SqliteLoader {
    db_path: String,
    battery_capacity_kwh: f32,
    initial_soc: f32,
    hdo_low_tariff_czk: f32,
    hdo_high_tariff_czk: f32,
}

impl SqliteLoader {
    pub fn new(db_path: String, battery_capacity_kwh: f32, initial_soc: f32) -> Self {
        Self {
            db_path,
            battery_capacity_kwh,
            initial_soc,
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        }
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(&self.db_path)
            .with_context(|| format!("Failed to open database at {}", self.db_path))
    }

    fn is_hdo_low_tariff(&self, hour: usize) -> bool {
        // Default Czech HDO schedule
        (0..6).contains(&hour) || (13..15).contains(&hour) || (20..22).contains(&hour)
    }
}

impl DataLoader for SqliteLoader {
    fn load(&self, date: Option<NaiveDate>) -> Result<SyntheticDay> {
        let date = date.ok_or_else(|| anyhow::anyhow!("Date is required for SQLite loader"))?;

        let conn = self.connect()?;

        // Calculate timestamp range for the day
        let start_of_day = date.and_hms_opt(0, 0, 0).expect("valid time");
        let end_of_day = date.and_hms_opt(23, 59, 59).expect("valid time");

        let start_ts = Utc.from_utc_datetime(&start_of_day).timestamp();
        let end_ts = Utc.from_utc_datetime(&end_of_day).timestamp();

        // Load consumption and solar data (house_load_w, pv_power_w from historical_plant_data)
        let mut stmt = conn.prepare(
            "SELECT timestamp, house_load_w, pv_power_w
             FROM historical_plant_data
             WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;

        let plant_records: Vec<(i64, f32, f32)> = stmt
            .query_map([start_ts, end_ts], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)? as f32,
                    row.get::<_, f64>(2).unwrap_or(0.0) as f32,
                ))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        // Load price data
        let mut stmt = conn.prepare(
            "SELECT ts, price FROM prices
             WHERE ts >= ?1 AND ts <= ?2
             ORDER BY ts ASC",
        )?;

        let price_records: Vec<(i64, f32)> = stmt
            .query_map([start_ts, end_ts], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)? as f32))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        // Aggregate into 96 15-minute blocks
        let mut blocks = Vec::with_capacity(96);
        let mut total_consumption = 0.0;
        let mut total_solar = 0.0;
        let base_dt = Utc.from_utc_datetime(&start_of_day);

        for i in 0..96 {
            let block_start = base_dt + chrono::Duration::minutes(i as i64 * 15);
            let block_end = block_start + chrono::Duration::minutes(15);
            let block_start_ts = block_start.timestamp();
            let block_end_ts = block_end.timestamp();
            let hour = i / 4;

            // Aggregate consumption and solar (5-min samples -> 15-min block)
            // Watts to kWh for 5-min interval: W / 1000 * (5/60) = kWh
            let samples_in_block: Vec<(f32, f32)> = plant_records
                .iter()
                .filter(|(ts, _, _)| *ts >= block_start_ts && *ts < block_end_ts)
                .map(|(_, load_w, pv_w)| {
                    let consumption = load_w / 1000.0 * (5.0 / 60.0);
                    let solar = pv_w / 1000.0 * (5.0 / 60.0);
                    (consumption, solar)
                })
                .collect();

            let (consumption_kwh, solar_kwh) = if samples_in_block.is_empty() {
                (0.25, 0.0) // Default: 1kW base load, no solar
            } else {
                let consumption: f32 = samples_in_block.iter().map(|(c, _)| c).sum();
                let solar: f32 = samples_in_block.iter().map(|(_, s)| s).sum();
                (consumption, solar)
            };

            total_consumption += consumption_kwh;
            total_solar += solar_kwh;

            // Find price for this block (prices are already in 15-min blocks)
            let price_czk_per_kwh = price_records
                .iter()
                .find(|(ts, _)| {
                    let price_time = Utc.timestamp_opt(*ts, 0).single().unwrap_or_default();
                    price_time >= block_start && price_time < block_end
                })
                .map(|(_, price)| *price)
                .unwrap_or(2.5); // Default spot price

            // Calculate grid fees
            let is_hdo_low = self.is_hdo_low_tariff(hour);
            let grid_fee_czk_per_kwh = if is_hdo_low {
                self.hdo_low_tariff_czk
            } else {
                self.hdo_high_tariff_czk
            };

            let effective_price_czk_per_kwh = price_czk_per_kwh + grid_fee_czk_per_kwh;

            blocks.push(SyntheticBlock {
                index: i,
                timestamp: block_start,
                consumption_kwh,
                solar_kwh,
                price_czk_per_kwh,
                grid_fee_czk_per_kwh,
                effective_price_czk_per_kwh,
                is_hdo_low_tariff: is_hdo_low,
            });
        }

        Ok(SyntheticDay {
            date,
            blocks,
            price_scenario_name: format!("Historical ({})", date),
            total_consumption_kwh: total_consumption,
            total_solar_kwh: total_solar,
            initial_soc: self.initial_soc,
            battery_capacity_kwh: self.battery_capacity_kwh,
        })
    }
}

/// Loader for JSON export files (FluxionExport format)
pub struct JsonExportLoader {
    json_path: String,
    battery_capacity_kwh: f32,
    initial_soc: f32,
}

impl JsonExportLoader {
    pub fn new(json_path: String, battery_capacity_kwh: f32, initial_soc: f32) -> Self {
        Self {
            json_path,
            battery_capacity_kwh,
            initial_soc,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FluxionExport {
    prices: PricesData,
    consumption: ConsumptionData,
    inv: Vec<InverterData>,
}

#[derive(Debug, Deserialize)]
struct PricesData {
    blocks: Vec<PriceBlock>,
}

#[derive(Debug, Deserialize)]
struct PriceBlock {
    ts: i64,
    p: f32,
    #[allow(dead_code)]
    st: String,
}

#[derive(Debug, Deserialize)]
struct ConsumptionData {
    ema_kwh: f32,
}

#[derive(Debug, Deserialize)]
struct InverterData {
    #[serde(rename = "bat_cap")]
    battery_capacity: f32,
}

impl DataLoader for JsonExportLoader {
    fn load(&self, _date: Option<NaiveDate>) -> Result<SyntheticDay> {
        let content = std::fs::read_to_string(&self.json_path)
            .with_context(|| format!("Failed to read JSON file: {}", self.json_path))?;

        let export: FluxionExport = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse JSON file: {}", self.json_path))?;

        // Extract battery capacity from export if available
        let battery_capacity_kwh = export
            .inv
            .first()
            .map(|inv| inv.battery_capacity)
            .unwrap_or(self.battery_capacity_kwh);

        // Use EMA consumption to generate per-block consumption
        let daily_consumption_kwh = export.consumption.ema_kwh;
        let consumption_per_block = daily_consumption_kwh / 96.0;

        // Convert price blocks to SyntheticBlocks
        let mut blocks = Vec::with_capacity(96);
        let mut total_consumption = 0.0;

        for (i, price_block) in export.prices.blocks.iter().enumerate().take(96) {
            let timestamp = DateTime::from_timestamp(price_block.ts, 0).unwrap_or_else(Utc::now);

            let hour = i / 4;

            // Determine HDO tariff (default Czech schedule)
            let is_hdo_low =
                (0..6).contains(&hour) || (13..15).contains(&hour) || (20..22).contains(&hour);
            let grid_fee_czk_per_kwh = if is_hdo_low { 0.50 } else { 1.80 };

            let effective_price_czk_per_kwh = price_block.p + grid_fee_czk_per_kwh;

            total_consumption += consumption_per_block;

            blocks.push(SyntheticBlock {
                index: i,
                timestamp,
                consumption_kwh: consumption_per_block,
                solar_kwh: 0.0, // JSON exports don't include solar breakdown
                price_czk_per_kwh: price_block.p,
                grid_fee_czk_per_kwh,
                effective_price_czk_per_kwh,
                is_hdo_low_tariff: is_hdo_low,
            });
        }

        // If less than 96 blocks, fill remaining with defaults
        while blocks.len() < 96 {
            let i = blocks.len();
            let last_block = blocks.last().unwrap();
            let timestamp = last_block.timestamp + chrono::Duration::minutes(15);

            blocks.push(SyntheticBlock {
                index: i,
                timestamp,
                consumption_kwh: consumption_per_block,
                solar_kwh: 0.0,
                price_czk_per_kwh: 2.5,
                grid_fee_czk_per_kwh: 1.80,
                effective_price_czk_per_kwh: 4.3,
                is_hdo_low_tariff: false,
            });
            total_consumption += consumption_per_block;
        }

        let date = blocks
            .first()
            .map(|b| b.timestamp.date_naive())
            .unwrap_or_else(|| Utc::now().date_naive());

        Ok(SyntheticDay {
            date,
            blocks,
            price_scenario_name: format!(
                "JSON Export ({})",
                Path::new(&self.json_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
            total_consumption_kwh: total_consumption,
            total_solar_kwh: 0.0,
            initial_soc: self.initial_soc,
            battery_capacity_kwh,
        })
    }
}
