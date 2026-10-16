// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Daily savings against plain self-use: how much less the site paid than it would have with
//! `PlanMode::SelfUse` on the same recorded load and PV.
//!
//! Definitions: `docs/legacy-levers/brief-H6-savings.md`. Pure functions: no I/O, no clock.
//! The live computation and the offline `savings_report` example both call these.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::strategy::winter_reserve::{
    BatteryParams, PlanBlock, PlanMode, cheap_reference, simulate_block,
};

/// One 15-minute block of recorded site data. Prices are the planner's effective buy price and
/// the sell price; energies are kWh over the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordedBlock {
    pub start: DateTime<Utc>,
    pub buy_czk: f32,
    pub sell_czk: f32,
    pub load_kwh: f32,
    pub pv_kwh: f32,
    pub import_kwh: f32,
    pub export_kwh: f32,
}

/// One local day. Money in CZK, wear in CZK (`discharged kWh x wear cost`), reported apart from
/// the headline `savings`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DaySavings {
    pub cost_actual: f32,
    pub cost_selfuse: f32,
    /// End-of-day battery difference valued at `cheap_ref x efficiency`.
    pub adj: f32,
    /// `cost_selfuse - cost_actual + adj`.
    pub savings: f32,
    pub wear_actual: f32,
    pub wear_selfuse: f32,
}

/// Plain self-use run over a day.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SelfUseRun {
    pub cost_czk: f32,
    pub wear_czk: f32,
    pub energy_end_kwh: f32,
}

fn planner_block(block: &RecordedBlock) -> PlanBlock {
    PlanBlock {
        start: block.start,
        duration_minutes: 15,
        buy_czk: block.buy_czk,
        sell_czk: block.sell_czk,
        load_kwh: block.load_kwh,
        pv_kwh: block.pv_kwh,
        reserve_load_kwh: block.load_kwh,
        reserve_pv_kwh: block.pv_kwh,
        force_charge: false,
        no_discharge: false,
        export_min_soc_pct: None,
        force_charge_at_or_below_czk: None,
        export_allowed: true,
    }
}

/// Grid cost of the recorded flows: `Σ import·buy − export·sell`.
pub fn actual_cost(blocks: &[RecordedBlock]) -> f32 {
    blocks
        .iter()
        .map(|b| b.import_kwh * b.buy_czk - b.export_kwh * b.sell_czk)
        .sum()
}

/// Battery discharge implied by the recorded energy balance: what the house and the export took
/// beyond PV and import. The recorded data has no battery power, so this is the balance.
fn actual_discharge_kwh(block: &RecordedBlock) -> f32 {
    (block.load_kwh + block.export_kwh - block.pv_kwh - block.import_kwh).max(0.0)
}

/// Wear of the recorded day: discharged kWh x wear cost.
pub fn actual_wear(blocks: &[RecordedBlock], wear_czk_per_kwh: f32) -> f32 {
    blocks.iter().map(actual_discharge_kwh).sum::<f32>() * wear_czk_per_kwh
}

/// Plain self-use over the blocks, starting from `energy_start_kwh`, with the same recorded load
/// and PV.
pub fn simulate_selfuse(
    blocks: &[RecordedBlock],
    energy_start_kwh: f32,
    battery: &BatteryParams,
) -> SelfUseRun {
    let energy_min = battery.min_soc_pct / 100.0 * battery.capacity_kwh;
    let mut run = SelfUseRun {
        energy_end_kwh: energy_start_kwh,
        ..SelfUseRun::default()
    };
    for block in blocks {
        let outcome = simulate_block(
            PlanMode::SelfUse,
            run.energy_end_kwh,
            &planner_block(block),
            energy_min,
            battery,
        );
        run.cost_czk +=
            outcome.grid_import_kwh * block.buy_czk - outcome.grid_export_kwh * block.sell_czk;
        run.wear_czk += outcome.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
        run.energy_end_kwh = outcome.energy_end_kwh;
    }
    run
}

/// `cheap_ref` of a day: the planner's reference over that day's buy prices.
pub fn day_cheap_ref(blocks: &[RecordedBlock], cheap_percentile: f32) -> f32 {
    let buys: Vec<f32> = blocks.iter().map(|b| b.buy_czk).collect();
    cheap_reference(&buys, cheap_percentile)
}

