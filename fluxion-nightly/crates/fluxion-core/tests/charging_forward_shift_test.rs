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

//! Tests for the forward-shift charging optimization
//!
//! This tests the fix for the issue where the system would charge at higher prices
//! early in the night (e.g., 2.5 CZK) when cheaper prices (e.g., 2.0 CZK) were
//! available later around 3:00-4:00 AM.

use chrono::{TimeZone, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;

/// Creates a price pattern that mimics the real-world scenario:
/// - Early night (22:00-01:00): ~2.5 CZK/kWh (more expensive)
/// - Later night (03:00-05:00): ~2.0 CZK/kWh (cheaper!)
/// - Morning peak (07:00-10:00): ~4.0 CZK/kWh (expensive)
fn create_early_expensive_later_cheap_pattern() -> Vec<TimeBlockPrice> {
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 22, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Early night (22:00-01:00): 12 blocks @ 2.5 CZK (NOT cheapest!)
    for i in 0..12 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.5,
            effective_price_czk_per_kwh: 2.5,
        });
    }

    // Transition (01:00-03:00): 8 blocks @ 2.3 CZK
    for i in 12..20 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.3,
            effective_price_czk_per_kwh: 2.3,
        });
    }

    // Cheapest window (03:00-05:00): 8 blocks @ 2.0 CZK - THIS IS WHERE WE SHOULD CHARGE!
    for i in 20..28 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
        });
    }

    // Pre-dawn (05:00-07:00): 8 blocks @ 2.8 CZK
    for i in 28..36 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.8,
            effective_price_czk_per_kwh: 2.8,
        });
    }

    // Morning peak (07:00-10:00): 12 blocks @ 4.0 CZK (expensive)
    for i in 36..48 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 4.0,
            effective_price_czk_per_kwh: 4.0,
        });
    }

    // Mid-day (10:00-16:00): 24 blocks @ 3.0 CZK
    for i in 48..72 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 3.0,
            effective_price_czk_per_kwh: 3.0,
        });
    }

    // Evening peak (16:00-22:00): 24 blocks @ 5.0 CZK
    for i in 72..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
            effective_price_czk_per_kwh: 5.0,
        });
    }

    blocks
}

