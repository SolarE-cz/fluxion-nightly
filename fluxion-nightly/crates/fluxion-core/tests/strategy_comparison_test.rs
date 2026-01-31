// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

use chrono::{DateTime, TimeZone, Timelike, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
    WinterAdaptiveV2Config, WinterAdaptiveV2Strategy, WinterAdaptiveV3Config,
    WinterAdaptiveV3Strategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use serde::Deserialize;
use std::path::Path;

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

    // Morning peak (08:00-10:00): 8 blocks @ 4.5 CZK (expensive)
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

    // Evening peak (17:00-22:00): 20 blocks @ 5.0 CZK (most expensive)
    for i in 68..88 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
            effective_price_czk_per_kwh: 5.0,
        });
    }

    // Late evening (22:00-00:00): 8 blocks @ 2.5 CZK (starting to drop)
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
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
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
            effective_price_czk_per_kwh: 3.0,
        });
    }

    // Price spike at hour 10 (blocks 40-43)
    for i in 40..44 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 9.5, // Above 8.0 threshold
            effective_price_czk_per_kwh: 9.5,
        });
    }

    // Back to normal
    for i in 44..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
            effective_price_czk_per_kwh: 3.0,
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
        solar_forecast_total_today_kwh: 0.0,
        solar_forecast_remaining_today_kwh: 0.0,
        solar_forecast_tomorrow_kwh: 0.0,
        battery_avg_charge_price_czk_per_kwh: 0.0,
        hourly_consumption_profile: None,
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
        solar_forecast_total_today_kwh: 0.0,
        solar_forecast_remaining_today_kwh: 0.0,
        solar_forecast_tomorrow_kwh: 0.0,
        battery_avg_charge_price_czk_per_kwh: 0.0,
        hourly_consumption_profile: None,
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

// ============================================================================
// V1 vs V2 vs V3 Comprehensive Comparison with Real Data
// ============================================================================

/// HDO tariff constants (CZK/kWh)
const HDO_LOW_TARIFF_CZK: f32 = 0.50;
const HDO_HIGH_TARIFF_CZK: f32 = 1.80;

/// JSON structures for parsing Fluxion export files
#[derive(Debug, Deserialize)]
struct FluxionExport {
    prices: PricesData,
    consumption: ConsumptionData,
    inv: Vec<InverterData>,
}

#[derive(Debug, Deserialize)]
struct PricesData {
    blocks: Vec<PriceBlock>,
}

#[derive(Debug, Deserialize)]
struct PriceBlock {
    ts: i64,
    p: f32,
    #[allow(dead_code)]
    st: String,
}

#[derive(Debug, Deserialize)]
struct ConsumptionData {
    ema_kwh: f32,
}

#[derive(Debug, Deserialize)]
struct InverterData {
    bat_cap: f32,
}

/// Test scenario loaded from export file
struct TestScenario {
    name: String,
    battery_capacity_kwh: f32,
    daily_consumption_kwh: f32,
    blocks: Vec<TimeBlockPrice>,
    initial_soc: f32,
}

/// Block-by-block decision record
#[derive(Clone)]
#[allow(dead_code)]
struct BlockDecision {
    block_start: DateTime<Utc>,
    spot_price: f32,
    effective_price: f32,
    mode: InverterOperationMode,
    soc_before: f32,
    soc_after: f32,
    grid_import_kwh: f32,
    cost_czk: f32,
    reason: String,
}

/// Full simulation result
struct SimulationResult {
    strategy_name: String,
    total_grid_import_kwh: f32,
    total_grid_import_cost_czk: f32,
    total_battery_charge_kwh: f32,
    total_battery_discharge_kwh: f32,
    final_soc: f32,
    decisions: Vec<BlockDecision>,
}

/// Check if a datetime is in HDO low tariff period
/// Using typical Czech HDO schedule: 00:00-06:00, 14:00-17:00
fn is_hdo_low_tariff(dt: DateTime<Utc>) -> bool {
    let hour = dt.hour();
    // Low tariff: overnight (00-06) and afternoon (14-17)
    (0..6).contains(&hour) || (14..17).contains(&hour)
}

/// Calculate grid fee based on HDO schedule
fn get_grid_fee(dt: DateTime<Utc>) -> f32 {
    if is_hdo_low_tariff(dt) {
        HDO_LOW_TARIFF_CZK
    } else {
        HDO_HIGH_TARIFF_CZK
    }
}

