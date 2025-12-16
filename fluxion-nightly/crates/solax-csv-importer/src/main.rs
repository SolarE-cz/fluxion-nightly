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
use clap::Parser;
use rusqlite::{Connection, params};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "solax-csv-importer")]
#[command(about = "Import Solax export CSV data into SQLite database", long_about = None)]
struct Cli {
    /// Path to the CSV file to import
    #[arg(short, long)]
    csv: PathBuf,

    /// Path to the SQLite database (will be created if it doesn't exist)
    #[arg(short, long, default_value = "solax_data.db")]
    database: PathBuf,
}

#[derive(Debug)]
struct SolaxRecord {
    update_time: NaiveDateTime,
    device_working_condition: String,
    daily_pv_yield: f64,
    total_pv_yield: f64,
    daily_inverter_output: f64,
    total_inverter_output: f64,
    daily_inverter_eps_yield: f64,
    total_inverter_eps_yield: f64,
    daily_battery_discharge: f64,
    total_battery_discharge: f64,
    daily_battery_charge: f64,
    total_battery_charge: f64,
    daily_exported_energy: f64,
    total_exported_energy: f64,
    daily_imported_energy: f64,
    total_imported_energy: f64,
    total_battery_soc: f64,
    realtime_power: i32,
    total_pv_power: i32,
    total_battery_power: i32,
    mbmu_sn: String,
    mbmu_battery_type: String,
    mbmu_battery_soc: f64,
    mbmu_battery_temperature: f64,
    mbmu_battery_current: f64,
    mbmu_battery_voltage: f64,
    mbmu_battery_power: i32,
    grid_power: i32,
    inv_power_factor: f64,
    grid_frequency: f64,
    effective_ac_output_time: f64,
    total_effective_ac_output_time: f64,
    mppt1_power: i32,
    mppt1_voltage: f64,
    mppt1_current: f64,
    mppt2_power: i32,
    mppt2_voltage: f64,
    mppt2_current: f64,
    ac_power_l1: i32,
    ac_voltage_l1: f64,
    ac_current_l1: f64,
    ac_power_l2: i32,
    ac_voltage_l2: f64,
    ac_current_l2: f64,
    ac_power_l3: i32,
    ac_voltage_l3: f64,
    ac_current_l3: f64,
    eps_power_l1: i32,
    eps_voltage_l1: f64,
    eps_current_l1: f64,
    eps_power_l2: i32,
    eps_voltage_l2: f64,
    eps_current_l2: f64,
    eps_power_l3: i32,
    eps_voltage_l3: f64,
    eps_current_l3: f64,
    inverter_temperature: f64,
}

fn create_database(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS solax_data (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            update_time TEXT NOT NULL UNIQUE,
            device_working_condition TEXT NOT NULL,
            daily_pv_yield REAL NOT NULL,
            total_pv_yield REAL NOT NULL,
            daily_inverter_output REAL NOT NULL,
            total_inverter_output REAL NOT NULL,
            daily_inverter_eps_yield REAL NOT NULL,
            total_inverter_eps_yield REAL NOT NULL,
            daily_battery_discharge REAL NOT NULL,
            total_battery_discharge REAL NOT NULL,
            daily_battery_charge REAL NOT NULL,
            total_battery_charge REAL NOT NULL,
            daily_exported_energy REAL NOT NULL,
            total_exported_energy REAL NOT NULL,
            daily_imported_energy REAL NOT NULL,
            total_imported_energy REAL NOT NULL,
            total_battery_soc REAL NOT NULL,
            realtime_power INTEGER NOT NULL,
            total_pv_power INTEGER NOT NULL,
            total_battery_power INTEGER NOT NULL,
            mbmu_sn TEXT NOT NULL,
            mbmu_battery_type TEXT NOT NULL,
            mbmu_battery_soc REAL NOT NULL,
            mbmu_battery_temperature REAL NOT NULL,
            mbmu_battery_current REAL NOT NULL,
            mbmu_battery_voltage REAL NOT NULL,
            mbmu_battery_power INTEGER NOT NULL,
            grid_power INTEGER NOT NULL,
            inv_power_factor REAL NOT NULL,
            grid_frequency REAL NOT NULL,
            effective_ac_output_time REAL NOT NULL,
            total_effective_ac_output_time REAL NOT NULL,
            mppt1_power INTEGER NOT NULL,
            mppt1_voltage REAL NOT NULL,
            mppt1_current REAL NOT NULL,
            mppt2_power INTEGER NOT NULL,
            mppt2_voltage REAL NOT NULL,
            mppt2_current REAL NOT NULL,
            ac_power_l1 INTEGER NOT NULL,
            ac_voltage_l1 REAL NOT NULL,
            ac_current_l1 REAL NOT NULL,
            ac_power_l2 INTEGER NOT NULL,
            ac_voltage_l2 REAL NOT NULL,
            ac_current_l2 REAL NOT NULL,
            ac_power_l3 INTEGER NOT NULL,
            ac_voltage_l3 REAL NOT NULL,
            ac_current_l3 REAL NOT NULL,
            eps_power_l1 INTEGER NOT NULL,
            eps_voltage_l1 REAL NOT NULL,
            eps_current_l1 REAL NOT NULL,
            eps_power_l2 INTEGER NOT NULL,
            eps_voltage_l2 REAL NOT NULL,
            eps_current_l2 REAL NOT NULL,
            eps_power_l3 INTEGER NOT NULL,
            eps_voltage_l3 REAL NOT NULL,
            eps_current_l3 REAL NOT NULL,
            inverter_temperature REAL NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_update_time ON solax_data(update_time)",
        [],
    )?;

    Ok(())
}

