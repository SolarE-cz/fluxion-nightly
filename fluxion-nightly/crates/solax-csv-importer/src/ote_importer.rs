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
use chrono::NaiveDate;
use clap::Parser;
use fluxion_core::pricing::ote::OteMarketData;
use rusqlite::{Connection, params};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ote-price-importer")]
#[command(about = "Import OTE market prices into SQLite database", long_about = None)]
struct Cli {
    /// Path to the SQLite database (will be created if it doesn't exist)
    #[arg(short, long, default_value = "solax_data.db")]
    database: PathBuf,

    /// Start date for price data (format: YYYY-MM-DD)
    #[arg(short, long)]
    start_date: String,

    /// End date for price data (format: YYYY-MM-DD)
    #[arg(short, long)]
    end_date: String,
}

fn create_prices_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ote_prices (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            datetime TEXT NOT NULL UNIQUE,
            price_eur REAL NOT NULL,
            price_czk REAL NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_ote_datetime ON ote_prices(datetime)",
        [],
    )?;

    Ok(())
}

fn main() -> Result<()> {
    // Set up logging
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    println!("Opening database: {}", cli.database.display());
    let conn = Connection::open(&cli.database).context("Failed to open database")?;

    println!("Creating prices table...");
    create_prices_table(&conn)?;

    let start_date = NaiveDate::parse_from_str(&cli.start_date, "%Y-%m-%d")
        .context(format!("Failed to parse start date: {}", cli.start_date))?;

    let end_date = NaiveDate::parse_from_str(&cli.end_date, "%Y-%m-%d")
        .context(format!("Failed to parse end date: {}", cli.end_date))?;

    println!("Fetching OTE prices from {start_date} to {end_date}...");

    let fetcher = OteMarketData::new();
    let records = fetcher
        .fetch_range(start_date, end_date)
        .context("Failed to fetch OTE price data")?;

    println!("Fetched {} price records", records.len());

    if records.is_empty() {
        println!("No records to insert");
        return Ok(());
    }

    println!("Inserting records into database...");
    let mut inserted = 0;
    let mut skipped = 0;

    for record in records {
        let result = conn.execute(
            "INSERT INTO ote_prices (datetime, price_eur, price_czk) VALUES (?1, ?2, ?3)",
            params![
                record.datetime.to_rfc3339(),
                record.price_eur,
                record.price_czk,
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
    println!("Import completed successfully!");

    Ok(())
}
