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

use fluxion_types::day_profile::{DayMetrics, PriceStats};
use fluxion_types::pricing::TimeBlockPrice;

/// Compute all day metrics from available data.
/// Called by strategies with data from EvaluationContext.
pub fn compute_day_metrics(
    all_blocks: &[TimeBlockPrice],
    solar_remaining_kwh: f32,
    solar_tomorrow_kwh: f32,
    battery_charge_cost_basis: f32,
    daily_consumption_kwh: f32,
) -> DayMetrics {
    if all_blocks.is_empty() || daily_consumption_kwh <= 0.0 {
        return DayMetrics::default();
    }

    let (today_blocks, tomorrow_blocks) = split_today_tomorrow(all_blocks);

    let price_stats = compute_price_stats(today_blocks);

    let solar_ratio = solar_remaining_kwh / daily_consumption_kwh;

    // CV = std_dev / |avg|, guarded against near-zero average
    let price_cv = if price_stats.avg_czk.abs() < 0.01 {
        0.0
    } else {
        price_stats.std_dev_czk / price_stats.avg_czk.abs()
    };

    // Spread = (max - min) / |avg|
    let price_spread_ratio = if price_stats.avg_czk.abs() < 0.01 {
        0.0
    } else {
        (price_stats.max_czk - price_stats.min_czk) / price_stats.avg_czk.abs()
    };

    // Price level vs charge cost = (avg - charge_cost) / |charge_cost|
    let price_level_vs_charge_cost = if battery_charge_cost_basis.abs() < 0.01 {
        0.0
    } else {
        (price_stats.avg_czk - battery_charge_cost_basis) / battery_charge_cost_basis.abs()
    };

    // Negative price fraction
    let negative_count = today_blocks
        .iter()
        .filter(|b| b.effective_price_czk_per_kwh < 0.0)
        .count();
    let negative_price_fraction = if today_blocks.is_empty() {
        0.0
    } else {
        negative_count as f32 / today_blocks.len() as f32
    };

    // Tomorrow price ratio = avg_tomorrow / avg_today
    let tomorrow_price_ratio = tomorrow_blocks.and_then(|blocks| {
        let tomorrow_stats = compute_price_stats(blocks);
        if tomorrow_stats.block_count == 0 || price_stats.avg_czk.abs() < 0.01 {
            None
        } else {
            Some(tomorrow_stats.avg_czk / price_stats.avg_czk)
        }
    });

    let tomorrow_solar_ratio = solar_tomorrow_kwh / daily_consumption_kwh;

    DayMetrics {
        solar_ratio,
        price_cv,
        price_spread_ratio,
        price_level_vs_charge_cost,
        negative_price_fraction,
        tomorrow_price_ratio,
        tomorrow_solar_ratio,
        price_stats,
    }
}

/// Compute price statistics for a set of blocks.
pub fn compute_price_stats(blocks: &[TimeBlockPrice]) -> PriceStats {
    if blocks.is_empty() {
        return PriceStats::default();
    }

    let prices: Vec<f32> = blocks
        .iter()
        .map(|b| b.effective_price_czk_per_kwh)
        .collect();
    let n = prices.len() as f32;

    let avg = prices.iter().sum::<f32>() / n;
    let min = prices.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = prices.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    let variance = prices.iter().map(|p| (p - avg).powi(2)).sum::<f32>() / n;
    let std_dev = variance.sqrt();

    let mut sorted = prices.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if sorted.len().is_multiple_of(2) {
        let mid = sorted.len() / 2;
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    };

    PriceStats {
        avg_czk: avg,
        min_czk: min,
        max_czk: max,
        std_dev_czk: std_dev,
        median_czk: median,
        block_count: blocks.len(),
    }
}

/// Split a combined price block slice into today's and tomorrow's blocks.
/// Assumes blocks are sorted chronologically. Today = date of first block.
pub fn split_today_tomorrow(
    all_blocks: &[TimeBlockPrice],
) -> (&[TimeBlockPrice], Option<&[TimeBlockPrice]>) {
    if all_blocks.is_empty() {
        return (&[], None);
    }

    let today_date = all_blocks[0].block_start.date_naive();

    // Find the first block that belongs to a different date
    let split_idx = all_blocks
        .iter()
        .position(|b| b.block_start.date_naive() != today_date);

    match split_idx {
        Some(idx) => {
            let (today, tomorrow) = all_blocks.split_at(idx);
            let tomorrow = if tomorrow.is_empty() {
                None
            } else {
                Some(tomorrow)
            };
            (today, tomorrow)
        }
        None => (all_blocks, None),
    }
}