fn parse_value_or_default<T: std::str::FromStr + Default>(s: &str) -> T {
    if s.is_empty() {
        T::default()
    } else {
        s.parse().unwrap_or_default()
    }
}

fn parse_csv(csv_path: &PathBuf) -> Result<Vec<SolaxRecord>> {
    let mut reader = csv::Reader::from_path(csv_path).context("Failed to open CSV file")?;

    let mut records = Vec::new();

    for result in reader.records() {
        let record = result.context("Failed to read CSV record")?;

        let datetime_str = record[0].trim_end_matches('.');
        let update_time = NaiveDateTime::parse_from_str(datetime_str, "%Y-%m-%d %H:%M:%S")
            .context(format!("Failed to parse datetime: {}", &record[0]))?;

        records.push(SolaxRecord {
            update_time,
            device_working_condition: record[1].to_string(),
            daily_pv_yield: parse_value_or_default(&record[2]),
            total_pv_yield: parse_value_or_default(&record[3]),
            daily_inverter_output: parse_value_or_default(&record[4]),
            total_inverter_output: parse_value_or_default(&record[5]),
            daily_inverter_eps_yield: parse_value_or_default(&record[6]),
            total_inverter_eps_yield: parse_value_or_default(&record[7]),
            daily_battery_discharge: parse_value_or_default(&record[8]),
            total_battery_discharge: parse_value_or_default(&record[9]),
            daily_battery_charge: parse_value_or_default(&record[10]),
            total_battery_charge: parse_value_or_default(&record[11]),
            daily_exported_energy: parse_value_or_default(&record[12]),
            total_exported_energy: parse_value_or_default(&record[13]),
            daily_imported_energy: parse_value_or_default(&record[14]),
            total_imported_energy: parse_value_or_default(&record[15]),
            total_battery_soc: parse_value_or_default(&record[16]),
            realtime_power: parse_value_or_default(&record[17]),
            total_pv_power: parse_value_or_default(&record[18]),
            total_battery_power: parse_value_or_default(&record[19]),
            mbmu_sn: record[20].to_string(),
            mbmu_battery_type: record[21].to_string(),
            mbmu_battery_soc: parse_value_or_default(&record[22]),
            mbmu_battery_temperature: parse_value_or_default(&record[23]),
            mbmu_battery_current: parse_value_or_default(&record[24]),
            mbmu_battery_voltage: parse_value_or_default(&record[25]),
            mbmu_battery_power: parse_value_or_default(&record[26]),
            grid_power: parse_value_or_default(&record[27]),
            inv_power_factor: parse_value_or_default(&record[28]),
            grid_frequency: parse_value_or_default(&record[29]),
            effective_ac_output_time: parse_value_or_default(&record[30]),
            total_effective_ac_output_time: parse_value_or_default(&record[31]),
            mppt1_power: parse_value_or_default(&record[32]),
            mppt1_voltage: parse_value_or_default(&record[33]),
            mppt1_current: parse_value_or_default(&record[34]),
            mppt2_power: parse_value_or_default(&record[35]),
            mppt2_voltage: parse_value_or_default(&record[36]),
            mppt2_current: parse_value_or_default(&record[37]),
            ac_power_l1: parse_value_or_default(&record[38]),
            ac_voltage_l1: parse_value_or_default(&record[39]),
            ac_current_l1: parse_value_or_default(&record[40]),
            ac_power_l2: parse_value_or_default(&record[41]),
            ac_voltage_l2: parse_value_or_default(&record[42]),
            ac_current_l2: parse_value_or_default(&record[43]),
            ac_power_l3: parse_value_or_default(&record[44]),
            ac_voltage_l3: parse_value_or_default(&record[45]),
            ac_current_l3: parse_value_or_default(&record[46]),
            eps_power_l1: parse_value_or_default(&record[47]),
            eps_voltage_l1: parse_value_or_default(&record[48]),
            eps_current_l1: parse_value_or_default(&record[49]),
            eps_power_l2: parse_value_or_default(&record[50]),
            eps_voltage_l2: parse_value_or_default(&record[51]),
            eps_current_l2: parse_value_or_default(&record[52]),
            eps_power_l3: parse_value_or_default(&record[53]),
            eps_voltage_l3: parse_value_or_default(&record[54]),
            eps_current_l3: parse_value_or_default(&record[55]),
            inverter_temperature: parse_value_or_default(&record[56]),
        });
    }

    Ok(records)
}

