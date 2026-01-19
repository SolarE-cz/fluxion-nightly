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

// Debug test to understand V2 charge scheduling

use chrono::{TimeZone, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
    winter_adaptive_v2::arbitrage,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::pricing::TimeBlockPrice;

fn create_czech_pattern() -> Vec<TimeBlockPrice> {
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Overnight valley (00:00-06:00): 24 blocks @ 1.5 CZK
    for i in 0..24 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.5,
            effective_price_czk_per_kwh: 1.5,
        });
    }

    // Morning ramp (06:00-08:00): 8 blocks @ 3.0 CZK
    for i in 24..32 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
            effective_price_czk_per_kwh: 3.0,
        });
    }

    // Morning peak (08:00-10:00): 8 blocks @ 4.5 CZK
    for i in 32..40 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 4.5,
            effective_price_czk_per_kwh: 4.5,
        });
    }

    // Midday valley (10:00-14:00): 16 blocks @ 2.0 CZK
    for i in 40..56 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
        });
    }

    // Afternoon ramp (14:00-17:00): 12 blocks @ 3.5 CZK
    for i in 56..68 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.5,
            effective_price_czk_per_kwh: 3.5,
        });
    }

    // Evening peak (17:00-22:00): 20 blocks @ 5.0 CZK
    for i in 68..88 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
            effective_price_czk_per_kwh: 5.0,
        });
    }

    // Late evening (22:00-00:00): 8 blocks @ 2.5 CZK
    for i in 88..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.5,
            effective_price_czk_per_kwh: 2.5,
        });
    }

    blocks
}

#[test]
fn debug_v2_charge_scheduling() {
    let blocks = create_czech_pattern();

    // Detect windows
    let windows = arbitrage::detect_windows(&blocks);

    println!("\n=== Window Detection ===");
    for (idx, window) in windows.iter().enumerate() {
        println!("\nWindow {}:", idx + 1);
        println!("  Valley blocks: {:?}", window.valley_slots);
        println!(
            "  Valley hours: {:?}",
            window
                .valley_slots
                .iter()
                .map(|&i| i / 4)
                .collect::<Vec<_>>()
        );
        println!("  Peak blocks: {:?}", window.peak_slots);
        println!(
            "  Peak hours: {:?}",
            window.peak_slots.iter().map(|&i| i / 4).collect::<Vec<_>>()
        );
    }

    // Now test full strategy evaluation at various hours
    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 0.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..ControlConfig::default()
    };

    let v2_config = WinterAdaptiveV2Config::default();
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    println!("\n=== Charge Decisions at Overnight Hours ===");
    for hour in [0, 1, 2, 3, 4, 5] {
        let block_index = hour * 4;
        let context = EvaluationContext {
            price_block: &blocks[block_index],
            control_config: &control_config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.1,
            all_price_blocks: Some(&blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(5.0),
            consumption_today_kwh: Some(8.0),
        };

        let eval = v2_strategy.evaluate(&context);
        println!(
            "Hour {:02}:00 (block {}): {:?} - {}",
            hour, block_index, eval.mode, eval.reason
        );
    }
}
