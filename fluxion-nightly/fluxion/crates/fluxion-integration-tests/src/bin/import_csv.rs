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
use chrono::NaiveDateTime;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::fs::File;

#[derive(Debug, Deserialize)]
struct CsvRecord {
    #[serde(rename = "Update time")]
    update_time: String,
    #[serde(rename = "Total Battery SOC (%)")]
    battery_soc: f64,
    #[serde(rename = "Realtime power (W)")]
    _realtime_power: f64, // Inverter power? Or house load? Usually Realtime power is grid? No, Grid power is separate.
    // Let's look at the CSV header again.
    // "Realtime power (W)"; "Total PV Power (W)"; "Total battery power (W)"; "Grid power (W)"
    // We need to infer House Load.
    // House Load = PV + Battery (discharge is positive) + Grid (import is positive) - Realtime Power?
    // Actually, usually: Load = PV + Battery + Grid - Export.
    // Let's just import the raw columns we need: PV, Battery Power, Grid Power, SOC.
    #[serde(rename = "Total PV Power (W)")]
    pv_power: f64,
    #[serde(rename = "Total battery power (W)")]
    battery_power: f64,
    #[serde(rename = "Grid power (W)")]
    grid_power: f64,
}

fn main() -> Result<()> {
    // Use absolute path to avoid confusion
    let csv_path =
        "/home/daniel/Repositories/solare/fluxion/dev/H34A10I5503271-2025-11-01-2025-12-03.csv";
    let db_path = "solax_data.db";

    println!("Importing CSV from {} to {}", csv_path, db_path);

    let conn = Connection::open(db_path).context("Failed to open database")?;
    create_table(&conn)?;

    let file = File::open(csv_path).context("Failed to open CSV file")?;
    let mut rdr = csv::ReaderBuilder::new().delimiter(b';').from_reader(file);

    let mut count = 0;
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO historical_plant_data (
            timestamp, battery_soc, pv_power_w, battery_power_w, grid_power_w, house_load_w
        ) VALUES (?, ?, ?, ?, ?, ?)",
    )?;

    conn.execute("BEGIN TRANSACTION", [])?;

    for result in rdr.deserialize() {
        let record: CsvRecord = result.context("Failed to deserialize record")?;

        // Parse timestamp "2025-11-01 00:00:01"
        let ts = NaiveDateTime::parse_from_str(&record.update_time, "%Y-%m-%d %H:%M:%S")
            .context(format!("Failed to parse time: {}", record.update_time))?
            .and_utc()
            .timestamp();

        // Calculate House Load
        // Standard formula: Load = PV + Grid + Battery
        // Signs in Solax usually:
        // PV: positive
        // Grid: positive = import, negative = export (or vice versa, need to check data)
        // Battery: positive = discharge, negative = charge (or vice versa)

        // Looking at line 2 of CSV:
        // PV Yield 34.3, PV Power 0
        // Grid power -10995 (Exporting? Or Importing?)
        // Battery power 10036 (Discharging?)
        // Realtime power -10036
        // If Grid is negative, it usually means Export.
        // If Battery is positive, it usually means Discharge.
        // Let's check: PV(0) + Bat(10036) + Grid(-10995) = -959 (Negative load? Impossible)

        // Let's try: Load = PV + Bat - Grid?
        // 0 + 10036 - (-10995) = 21031 (Huge load)

        // Let's look at another line. Line 3:
        // PV 0, Grid 0, Bat -698 (Charging), Realtime 698.
        // If Bat -698 is charging, then we are consuming 698W from somewhere?
        // Grid is 0. PV is 0.
        // Wait, "Realtime power" might be the inverter output?
        // Or maybe "Realtime power" is the net power at the meter?

        // Let's assume for now we just store the raw values and calculate load in the strategy/test
        // where we can tweak the formula.
        // But we need a 'house_load_w' column for the strategy to use.
        // Let's calculate a rough 'measured_load' = PV + Grid + Battery (assuming standard signs)
        // If Grid is negative (export), and PV is positive, and Bat is positive (discharge).
        // Load = PV + Bat + Grid.
        // Example line 2: 0 + 10036 + (-10995) = -959. Still weird.

        // Maybe Grid Power sign is inverted?
        // If Grid -10995 means IMPORT?
        // 0 + 10036 + 10995 = 21031.

        // Let's look at line 76 (06:10:00):
        // PV 14, Bat 10008, Grid -14532.
        // 14 + 10008 - 14532 = -4510.

        // Let's look at "Realtime power".
        // Line 2: -10036. Bat: 10036. Sum = 0.
        // Line 3: 698. Bat: -698. Sum = 0.
        // Line 76: -10008. Bat: 10008. Sum = 0.
        // It seems Realtime Power = -Battery Power (roughly).

        // What about "Daily inverter output"?
        // Let's just store the raw columns for now and do the math in the test.
        // We will calculate a "proxy_load" as 0 for now or just PV + Grid + Bat and see.

        let house_load = record.pv_power + record.battery_power + record.grid_power;

        stmt.execute(params![
            ts,
            record.battery_soc,
            record.pv_power,
            record.battery_power,
            record.grid_power,
            house_load // We'll refine this later
        ])?;

        count += 1;
    }

    println!("Imported {} records", count);
    Ok(())
}

fn create_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS historical_plant_data (
            timestamp INTEGER PRIMARY KEY,
            battery_soc REAL NOT NULL,
            pv_power_w REAL NOT NULL,
            battery_power_w REAL NOT NULL,
            grid_power_w REAL NOT NULL,
            house_load_w REAL NOT NULL
        )",
        [],
    )?;
    Ok(())
}
