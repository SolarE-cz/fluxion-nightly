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

pub mod ote;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fluxion_types::pricing::{
    FixedPriceData, PriceAnalysis, PriceRange, SpotPriceData, TimeBlockPrice,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Configuration for price data source
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSourceConfig {
    /// Whether to use spot prices for buying
    pub use_spot_for_buy: bool,

    /// Whether to use spot prices for selling
    pub use_spot_for_sell: bool,

    /// Fixed buy prices (fallback)
    pub fixed_buy_prices: Vec<f32>,

    /// Fixed sell prices (fallback)
    pub fixed_sell_prices: Vec<f32>,
}

/// Parse spot price data from HA entity response
pub fn parse_spot_price_response(entity_state: &serde_json::Value) -> Result<SpotPriceData> {
    debug!("🔍 Parsing spot price entity state");

    let last_updated = entity_state
        .get("last_updated")
        .and_then(|v| v.as_str())
        .context("Missing last_updated field")?;

    let ha_last_updated = DateTime::parse_from_rfc3339(last_updated)
        .context("Failed to parse last_updated timestamp")?
        .with_timezone(&Utc);

    // Debug: Show what attributes are available
    if let Some(attributes) = entity_state.get("attributes")
        && let Some(obj) = attributes.as_object()
    {
        debug!(
            "   Available attributes: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    // Try to get forecast attribute
    let attributes = entity_state
        .get("attributes")
        .context("Missing attributes field")?;

    // Check if this is a forecast-type sensor or simple sensor
    if let Some(forecast) = attributes.get("forecast") {
        // This is a forecast sensor with multiple time blocks
        let forecast_array = forecast.as_array().context("Forecast is not an array")?;
        return parse_forecast_sensor(forecast_array, ha_last_updated);
    }

    // Check for "today" and "tomorrow" arrays (Czech spot price format)
    // These can be either hourly (24 entries) or 15-minute (96 entries)
    if let (Some(today), Some(tomorrow)) = (attributes.get("today"), attributes.get("tomorrow"))
        && let (Some(today_arr), Some(tomorrow_arr)) = (today.as_array(), tomorrow.as_array())
    {
        return parse_price_arrays(today_arr, tomorrow_arr, ha_last_updated);
    }

    // Check for single "today" array only (new 15-minute format)
    if let Some(today) = attributes.get("today")
        && let Some(today_arr) = today.as_array()
    {
        return parse_single_day_array(today_arr, ha_last_updated);
    }

    // Check for timestamp-keyed attributes (common Czech integration format)
    // Attributes have ISO 8601 timestamp keys mapping directly to price values
    if let Some(attrs_obj) = attributes.as_object() {
        // Look for timestamp-like keys (ISO 8601 format with 'T')
        let timestamp_keys: Vec<&String> = attrs_obj
            .keys()
            .filter(|k| k.contains('T') && k.contains(':'))
            .collect();

        if !timestamp_keys.is_empty() {
            return parse_timestamp_keyed_attributes(attrs_obj, ha_last_updated);
        }
    }

    // Fallback error with helpful message
    let attrs = attributes
        .as_object()
        .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    anyhow::bail!(
        "Unsupported price sensor format. Available attributes: {:?}. \n\n\
        Supported formats:\n\
        1. Forecast sensor with 'forecast' attribute containing array of {{start, end, price}} objects\n\
        2. Hourly sensor with 'today' and 'tomorrow' attributes containing price arrays\n\
        3. Timestamp-keyed sensor with ISO 8601 timestamp keys mapping to prices\n\n\
        Please check your sensor.current_spot_electricity_price_15min entity in Home Assistant.",
        attrs
    );
}

/// Parse forecast-style sensor data
fn parse_forecast_sensor(
    forecast_array: &[serde_json::Value],
    ha_last_updated: DateTime<Utc>,
) -> Result<SpotPriceData> {
    debug!("Parsing {} price forecast entries", forecast_array.len());

    let mut time_block_prices = Vec::new();

    for entry in forecast_array {
        let start_str = entry
            .get("start")
            .and_then(|v| v.as_str())
            .context("Missing start time in forecast entry")?;

        let price = entry
            .get("price")
            .and_then(|v| v.as_f64())
            .context("Missing price in forecast entry")? as f32;

        let block_start = DateTime::parse_from_rfc3339(start_str)
            .context("Failed to parse block start time")?
            .with_timezone(&Utc);

        // Determine block duration
        let duration_minutes = if let Some(end) = entry.get("end") {
            let end_str = end.as_str().context("End time is not a string")?;
            let block_end = DateTime::parse_from_rfc3339(end_str)?.with_timezone(&Utc);
            (block_end - block_start).num_minutes() as u32
        } else {
            // Assume hourly if no end time
            60
        };

        time_block_prices.push(TimeBlockPrice {
            block_start,
            duration_minutes,
            price_czk_per_kwh: price,
        });
    }

    // Convert hourly to 15-minute blocks if needed
    let time_block_prices = convert_to_15min_blocks(time_block_prices);

    let data = SpotPriceData {
        time_block_prices,
        block_duration_minutes: 15,
        fetched_at: Utc::now(),
        ha_last_updated,
    };

    info!(
        "Parsed spot price data: {} blocks, updated at {}",
        data.time_block_prices.len(),
        ha_last_updated
    );

    Ok(data)
}

/// Convert hourly price blocks to 15-minute blocks
fn convert_to_15min_blocks(blocks: Vec<TimeBlockPrice>) -> Vec<TimeBlockPrice> {
    let mut result = Vec::new();

    for block in blocks {
        if block.duration_minutes == 15 {
            // Already 15-minute block
            result.push(block);
        } else if block.duration_minutes == 60 {
            // Split hourly into 4x15-minute blocks
            for i in 0..4 {
                result.push(TimeBlockPrice {
                    block_start: block.block_start + chrono::Duration::minutes(i * 15),
                    duration_minutes: 15,
                    price_czk_per_kwh: block.price_czk_per_kwh,
                });
            }
        } else {
            // Unexpected duration, keep as-is but warn
            warn!(
                "Unexpected block duration: {}min, keeping as-is",
                block.duration_minutes
            );
            result.push(block);
        }
    }

    result
}

/// Parse single day array (new 15-minute format with only "today" attribute)
fn parse_single_day_array(
    today: &[serde_json::Value],
    ha_last_updated: DateTime<Utc>,
) -> Result<SpotPriceData> {
    debug!("Parsing single-day price data: {} blocks", today.len());

    let now = Utc::now();
    let today_date = now.date_naive();

    // Detect if this is hourly (24 entries) or 15-minute (96 entries)
    let is_15min = today.len() == 96;
    let block_duration = if is_15min { 15 } else { 60 };

    debug!("   Detected {} minute blocks", block_duration);

    let mut time_block_prices = Vec::new();

    // Parse today's prices
    for (idx, price_value) in today.iter().enumerate() {
        let price = price_value
            .as_f64()
            .with_context(|| format!("Block {} is not a number", idx))? as f32;

        let block_start = if is_15min {
            // 96 blocks per day, 15 minutes each
            let minutes_offset = idx * 15;
            today_date
                .and_hms_opt(
                    (minutes_offset / 60) as u32,
                    (minutes_offset % 60) as u32,
                    0,
                )
                .context("Invalid time calculation")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        } else {
            // 24 blocks per day, 60 minutes each
            today_date
                .and_hms_opt(idx as u32, 0, 0)
                .context("Invalid hour")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        };

        time_block_prices.push(TimeBlockPrice {
            block_start,
            duration_minutes: block_duration,
            price_czk_per_kwh: price,
        });
    }

    // Convert hourly to 15-minute blocks if needed
    let time_block_prices = if is_15min {
        time_block_prices // Already 15-minute blocks
    } else {
        convert_to_15min_blocks(time_block_prices)
    };

    let data = SpotPriceData {
        time_block_prices,
        block_duration_minutes: 15,
        fetched_at: Utc::now(),
        ha_last_updated,
    };

    info!(
        "✅ Parsed single-day price data: {} total 15-min blocks",
        data.time_block_prices.len(),
    );

    Ok(data)
}

/// Parse price arrays (Czech spot price format with "today" and "tomorrow" attributes)
/// Supports both hourly (24 entries) and 15-minute (96 entries) formats
fn parse_price_arrays(
    today: &[serde_json::Value],
    tomorrow: &[serde_json::Value],
    ha_last_updated: DateTime<Utc>,
) -> Result<SpotPriceData> {
    // Detect if this is hourly or 15-minute format
    let is_15min = today.len() == 96;
    let block_duration = if is_15min { 15 } else { 60 };

    debug!(
        "Parsing price data: {} today, {} tomorrow ({} min blocks)",
        today.len(),
        tomorrow.len(),
        block_duration
    );

    let now = Utc::now();
    let today_date = now.date_naive();

    let mut time_block_prices = Vec::new();

    // Parse today's prices
    for (idx, price_value) in today.iter().enumerate() {
        let price = price_value
            .as_f64()
            .with_context(|| format!("Today block {} is not a number", idx))?
            as f32;

        let block_start = if is_15min {
            // 96 blocks per day, 15 minutes each
            let minutes_offset = idx * 15;
            today_date
                .and_hms_opt(
                    (minutes_offset / 60) as u32,
                    (minutes_offset % 60) as u32,
                    0,
                )
                .context("Invalid time calculation")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        } else {
            // 24 blocks per day, hourly
            today_date
                .and_hms_opt(idx as u32, 0, 0)
                .context("Invalid hour")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        };

        time_block_prices.push(TimeBlockPrice {
            block_start,
            duration_minutes: block_duration,
            price_czk_per_kwh: price,
        });
    }

    // Parse tomorrow's prices
    let tomorrow_date = today_date + chrono::Duration::days(1);
    for (idx, price_value) in tomorrow.iter().enumerate() {
        let price = price_value
            .as_f64()
            .with_context(|| format!("Tomorrow block {} is not a number", idx))?
            as f32;

        let block_start = if is_15min {
            // 96 blocks per day, 15 minutes each
            let minutes_offset = idx * 15;
            tomorrow_date
                .and_hms_opt(
                    (minutes_offset / 60) as u32,
                    (minutes_offset % 60) as u32,
                    0,
                )
                .context("Invalid time calculation")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        } else {
            // 24 blocks per day, hourly
            tomorrow_date
                .and_hms_opt(idx as u32, 0, 0)
                .context("Invalid hour")?
                .and_local_timezone(chrono::Local)
                .single()
                .context("Ambiguous time")?
                .with_timezone(&Utc)
        };

        time_block_prices.push(TimeBlockPrice {
            block_start,
            duration_minutes: block_duration,
            price_czk_per_kwh: price,
        });
    }

    // Convert hourly to 15-minute blocks if needed
    let time_block_prices = if is_15min {
        time_block_prices // Already 15-minute blocks
    } else {
        convert_to_15min_blocks(time_block_prices)
    };

    let data = SpotPriceData {
        time_block_prices,
        block_duration_minutes: 15,
        fetched_at: Utc::now(),
        ha_last_updated,
    };

    // Always 96 blocks per day after conversion to 15-minute intervals
    info!(
        "✅ Parsed price data: {} total 15-min blocks (96 today + {} tomorrow)",
        data.time_block_prices.len(),
        data.time_block_prices.len() - 96
    );

    Ok(data)
}

/// Parse timestamp-keyed attributes (timestamps as keys mapping to prices)
/// Common format for Czech spot price integrations where timestamps map to prices
/// Supports both hourly and 15-minute intervals
fn parse_timestamp_keyed_attributes(
    attributes: &serde_json::Map<String, serde_json::Value>,
    ha_last_updated: DateTime<Utc>,
) -> Result<SpotPriceData> {
    debug!("Parsing timestamp-keyed price data");

    let mut time_block_prices = Vec::new();

    // Collect all timestamp keys and parse them
    for (key, value) in attributes.iter() {
        // Skip non-timestamp attributes (like unit_of_measurement, icon, etc.)
        if !key.contains('T') || !key.contains(':') {
            continue;
        }

        // Try to parse the key as an ISO 8601 timestamp
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(key) {
            // Parse the price value
            if let Some(price) = value.as_f64() {
                time_block_prices.push(TimeBlockPrice {
                    block_start: timestamp.with_timezone(&Utc),
                    duration_minutes: 0, // Will be calculated below
                    price_czk_per_kwh: price as f32,
                });
            } else {
                warn!(
                    "Failed to parse price value for timestamp {}: {:?}",
                    key, value
                );
            }
        } else {
            debug!("Skipping non-timestamp attribute: {}", key);
        }
    }

    if time_block_prices.is_empty() {
        anyhow::bail!("No valid timestamp-price pairs found in attributes");
    }

    // Sort by timestamp
    time_block_prices.sort_by_key(|block| block.block_start);

    // Detect block duration from first two timestamps
    let detected_duration = if time_block_prices.len() >= 2 {
        let diff =
            (time_block_prices[1].block_start - time_block_prices[0].block_start).num_minutes();
        diff as u32
    } else {
        60 // Default to hourly if only one block
    };

    debug!(
        "🔍 Found {} price blocks in sensor, detected {} minute intervals",
        time_block_prices.len(),
        detected_duration
    );

    // Set the detected duration for all blocks
    for block in &mut time_block_prices {
        block.duration_minutes = detected_duration;
    }

    // Convert to 15-minute blocks if needed
    let time_block_prices = if detected_duration == 15 {
        time_block_prices // Already 15-minute blocks
    } else if detected_duration == 60 {
        convert_to_15min_blocks(time_block_prices) // Convert hourly to 15-minute
    } else {
        warn!(
            "Unexpected block duration: {}min, keeping as-is",
            detected_duration
        );
        time_block_prices
    };

    let data = SpotPriceData {
        time_block_prices,
        block_duration_minutes: 15,
        fetched_at: Utc::now(),
        ha_last_updated,
    };

    let source_blocks = if detected_duration == 15 {
        data.time_block_prices.len()
    } else {
        data.time_block_prices.len() / 4
    };

    debug!(
        "✅ Parsed timestamp-keyed price data: {} total 15-min blocks from {} source blocks ({}min each)",
        data.time_block_prices.len(),
        source_blocks,
        detected_duration
    );

    if !data.time_block_prices.is_empty() {
        let first = &data.time_block_prices[0];
        let last = data.time_block_prices.last().unwrap();
        debug!(
            "   First block: {} at {:.4} CZK/kWh",
            first.block_start.format("%Y-%m-%d %H:%M"),
            first.price_czk_per_kwh
        );
        debug!(
            "   Last block:  {} at {:.4} CZK/kWh",
            last.block_start.format("%Y-%m-%d %H:%M"),
            last.price_czk_per_kwh
        );
    }

    Ok(data)
}

/// Detect if price data has changed by comparing timestamps
pub fn has_price_data_changed(current: &SpotPriceData, new_last_updated: DateTime<Utc>) -> bool {
    current.ha_last_updated != new_last_updated
}

/// Analyze price data and identify cheapest/most expensive blocks
///
/// # Arguments
/// * `time_block_prices` - Price data for 15-minute blocks (either spot or fixed prices)
/// * `force_charge_hours` - Number of hours to identify as cheapest (for charging), 0 to disable
/// * `force_discharge_hours` - Number of hours to identify as most expensive (for discharging), 0 to disable
/// * `use_spot_for_buy` - Deprecated: Price source selection is now handled by ConfigurablePriceDataSource
/// * `use_spot_for_sell` - Deprecated: Price source selection is now handled by ConfigurablePriceDataSource
/// * `min_consecutive_blocks` - Minimum consecutive blocks for force operations (for consecutive charging)
///
/// # Returns
/// `PriceAnalysis` with identified block indices and price statistics
///
/// # Note
/// The `use_spot_for_buy` and `use_spot_for_sell` flags are kept for backward compatibility but
/// no longer control block identification. Price source selection (spot vs fixed) is now handled
/// upstream by `ConfigurablePriceDataSource`. This function always identifies cheap/expensive blocks
/// from whatever prices it receives, as long as `force_charge_hours` or `force_discharge_hours` > 0.
pub fn analyze_prices(
    time_block_prices: &[TimeBlockPrice],
    force_charge_hours: usize,
    force_discharge_hours: usize,
    _use_spot_for_buy: bool,
    _use_spot_for_sell: bool,
    min_consecutive_blocks: usize,
) -> PriceAnalysis {
    if time_block_prices.is_empty() {
        warn!("Cannot analyze empty price data");
        return PriceAnalysis::default();
    }

    // Calculate price statistics
    let prices: Vec<f32> = time_block_prices
        .iter()
        .map(|b| b.price_czk_per_kwh)
        .collect();

    let min = prices.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = prices.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = prices.iter().sum();
    let avg = sum / prices.len() as f32;

    let price_range = PriceRange {
        min_czk_per_kwh: min,
        max_czk_per_kwh: max,
        avg_czk_per_kwh: avg,
    };

    // Create indexed prices for sorting
    let mut indexed_prices: Vec<(usize, f32)> = time_block_prices
        .iter()
        .enumerate()
        .map(|(idx, block)| (idx, block.price_czk_per_kwh))
        .collect();

    // Sort by price (ascending)
    indexed_prices.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // Convert hour-based config to block count (4 blocks per hour)
    let discharge_block_count = force_discharge_hours * 4;

    // Identify cheapest consecutive blocks for charging
    // Note: The price source (spot vs fixed) is determined upstream by ConfigurablePriceDataSource.
    // Here we always identify cheap blocks from whatever prices we received.
    let charge_blocks = if force_charge_hours > 0 {
        let blocks = find_cheapest_consecutive_blocks(
            time_block_prices,
            min_consecutive_blocks,
            force_charge_hours,
        );
        debug!(
            "Identified {} consecutive cheapest blocks for charging (from {} hours, min {} consecutive)",
            blocks.len(),
            force_charge_hours,
            min_consecutive_blocks
        );
        blocks
    } else {
        Vec::new()
    };

    // Identify most expensive blocks for discharging
    // Note: The price source (spot vs fixed) is determined upstream by ConfigurablePriceDataSource.
    // Here we always identify expensive blocks from whatever prices we received.
    let discharge_blocks = if force_discharge_hours > 0 {
        let count = discharge_block_count.min(indexed_prices.len());
        let mut blocks: Vec<usize> = indexed_prices
            .iter()
            .rev()
            .take(count)
            .map(|(idx, _)| *idx)
            .collect();
        blocks.sort_unstable(); // Sort by time, not price
        debug!(
            "Identified {} most expensive blocks for discharging (from {} hours)",
            blocks.len(),
            force_discharge_hours
        );
        blocks
    } else {
        Vec::new()
    };

    PriceAnalysis {
        charge_blocks,
        discharge_blocks,
        price_range,
        analyzed_at: Utc::now(),
    }
}

/// Find the cheapest consecutive blocks for charging
///
/// This function finds consecutive time periods with the lowest average price,
/// respecting the minimum consecutive blocks requirement.
///
/// # Arguments
/// * `time_block_prices` - Price data for 15-minute blocks
/// * `min_consecutive_blocks` - Minimum number of consecutive blocks required
/// * `total_charge_hours` - Total hours of charging desired
///
/// # Returns
/// Vector of block indices representing the cheapest consecutive periods
pub fn find_cheapest_consecutive_blocks(
    time_block_prices: &[TimeBlockPrice],
    min_consecutive_blocks: usize,
    total_charge_hours: usize,
) -> Vec<usize> {
    if time_block_prices.is_empty() || min_consecutive_blocks == 0 {
        return Vec::new();
    }

    let total_blocks_needed = total_charge_hours * 4; // 4 blocks per hour
    if total_blocks_needed == 0 {
        return Vec::new();
    }

    let mut selected_blocks = Vec::new();
    let mut used_blocks = vec![false; time_block_prices.len()];

    // Find consecutive sequences of the required minimum length
    while selected_blocks.len() < total_blocks_needed {
        let mut best_avg_price = f32::INFINITY;
        let mut best_start_idx = None;
        let mut best_length = min_consecutive_blocks;

        // Try different sequence lengths, starting from minimum required
        for seq_length in min_consecutive_blocks
            ..=time_block_prices
                .len()
                .min(total_blocks_needed - selected_blocks.len() + min_consecutive_blocks - 1)
        {
            // Try all possible starting positions for this sequence length
            for start_idx in 0..=(time_block_prices.len().saturating_sub(seq_length)) {
                // Check if any blocks in this range are already used
                if (start_idx..start_idx + seq_length).any(|i| used_blocks[i]) {
                    continue;
                }

                // Calculate average price for this sequence
                let total_price: f32 = (start_idx..start_idx + seq_length)
                    .map(|i| time_block_prices[i].price_czk_per_kwh)
                    .sum();
                let avg_price = total_price / seq_length as f32;

                if avg_price < best_avg_price {
                    best_avg_price = avg_price;
                    best_start_idx = Some(start_idx);
                    best_length = seq_length;
                }
            }
        }

        // If we found a good sequence, mark it as selected
        if let Some(start_idx) = best_start_idx {
            let end_idx = start_idx + best_length;
            #[expect(clippy::needless_range_loop)]
            for i in start_idx..end_idx {
                if selected_blocks.len() < total_blocks_needed {
                    selected_blocks.push(i);
                    used_blocks[i] = true;
                }
            }

            debug!(
                "Selected consecutive charging sequence: blocks {}-{} (avg price: {:.3} CZK/kWh)",
                start_idx,
                end_idx - 1,
                best_avg_price
            );
        } else {
            // No more valid sequences found
            break;
        }
    }

    selected_blocks.sort_unstable();
    selected_blocks
}

/// Create fixed price data from hourly arrays
pub fn create_fixed_price_data(
    buy_prices: Vec<f32>,
    sell_prices: Vec<f32>,
) -> Result<FixedPriceData> {
    // Validate lengths
    if buy_prices.len() != 24 && buy_prices.len() != 96 {
        anyhow::bail!(
            "buy_prices must have 24 or 96 values, got {}",
            buy_prices.len()
        );
    }
    if sell_prices.len() != 24 && sell_prices.len() != 96 {
        anyhow::bail!(
            "sell_prices must have 24 or 96 values, got {}",
            sell_prices.len()
        );
    }

    let block_duration = if buy_prices.len() == 24 { 60 } else { 15 };

    let mut data = FixedPriceData {
        buy_prices,
        sell_prices,
        block_duration_minutes: block_duration,
    };

    // Expand to 15-min if needed
    if data.block_duration_minutes == 60 {
        data.expand_to_15min_blocks();
    }

    Ok(data)
}