/// Estimate daily consumption from hourly profile or fallback.
pub fn estimate_daily_consumption(
    hourly_profile: Option<&[f32; 24]>,
    fallback_load_kw: f32,
) -> f32 {
    match hourly_profile {
        Some(profile) => profile.iter().sum(),
        None => fallback_load_kw * 24.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn make_block(hour: u32, minute: u32, effective_price: f32) -> TimeBlockPrice {
        make_block_on_date(2025, 6, 15, hour, minute, effective_price)
    }

    fn make_block_on_date(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        effective_price: f32,
    ) -> TimeBlockPrice {
        TimeBlockPrice {
            block_start: Utc
                .with_ymd_and_hms(year, month, day, hour, minute, 0)
                .unwrap(),
            duration_minutes: 15,
            price_czk_per_kwh: effective_price * 0.8, // spot = 80% of effective for test purposes
            effective_price_czk_per_kwh: effective_price,
            spot_sell_price_czk_per_kwh: None,
        }
    }

    #[test]
    fn test_price_stats_basic() {
        let blocks = vec![
            make_block(0, 0, 2.0),
            make_block(0, 15, 4.0),
            make_block(0, 30, 6.0),
            make_block(0, 45, 8.0),
        ];
        let stats = compute_price_stats(&blocks);

        assert_eq!(stats.block_count, 4);
        assert!((stats.avg_czk - 5.0).abs() < 0.001);
        assert!((stats.min_czk - 2.0).abs() < 0.001);
        assert!((stats.max_czk - 8.0).abs() < 0.001);
        assert!((stats.median_czk - 5.0).abs() < 0.001);
        // std_dev of [2,4,6,8]: variance = ((2-5)^2 + (4-5)^2 + (6-5)^2 + (8-5)^2) / 4
        //                                = (9 + 1 + 1 + 9) / 4 = 5.0
        //                       std_dev = sqrt(5.0) ≈ 2.236
        assert!((stats.std_dev_czk - 2.236).abs() < 0.01);
    }

    #[test]
    fn test_price_stats_empty() {
        let stats = compute_price_stats(&[]);
        assert_eq!(stats.block_count, 0);
        assert!((stats.avg_czk).abs() < 0.001);
    }

    #[test]
    fn test_price_stats_single_block() {
        let blocks = vec![make_block(0, 0, 3.5)];
        let stats = compute_price_stats(&blocks);

        assert_eq!(stats.block_count, 1);
        assert!((stats.avg_czk - 3.5).abs() < 0.001);
        assert!((stats.min_czk - 3.5).abs() < 0.001);
        assert!((stats.max_czk - 3.5).abs() < 0.001);
        assert!((stats.std_dev_czk).abs() < 0.001);
        assert!((stats.median_czk - 3.5).abs() < 0.001);
    }

    #[test]
    fn test_price_stats_median_odd() {
        let blocks = vec![
            make_block(0, 0, 1.0),
            make_block(0, 15, 3.0),
            make_block(0, 30, 5.0),
        ];
        let stats = compute_price_stats(&blocks);
        assert!((stats.median_czk - 3.0).abs() < 0.001);
    }

    #[test]
    fn test_cv_calculation() {
        // Prices: [2, 4, 6, 8], avg=5, std_dev=sqrt(5)≈2.236
        // CV = 2.236 / 5.0 = 0.4472
        let blocks = vec![
            make_block(0, 0, 2.0),
            make_block(0, 15, 4.0),
            make_block(0, 30, 6.0),
            make_block(0, 45, 8.0),
        ];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.price_cv - 0.4472).abs() < 0.01);
    }

    #[test]
    fn test_cv_near_zero_avg() {
        // Prices around zero: [-0.005, 0.005]
        let blocks = vec![make_block(0, 0, -0.005), make_block(0, 15, 0.005)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        // avg ≈ 0.0, guard should trigger → CV = 0.0
        assert!((metrics.price_cv).abs() < 0.001);
    }

    #[test]
    fn test_solar_ratio() {
        // solar=12, consumption=10 → ratio=1.2
        let blocks = vec![make_block(0, 0, 3.0)];
        let metrics = compute_day_metrics(&blocks, 12.0, 0.0, 3.0, 10.0);
        assert!((metrics.solar_ratio - 1.2).abs() < 0.001);
        assert!(metrics.is_high_solar());
        assert!(!metrics.is_low_solar());
        assert!(!metrics.is_medium_solar());
    }

    #[test]
    fn test_solar_ratio_medium() {
        let blocks = vec![make_block(0, 0, 3.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.solar_ratio - 1.0).abs() < 0.001);
        assert!(metrics.is_medium_solar());
    }

    #[test]
    fn test_solar_ratio_low() {
        let blocks = vec![make_block(0, 0, 3.0)];
        let metrics = compute_day_metrics(&blocks, 5.0, 0.0, 3.0, 10.0);
        assert!((metrics.solar_ratio - 0.5).abs() < 0.001);
        assert!(metrics.is_low_solar());
    }

    #[test]
    fn test_price_level_vs_charge_cost() {
        // avg_price=4.5, charge_cost=3.0 → (4.5-3.0)/3.0 = 0.5
        let blocks = vec![make_block(0, 0, 3.0), make_block(0, 15, 6.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.price_level_vs_charge_cost - 0.5).abs() < 0.001);
        assert!(metrics.is_expensive(0.3));
        assert!(!metrics.is_cheap(-0.1));
    }

    #[test]
    fn test_price_level_cheap() {
        // avg_price=2.0, charge_cost=3.0 → (2.0-3.0)/3.0 = -0.333
        let blocks = vec![make_block(0, 0, 1.0), make_block(0, 15, 3.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.price_level_vs_charge_cost - (-1.0 / 3.0)).abs() < 0.01);
        assert!(metrics.is_cheap(-0.2));
    }

    #[test]
    fn test_price_level_zero_charge_cost() {
        let blocks = vec![make_block(0, 0, 3.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 0.0, 10.0);
        assert!((metrics.price_level_vs_charge_cost).abs() < 0.001);
    }

    #[test]
    fn test_negative_price_fraction() {
        let blocks = vec![
            make_block(0, 0, -1.0),
            make_block(0, 15, -0.5),
            make_block(0, 30, 3.0),
            make_block(0, 45, 5.0),
        ];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.negative_price_fraction - 0.5).abs() < 0.001);
        assert!(metrics.has_negative_prices());
        assert!(metrics.significant_negative_prices(0.10));
    }

    #[test]
    fn test_no_negative_prices() {
        let blocks = vec![make_block(0, 0, 1.0), make_block(0, 15, 2.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.negative_price_fraction).abs() < 0.001);
        assert!(!metrics.has_negative_prices());
    }

    #[test]
    fn test_spread_ratio() {
        // Prices: [2, 8], avg=5, spread = (8-2)/5 = 1.2
        let blocks = vec![make_block(0, 0, 2.0), make_block(0, 15, 8.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!((metrics.price_spread_ratio - 1.2).abs() < 0.001);
    }

    #[test]
    fn test_tomorrow_price_ratio() {
        let blocks = vec![
            // Today (June 15): avg = 3.0
            make_block_on_date(2025, 6, 15, 10, 0, 2.0),
            make_block_on_date(2025, 6, 15, 10, 15, 4.0),
            // Tomorrow (June 16): avg = 6.0
            make_block_on_date(2025, 6, 16, 10, 0, 5.0),
            make_block_on_date(2025, 6, 16, 10, 15, 7.0),
        ];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!(metrics.tomorrow_price_ratio.is_some());
        // ratio = 6.0 / 3.0 = 2.0
        assert!((metrics.tomorrow_price_ratio.unwrap() - 2.0).abs() < 0.001);
        assert!(metrics.is_tomorrow_expensive(1.3));
        assert!(!metrics.is_tomorrow_cheap(0.7));
    }

    #[test]
    fn test_tomorrow_price_ratio_unavailable() {
        let blocks = vec![make_block(0, 0, 3.0), make_block(0, 15, 5.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!(metrics.tomorrow_price_ratio.is_none());
        assert!(!metrics.is_tomorrow_expensive(1.3));
        assert!(!metrics.is_tomorrow_cheap(0.7));
    }

    #[test]
    fn test_tomorrow_solar_ratio() {
        let blocks = vec![make_block(0, 0, 3.0)];
        // tomorrow_solar=15, consumption=10 → ratio=1.5
        let metrics = compute_day_metrics(&blocks, 10.0, 15.0, 3.0, 10.0);
        assert!((metrics.tomorrow_solar_ratio - 1.5).abs() < 0.001);
        assert!(metrics.is_tomorrow_sunny());
    }

    #[test]
    fn test_split_today_tomorrow() {
        let blocks = vec![
            make_block_on_date(2025, 6, 15, 23, 0, 3.0),
            make_block_on_date(2025, 6, 15, 23, 15, 3.0),
            make_block_on_date(2025, 6, 16, 0, 0, 5.0),
            make_block_on_date(2025, 6, 16, 0, 15, 5.0),
        ];
        let (today, tomorrow) = split_today_tomorrow(&blocks);
        assert_eq!(today.len(), 2);
        assert!(tomorrow.is_some());
        assert_eq!(tomorrow.unwrap().len(), 2);
    }

    #[test]
    fn test_split_today_only() {
        let blocks = vec![make_block(0, 0, 3.0), make_block(0, 15, 3.0)];
        let (today, tomorrow) = split_today_tomorrow(&blocks);
        assert_eq!(today.len(), 2);
        assert!(tomorrow.is_none());
    }

    #[test]
    fn test_split_empty() {
        let (today, tomorrow) = split_today_tomorrow(&[]);
        assert!(today.is_empty());
        assert!(tomorrow.is_none());
    }

    #[test]
    fn test_estimate_daily_consumption_from_profile() {
        let profile: [f32; 24] = [
            0.3, 0.2, 0.2, 0.2, 0.3, 0.5, 0.8, 1.0, 0.9, 0.7, 0.6, 0.5, 0.5, 0.5, 0.6, 0.7, 0.8,
            1.0, 1.2, 1.0, 0.8, 0.6, 0.4, 0.3,
        ];
        let expected: f32 = profile.iter().sum();
        let result = estimate_daily_consumption(Some(&profile), 1.0);
        assert!((result - expected).abs() < 0.001);
    }

    #[test]
    fn test_estimate_daily_consumption_fallback() {
        let result = estimate_daily_consumption(None, 1.5);
        assert!((result - 36.0).abs() < 0.001); // 1.5 * 24 = 36
    }

    #[test]
    fn test_empty_blocks() {
        let metrics = compute_day_metrics(&[], 10.0, 5.0, 3.0, 10.0);
        assert!((metrics.solar_ratio).abs() < 0.001);
        assert!((metrics.price_cv).abs() < 0.001);
        assert!(metrics.tomorrow_price_ratio.is_none());
    }

    #[test]
    fn test_zero_consumption() {
        let blocks = vec![make_block(0, 0, 3.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 5.0, 3.0, 0.0);
        // Should return default when daily_consumption <= 0
        assert!((metrics.solar_ratio).abs() < 0.001);
    }

    #[test]
    fn test_volatility_convenience() {
        let blocks = vec![make_block(0, 0, 1.0), make_block(0, 15, 10.0)];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        // Very spread out prices → high CV
        assert!(metrics.is_volatile(0.35));
        assert!(!metrics.is_stable(0.15));
    }

    #[test]
    fn test_stable_prices() {
        let blocks = vec![
            make_block(0, 0, 3.0),
            make_block(0, 15, 3.1),
            make_block(0, 30, 2.9),
            make_block(0, 45, 3.0),
        ];
        let metrics = compute_day_metrics(&blocks, 10.0, 0.0, 3.0, 10.0);
        assert!(metrics.is_stable(0.15));
        assert!(!metrics.is_volatile(0.35));
    }

    #[test]
    fn test_composability_high_solar_volatile_negative() {
        let blocks = vec![
            make_block(0, 0, -1.0),
            make_block(0, 15, -0.5),
            make_block(0, 30, 8.0),
            make_block(0, 45, 10.0),
        ];
        // solar=15, consumption=10 → high solar (1.5)
        let metrics = compute_day_metrics(&blocks, 15.0, 0.0, 3.0, 10.0);

        // All three conditions can be true simultaneously
        assert!(metrics.is_high_solar());
        assert!(metrics.is_volatile(0.35));
        assert!(metrics.has_negative_prices());
    }
}
