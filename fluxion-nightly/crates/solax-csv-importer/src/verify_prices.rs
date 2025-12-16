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
use clap::Parser;
use rusqlite::Connection;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "verify-prices")]
#[command(about = "Verify OTE prices in SQLite database", long_about = None)]
struct Cli {
    /// Path to the SQLite database
    #[arg(short, long, default_value = "solax_data.db")]
    database: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("Opening database: {}", cli.database.display());
    let conn = Connection::open(&cli.database).context("Failed to open database")?;

    // Count total records
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ote_prices", [], |row| row.get(0))
        .context("Failed to count records")?;

    println!("Total OTE price records: {count}");

    // Get date range
    let (min_date, max_date): (String, String) = conn
        .query_row(
            "SELECT MIN(datetime), MAX(datetime) FROM ote_prices",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("Failed to get date range")?;

    println!("Date range: {min_date} to {max_date}");

    // Show some sample records
    println!("\nFirst 5 records:");
    let mut stmt = conn
        .prepare("SELECT datetime, price_eur, price_czk FROM ote_prices ORDER BY datetime LIMIT 5")
        .context("Failed to prepare statement")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .context("Failed to query records")?;

    for row in rows {
        let (datetime, price_eur, price_czk) = row?;
        println!("  {datetime} - EUR: {price_eur:.2}, CZK: {price_czk:.2}");
    }

    println!("\nLast 5 records:");
    let mut stmt = conn
        .prepare(
            "SELECT datetime, price_eur, price_czk FROM ote_prices ORDER BY datetime DESC LIMIT 5",
        )
        .context("Failed to prepare statement")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .context("Failed to query records")?;

    let mut last_records = Vec::new();
    for row in rows {
        last_records.push(row?);
    }
    last_records.reverse();

    for (datetime, price_eur, price_czk) in last_records {
        println!("  {datetime} - EUR: {price_eur:.2}, CZK: {price_czk:.2}");
    }

    // Get average price for November
    let avg_price: f64 = conn
        .query_row(
            "SELECT AVG(price_eur) FROM ote_prices WHERE datetime LIKE '2025-11-%'",
            [],
            |row| row.get(0),
        )
        .context("Failed to get average price")?;

    println!("\nAverage price for November 2025: {avg_price:.2} EUR/MWh");

    Ok(())
}
