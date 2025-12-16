// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

use chrono::{TimeZone, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
    WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;

/// Helper to create realistic Czech price pattern for testing
fn create_czech_price_pattern() -> Vec<TimeBlockPrice> {
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Overnight valley (00:00-06:00): 24 blocks @ 1.5 CZK (cheap charging)
    for i in 0..24 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.5,
        });
    }

    // Morning ramp (06:00-08:00): 8 blocks @ 3.0 CZK
    for i in 24..32 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
        });
    }

    // Morning peak (08:00-10:00): 8 blocks @ 4.5 CZK (expensive)
    for i in 32..40 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 4.5,
        });
    }

    // Midday valley (10:00-14:00): 16 blocks @ 2.0 CZK
    for i in 40..56 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
        });
    }

    // Afternoon ramp (14:00-17:00): 12 blocks @ 3.5 CZK
    for i in 56..68 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.5,
        });
    }

    // Evening peak (17:00-22:00): 20 blocks @ 5.0 CZK (most expensive)
    for i in 68..88 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
        });
    }

    // Late evening (22:00-00:00): 8 blocks @ 2.5 CZK (starting to drop)
    for i in 88..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.5,
        });
    }

    blocks
}

#[derive(Debug)]
struct StrategyComparison {
    hour: u32,
    price: f32,
    v1_mode: InverterOperationMode,
    v1_reason: String,
    v2_mode: InverterOperationMode,
    v2_reason: String,
    modes_match: bool,
}

#[test]
fn compare_v1_v2_on_czech_pattern() {
    // Create price blocks
    let blocks = create_czech_price_pattern();

    // Setup control config
    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 0.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..ControlConfig::default()
    };

    // Create strategies
    let v1_config = WinterAdaptiveConfig::default();
    let v1_strategy = WinterAdaptiveStrategy::new(v1_config);

    let v2_config = WinterAdaptiveV2Config::default();
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    // Compare decisions at key hours
    let mut comparisons = Vec::new();
    let test_hours = vec![0, 2, 4, 8, 10, 12, 17, 19, 21, 23]; // Key hours to check

    for &hour in &test_hours {
        let block_index = hour * 4; // 4 blocks per hour
        let block = &blocks[block_index];

        let context = EvaluationContext {
            price_block: block,
            control_config: &control_config,
            current_battery_soc: 50.0,
            solar_forecast_kwh: 0.0, // Winter - no solar
            consumption_forecast_kwh: 0.5,
            grid_export_price_czk_per_kwh: 0.1,
            all_price_blocks: Some(&blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(5.0),
            consumption_today_kwh: Some(8.0),
        };

        let v1_eval = v1_strategy.evaluate(&context);
        let v2_eval = v2_strategy.evaluate(&context);

        comparisons.push(StrategyComparison {
            hour: hour as u32,
            price: block.price_czk_per_kwh,
            v1_mode: v1_eval.mode,
            v1_reason: v1_eval.reason,
            v2_mode: v2_eval.mode,
            v2_reason: v2_eval.reason,
            modes_match: v1_eval.mode == v2_eval.mode,
        });
    }

    // Print comparison table
    println!("\n=== V1 vs V2 Strategy Comparison ===\n");
    println!(
        "{:<6} {:<8} {:<20} {:<20} {:<8}",
        "Hour", "Price", "V1 Mode", "V2 Mode", "Match?"
    );
    println!("{:-<80}", "");

    for comp in &comparisons {
        let match_indicator = if comp.modes_match { "✓" } else { "✗" };
        println!(
            "{:<6} {:<8.2} {:<20} {:<20} {:<8}",
            comp.hour,
            comp.price,
            format!("{:?}", comp.v1_mode),
            format!("{:?}", comp.v2_mode),
            match_indicator
        );
    }

    println!("\n=== Detailed Reasons ===\n");
    for comp in &comparisons {
        println!("Hour {:02}:00 @ {:.2} CZK/kWh:", comp.hour, comp.price);
        println!("  V1: {:?} - {}", comp.v1_mode, comp.v1_reason);
        println!("  V2: {:?} - {}", comp.v2_mode, comp.v2_reason);
        if !comp.modes_match {
            println!("  ⚠️  MISMATCH");
        }
        println!();
    }

    // Statistics
    let matches = comparisons.iter().filter(|c| c.modes_match).count();
    let total = comparisons.len();
    let agreement_pct = (matches as f32 / total as f32) * 100.0;

    println!("=== Summary ===");
    println!("Agreement: {}/{} ({:.1}%)", matches, total, agreement_pct);

    // Pattern analysis
    let v1_force_charge = comparisons
        .iter()
        .filter(|c| c.v1_mode == InverterOperationMode::ForceCharge)
        .count();
    let v2_force_charge = comparisons
        .iter()
        .filter(|c| c.v2_mode == InverterOperationMode::ForceCharge)
        .count();

    let v1_self_use = comparisons
        .iter()
        .filter(|c| c.v1_mode == InverterOperationMode::SelfUse)
        .count();
    let v2_self_use = comparisons
        .iter()
        .filter(|c| c.v2_mode == InverterOperationMode::SelfUse)
        .count();

    let v1_backup = comparisons
        .iter()
        .filter(|c| c.v1_mode == InverterOperationMode::BackUpMode)
        .count();
    let v2_backup = comparisons
        .iter()
        .filter(|c| c.v2_mode == InverterOperationMode::BackUpMode)
        .count();

    println!("\nMode Distribution:");
    println!(
        "  ForceCharge: V1={}, V2={}",
        v1_force_charge, v2_force_charge
    );
    println!("  SelfUse:     V1={}, V2={}", v1_self_use, v2_self_use);
    println!("  BackUpMode:  V1={}, V2={}", v1_backup, v2_backup);

    // Don't assert - just report differences for human analysis
    // The strategies are expected to differ as V2 has different logic
}