/// Savings of one local day. `soc_start_pct` is the recorded SOC at the day's first block,
/// `soc_end_pct` the recorded SOC at the next day's first block (or the last known).
pub fn day_savings(
    blocks: &[RecordedBlock],
    soc_start_pct: f32,
    soc_end_pct: f32,
    cheap_ref_czk: f32,
    battery: &BatteryParams,
) -> DaySavings {
    let selfuse = simulate_selfuse(
        blocks,
        soc_start_pct / 100.0 * battery.capacity_kwh,
        battery,
    );
    let cost_actual = actual_cost(blocks);
    let delta_energy_kwh = soc_end_pct / 100.0 * battery.capacity_kwh - selfuse.energy_end_kwh;
    let adj = delta_energy_kwh * cheap_ref_czk * battery.efficiency;
    DaySavings {
        cost_actual,
        cost_selfuse: selfuse.cost_czk,
        adj,
        savings: selfuse.cost_czk - cost_actual + adj,
        wear_actual: actual_wear(blocks, battery.wear_cost_czk_per_kwh),
        wear_selfuse: selfuse.wear_czk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.125,
            max_export_kw: 9.5,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: false,
        }
    }

    /// Consecutive 15-minute blocks from 2026-10-13 00:00 UTC.
    fn blocks(
        count: usize,
        buy: f32,
        load: f32,
        pv: f32,
        import: f32,
        export: f32,
    ) -> Vec<RecordedBlock> {
        let base = Utc.with_ymd_and_hms(2026, 10, 13, 0, 0, 0).unwrap();
        (0..count)
            .map(|i| RecordedBlock {
                start: base + Duration::minutes(15 * i as i64),
                buy_czk: buy,
                sell_czk: 0.0,
                load_kwh: load,
                pv_kwh: pv,
                import_kwh: import,
                export_kwh: export,
            })
            .collect()
    }

    fn approx(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn flat_day_without_battery_cycling_saves_nothing() {
        // Flat 4 CZK price. PV covers the load in every block, so the battery is never touched:
        // the recorded day is exactly what self-use does.
        let day = blocks(96, 4.0, 0.25, 0.25, 0.0, 0.0);
        let result = day_savings(&day, 50.0, 50.0, day_cheap_ref(&day, 0.2), &battery());
        assert!(approx(result.savings, 0.0, 0.01), "{result:?}");
        assert!(approx(result.adj, 0.0, 0.01), "{result:?}");
    }

    #[test]
    fn flat_day_with_identical_cycling_saves_nothing() {
        // Flat 4 CZK price, 4 deficit blocks of 0.25 kWh at night. Self-use covers them from the
        // battery, so the recorded day shows the same 1.0 kWh discharge and no grid import.
        // Start 50% = 11.5 kWh, end 10.5 kWh = 45.65%. Wear 1.0 kWh x 0.125 = 0.125 CZK both ways.
        let day = blocks(4, 4.0, 0.25, 0.0, 0.0, 0.0);
        let soc_end_pct = (11.5 - 1.0) / 23.0 * 100.0;
        let result = day_savings(
            &day,
            50.0,
            soc_end_pct,
            day_cheap_ref(&day, 0.2),
            &battery(),
        );
        assert!(approx(result.savings, 0.0, 0.01), "{result:?}");
        assert!(approx(result.wear_actual, 0.125, 1e-4), "{result:?}");
        assert!(approx(result.wear_selfuse, 0.125, 1e-4), "{result:?}");
    }

    #[test]
    fn night_charge_displaces_evening_import() {
        // Start at 10% (2.3 kWh, the floor), end at 10%.
        // Night (5 blocks, 2 CZK): the grid charges 10 kWh stored = 10 / 0.9 = 11.1111 kWh import.
        // Evening (4 blocks, 8 CZK): 10 kWh load, served by the battery in the actual run.
        // Self-use starts at the floor, so it imports all 10 kWh of evening load.
        // savings = 10 x 8 - (10 / 0.9) x 2 = 80 - 22.2222 = 57.7778 CZK.
        // Wear: 10 kWh discharged x 0.125 = 1.25 CZK actual, 0 self-use.
        let mut day = blocks(5, 2.0, 0.0, 0.0, 10.0 / 0.9 / 5.0, 0.0);
        day.extend(blocks(4, 8.0, 2.5, 0.0, 0.0, 0.0));
        let result = day_savings(&day, 10.0, 10.0, day_cheap_ref(&day, 0.2), &battery());
        assert!(approx(result.cost_selfuse, 80.0, 0.01), "{result:?}");
        assert!(approx(result.cost_actual, 22.2222, 0.01), "{result:?}");
        assert!(approx(result.savings, 57.7778, 0.01), "{result:?}");
        assert!(approx(result.wear_actual, 1.25, 0.01), "{result:?}");
        assert!(approx(result.wear_selfuse, 0.0, 0.01), "{result:?}");
    }

    #[test]
    fn battery_left_fuller_is_valued_at_cheap_ref() {
        // 10% -> 50% at day end: delta = 0.4 x 23 = 9.2 kWh. adj = 9.2 x 3.0 x 0.9 = 24.84 CZK.
        // No recorded flows, so savings equals adj.
        let day = blocks(1, 3.0, 0.0, 0.0, 0.0, 0.0);
        let result = day_savings(&day, 10.0, 50.0, 3.0, &battery());
        assert!(approx(result.adj, 24.84, 0.01), "{result:?}");
        assert!(approx(result.savings, 24.84, 0.01), "{result:?}");
    }
}
