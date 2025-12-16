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
use calamine::{Reader, Xlsx};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use reqwest::blocking::Client;
use std::io::Cursor;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct PriceRecord {
    pub datetime: DateTime<Utc>,
    pub price_eur: f32,
    pub price_czk: f32,
}

pub struct OteMarketData {
    client: Client,
}

impl Default for OteMarketData {
    fn default() -> Self {
        Self::new()
    }
}

impl OteMarketData {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    /// Download Excel file for a specific date
    /// URL pattern: https://www.ote-cr.cz/pubweb/attachments/01/{year}/month{month:02}/day{day:02}/DM_15MIN_{day:02}_{month:02}_{year}_EN.xlsx
    fn download_excel(&self, date: NaiveDate) -> Result<Vec<u8>> {
        let url = format!(
            "https://www.ote-cr.cz/pubweb/attachments/01/{}/month{:02}/day{:02}/DM_15MIN_{:02}_{:02}_{}_EN.xlsx",
            date.year(),
            date.month(),
            date.day(),
            date.day(),
            date.month(),
            date.year()
        );

        info!("Downloading OTE data from: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .context("Failed to send request to OTE")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to download Excel file: HTTP {}", response.status());
        }

        let bytes = response.bytes().context("Failed to read response bytes")?;
        Ok(bytes.to_vec())
    }

    /// Parse Excel file and extract price records
    fn parse_excel(&self, bytes: &[u8], date: NaiveDate) -> Result<Vec<PriceRecord>> {
        let cursor = Cursor::new(bytes);
        let mut workbook: Xlsx<_> = Xlsx::new(cursor).context("Failed to open Excel workbook")?;

        let sheet_names = workbook.sheet_names().to_vec();
        if sheet_names.is_empty() {
            anyhow::bail!("No sheets found in Excel file");
        }

        let range = workbook
            .worksheet_range(&sheet_names[0])
            .context("Failed to read worksheet")?;

        let mut records = Vec::new();
        let mut found_table_header = false;
        let mut hour_col_idx = None;
        let mut price_eur_col_idx = None;
        let mut price_czk_col_idx = None;

        // Find the data table header (looks for "Period" and "15 min price")
        for (row_idx, row) in range.rows().enumerate() {
            if found_table_header {
                // Parse data rows - skip empty rows
                if row.is_empty() || row.iter().all(|cell| matches!(cell, calamine::Data::Empty)) {
                    // Skip empty row but don't break - data might be coming
                    // Only break if we've already seen data (records is not empty)
                    if !records.is_empty() {
                        break;
                    }
                    continue;
                }

                // Extract period number (1-96 for 15-min intervals)
                let period = match hour_col_idx.and_then(|idx| row.get(idx)) {
                    Some(val) => match val {
                        calamine::Data::Int(p) => *p as u32,
                        calamine::Data::Float(p) => *p as u32,
                        _ => continue,
                    },
                    _ => continue,
                };

                if !(1..=96).contains(&period) {
                    continue; // Invalid period
                }

                // Convert period (1-based) to hour and minute
                // Period 1 = 00:00-00:15, Period 2 = 00:15-00:30, etc.
                let total_minutes = (period - 1) * 15;
                let hour = total_minutes / 60;
                let minute = total_minutes % 60;

                // Extract 15-min price in EUR
                let price_eur = match price_eur_col_idx.and_then(|idx| row.get(idx)) {
                    Some(val) => match val {
                        calamine::Data::Float(p) => *p as f32,
                        calamine::Data::Int(p) => *p as f32,
                        _ => continue,
                    },
                    _ => continue,
                };

                // For CZK, we would need EUR/CZK exchange rate
                // For now, leave it at 0 or calculate using a fixed rate
                let price_czk = price_eur * 24.0; // Approximate EUR/CZK rate

                let time = date.and_hms_opt(hour, minute, 0).context("Invalid time")?;
                let datetime = time.and_utc();

                records.push(PriceRecord {
                    datetime,
                    price_eur,
                    price_czk,
                });
            } else {
                // Look for table header
                // We're looking for a row with "Period" and "15 min price"
                for (col_idx, cell) in row.iter().enumerate() {
                    if let calamine::Data::String(s) = cell {
                        let lower = s.to_lowercase();
                        if lower.contains("period") {
                            hour_col_idx = Some(col_idx);
                        } else if lower.contains("15 min price") || lower.contains("15min price") {
                            price_eur_col_idx = Some(col_idx);
                        } else if lower.contains("czk") && lower.contains("mwh") {
                            price_czk_col_idx = Some(col_idx);
                        }
                    }
                }

                if hour_col_idx.is_some() && price_eur_col_idx.is_some() {
                    found_table_header = true;
                    info!(
                        "Found price table header at row {}, period_col={:?}, eur_col={:?}, czk_col={:?}",
                        row_idx, hour_col_idx, price_eur_col_idx, price_czk_col_idx
                    );
                }
            }
        }

        if records.is_empty() {
            warn!("No price records found in Excel file for {}", date);
        } else {
            info!("Parsed {} price records for {}", records.len(), date);
        }

        Ok(records)
    }

    /// Fetch and parse prices for a specific date
    pub fn fetch_day(&self, date: NaiveDate) -> Result<Vec<PriceRecord>> {
        let bytes = self.download_excel(date)?;
        self.parse_excel(&bytes, date)
    }

    /// Fetch prices for a date range
    pub fn fetch_range(&self, start: NaiveDate, end: NaiveDate) -> Result<Vec<PriceRecord>> {
        let mut all_records = Vec::new();
        let mut current = start;

        while current <= end {
            match self.fetch_day(current) {
                Ok(mut records) => {
                    all_records.append(&mut records);
                }
                Err(e) => {
                    warn!("Failed to fetch data for {}: {}", current, e);
                }
            }
            current = current.succ_opt().context("Date overflow")?;
        }

        Ok(all_records)
    }
}