#[test]
fn test_v2_arbitrage_detection_on_pattern() {
    use fluxion_core::strategy::winter_adaptive_v2::arbitrage;

    let blocks = create_czech_price_pattern();
    let windows = arbitrage::detect_windows(&blocks);

    println!("\n=== Arbitrage Window Detection ===\n");
    println!("Detected {} windows", windows.len());

    for (idx, window) in windows.iter().enumerate() {
        let valley_start = window.valley_slots.first().copied().unwrap_or(0);
        let valley_end = window.valley_slots.last().copied().unwrap_or(0);
        let peak_start = window.peak_slots.first().copied().unwrap_or(0);
        let peak_end = window.peak_slots.last().copied().unwrap_or(0);

        println!("Window {}:", idx + 1);
        println!(
            "  Valley: blocks {}-{} ({} blocks) @ avg {:.2} CZK/kWh",
            valley_start,
            valley_end,
            window.valley_slots.len(),
            window.valley_avg_price
        );
        println!(
            "  Peak:   blocks {}-{} ({} blocks) @ avg {:.2} CZK/kWh",
            peak_start,
            peak_end,
            window.peak_slots.len(),
            window.peak_avg_price
        );
        println!(
            "  Spread: {:.2} CZK/kWh",
            window.peak_avg_price - window.valley_avg_price
        );
        println!();
    }

    // We expect at least 1 window (overnight valley → morning/evening peaks)
    assert!(
        !windows.is_empty(),
        "Should detect at least 1 arbitrage window in Czech pattern"
    );

    // Check that windows have meaningful spreads
    for window in &windows {
        let spread = window.peak_avg_price - window.valley_avg_price;
        assert!(
            spread > 0.5,
            "Arbitrage window should have meaningful spread (>{:.2})",
            spread
        );
    }
}