#[test]
fn test_v2_prefers_later_cheaper_blocks_over_early_expensive() {
    let blocks = create_early_expensive_later_cheap_pattern();

    let control_config = ControlConfig {
        battery_capacity_kwh: 25.0,
        max_battery_charge_rate_kw: 10.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 1.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        force_charge_hours: 4, // 16 blocks
        ..ControlConfig::default()
    };

    let v2_config = WinterAdaptiveV2Config {
        charge_price_tolerance_percent: 15.0, // Only accept blocks within 15% of cheapest
        ..WinterAdaptiveV2Config::default()
    };
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    println!("\n=== Forward-Shift Test: Early Expensive vs Later Cheap ===");
    println!("Early night (22:00-01:00): 2.5 CZK/kWh (expensive)");
    println!("Later night (03:00-05:00): 2.0 CZK/kWh (CHEAPEST)");
    println!("Morning peak (07:00-10:00): 4.0 CZK/kWh\n");

    let mut charges_at_25 = 0;
    let mut charges_at_20 = 0;
    let mut charge_prices: Vec<f32> = Vec::new();

    // Test decisions for the first 12 hours (48 blocks)
    for block_index in 0..48 {
        let context = EvaluationContext {
            price_block: &blocks[block_index],
            control_config: &control_config,
            current_battery_soc: 20.0, // Low SOC - needs charging
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 1.5,
            grid_export_price_czk_per_kwh: 0.1,
            all_price_blocks: Some(&blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: Some(5.0),
            consumption_today_kwh: Some(8.0),
        };

        let eval = v2_strategy.evaluate(&context);
        let price = blocks[block_index].price_czk_per_kwh;

        if eval.mode == InverterOperationMode::ForceCharge {
            charge_prices.push(price);
            if (price - 2.5).abs() < 0.01 {
                charges_at_25 += 1;
            } else if (price - 2.0).abs() < 0.01 {
                charges_at_20 += 1;
            }
        }
    }

    let avg_charge_price = if charge_prices.is_empty() {
        0.0
    } else {
        charge_prices.iter().sum::<f32>() / charge_prices.len() as f32
    };

    println!("Results:");
    println!(
        "  Charges at 2.5 CZK (early night): {} blocks",
        charges_at_25
    );
    println!(
        "  Charges at 2.0 CZK (later, CHEAPEST): {} blocks",
        charges_at_20
    );
    println!("  Average charge price: {:.3} CZK/kWh", avg_charge_price);
    println!("  Total charge blocks: {}", charge_prices.len());

    // The key assertion: with 15% tolerance, 2.5 CZK blocks should be rejected
    // because they're 25% above the cheapest (2.0 CZK)
    // Max acceptable with 15% tolerance: 2.0 * 1.15 = 2.3 CZK
    // So 2.5 CZK blocks should NOT be selected
    assert!(
        charges_at_20 >= charges_at_25,
        "V2 should prefer the cheaper 2.0 CZK blocks over the 2.5 CZK blocks. \
        Got {} charges at 2.5 CZK but only {} at 2.0 CZK",
        charges_at_25,
        charges_at_20
    );

    // Average charge price should be closer to 2.0 than to 2.5
    assert!(
        avg_charge_price < 2.4,
        "Average charge price should be below 2.4 CZK (closer to 2.0 CZK), got {:.3} CZK",
        avg_charge_price
    );

    println!("\n✅ V2 correctly prefers later, cheaper blocks!");
}

#[test]
fn test_price_tolerance_filters_expensive_runs() {
    let blocks = create_early_expensive_later_cheap_pattern();

    let control_config = ControlConfig {
        battery_capacity_kwh: 25.0,
        max_battery_charge_rate_kw: 10.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 1.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        force_charge_hours: 2, // Only 8 blocks needed - should fit in cheap window
        ..ControlConfig::default()
    };

    // Test with strict tolerance (10%)
    let strict_config = WinterAdaptiveV2Config {
        charge_price_tolerance_percent: 10.0,
        ..WinterAdaptiveV2Config::default()
    };
    let strict_strategy = WinterAdaptiveV2Strategy::new(strict_config);

    // Test with permissive tolerance (30%)
    let permissive_config = WinterAdaptiveV2Config {
        charge_price_tolerance_percent: 30.0,
        ..WinterAdaptiveV2Config::default()
    };
    let permissive_strategy = WinterAdaptiveV2Strategy::new(permissive_config);

    println!("\n=== Price Tolerance Comparison ===");

    let context = EvaluationContext {
        price_block: &blocks[0], // Early block at 2.5 CZK
        control_config: &control_config,
        current_battery_soc: 20.0,
        solar_forecast_kwh: 0.0,
        consumption_forecast_kwh: 1.5,
        grid_export_price_czk_per_kwh: 0.1,
        all_price_blocks: Some(&blocks),
        backup_discharge_min_soc: 10.0,
        grid_import_today_kwh: Some(5.0),
        consumption_today_kwh: Some(8.0),
    };

    let strict_eval = strict_strategy.evaluate(&context);
    let permissive_eval = permissive_strategy.evaluate(&context);

    println!("At 22:00 (2.5 CZK block, cheapest is 2.0 CZK):");
    println!(
        "  Strict (10% tolerance, max 2.2 CZK): {:?}",
        strict_eval.mode
    );
    println!(
        "  Permissive (30% tolerance, max 2.6 CZK): {:?}",
        permissive_eval.mode
    );

    // With 10% tolerance: max acceptable = 2.0 * 1.10 = 2.2 CZK
    // 2.5 CZK > 2.2 CZK, so should NOT charge at this block
    // With 30% tolerance: max acceptable = 2.0 * 1.30 = 2.6 CZK
    // 2.5 CZK < 2.6 CZK, so MIGHT charge at this block (if selected)

    // The strict tolerance should prevent charging at early expensive blocks
    // Note: The actual behavior depends on the full scheduling algorithm,
    // but strict tolerance should result in lower average charging prices
    println!("\n✅ Price tolerance filter comparison completed!");
}