fn insert_records(conn: &Connection, records: Vec<SolaxRecord>) -> Result<usize> {
    let mut inserted = 0;
    let mut skipped = 0;

    for record in records {
        let result = conn.execute(
            "INSERT INTO solax_data (
                update_time, device_working_condition, daily_pv_yield, total_pv_yield,
                daily_inverter_output, total_inverter_output, daily_inverter_eps_yield,
                total_inverter_eps_yield, daily_battery_discharge, total_battery_discharge,
                daily_battery_charge, total_battery_charge, daily_exported_energy,
                total_exported_energy, daily_imported_energy, total_imported_energy,
                total_battery_soc, realtime_power, total_pv_power, total_battery_power,
                mbmu_sn, mbmu_battery_type, mbmu_battery_soc, mbmu_battery_temperature,
                mbmu_battery_current, mbmu_battery_voltage, mbmu_battery_power, grid_power,
                inv_power_factor, grid_frequency, effective_ac_output_time,
                total_effective_ac_output_time, mppt1_power, mppt1_voltage, mppt1_current,
                mppt2_power, mppt2_voltage, mppt2_current, ac_power_l1, ac_voltage_l1,
                ac_current_l1, ac_power_l2, ac_voltage_l2, ac_current_l2, ac_power_l3,
                ac_voltage_l3, ac_current_l3, eps_power_l1, eps_voltage_l1, eps_current_l1,
                eps_power_l2, eps_voltage_l2, eps_current_l2, eps_power_l3, eps_voltage_l3,
                eps_current_l3, inverter_temperature
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30,
                ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44,
                ?45, ?46, ?47, ?48, ?49, ?50, ?51, ?52, ?53, ?54, ?55, ?56, ?57
            )",
            params![
                record.update_time.to_string(),
                record.device_working_condition,
                record.daily_pv_yield,
                record.total_pv_yield,
                record.daily_inverter_output,
                record.total_inverter_output,
                record.daily_inverter_eps_yield,
                record.total_inverter_eps_yield,
                record.daily_battery_discharge,
                record.total_battery_discharge,
                record.daily_battery_charge,
                record.total_battery_charge,
                record.daily_exported_energy,
                record.total_exported_energy,
                record.daily_imported_energy,
                record.total_imported_energy,
                record.total_battery_soc,
                record.realtime_power,
                record.total_pv_power,
                record.total_battery_power,
                record.mbmu_sn,
                record.mbmu_battery_type,
                record.mbmu_battery_soc,
                record.mbmu_battery_temperature,
                record.mbmu_battery_current,
                record.mbmu_battery_voltage,
                record.mbmu_battery_power,
                record.grid_power,
                record.inv_power_factor,
                record.grid_frequency,
                record.effective_ac_output_time,
                record.total_effective_ac_output_time,
                record.mppt1_power,
                record.mppt1_voltage,
                record.mppt1_current,
                record.mppt2_power,
                record.mppt2_voltage,
                record.mppt2_current,
                record.ac_power_l1,
                record.ac_voltage_l1,
                record.ac_current_l1,
                record.ac_power_l2,
                record.ac_voltage_l2,
                record.ac_current_l2,
                record.ac_power_l3,
                record.ac_voltage_l3,
                record.ac_current_l3,
                record.eps_power_l1,
                record.eps_voltage_l1,
                record.eps_current_l1,
                record.eps_power_l2,
                record.eps_voltage_l2,
                record.eps_current_l2,
                record.eps_power_l3,
                record.eps_voltage_l3,
                record.eps_current_l3,
                record.inverter_temperature,
            ],
        );

        match result {
            Ok(_) => inserted += 1,
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                skipped += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }

    println!("Inserted {inserted} records, skipped {skipped} duplicates");
    Ok(inserted)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("Opening database: {}", cli.database.display());
    let conn = Connection::open(&cli.database).context("Failed to open database")?;

    println!("Creating database schema...");
    create_database(&conn)?;

    println!("Parsing CSV file: {}", cli.csv.display());
    let records = parse_csv(&cli.csv)?;
    println!("Parsed {} records", records.len());

    println!("Inserting records into database...");
    insert_records(&conn, records)?;

    println!("Import completed successfully!");

    Ok(())
}