/// Load test scenario from Fluxion export JSON
fn load_scenario_from_export(path: &str, name: &str) -> Option<TestScenario> {
    let content = std::fs::read_to_string(path).ok()?;
    let export: FluxionExport = serde_json::from_str(&content).ok()?;

    // Convert price blocks to TimeBlockPrice
    let blocks: Vec<TimeBlockPrice> = export
        .prices
        .blocks
        .iter()
        .map(|b| TimeBlockPrice {
            block_start: DateTime::from_timestamp(b.ts, 0).unwrap_or_else(Utc::now),
            duration_minutes: 15,
            price_czk_per_kwh: b.p,
            effective_price_czk_per_kwh: b.p,
        })
        .collect();

    Some(TestScenario {
        name: name.to_string(),
        battery_capacity_kwh: export.inv.first().map(|i| i.bat_cap).unwrap_or(10.0),
        daily_consumption_kwh: export.consumption.ema_kwh,
        blocks,
        initial_soc: 50.0, // Start at 50%
    })
}

/// Calculate baseline cost (no battery optimization)
fn calculate_baseline(scenario: &TestScenario) -> f32 {
    let consumption_per_block = scenario.daily_consumption_kwh / 96.0;

    scenario
        .blocks
        .iter()
        .map(|b| {
            let grid_fee = get_grid_fee(b.block_start);
            let effective_price = b.price_czk_per_kwh + grid_fee;
            consumption_per_block * effective_price
        })
        .sum()
}

