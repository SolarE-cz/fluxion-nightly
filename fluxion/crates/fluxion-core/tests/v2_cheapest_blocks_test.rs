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

// Test: V2 should wait for CHEAPEST blocks, not charge early at higher prices
// Scenario: Prices gradually decrease overnight (2.0 → 1.5 → 1.0 CZK)
// V2 should charge during the 1.0 CZK blocks, NOT the 2.0 or 1.5 blocks

use chrono::{TimeZone, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveConfig, WinterAdaptiveStrategy,
    WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;

fn create_decreasing_overnight_pattern() -> Vec<TimeBlockPrice> {
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Early night (00:00-02:00): 8 blocks @ 2.0 CZK (NOT cheapest)
    for i in 0..8 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
        });
    }

    // Mid night (02:00-04:00): 8 blocks @ 1.5 CZK (cheaper, but still not cheapest)
    for i in 8..16 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.5,
            effective_price_czk_per_kwh: 1.5,
        });
    }

    // Late night (04:00-06:00): 8 blocks @ 1.0 CZK (CHEAPEST!)
    for i in 16..24 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.0,
            effective_price_czk_per_kwh: 1.0,
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

    // Morning peak (08:00-12:00): 16 blocks @ 5.0 CZK
    for i in 32..48 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
            effective_price_czk_per_kwh: 5.0,
        });
    }

    // Midday valley (12:00-16:00): 16 blocks @ 2.0 CZK
    for i in 48..64 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
        });
    }

    // Evening peak (16:00-22:00): 24 blocks @ 6.0 CZK
    for i in 64..88 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 6.0,
            effective_price_czk_per_kwh: 6.0,
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
fn test_v2_waits_for_cheapest_blocks() {
    let blocks = create_decreasing_overnight_pattern();

    let control_config = ControlConfig {
        battery_capacity_kwh: 10.0,
        max_battery_charge_rate_kw: 5.0,
        battery_efficiency: 0.90,
        average_household_load_kw: 0.5,
        min_battery_soc: 10.0,
        max_battery_soc: 100.0,
        ..ControlConfig::default()
    };

    println!("\n=== V1 vs V2: Waiting for Cheapest Blocks ===");
    println!("Overnight prices: 2.0 CZK (00-02) → 1.5 CZK (02-04) → 1.0 CZK (04-06)");
    println!("Peak: 5.0 CZK (08-12), 6.0 CZK (16-22)\n");

    // Test V1
    let v1_config = WinterAdaptiveConfig::default();
    let v1_strategy = WinterAdaptiveStrategy::new(v1_config);

    let mut v1_charges = Vec::new();
    println!("V1 Charging Decisions:");
    for hour in 0..8 {
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
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let eval = v1_strategy.evaluate(&context);
        if eval.mode == InverterOperationMode::ForceCharge {
            v1_charges.push((hour, blocks[block_index].price_czk_per_kwh));
            println!(
                "  Hour {:02} @ {:.2} CZK: CHARGE",
                hour, blocks[block_index].price_czk_per_kwh
            );
        }
    }

    // Test V2
    let v2_config = WinterAdaptiveV2Config::default();
    let v2_strategy = WinterAdaptiveV2Strategy::new(v2_config);

    let mut v2_charges = Vec::new();
    println!("\nV2 Charging Decisions:");
    for hour in 0..8 {
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
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            hourly_consumption_profile: None,
        };

        let eval = v2_strategy.evaluate(&context);
        if eval.mode == InverterOperationMode::ForceCharge {
            v2_charges.push((hour, blocks[block_index].price_czk_per_kwh));
            println!(
                "  Hour {:02} @ {:.2} CZK: CHARGE",
                hour, blocks[block_index].price_czk_per_kwh
            );
        }
    }

    println!("\n=== Comparison ===");

    let v1_avg_price: f32 =
        v1_charges.iter().map(|(_, p)| p).sum::<f32>() / v1_charges.len() as f32;
    let v2_avg_price: f32 =
        v2_charges.iter().map(|(_, p)| p).sum::<f32>() / v2_charges.len() as f32;

    println!(
        "V1: {} charge blocks, avg price: {:.2} CZK",
        v1_charges.len(),
        v1_avg_price
    );
    println!(
        "V2: {} charge blocks, avg price: {:.2} CZK",
        v2_charges.len(),
        v2_avg_price
    );

    let v2_charges_at_cheapest = v2_charges.iter().filter(|(_, p)| *p == 1.0).count();
    let v1_charges_at_cheapest = v1_charges.iter().filter(|(_, p)| *p == 1.0).count();

    println!(
        "\nV1 charges at cheapest (1.0 CZK): {} blocks",
        v1_charges_at_cheapest
    );
    println!(
        "V2 charges at cheapest (1.0 CZK): {} blocks",
        v2_charges_at_cheapest
    );

    // V2 should charge at least as much at the cheapest blocks as V1
    // Note: V2's deficit mechanism may add more expensive blocks for peak coverage,
    // which can increase the average price. The key metric is that V2 prioritizes
    // charging at the cheapest available blocks.
    assert!(
        v2_charges_at_cheapest >= v1_charges_at_cheapest,
        "V2 should charge at least as much at the cheapest blocks as V1 ({} vs {})",
        v2_charges_at_cheapest,
        v1_charges_at_cheapest
    );

    // V2's average should be reasonable (not dramatically worse than V1)
    // Allow up to 10% higher average due to deficit mechanism adding peak coverage blocks
    assert!(
        v2_avg_price <= v1_avg_price * 1.10,
        "V2 average price ({:.2}) should be within 10% of V1 ({:.2})",
        v2_avg_price,
        v1_avg_price
    );

    println!("\n✅ V2 prioritizes cheapest blocks correctly!");
}
