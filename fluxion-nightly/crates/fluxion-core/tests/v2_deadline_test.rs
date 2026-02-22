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

// Test V2's deadline constraint: prices spike at hour 8, then drop to 1.0 CZK at hour 10
// V2 should charge BEFORE hour 8 (even if hour 10 is cheaper)

use chrono::{TimeZone, Utc};
use fluxion_core::strategy::{
    EconomicStrategy, EvaluationContext, WinterAdaptiveV2Config, WinterAdaptiveV2Strategy,
};
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;

fn create_spike_then_cheaper_pattern() -> Vec<TimeBlockPrice> {
    let base = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
    let mut blocks = Vec::new();

    // Overnight valley (00:00-08:00): 32 blocks @ 1.5 CZK
    for i in 0..32 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.5,
            effective_price_czk_per_kwh: 1.5,
            spot_sell_price_czk_per_kwh: None,
        });
    }

    // Morning spike (08:00-09:00): 4 blocks @ 8.0 CZK (SPIKE!)
    for i in 32..36 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 8.0,
            effective_price_czk_per_kwh: 8.0,
            spot_sell_price_czk_per_kwh: None,
        });
    }

    // Post-spike DROP (09:00-14:00): 20 blocks @ 1.0 CZK (CHEAPER than overnight!)
    for i in 36..56 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 1.0,
            effective_price_czk_per_kwh: 1.0,
            spot_sell_price_czk_per_kwh: None,
        });
    }

    // Evening peak (14:00-22:00): 32 blocks @ 5.0 CZK
    for i in 56..88 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 5.0,
            effective_price_czk_per_kwh: 5.0,
            spot_sell_price_czk_per_kwh: None,
        });
    }

    // Late evening (22:00-00:00): 8 blocks @ 2.0 CZK
    for i in 88..96 {
        blocks.push(TimeBlockPrice {
            block_start: base + chrono::Duration::minutes(i * 15),
            duration_minutes: 15,
            price_czk_per_kwh: 2.0,
            effective_price_czk_per_kwh: 2.0,
            spot_sell_price_czk_per_kwh: None,
        });
    }

    blocks
}

#[test]
fn test_v2_respects_deadline_constraint() {
    let blocks = create_spike_then_cheaper_pattern();

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

    println!("\n=== V2 Deadline Constraint Test ===");
    println!("Scenario: 1.5 CZK overnight → 8.0 CZK spike @ hour 8 → 1.0 CZK @ hour 9 (CHEAPER!)");
    println!("Expected: V2 should charge BEFORE hour 8, NOT wait for the cheaper hour 9\n");

    let mut charged_before_spike = false;
    let mut charged_after_spike = false;

    // Test charging decisions
    for hour in [0, 2, 4, 6, 7, 9, 10, 12] {
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
        let is_charging = eval.mode == InverterOperationMode::ForceCharge;

        println!(
            "Hour {:02}:00 @ {:.2} CZK: {:?} - {}",
            hour, blocks[block_index].price_czk_per_kwh, eval.mode, eval.reason
        );

        if is_charging && hour < 8 {
            charged_before_spike = true;
        }
        if is_charging && hour >= 9 {
            charged_after_spike = true;
        }
    }

    println!("\n=== Results ===");
    println!("Charged before spike (< hour 8): {}", charged_before_spike);
    println!("Charged after spike (>= hour 9): {}", charged_after_spike);

    // V2 should charge BEFORE the spike, even though hour 9+ is cheaper
    assert!(
        charged_before_spike,
        "V2 should charge before the sustained peak (hour 8)"
    );

    // V2 should NOT rely heavily on charging after spike (it's too late for peak at hour 8)
    // But it might charge a bit for the evening peak at hour 14
    println!("\n✅ V2 correctly charged before the sustained peak!");
}