/// Simulate a strategy over the full day
fn simulate_strategy<S: EconomicStrategy>(
    strategy: &S,
    scenario: &TestScenario,
    strategy_name: &str,
) -> SimulationResult {
    let control_config = ControlConfig {
        battery_capacity_kwh: scenario.battery_capacity_kwh,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: scenario.daily_consumption_kwh / 24.0,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        force_charge_hours: 3,           // Minimum 3 hours charging
        min_consecutive_force_blocks: 8, // 2 hours minimum
        ..ControlConfig::default()
    };

    let consumption_per_block = scenario.daily_consumption_kwh / 96.0;
    let charge_rate_per_block = control_config.max_battery_charge_rate_kw * 0.25; // 15min block

    let mut soc = scenario.initial_soc;
    let mut total_grid_import_kwh = 0.0;
    let mut total_grid_import_cost = 0.0;
    let mut total_charge_kwh = 0.0;
    let mut total_discharge_kwh = 0.0;
    let mut decisions = Vec::new();

    for (idx, block) in scenario.blocks.iter().enumerate() {
        let soc_before = soc;
        let spot_price = block.price_czk_per_kwh;
        let grid_fee = get_grid_fee(block.block_start);
        let effective_price = spot_price + grid_fee;

        // Build evaluation context
        let context = EvaluationContext {
            price_block: block,
            control_config: &control_config,
            current_battery_soc: soc,
            solar_forecast_kwh: 0.0, // Winter - no solar
            consumption_forecast_kwh: consumption_per_block,
            grid_export_price_czk_per_kwh: 0.1,
            all_price_blocks: Some(&scenario.blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(total_grid_import_kwh),
            consumption_today_kwh: Some(idx as f32 * consumption_per_block),
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        // Get strategy decision
        let eval = strategy.evaluate(&context);
        let mode = eval.mode;
        let reason = eval.reason.clone();

        // Apply mode to update SOC and track costs
        let (grid_import, cost, soc_delta, charge, discharge) = match mode {
            InverterOperationMode::ForceCharge => {
                // Charge battery from grid
                let soc_headroom = (100.0 - soc) / 100.0 * scenario.battery_capacity_kwh;
                let actual_charge = charge_rate_per_block.min(soc_headroom);
                let grid_import = actual_charge + consumption_per_block;
                let cost = grid_import * effective_price;
                let soc_gain = (actual_charge / scenario.battery_capacity_kwh) * 100.0;
                (grid_import, cost, soc_gain, actual_charge, 0.0)
            }
            InverterOperationMode::SelfUse | InverterOperationMode::BackUpMode => {
                // Try to cover consumption from battery
                let available_battery = ((soc - control_config.min_battery_soc).max(0.0) / 100.0)
                    * scenario.battery_capacity_kwh;
                let discharge = consumption_per_block.min(available_battery);
                let grid_import = consumption_per_block - discharge;
                let cost = grid_import * effective_price;
                let soc_loss = (discharge / scenario.battery_capacity_kwh) * 100.0;
                (grid_import, cost, -soc_loss, 0.0, discharge)
            }
            InverterOperationMode::ForceDischarge => {
                // Force discharge to grid (unlikely in winter)
                let available_battery = ((soc - control_config.min_battery_soc).max(0.0) / 100.0)
                    * scenario.battery_capacity_kwh;
                let discharge = charge_rate_per_block.min(available_battery);
                let soc_loss = (discharge / scenario.battery_capacity_kwh) * 100.0;
                // Still need to cover consumption from grid
                let grid_import = consumption_per_block;
                let cost = grid_import * effective_price;
                (grid_import, cost, -soc_loss, 0.0, discharge)
            }
        };

        // Update totals
        soc = (soc + soc_delta).clamp(control_config.min_battery_soc, 100.0);
        total_grid_import_kwh += grid_import;
        total_grid_import_cost += cost;
        total_charge_kwh += charge;
        total_discharge_kwh += discharge;

        decisions.push(BlockDecision {
            block_start: block.block_start,
            spot_price,
            effective_price,
            mode,
            soc_before,
            soc_after: soc,
            grid_import_kwh: grid_import,
            cost_czk: cost,
            reason,
        });
    }

    SimulationResult {
        strategy_name: strategy_name.to_string(),
        total_grid_import_kwh,
        total_grid_import_cost_czk: total_grid_import_cost,
        total_battery_charge_kwh: total_charge_kwh,
        total_battery_discharge_kwh: total_discharge_kwh,
        final_soc: soc,
        decisions,
    }
}

/// Print simulation results as a formatted table
fn print_comparison(scenario_name: &str, baseline: f32, results: &[SimulationResult]) {
    println!("\n{}", "=".repeat(80));
    println!("  {}", scenario_name);
    println!("{}", "=".repeat(80));
    println!();
    println!("Baseline (no battery): {:.2} CZK", baseline);
    println!();
    println!(
        "{:<25} {:>12} {:>12} {:>12} {:>10}",
        "Strategy", "Grid Import", "Cost (CZK)", "Savings", "Savings %"
    );
    println!("{:-<80}", "");

    for result in results {
        let savings = baseline - result.total_grid_import_cost_czk;
        let savings_pct = (savings / baseline) * 100.0;
        println!(
            "{:<25} {:>10.2} kWh {:>10.2} {:>10.2} {:>9.1}%",
            result.strategy_name,
            result.total_grid_import_kwh,
            result.total_grid_import_cost_czk,
            savings,
            savings_pct
        );
    }

    println!();
    println!("Battery Activity:");
    println!(
        "{:<25} {:>12} {:>12} {:>12}",
        "Strategy", "Charged kWh", "Discharged", "Final SOC"
    );
    println!("{:-<80}", "");

    for result in results {
        println!(
            "{:<25} {:>10.2} kWh {:>10.2} kWh {:>10.1}%",
            result.strategy_name,
            result.total_battery_charge_kwh,
            result.total_battery_discharge_kwh,
            result.final_soc
        );
    }
}

/// Print block-by-block breakdown for debugging
fn print_block_breakdown(results: &[SimulationResult], hours: &[u32]) {
    println!("\n{}", "=".repeat(100));
    println!("  Block-by-Block Comparison (selected hours)");
    println!("{}", "=".repeat(100));

    for &hour in hours {
        let block_idx = (hour * 4) as usize;
        if block_idx >= results[0].decisions.len() {
            continue;
        }

        let decision = &results[0].decisions[block_idx];
        println!(
            "\nHour {:02}:00 | Spot: {:.2} CZK | Grid Fee: {:.2} CZK | Eff: {:.2} CZK",
            hour,
            decision.spot_price,
            decision.effective_price - decision.spot_price,
            decision.effective_price
        );
        println!("{:-<100}", "");
        println!(
            "{:<15} {:>10} {:>10} {:>10} {:>12} Reason",
            "Strategy", "Mode", "SOC Before", "SOC After", "Cost",
        );

        for result in results {
            if block_idx < result.decisions.len() {
                let d = &result.decisions[block_idx];
                println!(
                    "{:<15} {:>10} {:>9.1}% {:>9.1}% {:>10.2} CZK  {}",
                    result.strategy_name.chars().take(15).collect::<String>(),
                    format!("{:?}", d.mode).chars().take(10).collect::<String>(),
                    d.soc_before,
                    d.soc_after,
                    d.cost_czk,
                    d.reason.chars().take(50).collect::<String>()
                );
            }
        }
    }
}

#[test]
fn test_v1_v2_v3_comparison_site1() {
    // Try to load real data, fall back to synthetic if not found
    // Try multiple possible paths (workspace root, crate root, test binary location)
    let possible_paths = [
        "../data/fluxion_export_20260114_165415.json",
        "../../data/fluxion_export_20260114_165415.json",
        "../../../data/fluxion_export_20260114_165415.json",
        "data/fluxion_export_20260114_165415.json",
    ];

    let scenario = possible_paths
        .iter()
        .find(|p| Path::new(p).exists())
        .and_then(|p| {
            println!("Found real data at: {}", p);
            load_scenario_from_export(p, "Site 1 (34kWh battery, 55kWh/day)")
        });

    let scenario = if scenario.is_none() {
        println!(
            "Real data not found (tried: {:?}), using synthetic scenario",
            possible_paths
        );
        println!("Current dir: {:?}", std::env::current_dir());
        None
    } else {
        scenario
    };

    let scenario = scenario.unwrap_or_else(|| {
        // Create synthetic scenario with Czech pattern
        let blocks = create_czech_price_pattern();
        TestScenario {
            name: "Synthetic Site (10kWh battery, 12kWh/day)".to_string(),
            battery_capacity_kwh: 10.0,
            daily_consumption_kwh: 12.0,
            blocks,
            initial_soc: 50.0,
        }
    });

    // Calculate baseline
    let baseline = calculate_baseline(&scenario);

    // Create and run V1
    let v1_config = WinterAdaptiveConfig::default();
    let v1 = WinterAdaptiveStrategy::new(v1_config);
    let v1_result = simulate_strategy(&v1, &scenario, "V1 Winter-Adaptive");

    // Create and run V2
    let v2_config = WinterAdaptiveV2Config::default();
    let v2 = WinterAdaptiveV2Strategy::new(v2_config);
    let v2_result = simulate_strategy(&v2, &scenario, "V2 Winter-Adaptive");

    // Create and run V3
    let v3_config = WinterAdaptiveV3Config::default();
    let v3 = WinterAdaptiveV3Strategy::new(v3_config);
    let v3_result = simulate_strategy(&v3, &scenario, "V3 Winter-Adaptive");

    // Print comparison
    let results = vec![v1_result, v2_result, v3_result];
    print_comparison(&scenario.name, baseline, &results);
    print_block_breakdown(&results, &[0, 2, 4, 8, 12, 15, 18, 20, 22]);
}

#[test]
fn test_v1_v2_v3_comparison_site2() {
    // Try to load real data (Lukas site)
    let possible_paths = [
        "../data/fluxion_export_20260114_171350_lukas.json",
        "../../data/fluxion_export_20260114_171350_lukas.json",
        "../../../data/fluxion_export_20260114_171350_lukas.json",
        "data/fluxion_export_20260114_171350_lukas.json",
    ];

    let scenario = possible_paths
        .iter()
        .find(|p| Path::new(p).exists())
        .and_then(|p| {
            println!("Found real data at: {}", p);
            load_scenario_from_export(p, "Site 2 - Lukas (57kWh battery, 10kWh/day)")
        });

    let scenario = if scenario.is_none() {
        println!("Real data not found (tried: {:?})", possible_paths);
        None
    } else {
        scenario
    };

    let scenario = scenario.unwrap_or_else(|| {
        // Create synthetic scenario
        let blocks = create_czech_price_pattern();
        TestScenario {
            name: "Synthetic Site (20kWh battery, 8kWh/day)".to_string(),
            battery_capacity_kwh: 20.0,
            daily_consumption_kwh: 8.0,
            blocks,
            initial_soc: 50.0,
        }
    });

    // Calculate baseline
    let baseline = calculate_baseline(&scenario);

    // Create and run strategies
    let v1 = WinterAdaptiveStrategy::new(WinterAdaptiveConfig::default());
    let v1_result = simulate_strategy(&v1, &scenario, "V1 Winter-Adaptive");

    let v2 = WinterAdaptiveV2Strategy::new(WinterAdaptiveV2Config::default());
    let v2_result = simulate_strategy(&v2, &scenario, "V2 Winter-Adaptive");

    let v3 = WinterAdaptiveV3Strategy::new(WinterAdaptiveV3Config::default());
    let v3_result = simulate_strategy(&v3, &scenario, "V3 Winter-Adaptive");

    // Print comparison
    let results = vec![v1_result, v2_result, v3_result];
    print_comparison(&scenario.name, baseline, &results);
    print_block_breakdown(&results, &[0, 2, 4, 8, 12, 15, 18, 20, 22]);
}

// test_v3_hdo_benefit removed - V3 no longer has HDO-specific configuration
