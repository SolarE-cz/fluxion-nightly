// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use rusqlite::Connection;

use crate::types::{HistoricalRecord, PriceRecord};

/// Trait for data sources that provide historical plant data.
/// This abstraction allows swapping between SQLite (development) and InfluxDB (production).
pub trait DataSource: Send + Sync {
    /// List all days that have available data
    fn get_available_days(&self) -> Result<Vec<NaiveDate>>;

    /// Get historical plant data for a specific day
    fn get_day_data(&self, date: NaiveDate) -> Result<Vec<HistoricalRecord>>;

    /// Get price data for a specific day
    fn get_prices(&self, date: NaiveDate) -> Result<Vec<PriceRecord>>;

    /// Get all price data (for simulation across multiple days)
    fn get_all_prices(&self) -> Result<Vec<PriceRecord>>;
}

/// SQLite-based data source for historical plant data.
/// Used during development with the solax_data.db test database.
#[derive(Debug, Clone)]
pub struct SqliteDataSource {
    db_path: PathBuf,
}

impl SqliteDataSource {
    /// Create a new SQLite data source with the given database path
    pub fn new<P: AsRef<Path>>(db_path: P) -> Self {
        Self {
            db_path: db_path.as_ref().to_path_buf(),
        }
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(&self.db_path)
            .with_context(|| format!("Failed to open database at {}", self.db_path.display()))
    }
}

impl DataSource for SqliteDataSource {
    fn get_available_days(&self) -> Result<Vec<NaiveDate>> {
        let conn = self.connect()?;

        let mut stmt = conn.prepare(
            "SELECT DISTINCT date(timestamp, 'unixepoch') as day
             FROM historical_plant_data
             ORDER BY day ASC",
        )?;

        let days: Vec<NaiveDate> = stmt
            .query_map([], |row| {
                let day_str: String = row.get(0)?;
                Ok(day_str)
            })?
            .filter_map(std::result::Result::ok)
            .filter_map(|day_str| NaiveDate::parse_from_str(&day_str, "%Y-%m-%d").ok())
            .collect();

        Ok(days)
    }

    fn get_day_data(&self, date: NaiveDate) -> Result<Vec<HistoricalRecord>> {
        let conn = self.connect()?;

        // Calculate Unix timestamp range for the day
        let start_of_day = date.and_hms_opt(0, 0, 0).expect("valid time");
        let end_of_day = date.and_hms_opt(23, 59, 59).expect("valid time");

        let start_ts = Utc.from_utc_datetime(&start_of_day).timestamp();
        let end_ts = Utc.from_utc_datetime(&end_of_day).timestamp();

        let mut stmt = conn.prepare(
            "SELECT timestamp, battery_soc, pv_power_w, battery_power_w, grid_power_w, house_load_w
             FROM historical_plant_data
             WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;

        #[expect(clippy::cast_possible_truncation)]
        let records: Vec<HistoricalRecord> = stmt
            .query_map([start_ts, end_ts], |row| {
                let ts: i64 = row.get(0)?;
                Ok(HistoricalRecord {
                    timestamp: Utc.timestamp_opt(ts, 0).single().unwrap_or_default(),
                    battery_soc: row.get::<_, f64>(1)? as f32,
                    pv_power_w: row.get::<_, f64>(2)? as f32,
                    battery_power_w: row.get::<_, f64>(3)? as f32,
                    grid_power_w: row.get::<_, f64>(4)? as f32,
                    house_load_w: row.get::<_, f64>(5)? as f32,
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(records)
    }

    fn get_prices(&self, date: NaiveDate) -> Result<Vec<PriceRecord>> {
        let conn = self.connect()?;

        // Calculate Unix timestamp range for the day
        let start_of_day = date.and_hms_opt(0, 0, 0).expect("valid time");
        let end_of_day = date.and_hms_opt(23, 59, 59).expect("valid time");

        let start_ts = Utc.from_utc_datetime(&start_of_day).timestamp();
        let end_ts = Utc.from_utc_datetime(&end_of_day).timestamp();

        let mut stmt = conn.prepare(
            "SELECT ts, price FROM prices
             WHERE ts >= ?1 AND ts <= ?2
             ORDER BY ts ASC",
        )?;

        #[expect(clippy::cast_possible_truncation)]
        let prices: Vec<PriceRecord> = stmt
            .query_map([start_ts, end_ts], |row| {
                let ts: i64 = row.get(0)?;
                Ok(PriceRecord {
                    timestamp: Utc.timestamp_opt(ts, 0).single().unwrap_or_default(),
                    price_czk_per_kwh: row.get::<_, f64>(1)? as f32,
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(prices)
    }

    fn get_all_prices(&self) -> Result<Vec<PriceRecord>> {
        let conn = self.connect()?;

        let mut stmt = conn.prepare("SELECT ts, price FROM prices ORDER BY ts ASC")?;

        #[expect(clippy::cast_possible_truncation)]
        let prices: Vec<PriceRecord> = stmt
            .query_map([], |row| {
                let ts: i64 = row.get(0)?;
                Ok(PriceRecord {
                    timestamp: Utc.timestamp_opt(ts, 0).single().unwrap_or_default(),
                    price_czk_per_kwh: row.get::<_, f64>(1)? as f32,
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(prices)
    }
}

/// Find the price at a given timestamp by looking up the nearest price record
#[must_use]
pub fn find_price_at_timestamp(prices: &[PriceRecord], timestamp: chrono::DateTime<Utc>) -> f32 {
    // Find the price block that contains this timestamp
    // Prices are in 15-minute blocks, so find the one that starts before or at this time
    let ts = timestamp.timestamp();

    for (i, price) in prices.iter().enumerate() {
        let price_ts = price.timestamp.timestamp();
        let next_ts = prices
            .get(i + 1)
            .map_or(i64::MAX, |p| p.timestamp.timestamp());

        if ts >= price_ts && ts < next_ts {
            return price.price_czk_per_kwh;
        }
    }

    // Fallback: return the last price or a default
    prices.last().map_or(2.0, |p| p.price_czk_per_kwh) // Default price if no data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_data_source_creation() {
        let ds = SqliteDataSource::new("/tmp/test.db");
        assert_eq!(ds.db_path, PathBuf::from("/tmp/test.db"));
    }
}
