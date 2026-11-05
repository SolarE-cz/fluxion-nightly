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

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExportData {
    bat_hist: Vec<BatteryHistory>,
    prices: Prices,
    consumption: Consumption,
}

#[derive(Debug, Deserialize)]
struct BatteryHistory {
    soc: f64,
    ts: i64,
}

#[derive(Debug, Deserialize)]
struct Prices {
    blocks: Vec<PriceBlock>,
}

#[derive(Debug, Deserialize)]
struct PriceBlock {
    p: f64,
    ts: i64,
}

#[derive(Debug, Deserialize)]
struct Consumption {
    today_kwh: f64,
    yesterday_kwh: f64,
}

fn main() -> Result<()> {
    println!("Fetching data from http://localhost:8099/export...");
    let response = reqwest::blocking::get("http://localhost:8099/export")
        .context("Failed to fetch data")?
        .json::<ExportData>()
        .context("Failed to parse JSON")?;

    println!(
        "Fetched {} battery history records",
        response.bat_hist.len()
    );
    println!("Fetched {} price blocks", response.prices.blocks.len());

    let db_path = "solax_data.db";
    let conn = Connection::open(db_path).context("Failed to open database")?;

    create_tables(&conn)?;
    insert_data(&conn, &response)?;

    println!("Data successfully saved to {}", db_path);
    Ok(())
}

fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS battery_history (
            ts INTEGER PRIMARY KEY,
            soc REAL NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS prices (
            ts INTEGER PRIMARY KEY,
            price REAL NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS consumption_summary (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            today_kwh REAL NOT NULL,
            yesterday_kwh REAL NOT NULL,
            updated_at INTEGER NOT NULL
        )",
        [],
    )?;

    Ok(())
}

fn insert_data(conn: &Connection, data: &ExportData) -> Result<()> {
    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO battery_history (ts, soc) VALUES (?, ?)")?;
    for record in &data.bat_hist {
        stmt.execute(params![record.ts, record.soc])?;
    }

    let mut stmt = conn.prepare("INSERT OR REPLACE INTO prices (ts, price) VALUES (?, ?)")?;
    for block in &data.prices.blocks {
        stmt.execute(params![block.ts, block.p])?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO consumption_summary (id, today_kwh, yesterday_kwh, updated_at) VALUES (1, ?, ?, ?)",
        params![data.consumption.today_kwh, data.consumption.yesterday_kwh, Utc::now().timestamp()],
    )?;

    Ok(())
}