#[test]
fn test_v2_spike_detection() {
    use fluxion_core::strategy::winter_adaptive_v2::spike_detection;

    // Create pattern with a price spike
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Normal prices
    for i in 0..40 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
        });
    }

    // Price spike at hour 10 (blocks 40-43)
    for i in 40..44 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 9.5, // Above 8.0 threshold
        });
    }

    // Back to normal
    for i in 44..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
        });
    }

    let net_consumption = vec![0.5; 96]; // 0.5 kWh per slot
    let spikes = spike_detection::detect_spikes(&blocks, 8.0, &net_consumption, 5.0);

    println!("\n=== Price Spike Detection ===\n");
    println!("Detected {} spikes", spikes.len());

    for spike in &spikes {
        let hour = spike.slot_index / 4;
        println!(
            "Spike at block {} (hour {}): {:.2} CZK/kWh, reserved {:.0} Wh",
            spike.slot_index, hour, spike.price_czk, spike.reserved_discharge_wh
        );
    }

    assert_eq!(spikes.len(), 4, "Should detect 4 spike blocks");
    assert!(
        spikes.iter().all(|s| s.price_czk >= 8.0),
        "All spikes should be above threshold"
    );
}

#[test]
fn compare_cost_overnight_charging() {
    // Scenario: Should both strategies charge during cheap overnight hours?
    let blocks = create_czech_price_pattern();

    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 0.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..ControlConfig::default()
    };

    let v1_config = WinterAdaptiveConfig::default();
    let v1_strategy = WinterAdaptiveStrategy::new(v1_config);

    let v2_config = WinterAdaptiveV2Config::default();
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    // Test at 02:00 (cheap overnight, should charge)
    let context_overnight = EvaluationContext {
        price_block: &blocks[8], // 02:00
        control_config: &control_config,
        current_battery_soc: 30.0, // Low battery
        solar_forecast_kwh: 0.0,
        consumption_forecast_kwh: 0.5,
        grid_export_price_czk_per_kwh: 0.1,
        all_price_blocks: Some(&blocks),
        backup_discharge_min_soc: 10.0,
        grid_import_today_kwh: Some(2.0),
        consumption_today_kwh: Some(4.0),
    };

    let v1_overnight = v1_strategy.evaluate(&context_overnight);
    let v2_overnight = v2_strategy.evaluate(&context_overnight);

    println!("\n=== Overnight Charging Test (02:00, 30% SOC) ===");
    println!("Price: {:.2} CZK/kWh", blocks[8].price_czk_per_kwh);
    println!("V1: {:?} - {}", v1_overnight.mode, v1_overnight.reason);
    println!("V2: {:?} - {}", v2_overnight.mode, v2_overnight.reason);

    // Both should charge during cheap overnight period with low battery
    // (but we don't assert - just observe the behavior)
}

#[test]
fn compare_peak_discharge_behavior() {
    // Scenario: Should both strategies discharge during evening peak?
    let blocks = create_czech_price_pattern();

    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 0.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..ControlConfig::default()
    };

    let v1_config = WinterAdaptiveConfig::default();
    let v1_strategy = WinterAdaptiveStrategy::new(v1_config);

    let v2_config = WinterAdaptiveV2Config::default();
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    // Test at 19:00 (evening peak, should discharge)
    let context_peak = EvaluationContext {
        price_block: &blocks[76], // 19:00
        control_config: &control_config,
        current_battery_soc: 85.0, // High battery from overnight charge
        solar_forecast_kwh: 0.0,
        consumption_forecast_kwh: 0.5,
        grid_export_price_czk_per_kwh: 0.1,
        all_price_blocks: Some(&blocks),
        backup_discharge_min_soc: 10.0,
        grid_import_today_kwh: Some(5.0),
        consumption_today_kwh: Some(8.0),
    };

    let v1_peak = v1_strategy.evaluate(&context_peak);
    let v2_peak = v2_strategy.evaluate(&context_peak);

    println!("\n=== Peak Discharge Test (19:00, 85% SOC) ===");
    println!("Price: {:.2} CZK/kWh", blocks[76].price_czk_per_kwh);
    println!("V1: {:?} - {}", v1_peak.mode, v1_peak.reason);
    println!("V2: {:?} - {}", v2_peak.mode, v2_peak.reason);

    // Both should allow discharge (SelfUse) during expensive peak with high battery
    // (but we don't assert - just observe the behavior)
}
