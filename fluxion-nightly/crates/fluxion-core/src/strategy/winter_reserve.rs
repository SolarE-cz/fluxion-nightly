// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Winter Reserve strategy: one DP planner that buys in the cheapest blocks,
//! exports only for real profit and keeps enough battery to last the night.
//!
//! Design and exact algorithm: `docs/winter-reserve/PRD.md`.
//! The planner (`plan`, `simulate_block`) is pure: no I/O, no clock.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::{DischargeWindowLimit, UserLevers};
use serde::{Deserialize, Serialize};

use super::{Assumptions, BlockEvaluation, EnergyFlows};

/// Re-plan on a head call once the cached plan is this old (PRD "Re-plan triggers").
const REPLAN_MAX_AGE_MIN: i64 = 15;

/// Battery and grid-connection limits of one site.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BatteryParams {
    pub capacity_kwh: f32,
    pub min_soc_pct: f32,
    pub max_soc_pct: f32,
    pub max_charge_kw: f32,
    pub max_discharge_kw: f32,
    /// Round-trip efficiency, applied on charge (stored = input * efficiency)
    pub efficiency: f32,
    pub wear_cost_czk_per_kwh: f32,
    pub max_export_kw: f32,
    /// Export stops this far above the night floor (kWh): SolaX mode changes are debounced
    /// by `min_mode_change_interval_secs` and Force Discharge runs at full power meanwhile.
    #[serde(default)]
    pub export_guard_margin_kwh: f32,
    /// Export is a battery discharge: offered only when `control.force_discharge_enabled`
    #[serde(default)]
    pub force_discharge_enabled: bool,
}

/// Tunables of the strategy (`strategies.winter_reserve` config section).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WinterReserveParams {
    pub min_arbitrage_profit_czk: f32,
    pub insane_price_multiplier: f32,
    pub cheap_percentile: f32,
    pub load_safety_factor: f32,
    pub pv_confidence: f32,
    pub min_mode_benefit_czk: f32,
    /// Extra benefit (CZK) a SelfUse/Hold switch needs before a kept mode is dropped
    pub mode_switch_min_benefit_czk: f32,
    pub soc_resolution_pct: f32,
}

impl Default for WinterReserveParams {
    fn default() -> Self {
        Self {
            min_arbitrage_profit_czk: 3.0,
            insane_price_multiplier: 3.0,
            cheap_percentile: 0.20,
            load_safety_factor: 1.10,
            pv_confidence: 0.70,
            min_mode_benefit_czk: 0.10,
            mode_switch_min_benefit_czk: 1.0,
            soc_resolution_pct: 0.5,
        }
    }
}

/// One block of planner input. `load_kwh` / `pv_kwh` are the expected flows, which the DP and the
/// forward pass plan on. `reserve_load_kwh` / `reserve_pv_kwh` are the conservative flows
/// (load * load_safety_factor, pv * pv_confidence), which only size the night reserve and the
/// export guard. Buy decisions must not assume the reserve view, or the planner buys energy the
/// sun will deliver.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlanBlock {
    pub start: DateTime<Utc>,
    pub duration_minutes: u32,
    /// Effective buy price incl. VAT, operator fee and HDO fee (CZK/kWh)
    pub buy_czk: f32,
    /// Sell price after operator fee (CZK/kWh)
    pub sell_czk: f32,
    /// Expected load (kWh)
    pub load_kwh: f32,
    /// Expected PV (kWh)
    pub pv_kwh: f32,
    /// Conservative load (kWh), used for the night reserve
    pub reserve_load_kwh: f32,
    /// Conservative PV (kWh), used for the night reserve
    pub reserve_pv_kwh: f32,
    /// Price lever: buy ≤ "Vynutit nabíjení" → the block must Charge.
    #[serde(default)]
    pub force_charge: bool,
    /// Price lever: buy ≤ "Zákaz vybíjení" → the battery must not discharge.
    #[serde(default)]
    pub no_discharge: bool,
    /// The "Vynutit nabíjení" threshold (CZK/kWh) the block was forced under; shown in the reason.
    #[serde(default)]
    pub force_charge_at_or_below_czk: Option<f32>,
    /// Discharge window lever: Export never goes below this SOC (%).
    #[serde(default)]
    pub export_min_soc_pct: Option<f32>,
    /// Discharge window lever: Export is allowed in this block.
    #[serde(default = "export_allowed_default")]
    pub export_allowed: bool,
}

fn export_allowed_default() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanMode {
    SelfUse,
    Charge,
    Hold,
    Export,
    /// Battery never discharges (price lever); PV surplus still charges it.
    NoDischarge,
}

/// Energy flows of one simulated block (kWh, all non-negative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockOutcome {
    pub energy_end_kwh: f32,
    pub grid_import_kwh: f32,
    pub grid_export_kwh: f32,
    pub battery_charge_kwh: f32,
    pub battery_discharge_kwh: f32,
    pub curtailed_kwh: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedBlock {
    pub start: DateTime<Utc>,
    pub mode: PlanMode,
    pub soc_start_pct: f32,
    pub soc_end_pct: f32,
    /// Night-reserve floor for this block (kWh); Export never goes below it
    pub floor_kwh: f32,
    pub outcome: BlockOutcome,
    pub cost_czk: f32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub blocks: Vec<PlannedBlock>,
    pub cheap_ref_czk: f32,
    pub median_buy_czk: f32,
    pub expected_cost_czk: f32,
}

impl BatteryParams {
    /// Stored-energy bounds in kWh: (Emin, Emax).
    fn bounds_kwh(&self) -> (f32, f32) {
        (
            self.min_soc_pct / 100.0 * self.capacity_kwh,
            self.max_soc_pct / 100.0 * self.capacity_kwh,
        )
    }
}

/// Grid side after the battery: import the deficit, or export the surplus up to the cap.
fn grid_balance(outcome: &mut BlockOutcome, balance_kwh: f32, max_export_kwh: f32) {
    if balance_kwh >= 0.0 {
        outcome.grid_import_kwh = balance_kwh;
    } else {
        outcome.grid_export_kwh = (-balance_kwh).min(max_export_kwh);
        outcome.curtailed_kwh = -balance_kwh - outcome.grid_export_kwh;
    }
}

/// Block cost in CZK: import at buy, minus export at sell, plus wear on discharged energy.
fn outcome_cost(outcome: &BlockOutcome, block: &PlanBlock, wear_czk_per_kwh: f32) -> f32 {
    outcome.grid_import_kwh * block.buy_czk - outcome.grid_export_kwh * block.sell_czk
        + outcome.battery_discharge_kwh * wear_czk_per_kwh
}

/// Linear interpolation of `values` (defined on the ascending `grid`) at `energy_kwh`.
fn interpolate(grid: &[f32], values: &[f32], energy_kwh: f32) -> f32 {
    let last = values.len() - 1;
    if last == 0 {
        return values[0];
    }
    let e = energy_kwh.clamp(grid[0], grid[last]);
    let idx = grid
        .partition_point(|&g| g <= e)
        .saturating_sub(1)
        .min(last - 1);
    let (g0, g1) = (grid[idx], grid[idx + 1]);
    if g1 <= g0 {
        return values[idx];
    }
    values[idx] + (e - g0) / (g1 - g0) * (values[idx + 1] - values[idx])
}

/// One DP step: the block, its night floor, and the value of the following step on the grid.
struct DpStep<'a> {
    block: &'a PlanBlock,
    floor_kwh: f32,
    battery: &'a BatteryParams,
    grid: &'a [f32],
    next_values: &'a [f32],
}

impl DpStep<'_> {
    /// Block cost of `mode` from `energy_kwh`, plus the interpolated value of the energy it leaves.
    fn value(&self, mode: PlanMode, energy_kwh: f32) -> (f32, BlockOutcome) {
        let outcome = simulate_block(mode, energy_kwh, self.block, self.floor_kwh, self.battery);
        let cost = outcome_cost(&outcome, self.block, self.battery.wear_cost_czk_per_kwh);
        let future = interpolate(self.grid, self.next_values, outcome.energy_end_kwh);
        (cost + future, outcome)
    }
}

/// Modes allowed in a block. SelfUse comes first, then Hold < Charge < Export (tie-break order).
fn candidate_modes(
    block: &PlanBlock,
    median_buy: f32,
    cheap_ref: f32,
    battery: &BatteryParams,
    params: &WinterReserveParams,
) -> Vec<PlanMode> {
    // A single candidate is the choice: `choose` returns it.
    if block.force_charge {
        return vec![PlanMode::Charge];
    }
    // The default is NoDischarge (first); Charge stays allowed below the median as usual.
    if block.no_discharge {
        let mut modes = vec![PlanMode::NoDischarge];
        if block.buy_czk <= median_buy {
            modes.push(PlanMode::Charge);
        }
        return modes;
    }
    let mut modes = vec![PlanMode::SelfUse, PlanMode::Hold];
    if block.buy_czk <= median_buy {
        modes.push(PlanMode::Charge);
    }
    let export_profit =
        block.sell_czk - cheap_ref / battery.efficiency - battery.wear_cost_czk_per_kwh;
    if block.export_allowed
        && battery.force_discharge_enabled
        && block.sell_czk > 0.0
        && export_profit >= params.min_arbitrage_profit_czk
    {
        modes.push(PlanMode::Export);
    }
    modes
}

/// Two block outcomes are physically the same when the battery and the grid see the same flows.
/// Then the two modes differ only on paper, and switching the inverter mode changes nothing.
fn same_physical_outcome(a: &BlockOutcome, b: &BlockOutcome) -> bool {
    const TOL: f32 = 1e-6;
    let close = |x: f32, y: f32| (x - y).abs() <= TOL;
    close(a.energy_end_kwh, b.energy_end_kwh)
        && close(a.battery_charge_kwh, b.battery_charge_kwh)
        && close(a.battery_discharge_kwh, b.battery_discharge_kwh)
        && close(a.grid_import_kwh, b.grid_import_kwh)
        && close(a.grid_export_kwh, b.grid_export_kwh)
        && close(a.curtailed_kwh, b.curtailed_kwh)
}

/// Anti-churn choice (PRD): SelfUse unless another mode is better by more than `min_benefit`.
/// `modes[0]` must be SelfUse. Returns (mode, value, outcome).
///
/// `keep` is the mode applied to the previous block. When the chosen mode leaves the battery and
/// grid exactly as `keep` would (for example SelfUse and Hold at the SOC floor), `keep` is returned:
/// the inverter write would change nothing, and the planned cost is identical.
///
/// Between SelfUse and Hold only, `keep` is also returned when it is within
/// `mode_switch_min_benefit` of the chosen mode. Small forecast changes then do not flip the
/// mode back and forth. Charge, Export and NoDischarge are never held this way. The forward pass
/// passes this only for block 0, the live transition; later blocks pass 0.
fn choose(
    dp: &DpStep,
    modes: &[PlanMode],
    energy_kwh: f32,
    min_benefit: f32,
    mode_switch_min_benefit: f32,
    keep: Option<PlanMode>,
) -> (PlanMode, f32, BlockOutcome) {
    let evals: Vec<(PlanMode, f32, BlockOutcome)> = modes
        .iter()
        .map(|&mode| {
            let (value, outcome) = dp.value(mode, energy_kwh);
            (mode, value, outcome)
        })
        .collect();
    select_mode(&evals, min_benefit, mode_switch_min_benefit, keep)
}

/// The selection rule of `choose`, on evaluated candidates (`evals[0]` is SelfUse).
fn select_mode(
    evals: &[(PlanMode, f32, BlockOutcome)],
    min_benefit: f32,
    mode_switch_min_benefit: f32,
    keep: Option<PlanMode>,
) -> (PlanMode, f32, BlockOutcome) {
    let best = evals.iter().map(|e| e.1).fold(f32::INFINITY, f32::min);
    let chosen = if evals[0].1 <= best + min_benefit {
        evals[0]
    } else {
        evals[1..]
            .iter()
            .copied()
            .reduce(|a, b| if b.1 < a.1 { b } else { a })
            .unwrap_or(evals[0])
    };
    if let Some(kept) = keep.and_then(|mode| evals.iter().find(|e| e.0 == mode)) {
        let switch_only = |m: PlanMode| matches!(m, PlanMode::SelfUse | PlanMode::Hold);
        if same_physical_outcome(&kept.2, &chosen.2) {
            return *kept;
        }
        if switch_only(kept.0)
            && switch_only(chosen.0)
            && kept.1 <= chosen.1 + mode_switch_min_benefit
        {
            return *kept;
        }
    }
    chosen
}

/// Real sun: expected PV above 5 % of the block's hours.
fn has_real_sun(b: &PlanBlock) -> bool {
    b.pv_kwh > 0.05 * (b.duration_minutes as f32 / 60.0)
}

/// A sunny block whose PV covers its load (expected flows). Only such blocks end a night.
fn covers_load(b: &PlanBlock) -> bool {
    has_real_sun(b) && b.pv_kwh >= b.load_kwh
}

/// Local hour before which a no-sun head is still in the night (see `morning_blocks`).
const PRE_HORIZON_NIGHT_BEFORE_HOUR: u32 = 8;

/// Morning blocks (PRD "Mornings and floor"): pv >= load after a deficit run of at least 4 h.
/// A horizon whose first block has no real sun and starts before 08:00 local began its night
/// before the horizon, so that run counts as already long enough. A sunny but insufficient head,
/// or a no-sun head later in the day (fog, snow, forecast gap), does not.
fn morning_blocks(blocks: &[PlanBlock], tz: Tz) -> Vec<bool> {
    const MIN_NIGHT_MINUTES: u32 = 4 * 60;
    let night_head = blocks.first().is_some_and(|b| {
        !has_real_sun(b) && b.start.with_timezone(&tz).hour() < PRE_HORIZON_NIGHT_BEFORE_HOUR
    });
    let mut run_minutes = if night_head {
        MIN_NIGHT_MINUTES
    } else {
        0 // deficit run ending just before the current block
    };
    blocks
        .iter()
        .map(|b| {
            let covered = covers_load(b);
            let morning = covered && run_minutes >= MIN_NIGHT_MINUTES;
            // No-sun blocks extend the night even at zero load; only sunny blocks that cover the load end it.
            if covered {
                run_minutes = 0;
            } else {
                run_minutes += b.duration_minutes;
            }
            morning
        })
        .collect()
}

/// Night reserve floor per block (kWh), see PRD "Mornings and floor". The deficits and the open
/// night check use the conservative (reserve) flows; `morning_blocks` uses the expected flows.
fn night_floors(
    blocks: &[PlanBlock],
    battery: &BatteryParams,
    cheap_ref: f32,
    params: &WinterReserveParams,
    tz: Tz,
) -> Vec<f32> {
    let (emin, emax) = battery.bounds_kwh();
    let morning = morning_blocks(blocks, tz);
    // prefix[i] = sum of max(net, 0) over blocks[..i]
    let prefix: Vec<f32> = std::iter::once(0.0)
        .chain(blocks.iter().scan(0.0_f32, |acc, b| {
            *acc += (b.reserve_load_kwh - b.reserve_pv_kwh).max(0.0);
            Some(*acc)
        }))
        .collect();
    let n = blocks.len();
    // A horizon that ends in an unfinished night (no morning, last block a deficit) reserves all of Emax.
    // Same "covered" rule as `morning_blocks`: a block without real sun is night, even at zero load.
    let night_open = blocks.last().is_some_and(|b| {
        let real_sun = b.reserve_pv_kwh > 0.05 * (b.duration_minutes as f32 / 60.0);
        !(real_sun && b.reserve_pv_kwh >= b.reserve_load_kwh)
    });
    let mut floors = vec![emin; n];
    let mut next_morning = n;
    for (k, block) in blocks.iter().enumerate().rev() {
        let raw = if next_morning == n && night_open {
            emax
        } else {
            (emin + prefix[next_morning] - prefix[k + 1]).min(emax)
        };
        // Insane price only counts against a known, positive cheap reference.
        let insane =
            cheap_ref > 0.0 && block.sell_czk >= params.insane_price_multiplier * cheap_ref;
        floors[k] = if insane { emin } else { raw };
        if morning[k] {
            next_morning = k;
        }
    }
    floors
}

fn soc_pct_of(energy_kwh: f32, capacity_kwh: f32) -> f32 {
    if capacity_kwh > 0.0 {
        energy_kwh / capacity_kwh * 100.0
    } else {
        0.0
    }
}

fn plan_reason(
    mode: PlanMode,
    block: &PlanBlock,
    floor_kwh: f32,
    median_buy: f32,
    cheap_ref: f32,
    soc_start_pct: f32,
    soc_end_pct: f32,
) -> String {
    let (label, price): (String, String) = match mode {
        PlanMode::SelfUse => (
            "SELF-USE".into(),
            format!("buy {:.2} | sell {:.2}", block.buy_czk, block.sell_czk),
        ),
        PlanMode::Charge if block.force_charge => (
            format!(
                "CHARGE (price lever ≤ {:.2})",
                block.force_charge_at_or_below_czk.unwrap_or(block.buy_czk)
            ),
            format!("buy {:.2}", block.buy_czk),
        ),
        PlanMode::Charge => (
            "CHARGE".into(),
            format!("buy {:.2} ≤ median {median_buy:.2}", block.buy_czk),
        ),
        PlanMode::Hold => (
            "HOLD".into(),
            format!("buy {:.2} | sell {:.2}", block.buy_czk, block.sell_czk),
        ),
        PlanMode::Export => (
            "EXPORT".into(),
            format!("sell {:.2} | buy {:.2}", block.sell_czk, block.buy_czk),
        ),
        PlanMode::NoDischarge => (
            "NO-DISCHARGE (price lever)".into(),
            format!("buy {:.2} | sell {:.2}", block.buy_czk, block.sell_czk),
        ),
    };
    format!(
        "WinterReserve {label} {price} | cheap_ref {cheap_ref:.2} | floor {floor_kwh:.1} kWh | SOC {soc_start_pct:.0}→{soc_end_pct:.0}%"
    )
}

/// Simulate one block for a given mode, starting with `energy_kwh` stored.
/// `floor_kwh` only limits `PlanMode::Export`. See PRD "Block simulator".
pub fn simulate_block(
    mode: PlanMode,
    energy_kwh: f32,
    block: &PlanBlock,
    floor_kwh: f32,
    battery: &BatteryParams,
) -> BlockOutcome {
    let h = block.duration_minutes as f32 / 60.0;
    let (emin, emax) = battery.bounds_kwh();
    let eta = battery.efficiency.max(1e-6);
    let cmax = battery.max_charge_kw * h;
    let dmax = battery.max_discharge_kw * h;
    let xmax = battery.max_export_kw * h;
    let e = energy_kwh.clamp(emin, emax);
    let (load, pv) = (block.load_kwh, block.pv_kwh);
    let mut o = BlockOutcome::default();

    match mode {
        PlanMode::SelfUse | PlanMode::NoDischarge if pv >= load => {
            let surplus = pv - load;
            let c = surplus.min(cmax).min(((emax - e) / eta).max(0.0));
            o.battery_charge_kwh = c;
            o.energy_end_kwh = e + c * eta;
            o.grid_export_kwh = (surplus - c).min(xmax);
            o.curtailed_kwh = surplus - c - o.grid_export_kwh;
        }
        PlanMode::SelfUse | PlanMode::NoDischarge => {
            let deficit = load - pv;
            let q = match mode {
                PlanMode::SelfUse => deficit.min(dmax).min(e - emin).max(0.0),
                _ => 0.0,
            };
            o.battery_discharge_kwh = q;
            o.energy_end_kwh = e - q;
            o.grid_import_kwh = deficit - q;
        }
        PlanMode::Charge | PlanMode::Hold => {
            let c = if mode == PlanMode::Charge {
                cmax.min(((emax - e) / eta).max(0.0))
            } else {
                0.0
            };
            o.battery_charge_kwh = c;
            o.energy_end_kwh = e + c * eta;
            grid_balance(&mut o, load + c - pv, xmax);
        }
        PlanMode::Export => {
            // The inverter throttles discharge to what the house plus the export cap can take.
            let sink = xmax + (load - pv).max(0.0);
            let keep = emin.max(floor_kwh + battery.export_guard_margin_kwh);
            let q = (e - keep).max(0.0).min(dmax).min(sink);
            o.battery_discharge_kwh = q;
            o.energy_end_kwh = e - q;
            grid_balance(&mut o, load - (q + pv), xmax);
        }
    }
    o
}

/// Every block SelfUse, for inputs the planner cannot use (NaN prices, broken battery config).
fn self_use_plan(blocks: &[PlanBlock], soc_pct: f32, capacity: f32, why: &str) -> Plan {
    let energy_kwh = soc_pct / 100.0 * capacity;
    Plan {
        blocks: blocks
            .iter()
            .map(|block| PlannedBlock {
                start: block.start,
                mode: PlanMode::SelfUse,
                soc_start_pct: soc_pct,
                soc_end_pct: soc_pct,
                floor_kwh: 0.0,
                outcome: BlockOutcome {
                    energy_end_kwh: energy_kwh,
                    ..BlockOutcome::default()
                },
                cost_czk: 0.0,
                reason: format!("WinterReserve SELF-USE | {why}"),
            })
            .collect(),
        cheap_ref_czk: 0.0,
        median_buy_czk: 0.0,
        expected_cost_czk: 0.0,
    }
}

/// Mean of the lowest `ceil(percentile * n)` buy prices: the planner's `cheap_ref`.
/// Input order does not matter.
pub fn cheap_reference(buys: &[f32], percentile: f32) -> f32 {
    let mut sorted = buys.to_vec();
    sorted.sort_by(f32::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    // The epsilon keeps f32 rounding from turning 0.2 * 60 = 12 into 13.
    let n_cheap = ((f64::from(percentile) * n as f64 - 1e-6).ceil() as usize).clamp(1, n);
    sorted[..n_cheap].iter().sum::<f32>() / n_cheap as f32
}

/// Plan the whole horizon. `blocks[0]` is the current block, `soc_pct` the live SOC.
pub fn plan(
    blocks: &[PlanBlock],
    soc_pct: f32,
    tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
) -> Plan {
    plan_from(blocks, soc_pct, tz, battery, params, None)
}

/// `plan`, with the mode currently applied to the inverter (`current`, None = unknown). In blocks
/// without PV, a block keeps the previous block's mode (the first block keeps `current`) when both
/// leave the battery and grid exactly the same, so the mode does not flip at each re-plan. With
/// None the first block defaults to SelfUse.
pub fn plan_from(
    blocks: &[PlanBlock],
    soc_pct: f32,
    tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
    current: Option<PlanMode>,
) -> Plan {
    let n = blocks.len();
    if n == 0 {
        return Plan {
            blocks: Vec::new(),
            cheap_ref_czk: 0.0,
            median_buy_czk: 0.0,
            expected_cost_czk: 0.0,
        };
    }
    let (emin, emax) = battery.bounds_kwh();
    let capacity = battery.capacity_kwh;
    if blocks
        .iter()
        .any(|b| !b.buy_czk.is_finite() || !b.sell_czk.is_finite())
    {
        return self_use_plan(blocks, soc_pct, capacity, "invalid price data");
    }
    if battery.min_soc_pct >= battery.max_soc_pct || capacity.is_nan() || capacity <= 0.0 {
        return self_use_plan(blocks, soc_pct, capacity, "invalid battery config");
    }

    let mut buys: Vec<f32> = blocks.iter().map(|b| b.buy_czk).collect();
    buys.sort_by(f32::total_cmp);
    let median_buy = if n % 2 == 1 {
        buys[n / 2]
    } else {
        (buys[n / 2 - 1] + buys[n / 2]) / 2.0
    };
    let cheap_ref = cheap_reference(&buys, params.cheap_percentile);
    let floors: Vec<f32> = night_floors(blocks, battery, cheap_ref, params, tz)
        .into_iter()
        .zip(blocks)
        .map(|(floor, b)| match b.export_min_soc_pct {
            // Discharge window lever: Export never goes below its SOC limit.
            Some(pct) => floor.max(pct / 100.0 * capacity).min(emax),
            None => floor,
        })
        .collect();

    // Energy grid from Emin to Emax; the last point is exactly Emax.
    let span = (emax - emin).max(0.0);
    let grid_step = params.soc_resolution_pct / 100.0 * capacity;
    let steps = if span > 0.0 && grid_step > 0.0 {
        (span / grid_step).ceil() as usize
    } else {
        0
    };
    let grid: Vec<f32> = (0..=steps)
        .map(|i| {
            if i == steps {
                emin + span
            } else {
                emin + (i as f32 * grid_step).min(span)
            }
        })
        .collect();

    // values[k][i] = V_k at grid[i]; values[n] is the terminal value.
    let mut values: Vec<Vec<f32>> = vec![Vec::new(); n + 1];
    // Leftover energy is worth what it saves later: a cheap purchase, minus the wear of using it.
    let terminal_price = (cheap_ref - battery.wear_cost_czk_per_kwh).max(0.0);
    values[n] = grid.iter().map(|&e| -e * terminal_price).collect();
    for (k, block) in blocks.iter().enumerate().rev() {
        let (current, later) = values.split_at_mut(k + 1);
        let modes = candidate_modes(block, median_buy, cheap_ref, battery, params);
        let dp = DpStep {
            block,
            floor_kwh: floors[k],
            battery,
            grid: &grid,
            next_values: &later[0],
        };
        current[k] = grid
            .iter()
            .map(|&e| {
                choose(
                    &dp,
                    &modes,
                    e,
                    params.min_mode_benefit_czk,
                    params.mode_switch_min_benefit_czk,
                    None,
                )
                .1
            })
            .collect();
    }

    // Forward pass with the exact simulator, starting from the live SOC.
    let mut energy = (soc_pct / 100.0 * capacity).clamp(emin, emax);
    let mut planned = Vec::with_capacity(n);
    let mut expected_cost = 0.0;
    let mut previous = current;
    for (k, block) in blocks.iter().enumerate() {
        let modes = candidate_modes(block, median_buy, cheap_ref, battery, params);
        let dp = DpStep {
            block,
            floor_kwh: floors[k],
            battery,
            grid: &grid,
            next_values: &values[k + 1],
        };
        // Keep the applied mode in blocks without PV, or when it is SelfUse at any PV. The simulator
        // models Hold as never charging from PV, so a kept Hold could block real PV charging.
        // SelfUse charges from PV, so keeping it is safe.
        let keep = previous.filter(|m| block.pv_kwh <= 0.0 || *m == PlanMode::SelfUse);
        // Only block 0 is the live transition (keep is the mode on the inverter). Later blocks
        // are re-decided when they become block 0, so they get no SelfUse/Hold hysteresis.
        let switch_benefit = if k == 0 {
            params.mode_switch_min_benefit_czk
        } else {
            0.0
        };
        let (mode, _, outcome) = choose(
            &dp,
            &modes,
            energy,
            params.min_mode_benefit_czk,
            switch_benefit,
            keep,
        );
        previous = Some(mode);
        let cost = outcome_cost(&outcome, block, battery.wear_cost_czk_per_kwh);
        let soc_start_pct = soc_pct_of(energy, capacity);
        let soc_end_pct = soc_pct_of(outcome.energy_end_kwh, capacity);
        planned.push(PlannedBlock {
            start: block.start,
            mode,
            soc_start_pct,
            soc_end_pct,
            floor_kwh: floors[k],
            outcome,
            cost_czk: cost,
            reason: plan_reason(
                mode,
                block,
                floors[k],
                median_buy,
                cheap_ref,
                soc_start_pct,
                soc_end_pct,
            ),
        });
        expected_cost += cost;
        energy = outcome.energy_end_kwh;
    }

    Plan {
        blocks: planned,
        cheap_ref_czk: cheap_ref,
        median_buy_czk: median_buy,
        expected_cost_czk: expected_cost,
    }
}

/// Full planner input of the last head replan, written atomically (temp file, then rename).
/// Skipped when `LAST_PLAN_DIR` is not a directory.
const LAST_PLAN_DIR: &str = "/data";
const LAST_PLAN_PATH: &str = "/data/winter_reserve_last_plan.json";
const LAST_PLAN_TMP: &str = "/data/winter_reserve_last_plan.json.tmp";

/// When the dump last failed; the WARN is logged at most once an hour.
static LAST_PLAN_WARNED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Optional inputs a replan had, for the diagnostics line.
#[derive(Clone, Copy)]
struct PlanInputs {
    block_forecasts: bool,
    profile: bool,
}

#[derive(Serialize)]
struct PlanDumpBlock<'a> {
    #[serde(flatten)]
    block: &'a PlanBlock,
    floor_kwh: f32,
}

#[derive(Serialize)]
struct PlanDump<'a> {
    soc_pct: f32,
    blocks: Vec<PlanDumpBlock<'a>>,
}

/// One INFO line per head replan, and the full planner input as JSON to `LAST_PLAN_PATH`.
fn log_replan(horizon: &[PlanBlock], plan: &Plan, soc_pct: f32, inputs: PlanInputs, tz: Tz) {
    let sum = |f: fn(&PlanBlock) -> f32| horizon.iter().map(f).sum::<f32>();
    let stamp = |b: &PlanBlock| b.start.format("%Y-%m-%dT%H:%MZ").to_string();
    let first_morning = horizon
        .iter()
        .zip(morning_blocks(horizon, tz))
        .find(|(_, morning)| *morning)
        .map_or_else(|| "none".to_owned(), |(b, _)| stamp(b));
    let head = &plan.blocks[0];
    let yes_no = |on: bool| if on { "yes" } else { "no" };
    tracing::info!(
        "WinterReserve horizon: n={} first={} last={} block_forecasts={} profile={} \
         load_expected_kwh={:.2} pv_expected_kwh={:.2} load_reserve_kwh={:.2} pv_reserve_kwh={:.2} \
         first_morning={first_morning} head_floor_kwh={:.2} head_mode={:?} soc_pct={soc_pct:.1}",
        horizon.len(),
        stamp(&horizon[0]),
        stamp(&horizon[horizon.len() - 1]),
        yes_no(inputs.block_forecasts),
        yes_no(inputs.profile),
        sum(|b| b.load_kwh),
        sum(|b| b.pv_kwh),
        sum(|b| b.reserve_load_kwh),
        sum(|b| b.reserve_pv_kwh),
        head.floor_kwh,
        head.mode,
    );
    dump_last_plan(horizon, plan, soc_pct);
}

fn dump_last_plan(horizon: &[PlanBlock], plan: &Plan, soc_pct: f32) {
    if !std::path::Path::new(LAST_PLAN_DIR).is_dir() {
        return;
    }
    let dump = PlanDump {
        soc_pct,
        blocks: horizon
            .iter()
            .zip(&plan.blocks)
            .map(|(block, planned)| PlanDumpBlock {
                block,
                floor_kwh: planned.floor_kwh,
            })
            .collect(),
    };
    let written = serde_json::to_string(&dump)
        .map_err(std::io::Error::other)
        .and_then(|json| {
            std::fs::write(LAST_PLAN_TMP, json)?;
            std::fs::rename(LAST_PLAN_TMP, LAST_PLAN_PATH)
        });
    if let Err(err) = written {
        let mut warned_at = LAST_PLAN_WARNED_AT
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if warned_at.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(3600)) {
            *warned_at = Some(std::time::Instant::now());
            tracing::warn!("WinterReserve: cannot write {LAST_PLAN_PATH}: {err}");
        }
    }
}

/// Last plan and what it was built from (PRD "Re-plan triggers").
struct CachedPlan {
    plan: Plan,
    planned_at: DateTime<Utc>,
    horizon_last_start: DateTime<Utc>,
    price_fingerprint: u64,
}

/// The levers the planner reads. VPR and the kill switch are applied by execution, so they are
/// dropped: changing them does not change a plan.
pub fn planner_levers(levers: Option<&UserLevers>) -> Option<UserLevers> {
    levers.map(|l| UserLevers {
        vpr: None,
        automation_disabled: false,
        ..*l
    })
}

/// Hash of the horizon's `(start, buy, sell)`, prices rounded to 0.001 CZK, and of the planner
/// levers. Changes when prices or planner levers change.
fn price_fingerprint(horizon: &[PlanBlock], levers: Option<&UserLevers>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for b in horizon {
        b.start.hash(&mut hasher);
        ((b.buy_czk * 1000.0).round() as i64).hash(&mut hasher);
        ((b.sell_czk * 1000.0).round() as i64).hash(&mut hasher);
        // Flows at 0.1 kWh: a load profile or PV forecast arriving or changing re-plans at once.
        ((b.load_kwh * 10.0).round() as i64).hash(&mut hasher);
        ((b.pv_kwh * 10.0).round() as i64).hash(&mut hasher);
    }
    format!("{:?}", planner_levers(levers)).hash(&mut hasher);
    hasher.finish()
}

/// `EconomicStrategy` wrapper with the plan cache. See PRD "Strategy wrapper behaviour".
pub struct WinterReserveStrategy {
    enabled: bool,
    params: WinterReserveParams,
    clock: fn() -> DateTime<Utc>,
    cache: Mutex<Option<CachedPlan>>,
    /// Number of planner runs (tests check that cached calls do not re-plan).
    plan_count: AtomicUsize,
}

impl WinterReserveStrategy {
    pub const NAME: &'static str = "Winter-Reserve";

    pub fn new(enabled: bool, params: WinterReserveParams) -> Self {
        Self {
            enabled,
            params,
            clock: Utc::now,
            cache: Mutex::new(None),
            plan_count: AtomicUsize::new(0),
        }
    }

    /// Replace the wall clock (tests and replays).
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> DateTime<Utc>) -> Self {
        self.clock = clock;
        self
    }

    pub fn params(&self) -> &WinterReserveParams {
        &self.params
    }
}

fn battery_params(control: &ControlConfig) -> BatteryParams {
    let max_discharge_kw = control.max_battery_charge_rate_kw;
    let mode_change_kwh = max_discharge_kw * control.min_mode_change_interval_secs as f32 / 3600.0;
    BatteryParams {
        capacity_kwh: control.battery_capacity_kwh,
        min_soc_pct: control.min_battery_soc,
        max_soc_pct: control.max_battery_soc,
        max_charge_kw: control.max_battery_charge_rate_kw,
        max_discharge_kw,
        efficiency: control.battery_efficiency,
        wear_cost_czk_per_kwh: control.battery_wear_cost_czk_per_kwh,
        max_export_kw: control.maximum_export_power_w as f32 / 1000.0,
        export_guard_margin_kwh: mode_change_kwh.max(0.5),
        force_discharge_enabled: control.force_discharge_enabled,
    }
}

/// Morning discharge window: local hours 04:00–14:00.
fn in_morning_window(hour: u32) -> bool {
    (4..14).contains(&hour)
}

/// Evening discharge window: local hours 12:00–24:00.
fn in_evening_window(hour: u32) -> bool {
    hour >= 12
}

/// Indices of the blocks in `in_window` that are among the `quota` highest sell prices of their
/// local day (ties: earlier block first). Blocks that are forced to charge or must not discharge
/// cannot export, so they take no slot. `blocks` is every known block, not only the horizon.
fn export_eligible(
    blocks: &[PlanBlock],
    tz: Tz,
    in_window: fn(u32) -> bool,
    quota: usize,
) -> Vec<usize> {
    let mut ranked: Vec<(NaiveDate, usize)> = blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let local = b.start.with_timezone(&tz);
            let can_export = !b.force_charge && !b.no_discharge;
            (can_export && in_window(local.hour())).then(|| (local.date_naive(), i))
        })
        .collect();
    // Local day first, then highest sell price, then the earlier block.
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| blocks[b.1].sell_czk.total_cmp(&blocks[a.1].sell_czk))
            .then(a.1.cmp(&b.1))
    });
    let mut eligible = Vec::new();
    let mut day = None;
    let mut taken = 0;
    for (date, i) in ranked {
        if day != Some(date) {
            day = Some(date);
            taken = 0;
        }
        if taken < quota {
            eligible.push(i);
            taken += 1;
        }
    }
    eligible
}

/// A discharge window: its lever limit and its local-hour test.
type DischargeWindow = (Option<DischargeWindowLimit>, fn(u32) -> bool);

/// Price and discharge rules of the user levers (CONTRACT "Price levers", "Discharge levers").
/// `None` levers, or levers with no rule set, leave the blocks as the plain planner has them.
/// Discharge windows need a timezone; without one, export is unrestricted.
fn apply_user_levers(blocks: &mut [PlanBlock], levers: Option<&UserLevers>, tz: Option<Tz>) {
    let Some(levers) = levers else { return };
    for block in blocks.iter_mut() {
        block.force_charge_at_or_below_czk = levers.force_charge_at_or_below_czk;
        block.force_charge = levers
            .force_charge_at_or_below_czk
            .is_some_and(|at_or_below| block.buy_czk <= at_or_below);
        block.no_discharge = !block.force_charge
            && levers
                .no_discharge_at_or_below_czk
                .is_some_and(|at_or_below| block.buy_czk <= at_or_below);
    }

    let windows: [DischargeWindow; 2] = [
        (levers.morning_discharge, in_morning_window),
        (levers.evening_discharge, in_evening_window),
    ];
    if windows.iter().all(|(limit, _)| limit.is_none()) {
        return;
    }
    let Some(tz) = tz else { return };
    // With any window lever set, export is allowed only in the ranked blocks of a set window.
    for block in blocks.iter_mut() {
        block.export_allowed = false;
        block.export_min_soc_pct = None;
    }
    for (limit, in_window) in windows {
        let Some(limit) = limit else { continue };
        for i in export_eligible(blocks, tz, in_window, usize::from(limit.hours) * 4) {
            let block = &mut blocks[i];
            block.export_allowed = true;
            block.export_min_soc_pct = Some(
                block
                    .export_min_soc_pct
                    .map_or(limit.min_soc_pct, |pct| pct.max(limit.min_soc_pct)),
            );
        }
    }
}

fn inverter_mode(mode: PlanMode) -> InverterOperationMode {
    match mode {
        PlanMode::SelfUse => InverterOperationMode::SelfUse,
        PlanMode::Charge => InverterOperationMode::ForceCharge,
        PlanMode::Hold => InverterOperationMode::NoChargeNoDischarge,
        PlanMode::Export => InverterOperationMode::ForceDischarge,
        PlanMode::NoDischarge => InverterOperationMode::BackUpMode,
    }
}

impl WinterReserveStrategy {
    /// Plan blocks from the evaluated block to the end of the known horizon.
    /// Expected load: non-empty UTC-hour mean profile, else aligned flat forecast, else average load.
    /// Reserve load: max(mean, p75) times `planner_load_margin` when p75 is available, else expected
    /// times `load_safety_factor`. Expected PV: aligned forecast, else 0. Reserve PV: expected times
    /// `pv_confidence`. Forecasts are used only when aligned with `all_price_blocks`.
    /// The first entry is the evaluated block. User levers are applied to the blocks (see `apply_user_levers`).
    fn horizon_blocks(&self, context: &super::EvaluationContext) -> Vec<PlanBlock> {
        let block_start = context.price_block.block_start;
        let all = context
            .all_price_blocks
            .filter(|all| all.iter().any(|b| b.block_start == block_start))
            .unwrap_or(std::slice::from_ref(context.price_block));
        let start = all
            .iter()
            .position(|b| b.block_start == block_start)
            .unwrap_or(0);
        let forecasts = context.block_forecasts.filter(|f| f.len() == all.len());
        let control = context.control_config;
        let profile = context
            .hourly_consumption_profile
            .filter(|p| p.iter().sum::<f32>() > 0.0);
        let p75 = context.hourly_consumption_p75.filter(|_| profile.is_some());
        let mut blocks: Vec<PlanBlock> = all
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let hours = b.duration_minutes as f32 / 60.0;
                let hour = b.block_start.hour() as usize;
                // Expected load: UTC-hour mean profile if it has data, else flat block forecast, else
                // average load. The reserve load: with the p75 profile max(mean, p75) times the planner
                // margin, else the expected load times load_safety_factor.
                let (expected_load, reserve_load) = match (profile, p75) {
                    (Some(p), Some(q)) => (
                        p[hour] * hours,
                        p[hour].max(q[hour]) * hours * control.planner_load_margin,
                    ),
                    (Some(p), None) => (
                        p[hour] * hours,
                        p[hour] * hours * self.params.load_safety_factor,
                    ),
                    (None, _) => {
                        let base = match forecasts {
                            Some(f) => f[i].consumption_kwh,
                            None => control.average_household_load_kw * hours,
                        };
                        (base, base * self.params.load_safety_factor)
                    }
                };
                let pv = match forecasts {
                    Some(f) => f[i].solar_kwh,
                    None => 0.0,
                };
                PlanBlock {
                    start: b.block_start,
                    duration_minutes: b.duration_minutes,
                    buy_czk: b.effective_price_czk_per_kwh,
                    sell_czk: b
                        .spot_sell_price_czk_per_kwh
                        .unwrap_or(control.grid_export_fee_czk_per_kwh),
                    load_kwh: expected_load,
                    pv_kwh: pv,
                    reserve_load_kwh: reserve_load,
                    reserve_pv_kwh: pv * self.params.pv_confidence,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                }
            })
            .collect();
        // Levers rank over every known block, so the export set does not move as the evening passes.
        apply_user_levers(&mut blocks, context.user_levers, context.timezone);
        blocks.split_off(start)
    }

    fn run_plan(
        &self,
        horizon: &[PlanBlock],
        soc_pct: f32,
        tz: Tz,
        battery: &BatteryParams,
        current: Option<PlanMode>,
    ) -> Plan {
        self.plan_count.fetch_add(1, Ordering::Relaxed);
        plan_from(horizon, soc_pct, tz, battery, &self.params, current)
    }

    /// Planned entry for `horizon[0]`. Head calls reuse or refresh the cache; projection
    /// calls read the cache and, on a miss, plan without caching. `horizon` is never empty.
    /// `fingerprint` is `price_fingerprint` of the horizon (prices and flows) and levers.
    #[expect(clippy::too_many_arguments)]
    fn entry_for(
        &self,
        now: DateTime<Utc>,
        horizon: &[PlanBlock],
        soc_pct: f32,
        tz: Tz,
        battery: &BatteryParams,
        is_head: bool,
        fingerprint: u64,
    ) -> (PlannedBlock, Option<Plan>) {
        let first = horizon[0].start;
        let last = horizon[horizon.len() - 1].start;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());

        if !is_head {
            let hit = cache
                .as_ref()
                .and_then(|c| c.plan.blocks.iter().find(|b| b.start == first))
                .cloned();
            let entry = hit.unwrap_or_else(|| {
                self.run_plan(horizon, soc_pct, tz, battery, None)
                    .blocks
                    .swap_remove(0)
            });
            return (entry, None);
        }

        let stale = cache.as_ref().is_none_or(|c| {
            c.plan.blocks.first().map(|b| b.start) != Some(first)
                || c.horizon_last_start != last
                || c.price_fingerprint != fingerprint
                || now - c.planned_at > Duration::minutes(REPLAN_MAX_AGE_MIN)
                || c.plan
                    .blocks
                    .iter()
                    .find(|b| b.start == first)
                    .is_none_or(|b| {
                        let lo = b.soc_start_pct.min(b.soc_end_pct) - 5.0;
                        let hi = b.soc_start_pct.max(b.soc_end_pct) + 5.0;
                        soc_pct < lo || soc_pct > hi
                    })
        });
        let reusable = if stale {
            None
        } else {
            cache.as_ref().and_then(|c| c.plan.blocks.first()).cloned()
        };
        if let Some(entry) = reusable {
            return (entry, None);
        }

        // The head mode of the last plan (not read from the inverter). Overrides such as the kill
        // switch, VPR or fixed slots leave it unchanged, so the plan stays stable under them.
        let current = cache
            .as_ref()
            .filter(|c| now - c.planned_at <= Duration::minutes(REPLAN_MAX_AGE_MIN * 2))
            .and_then(|c| c.plan.blocks.first())
            .map(|b| b.mode);
        let plan = self.run_plan(horizon, soc_pct, tz, battery, current);
        let entry = plan.blocks[0].clone();
        let replanned = plan.clone();
        *cache = Some(CachedPlan {
            plan,
            planned_at: now,
            horizon_last_start: last,
            price_fingerprint: fingerprint,
        });
        (entry, Some(replanned))
    }
}

impl super::EconomicStrategy for WinterReserveStrategy {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn evaluate(&self, context: &super::EvaluationContext) -> super::BlockEvaluation {
        let now = (self.clock)();
        let tz = context.timezone.unwrap_or(chrono_tz::Europe::Prague);
        let battery = battery_params(context.control_config);
        let horizon = self.horizon_blocks(context);
        let block_start = context.price_block.block_start;
        let is_head = block_start <= now
            && now
                < block_start + Duration::minutes(i64::from(context.price_block.duration_minutes));
        let live_soc = context.current_battery_soc;
        let capacity = battery.capacity_kwh;
        let live_kwh = live_soc / 100.0 * capacity;

        let fingerprint = price_fingerprint(&horizon, context.user_levers);
        let (entry, replanned) =
            self.entry_for(now, &horizon, live_soc, tz, &battery, is_head, fingerprint);
        if let Some(plan) = &replanned {
            let inputs = PlanInputs {
                block_forecasts: context.block_forecasts.is_some(),
                profile: context.hourly_consumption_profile.is_some(),
            };
            log_replan(&horizon, plan, live_soc, inputs, tz);
        }
        let head = &horizon[0];

        // Runtime guards, head calls only.
        let mut mode = entry.mode;
        let mut reason = entry.reason.clone();
        if is_head
            && mode == PlanMode::Export
            && live_kwh <= entry.floor_kwh + battery.export_guard_margin_kwh
        {
            mode = PlanMode::SelfUse;
            reason = format!(
                "WinterReserve SELF-USE | guard: export held at night floor {:.1} kWh, SOC {live_soc:.0}%",
                entry.floor_kwh
            );
        } else if is_head && mode == PlanMode::Charge && live_soc >= battery.max_soc_pct - 1.0 {
            mode = PlanMode::SelfUse;
            reason = format!("WinterReserve SELF-USE | guard: charge held at SOC {live_soc:.0}%");
        }

        let start_kwh = if is_head {
            live_kwh
        } else {
            entry.soc_start_pct / 100.0 * capacity
        };
        let outcome = simulate_block(mode, start_kwh, head, entry.floor_kwh, &battery);

        let mut eval = BlockEvaluation::new(
            head.start,
            head.duration_minutes,
            inverter_mode(mode),
            self.name().to_string(),
        );
        eval.revenue_czk = outcome.grid_export_kwh * head.sell_czk;
        eval.cost_czk = outcome.grid_import_kwh * head.buy_czk
            + outcome.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
        eval.calculate_net_profit();
        eval.energy_flows = EnergyFlows {
            grid_import_kwh: outcome.grid_import_kwh,
            grid_export_kwh: outcome.grid_export_kwh,
            battery_charge_kwh: outcome.battery_charge_kwh,
            battery_discharge_kwh: outcome.battery_discharge_kwh,
            solar_generation_kwh: head.pv_kwh,
            household_consumption_kwh: head.load_kwh,
        };
        eval.assumptions = Assumptions {
            solar_forecast_kwh: head.pv_kwh,
            consumption_forecast_kwh: head.load_kwh,
            current_battery_soc: live_soc,
            battery_efficiency: battery.efficiency,
            battery_wear_cost_czk_per_kwh: battery.wear_cost_czk_per_kwh,
            grid_import_price_czk_per_kwh: head.buy_czk,
            grid_export_price_czk_per_kwh: head.sell_czk,
        };
        eval.reason = reason;
        eval
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::EconomicStrategy;
    use chrono::TimeZone;
    use fluxion_types::pricing::TimeBlockPrice;

    /// 2026-10-13 18:00 Europe/Prague (CEST)
    fn base() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 16, 0, 0).unwrap()
    }

    fn battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 96.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 30.0,
            max_discharge_kw: 30.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.125,
            max_export_kw: 10.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: true,
        }
    }

    /// 15-minute block i after 18:00 local, with the given prices and forecasts.
    fn pblock(i: i64, buy: f32, sell: f32, load: f32, pv: f32) -> PlanBlock {
        PlanBlock {
            start: base() + Duration::minutes(15 * i),
            duration_minutes: 15,
            buy_czk: buy,
            sell_czk: sell,
            load_kwh: load,
            pv_kwh: pv,
            reserve_load_kwh: load,
            reserve_pv_kwh: pv,
            force_charge: false,
            no_discharge: false,
            force_charge_at_or_below_czk: None,
            export_min_soc_pct: None,
            export_allowed: true,
        }
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// Night horizon 18:00 to 09:00 (60 blocks). Evening 18-22, night 22-06, morning 06-09
    /// with real sunrise (pv 2.0) from 06:00.
    fn night_blocks(
        evening: (f32, f32),
        night: (f32, f32),
        morning: (f32, f32),
        load: f32,
    ) -> Vec<PlanBlock> {
        (0..60)
            .map(|i| {
                let (buy, sell) = if i < 16 {
                    evening
                } else if i < 48 {
                    night
                } else {
                    morning
                };
                pblock(i, buy, sell, load, if i >= 48 { 2.0 } else { 0.0 })
            })
            .collect()
    }

    // ---- Simulator ----

    #[test]
    fn sim_selfuse_surplus_charges_and_exports_above_cap() {
        let b = battery();
        // 2 kWh pv, 0.5 load: surplus 1.5 kWh, all charged (room available)
        let o = simulate_block(
            PlanMode::SelfUse,
            50.0,
            &pblock(0, 3.0, 2.0, 0.5, 2.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 1.5));
        assert!(approx(o.energy_end_kwh, 50.0 + 1.5 * 0.9));
        assert!(approx(o.grid_export_kwh, 0.0));

        // 12 kWh pv, no load: charge limited to 7.5 kWh (30 kW * 0.25 h), export capped at 2.5 kWh
        let o = simulate_block(
            PlanMode::SelfUse,
            50.0,
            &pblock(0, 3.0, 2.0, 0.0, 12.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 7.5));
        assert!(approx(o.energy_end_kwh, 50.0 + 7.5 * 0.9));
        assert!(approx(o.grid_export_kwh, 2.5));
        assert!(approx(o.curtailed_kwh, 2.0));
    }

    #[test]
    fn sim_selfuse_charge_limited_by_emax() {
        let b = battery();
        let o = simulate_block(
            PlanMode::SelfUse,
            95.5,
            &pblock(0, 3.0, 2.0, 0.0, 5.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 96.0));
        assert!(approx(o.battery_charge_kwh, 0.5 / 0.9));
        assert!(approx(o.grid_export_kwh, 2.5));
        assert!(approx(o.curtailed_kwh, 5.0 - 0.5 / 0.9 - 2.5));
    }

    #[test]
    fn sim_selfuse_deficit_served_by_battery() {
        let b = battery();
        let o = simulate_block(
            PlanMode::SelfUse,
            50.0,
            &pblock(0, 3.0, 2.0, 1.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_discharge_kwh, 1.0));
        assert!(approx(o.energy_end_kwh, 49.0));
        assert!(approx(o.grid_import_kwh, 0.0));
    }

    #[test]
    fn sim_selfuse_deficit_clipped_by_emin_and_dmax() {
        let b = battery();
        // Only 0.4 kWh above Emin (9.6): remaining 0.6 imported
        let o = simulate_block(
            PlanMode::SelfUse,
            10.0,
            &pblock(0, 3.0, 2.0, 1.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 9.6));
        assert!(approx(o.grid_import_kwh, 0.6));
        // Deficit 10 kWh, dmax 7.5 kWh per block: 2.5 imported
        let o = simulate_block(
            PlanMode::SelfUse,
            50.0,
            &pblock(0, 3.0, 2.0, 10.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_discharge_kwh, 7.5));
        assert!(approx(o.grid_import_kwh, 2.5));
    }

    #[test]
    fn sim_charge_with_and_without_pv() {
        let b = battery();
        // Charge 7.5 kWh input with 1 kWh pv, 0.5 load: import 7.0
        let o = simulate_block(
            PlanMode::Charge,
            50.0,
            &pblock(0, 3.0, 2.0, 0.5, 1.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 7.5));
        assert!(approx(o.energy_end_kwh, 50.0 + 7.5 * 0.9));
        assert!(approx(o.grid_import_kwh, 7.0));
        assert!(approx(o.grid_export_kwh, 0.0));
        // Charge with 8 kWh pv, 0.5 load: grid balance 0.5 + 7.5 - 8 = 0
        let o = simulate_block(
            PlanMode::Charge,
            50.0,
            &pblock(0, 3.0, 2.0, 0.5, 8.0),
            9.6,
            &b,
        );
        assert!(approx(o.grid_import_kwh, 0.0));
        assert!(approx(o.grid_export_kwh, 0.0));
        // Charge with 12 kWh pv, no load: balance -4.5, export capped at 2.5, 2.0 curtailed
        let o = simulate_block(
            PlanMode::Charge,
            50.0,
            &pblock(0, 3.0, 2.0, 0.0, 12.0),
            9.6,
            &b,
        );
        assert!(approx(o.grid_export_kwh, 2.5));
        assert!(approx(o.curtailed_kwh, 2.0));
    }

    #[test]
    fn sim_hold_leaves_battery_untouched() {
        let b = battery();
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 1.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 50.0));
        assert!(approx(o.grid_import_kwh, 1.0));
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 1.0, 3.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 50.0));
        assert!(approx(o.grid_export_kwh, 2.0));
        assert!(approx(o.grid_import_kwh, 0.0));
    }

    #[test]
    fn sim_export_limited_by_floor_cap_and_dmax() {
        let b = battery();
        // Floor 40 with 50 stored, no load: the inverter throttles to the 2.5 kWh export cap
        let o = simulate_block(
            PlanMode::Export,
            50.0,
            &pblock(0, 3.0, 7.0, 0.0, 0.0),
            40.0,
            &b,
        );
        assert!(approx(o.battery_discharge_kwh, 2.5));
        assert!(approx(o.grid_export_kwh, 2.5));
        assert!(approx(o.curtailed_kwh, 0.0));
        // With 2 kWh of house load the battery serves the load plus the export cap
        let o = simulate_block(
            PlanMode::Export,
            50.0,
            &pblock(0, 3.0, 7.0, 2.0, 0.0),
            40.0,
            &b,
        );
        assert!(approx(o.battery_discharge_kwh, 4.5));
        assert!(approx(o.grid_export_kwh, 2.5));
        // Floor 40 with 42 stored: discharge stops at the floor, 2 kWh exported
        let o = simulate_block(
            PlanMode::Export,
            42.0,
            &pblock(0, 3.0, 7.0, 0.0, 0.0),
            40.0,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 40.0));
        assert!(approx(o.grid_export_kwh, 2.0));
        // dmax binds when export cap is large: 7.5 kWh discharged, 2 kW load, 5.5 exported
        let big = BatteryParams {
            max_export_kw: 30.0,
            ..battery()
        };
        let o = simulate_block(
            PlanMode::Export,
            90.0,
            &pblock(0, 3.0, 7.0, 2.0, 0.0),
            9.6,
            &big,
        );
        assert!(approx(o.battery_discharge_kwh, 7.5));
        assert!(approx(o.grid_export_kwh, 5.5));
        // Export never leaves the battery below Emin
        let o = simulate_block(
            PlanMode::Export,
            10.0,
            &pblock(0, 3.0, 7.0, 1.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 9.6));
        assert!(approx(o.grid_import_kwh, 0.6));
    }

    #[test]
    fn sim_battery_stays_within_bounds() {
        let b = battery();
        let (emin, emax) = b.bounds_kwh();
        let modes = [
            PlanMode::SelfUse,
            PlanMode::Hold,
            PlanMode::Charge,
            PlanMode::Export,
        ];
        for &mode in &modes {
            for step in 0..=20 {
                let e = emin + (emax - emin) * step as f32 / 20.0;
                for &(load, pv) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 12.0), (3.0, 3.0), (5.0, 1.0)] {
                    let o = simulate_block(mode, e, &pblock(0, 3.0, 2.0, load, pv), emin + 5.0, &b);
                    assert!(
                        o.energy_end_kwh >= emin - 1e-3 && o.energy_end_kwh <= emax + 1e-3,
                        "{mode:?} from {e} with load {load} pv {pv} ended at {}",
                        o.energy_end_kwh
                    );
                }
            }
        }
    }

    // ---- Planner ----

    #[test]
    fn night_coverage_no_export_charge_only_at_night() {
        let blocks = night_blocks((7.0, 6.0), (3.0, 2.0), (6.5, 5.5), 0.75);
        let p = plan(
            &blocks,
            20.0,
            chrono_tz::Europe::Prague,
            &battery(),
            &WinterReserveParams::default(),
        );
        assert!(p.blocks.iter().all(|b| b.mode != PlanMode::Export));
        for (i, b) in p.blocks.iter().enumerate() {
            if b.mode == PlanMode::Charge {
                assert!(
                    (16..48).contains(&i),
                    "charge at block {i} outside 22:00-06:00"
                );
            }
        }
        // Morning: battery covers the load, no grid import
        for b in &p.blocks[48..] {
            assert!(
                approx(b.outcome.grid_import_kwh, 0.0),
                "import {} in morning",
                b.outcome.grid_import_kwh
            );
        }
    }

    #[test]
    fn arbitrage_gate_passes_with_profit_and_blocks_without() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            max_export_kw: 30.0,
            ..battery()
        };
        // Evening buy 8.5, night 4, morning 6.5: cheap_ref 4, median 4, C1 threshold ~7.57
        let pass = night_blocks((8.5, 7.8), (4.0, 2.0), (6.5, 5.5), 0.75);
        let p = plan(&pass, 90.0, tz, &b, &params);
        assert!(approx(p.cheap_ref_czk, 4.0));
        let exports: Vec<&PlannedBlock> = p
            .blocks
            .iter()
            .filter(|x| x.mode == PlanMode::Export)
            .collect();
        assert!(!exports.is_empty(), "expected evening export at sell 7.8");
        let emin = b.bounds_kwh().0;
        for x in exports {
            assert!(x.floor_kwh > emin);
            assert!(x.outcome.energy_end_kwh >= x.floor_kwh - 1e-3);
        }

        let fail = night_blocks((8.5, 7.0), (4.0, 2.0), (6.5, 5.5), 0.75);
        let p = plan(&fail, 90.0, tz, &b, &params);
        assert!(p.blocks.iter().all(|x| x.mode != PlanMode::Export));
    }

    #[test]
    fn insane_price_drops_floor_to_emin() {
        let tz = chrono_tz::Europe::Prague;
        let b = BatteryParams {
            max_export_kw: 30.0,
            ..battery()
        };
        let blocks = night_blocks((8.5, 8.2), (4.0, 2.0), (6.5, 5.5), 0.75);
        let params = WinterReserveParams {
            insane_price_multiplier: 2.0,
            ..WinterReserveParams::default()
        };
        let p = plan(&blocks, 90.0, tz, &b, &params);
        let emin = b.bounds_kwh().0;
        // Evening blocks sell at 8.2 >= 2 * cheap_ref (4): floor is Emin
        for x in &p.blocks[..16] {
            assert!(
                approx(x.floor_kwh, emin),
                "floor {} at {}",
                x.floor_kwh,
                x.start
            );
        }
        let exports: Vec<&PlannedBlock> = p
            .blocks
            .iter()
            .filter(|x| x.mode == PlanMode::Export)
            .collect();
        assert!(!exports.is_empty(), "expected export at insane sell price");
    }

    #[test]
    fn floor_covers_deficit_until_morning_block() {
        let tz = chrono_tz::Europe::Prague;
        let b = battery();
        // Load 0.75 per block everywhere, pv 0 until 08:00 (block 56), then pv 2.0 (covers load)
        let blocks: Vec<PlanBlock> = (0..60)
            .map(|i| pblock(i, 3.0, 2.0, 0.75, if i >= 56 { 2.0 } else { 0.0 }))
            .collect();
        let p = plan(&blocks, 50.0, tz, &b, &WinterReserveParams::default());
        // 20:00 is block 8: floor = Emin + deficits from 20:15 (block 9) to 07:45 (block 55)
        let expected: f32 = b.bounds_kwh().0
            + blocks[9..56]
                .iter()
                .map(|x| (x.load_kwh - x.pv_kwh).max(0.0))
                .sum::<f32>();
        assert!(
            approx(p.blocks[8].floor_kwh, expected),
            "{} vs {expected}",
            p.blocks[8].floor_kwh
        );
    }

    #[test]
    fn short_midday_dip_is_not_a_night() {
        // Surplus all day, 1 h deficit (4 blocks) at 12:00: no morning block
        let blocks: Vec<PlanBlock> = (0..40)
            .map(|i| {
                pblock(
                    i,
                    3.0,
                    2.0,
                    0.75,
                    if (16..20).contains(&i) { 0.0 } else { 2.0 },
                )
            })
            .collect();
        assert!(
            morning_blocks(&blocks, chrono_tz::Europe::Prague)
                .iter()
                .all(|m| !m)
        );
    }

    #[test]
    fn thirteen_hour_night_gives_morning_block() {
        // 13 h deficit (52 blocks), then pv covers load from 08:00
        let blocks: Vec<PlanBlock> = (0..70)
            .map(|i| pblock(i, 3.0, 2.0, 0.75, if i >= 52 { 2.0 } else { 0.0 }))
            .collect();
        let flags = morning_blocks(&blocks, chrono_tz::Europe::Prague);
        assert!(flags[52]);
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
    }

    #[test]
    fn horizon_starting_in_deficit_counts_the_night_before_it() {
        // Horizon starts 05:15 Europe/Prague, 2 h before sunrise: blocks 0..8 are deficit, PV
        // covers the load from block 8 to 31, then the horizon ends in an open night.
        let dawn = Utc.with_ymd_and_hms(2026, 10, 16, 3, 15, 0).unwrap();
        let blocks: Vec<PlanBlock> = (0..40)
            .map(|i| {
                let mut b = pblock(
                    i,
                    3.0,
                    2.0,
                    0.75,
                    if (8..32).contains(&i) { 2.0 } else { 0.0 },
                );
                b.start = dawn + Duration::minutes(15 * i);
                b
            })
            .collect();
        let flags = morning_blocks(&blocks, chrono_tz::Europe::Prague);
        assert!(flags[8]);
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
        let b = battery();
        let (emin, emax) = b.bounds_kwh();
        let floors = night_floors(
            &blocks,
            &b,
            0.0,
            &WinterReserveParams::default(),
            chrono_tz::Europe::Prague,
        );
        // Block 0 reserves the deficits of blocks 1..8, up to the morning block.
        let expected: f32 = emin
            + blocks[1..8]
                .iter()
                .map(|x| (x.reserve_load_kwh - x.reserve_pv_kwh).max(0.0))
                .sum::<f32>();
        assert!(approx(floors[0], expected), "{} vs {expected}", floors[0]);
        assert!(floors[0] < emax);
    }

    #[test]
    fn sunny_insufficient_head_does_not_count_a_pre_horizon_night() {
        // Midday head with real but insufficient PV (0.5 < load 0.75), then 1 h of no sun, then
        // PV covers the load to the end: the run before the covered block is short, so no morning.
        let blocks: Vec<PlanBlock> = (0..12)
            .map(|i| {
                let pv = match i {
                    0 => 0.5,
                    1..=4 => 0.0,
                    _ => 2.0,
                };
                pblock(i, 3.0, 2.0, 0.75, pv)
            })
            .collect();
        assert!(
            morning_blocks(&blocks, chrono_tz::Europe::Prague)
                .iter()
                .all(|m| !m)
        );
        let b = battery();
        let (emin, _) = b.bounds_kwh();
        let floors = night_floors(
            &blocks,
            &b,
            0.0,
            &WinterReserveParams::default(),
            chrono_tz::Europe::Prague,
        );
        // No morning and no open night: block 0 reserves every deficit after it.
        let expected: f32 = emin
            + blocks[1..]
                .iter()
                .map(|x| (x.reserve_load_kwh - x.reserve_pv_kwh).max(0.0))
                .sum::<f32>();
        assert!(approx(floors[0], expected), "{} vs {expected}", floors[0]);
    }

    #[test]
    fn no_sun_daytime_head_does_not_count_a_pre_horizon_night() {
        // A 08:00, 09:00 or 11:00 Europe/Prague head with 0.01 kWh PV (no real sun, e.g. fog or a
        // forecast gap), blocks 1..4 deficit, PV covers from block 4 to 7, an 18 h night (blocks
        // 8..80), then the next morning at 80. Only the next morning may be flagged; the floor keeps
        // every deficit up to it, so the night reserve cannot be exported during the dip.
        for utc_hour in [6, 7, 9] {
            let head = Utc.with_ymd_and_hms(2026, 10, 16, utc_hour, 0, 0).unwrap(); // CEST = UTC+2
            let blocks: Vec<PlanBlock> = (0..84)
                .map(|i| {
                    let pv = match i {
                        0 => 0.01,
                        1..=3 | 8..=79 => 0.0,
                        _ => 2.0, // 4..=7 and 80..
                    };
                    let mut b = pblock(i, 3.0, 2.0, 0.75, pv);
                    b.start = head + Duration::minutes(15 * i);
                    b
                })
                .collect();
            let tz = chrono_tz::Europe::Prague;
            let flags = morning_blocks(&blocks, tz);
            assert_eq!(
                flags.iter().filter(|m| **m).count(),
                1,
                "head {utc_hour}:00 UTC"
            );
            assert!(flags[80], "head {utc_hour}:00 UTC");
            let b = battery();
            let (emin, _) = b.bounds_kwh();
            let floors = night_floors(&blocks, &b, 0.0, &WinterReserveParams::default(), tz);
            // Pre-fix value: block 0 reserves every deficit from block 1 to the morning at block 80.
            let expected: f32 = emin
                + blocks[1..80]
                    .iter()
                    .map(|x| (x.reserve_load_kwh - x.reserve_pv_kwh).max(0.0))
                    .sum::<f32>();
            assert!(approx(floors[0], expected), "{} vs {expected}", floors[0]);
            assert!(floors[0] > 50.0);
        }
    }

    #[test]
    fn horizon_starting_in_sun_still_needs_a_four_hour_night() {
        // Sunny head, then 2 h of deficit (blocks 1..9): not a night, no morning.
        let short: Vec<PlanBlock> = (0..30)
            .map(|i| pblock(i, 3.0, 2.0, 0.75, if i == 0 || i >= 9 { 2.0 } else { 0.0 }))
            .collect();
        assert!(
            morning_blocks(&short, chrono_tz::Europe::Prague)
                .iter()
                .all(|m| !m)
        );
        // Sunny head, then 4 h of deficit (blocks 1..17): the first sunny block after it is the morning.
        let long: Vec<PlanBlock> = (0..40)
            .map(|i| pblock(i, 3.0, 2.0, 0.75, if i == 0 || i >= 17 { 2.0 } else { 0.0 }))
            .collect();
        let flags = morning_blocks(&long, chrono_tz::Europe::Prague);
        assert!(flags[17]);
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
    }

    #[test]
    fn floor_covers_horizon_capped_at_emax_without_morning() {
        let tz = chrono_tz::Europe::Prague;
        let b = battery();
        let (emin, emax) = b.bounds_kwh();
        // Heavy load, no pv: deficit sum exceeds the battery, so the floor is capped at Emax
        let heavy = night_blocks((7.0, 5.0), (3.0, 2.0), (6.5, 5.5), 3.0);
        let p = plan(&heavy, 50.0, tz, &b, &WinterReserveParams::default());
        assert!(approx(p.blocks[0].floor_kwh, emax));
        // Light load, horizon ends in an unfinished night (last block deficit): reserve all of Emax
        let light: Vec<PlanBlock> = (0..60).map(|i| pblock(i, 3.0, 2.0, 0.5, 0.0)).collect();
        let p = plan(&light, 50.0, tz, &b, &WinterReserveParams::default());
        assert!(approx(p.blocks[0].floor_kwh, emax));
        // Same light load, but the horizon ends in surplus: the old rule (Emin + deficits) applies
        let surplus_end: Vec<PlanBlock> = (0..40)
            .map(|i| {
                pblock(
                    i,
                    3.0,
                    2.0,
                    0.75,
                    if (16..20).contains(&i) { 0.0 } else { 2.0 },
                )
            })
            .collect();
        let p = plan(&surplus_end, 50.0, tz, &b, &WinterReserveParams::default());
        assert!(approx(p.blocks[0].floor_kwh, emin + 4.0 * 0.75));
    }

    #[test]
    fn flat_prices_keep_selfuse() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = battery();
        // Flat buy 3.0 / sell 2.5, deficit 0.75 kWh per block
        let deficit: Vec<PlanBlock> = (0..40).map(|i| pblock(i, 3.0, 2.5, 0.75, 0.0)).collect();
        let p = plan(&deficit, 50.0, tz, &b, &params);
        assert!(
            p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse),
            "{:?}",
            p.blocks.iter().map(|x| x.mode).collect::<Vec<_>>()
        );
        // A heavy deficit (2.5 kWh per block) must not make Hold win on wear alone
        let heavy: Vec<PlanBlock> = (0..40).map(|i| pblock(i, 3.0, 2.5, 2.5, 0.0)).collect();
        let p = plan(&heavy, 90.0, tz, &b, &params);
        assert!(
            p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse),
            "{:?}",
            p.blocks.iter().map(|x| x.mode).collect::<Vec<_>>()
        );
        // Flat prices, surplus 0.5 kWh per block
        let surplus: Vec<PlanBlock> = (0..40).map(|i| pblock(i, 3.0, 2.5, 0.5, 1.0)).collect();
        let p = plan(&surplus, 50.0, tz, &b, &params);
        assert!(
            p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse),
            "{:?}",
            p.blocks.iter().map(|x| x.mode).collect::<Vec<_>>()
        );
    }

    #[test]
    fn plan_is_deterministic() {
        let blocks = night_blocks((7.0, 6.0), (3.0, 2.0), (6.5, 5.5), 0.75);
        let tz = chrono_tz::Europe::Prague;
        let a = plan(
            &blocks,
            60.0,
            tz,
            &battery(),
            &WinterReserveParams::default(),
        );
        let b = plan(
            &blocks,
            60.0,
            tz,
            &battery(),
            &WinterReserveParams::default(),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn plan_192_blocks_is_fast() {
        let blocks: Vec<PlanBlock> = (0..192)
            .map(|i| {
                let phase = (i % 96) as f32 / 96.0 * std::f32::consts::TAU;
                let buy = 3.0 + 4.0 * phase.sin().abs();
                let pv = if (24..72).contains(&(i % 96)) {
                    1.5
                } else {
                    0.0
                };
                pblock(i, buy, buy - 0.5, 0.6, pv)
            })
            .collect();
        let start = std::time::Instant::now();
        let p = plan(
            &blocks,
            50.0,
            chrono_tz::Europe::Prague,
            &battery(),
            &WinterReserveParams::default(),
        );
        let elapsed = start.elapsed();
        assert_eq!(p.blocks.len(), 192);
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "plan took {elapsed:?}"
        );
    }

    // ---- Wrapper ----

    fn control() -> ControlConfig {
        ControlConfig {
            // Planner export tests model the enabled state (force discharge is opt-in)
            force_discharge_enabled: true,
            battery_capacity_kwh: 96.0,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            max_battery_charge_rate_kw: 30.0,
            battery_efficiency: 0.9,
            battery_wear_cost_czk_per_kwh: 0.125,
            maximum_export_power_w: 10_000,
            grid_export_fee_czk_per_kwh: 0.5,
            ..ControlConfig::default()
        }
    }

    fn price_blocks() -> Vec<TimeBlockPrice> {
        (0..60)
            .map(|i: i64| {
                let (buy, sell) = if i < 16 {
                    (7.0, 6.0)
                } else if i < 48 {
                    (3.0, 2.0)
                } else {
                    (6.5, 5.5)
                };
                TimeBlockPrice {
                    buy_price_czk_per_kwh: None,
                    block_start: base() + Duration::minutes(15 * i),
                    duration_minutes: 15,
                    price_czk_per_kwh: buy,
                    effective_price_czk_per_kwh: buy,
                    spot_sell_price_czk_per_kwh: Some(sell),
                }
            })
            .collect()
    }

    fn ctx<'a>(
        blocks: &'a [TimeBlockPrice],
        index: usize,
        control: &'a ControlConfig,
        soc: f32,
    ) -> super::super::EvaluationContext<'a> {
        super::super::EvaluationContext {
            price_block: &blocks[index],
            control_config: control,
            current_battery_soc: soc,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.25,
            grid_export_price_czk_per_kwh: 0.5,
            all_price_blocks: Some(blocks),
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 3.0,
            hourly_consumption_profile: None,
            hourly_consumption_p75: None,
            block_forecasts: None,
            timezone: None,
            user_levers: None,
        }
    }

    /// Clock inside block 0 (18:05 local).
    fn clock_block0() -> DateTime<Utc> {
        base() + Duration::minutes(5)
    }

    fn strategy() -> WinterReserveStrategy {
        WinterReserveStrategy::new(true, WinterReserveParams::default()).with_clock(clock_block0)
    }

    #[test]
    fn head_calls_plan_once_then_reuse_cache() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        let first = s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        let second = s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 1);
        assert_eq!(first.mode, second.mode);
        assert!(first.reason.starts_with("WinterReserve"));
        assert!(second.reason.starts_with("WinterReserve"));
    }

    #[test]
    fn projection_returns_cached_block_without_replanning() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        let cached = s.cache.lock().unwrap().as_ref().unwrap().plan.blocks[5].mode;
        let eval = s.evaluate(&ctx(&blocks, 5, &ctl, 50.0));
        assert_eq!(eval.mode, inverter_mode(cached));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn projection_miss_plans_without_caching() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        let eval = s.evaluate(&ctx(&blocks, 5, &ctl, 50.0));
        assert!(eval.reason.starts_with("WinterReserve"));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 1);
        assert!(s.cache.lock().unwrap().is_none());
    }

    #[test]
    fn head_call_replans_when_soc_drifts_out_of_band() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        let (lo, hi) = {
            let guard = s.cache.lock().unwrap();
            let entry = &guard.as_ref().unwrap().plan.blocks[0];
            (
                entry.soc_start_pct.min(entry.soc_end_pct) - 5.0,
                entry.soc_start_pct.max(entry.soc_end_pct) + 5.0,
            )
        };
        let drifted = if hi + 10.0 <= 100.0 {
            hi + 10.0
        } else {
            lo - 10.0
        };
        s.evaluate(&ctx(&blocks, 0, &ctl, drifted));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 2);
    }

    /// Seed the cache with one hand-made entry for block 0 (fresh, with the current horizon).
    fn seed(s: &WinterReserveStrategy, blocks: &[TimeBlockPrice], entry: PlannedBlock) {
        let fingerprint =
            price_fingerprint(&s.horizon_blocks(&ctx(blocks, 0, &control(), 50.0)), None);
        *s.cache.lock().unwrap() = Some(CachedPlan {
            plan: Plan {
                blocks: vec![entry],
                cheap_ref_czk: 3.0,
                median_buy_czk: 3.0,
                expected_cost_czk: 0.0,
            },
            planned_at: clock_block0(),
            horizon_last_start: blocks.last().unwrap().block_start,
            price_fingerprint: fingerprint,
        });
    }

    fn entry(mode: PlanMode, soc_start: f32, soc_end: f32, floor: f32) -> PlannedBlock {
        PlannedBlock {
            start: base(),
            mode,
            soc_start_pct: soc_start,
            soc_end_pct: soc_end,
            floor_kwh: floor,
            outcome: BlockOutcome::default(),
            cost_czk: 0.0,
            reason: "WinterReserve TEST".to_string(),
        }
    }

    #[test]
    fn load_uses_utc_hour_profile_first() {
        let blocks = price_blocks();
        let ctl = control();
        let mut profile = [0.5_f32; 24];
        profile[16] = 2.0; // 16:00 UTC = 18:00 Prague, block 0
        let mut c = ctx(&blocks, 0, &ctl, 50.0);
        c.hourly_consumption_profile = Some(&profile);
        c.block_forecasts = None;
        let horizon = strategy().horizon_blocks(&c);
        // Expected: 2.0 kWh/h * 0.25 h, no safety factor
        assert!(approx(horizon[0].load_kwh, 2.0 * 0.25));
        assert!(approx(horizon[4].load_kwh, 0.5 * 0.25)); // 17:00 UTC
        // Reserve: 2.0 kWh/h * 0.25 h * load_safety_factor 1.10
        assert!(approx(horizon[0].reserve_load_kwh, 2.0 * 0.25 * 1.10));
        assert!(approx(horizon[4].reserve_load_kwh, 0.5 * 0.25 * 1.10)); // 17:00 UTC
        assert!(approx(horizon[0].pv_kwh, 0.0));
    }

    #[test]
    fn load_uses_p75_with_planner_margin_when_available() {
        let blocks = price_blocks();
        let mut ctl = control();
        ctl.planner_load_margin = 1.2;
        let mut profile = [0.5_f32; 24];
        profile[16] = 2.0;
        let mut p75 = [0.5_f32; 24];
        p75[16] = 3.0;
        let mut c = ctx(&blocks, 0, &ctl, 50.0);
        c.hourly_consumption_profile = Some(&profile);
        c.hourly_consumption_p75 = Some(&p75);
        c.block_forecasts = None;
        let horizon = strategy().horizon_blocks(&c);
        // Expected: mean 2.0 kWh/h * 0.25 h, no margin
        assert!(approx(horizon[0].load_kwh, 2.0 * 0.25));
        assert!(approx(horizon[4].load_kwh, 0.5 * 0.25)); // 17:00 UTC
        // Reserve: max(mean, p75) = 3.0 kWh/h * 0.25 h * planner_load_margin 1.2
        assert!(approx(horizon[0].reserve_load_kwh, 3.0 * 0.25 * 1.2));
        assert!(approx(horizon[4].reserve_load_kwh, 0.5 * 0.25 * 1.2)); // 17:00 UTC
    }

    /// Local hour (0..24) of block `i` of the big solar day: 05:00 of day 0, plus 15 min per block.
    fn solar_day_hour(i: usize) -> f32 {
        (5.0 + i as f32 * 0.25) % 24.0
    }

    /// Buy price (CZK/kWh) at local hour `h`: 05:00–06:30 cheap, a morning peak, a midday valley,
    /// an evening peak, then a cheaper night.
    fn solar_day_buy(h: f32) -> f32 {
        if h < 5.0 {
            6.5
        } else if h < 6.5 {
            5.9
        } else if h < 10.0 {
            7.5 + (h - 6.5) / 3.5 * 2.5
        } else if h < 17.0 {
            6.0 + (h - 10.0) / 7.0 * 1.7
        } else if h < 21.0 {
            7.8 + (h - 17.0) / 4.0 * 1.0
        } else {
            6.2 + (h - 21.0) / 3.0 * 0.6
        }
    }

    /// Unscaled PV bell from 07:30 to 17:30, peaking at 12:30.
    fn solar_bell(h: f32) -> f32 {
        if (7.5..17.5).contains(&h) {
            (-(h - 12.5).powi(2) / (2.0 * 2.6_f32.powi(2))).exp()
        } else {
            0.0
        }
    }

    /// The big solar day: 172 blocks from 05:00 local on day 0 to 23:45 local on day 1. Actual PV
    /// is the bell scaled to 160 kWh on day 0 and 110 kWh on day 1. Expected PV is actual PV times
    /// `expected_pv_factor`; reserve PV is actual PV times `pv_confidence`.
    fn solar_day_blocks(expected_pv_factor: f32) -> Vec<PlanBlock> {
        let start = Utc.with_ymd_and_hms(2026, 10, 16, 3, 0, 0).unwrap(); // 05:00 CEST
        let params = WinterReserveParams::default();
        let day_of = |i: usize| usize::from(i >= 76);
        let day_kwh = [160.0_f32, 110.0];
        let mut day_weight = [0.0_f32; 2];
        for i in 0..172 {
            day_weight[day_of(i)] += solar_bell(solar_day_hour(i));
        }
        (0..172)
            .map(|i| {
                let h = solar_day_hour(i);
                let actual_pv = solar_bell(h) / day_weight[day_of(i)] * day_kwh[day_of(i)];
                PlanBlock {
                    start: start + Duration::minutes(15 * i as i64),
                    duration_minutes: 15,
                    buy_czk: solar_day_buy(h),
                    sell_czk: solar_day_buy(h) - 2.4,
                    load_kwh: 0.75,
                    pv_kwh: actual_pv * expected_pv_factor,
                    reserve_load_kwh: 0.75 * params.load_safety_factor,
                    reserve_pv_kwh: actual_pv * params.pv_confidence,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                }
            })
            .collect()
    }

    #[test]
    fn big_solar_day_does_not_grid_charge_beyond_the_morning_bridge() {
        let blocks = solar_day_blocks(1.0);
        let b = BatteryParams {
            min_soc_pct: 12.0,
            ..battery()
        };
        let p = plan(
            &blocks,
            12.0,
            chrono_tz::Europe::Prague,
            &b,
            &WinterReserveParams::default(),
        );
        // Morning bridge: 3 kW from 06:30 until expected PV covers the load, plus a 2 kWh margin.
        let first = (0..76)
            .find(|&i| solar_day_hour(i) >= 6.5)
            .expect("06:30 is in the horizon");
        let cover = (first..76)
            .find(|&i| blocks[i].pv_kwh >= blocks[i].load_kwh)
            .expect("PV covers the load on day 0");
        let bridge_kwh = 3.0 * (solar_day_hour(cover) - solar_day_hour(first)) + 2.0;
        let charged: f32 = (0..76)
            .filter(|&i| solar_day_hour(i) < 10.0 && p.blocks[i].mode == PlanMode::Charge)
            .map(|i| p.blocks[i].outcome.battery_charge_kwh)
            .sum();
        assert!(
            charged <= bridge_kwh,
            "charge before 10:00 {charged:.2} kWh > bridge bound {bridge_kwh:.2} kWh"
        );
        let peak_soc = (0..76)
            .filter(|&i| (8.0..18.0).contains(&solar_day_hour(i)))
            .map(|i| p.blocks[i].soc_end_pct)
            .fold(0.0_f32, f32::max);
        assert!(peak_soc >= 95.0, "peak daytime SOC {peak_soc:.1} % < 95 %");
    }

    /// The live decision at 2026-10-16 03:00 UTC on a real 96 kWh site (75 blocks).
    #[derive(serde::Deserialize)]
    struct OverbuyFixture {
        battery_kwh: f32,
        min_soc_pct: f32,
        soc_pct: f32,
        max_charge_kw: f32,
        load_mean_kwh_per_h_utc: Vec<f32>,
        load_p75_kwh_per_h_utc: Vec<f32>,
        blocks: Vec<OverbuyBlock>,
    }

    #[derive(serde::Deserialize)]
    struct OverbuyBlock {
        start_utc: String,
        buy: f32,
        sell: f32,
        pv_forecast_kwh: f32,
        pv_actual_kwh: Option<f32>,
    }

    /// One 15-min slot. `pv_actual_kwh` is None where not yet measured.
    struct Slot {
        start: DateTime<Utc>,
        buy: f32,
        sell: f32,
        pv_forecast_kwh: f32,
        pv_actual_kwh: Option<f32>,
    }

    /// Which PV the planner sees: the forecast as made, the forecast × 1.44 (recalibrated), or the
    /// actual PV where known and the recalibrated forecast after that.
    #[derive(Clone, Copy)]
    enum OverbuyPv {
        Forecast,
        Recalibrated,
        ActualElseRecalibrated,
    }

    fn overbuy_fixture() -> OverbuyFixture {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/winter_reserve_overbuy_96kwh.json"
        ))
        .expect("overbuy fixture parses")
    }

    fn overbuy_control(f: &OverbuyFixture) -> ControlConfig {
        ControlConfig {
            battery_capacity_kwh: f.battery_kwh,
            min_battery_soc: f.min_soc_pct,
            max_battery_charge_rate_kw: f.max_charge_kw,
            ..control()
        }
    }

    /// The fixture's 75 slots. With `extend_next_day`, adds the 22:00–03:00 gap (the fixture's
    /// first prices, zero PV) and a copy of the fixture shifted by 24 h.
    fn overbuy_slots(f: &OverbuyFixture, extend_next_day: bool) -> Vec<Slot> {
        let mut slots: Vec<Slot> = f
            .blocks
            .iter()
            .map(|raw| Slot {
                start: raw.start_utc.parse().expect("start_utc"),
                buy: raw.buy,
                sell: raw.sell,
                pv_forecast_kwh: raw.pv_forecast_kwh,
                pv_actual_kwh: raw.pv_actual_kwh,
            })
            .collect();
        if !extend_next_day {
            return slots;
        }
        let first = slots[0].start;
        let last = slots[slots.len() - 1].start;
        let gap = (first + Duration::days(1) - last).num_minutes() / 15 - 1;
        for j in 0..gap {
            let src = &f.blocks[j as usize % f.blocks.len()];
            slots.push(Slot {
                start: last + Duration::minutes(15 * (j + 1)),
                buy: src.buy,
                sell: src.sell,
                pv_forecast_kwh: 0.0,
                pv_actual_kwh: Some(0.0),
            });
        }
        let copy: Vec<Slot> = f
            .blocks
            .iter()
            .map(|raw| Slot {
                start: raw.start_utc.parse::<DateTime<Utc>>().expect("start_utc")
                    + Duration::days(1),
                buy: raw.buy,
                sell: raw.sell,
                pv_forecast_kwh: raw.pv_forecast_kwh,
                pv_actual_kwh: None,
            })
            .collect();
        slots.extend(copy);
        slots
    }

    fn planner_pv(slot: &Slot, pv: OverbuyPv) -> f32 {
        match pv {
            OverbuyPv::Forecast => slot.pv_forecast_kwh,
            OverbuyPv::Recalibrated => slot.pv_forecast_kwh * 1.44,
            OverbuyPv::ActualElseRecalibrated => {
                slot.pv_actual_kwh.unwrap_or(slot.pv_forecast_kwh * 1.44)
            }
        }
    }

    /// Planner horizon from slot `start` to the end of `slots`, built by `horizon_blocks` as
    /// production builds it. `reserve_as_expected` gives the old single-view planner.
    fn overbuy_horizon_from(
        f: &OverbuyFixture,
        slots: &[Slot],
        start: usize,
        pv: OverbuyPv,
        reserve_as_expected: bool,
    ) -> Vec<PlanBlock> {
        let mean: [f32; 24] = f
            .load_mean_kwh_per_h_utc
            .clone()
            .try_into()
            .expect("24 means");
        let p75: [f32; 24] = f
            .load_p75_kwh_per_h_utc
            .clone()
            .try_into()
            .expect("24 p75 values");
        let prices: Vec<TimeBlockPrice> = slots
            .iter()
            .map(|s| TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: s.start,
                duration_minutes: 15,
                price_czk_per_kwh: s.buy,
                effective_price_czk_per_kwh: s.buy,
                spot_sell_price_czk_per_kwh: Some(s.sell),
            })
            .collect();
        let forecasts: Vec<fluxion_plugins::protocol::BlockForecast> = slots
            .iter()
            .map(|s| fluxion_plugins::protocol::BlockForecast {
                solar_kwh: planner_pv(s, pv),
                consumption_kwh: 0.0,
            })
            .collect();
        let control = overbuy_control(f);
        let mut c = ctx(&prices, start, &control, f.soc_pct);
        c.hourly_consumption_profile = Some(&mean);
        c.hourly_consumption_p75 = Some(&p75);
        c.block_forecasts = Some(&forecasts);
        c.timezone = Some(chrono_tz::Europe::Prague);
        let mut blocks = strategy().horizon_blocks(&c);
        if reserve_as_expected {
            for b in &mut blocks {
                b.load_kwh = b.reserve_load_kwh;
                b.pv_kwh = b.reserve_pv_kwh;
            }
        }
        blocks
    }

    /// Plans the fixture horizon once and prints the planned grid charge. Asserts nothing yet
    /// beyond the fixture loading; run with `--nocapture`.
    fn overbuy_report(
        label: &str,
        f: &OverbuyFixture,
        pv: OverbuyPv,
        reserve_as_expected: bool,
        extend_next_day: bool,
    ) {
        let slots = overbuy_slots(f, extend_next_day);
        let blocks = overbuy_horizon_from(f, &slots, 0, pv, reserve_as_expected);
        let p = plan_from(
            &blocks,
            f.soc_pct,
            chrono_tz::Europe::Prague,
            &battery_params(&overbuy_control(f)),
            &WinterReserveParams::default(),
            None,
        );
        let (mut charge, mut grid, mut cost, mut in_fixture) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        let mut first = None;
        for (i, (x, b)) in p.blocks.iter().zip(&blocks).enumerate() {
            if x.mode == PlanMode::Charge {
                let c = x.outcome.battery_charge_kwh;
                let g = c.min(x.outcome.grid_import_kwh);
                charge += c;
                grid += g;
                cost += g * b.buy_czk;
                if i < f.blocks.len() {
                    in_fixture += c;
                }
                first.get_or_insert(x.start);
            }
        }
        let soc_at_fixture_end = p.blocks[f.blocks.len() - 1].soc_end_pct;
        eprintln!(
            "H20 {label}: planned charge {charge:.2} kWh (grid {grid:.2} kWh, {cost:.2} CZK), \
             within fixture horizon {in_fixture:.2} kWh, first charge {first:?}, \
             SOC at last fixture block {soc_at_fixture_end:.1} %"
        );
    }

    /// Closed loop over the fixture, as the live site re-plans every 15 min: at each fixture block
    /// plan from the live SOC over the remaining horizon, apply only the first block to reality
    /// (`pv_actual` where known, else forecast × 1.44; load = mean profile × 1.0). Prints the first
    /// 12 steps and the run totals. Asserts nothing; run with `--nocapture`.
    fn overbuy_closed_loop(
        label: &str,
        f: &OverbuyFixture,
        extend_next_day: bool,
        reserve_as_expected: bool,
    ) {
        use chrono::Timelike;
        let slots = overbuy_slots(f, extend_next_day);
        let control = overbuy_control(f);
        let battery = battery_params(&control);
        let params = WinterReserveParams::default();
        let capacity = battery.capacity_kwh;
        let (_, emax) = battery.bounds_kwh();
        let mean: [f32; 24] = f
            .load_mean_kwh_per_h_utc
            .clone()
            .try_into()
            .expect("24 means");
        let tz = chrono_tz::Europe::Prague;
        let mut soc = f.soc_pct;
        let mut applied: Option<PlanMode> = None;
        let (mut charge, mut grid_kwh, mut grid_czk) = (0.0_f32, 0.0_f32, 0.0_f32);
        let (mut import_kwh, mut import_czk, mut export_kwh, mut export_czk) =
            (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        for k in 0..f.blocks.len() {
            let horizon =
                overbuy_horizon_from(f, &slots, k, OverbuyPv::Forecast, reserve_as_expected);
            let plan = plan_from(&horizon, soc, tz, &battery, &params, applied);
            let first = plan.blocks[0].clone();
            let planned_charge: f32 = plan
                .blocks
                .iter()
                .filter(|x| x.mode == PlanMode::Charge)
                .map(|x| x.outcome.battery_charge_kwh)
                .sum();
            let floor_last = plan.blocks.last().map_or(0.0, |x| x.floor_kwh);
            let s = &slots[k];
            let pv_real = s.pv_actual_kwh.unwrap_or(s.pv_forecast_kwh * 1.44);
            let load_real = mean[s.start.hour() as usize] * 0.25;
            let real = PlanBlock {
                start: s.start,
                duration_minutes: 15,
                buy_czk: s.buy,
                sell_czk: s.sell,
                load_kwh: load_real,
                pv_kwh: pv_real,
                reserve_load_kwh: load_real,
                reserve_pv_kwh: pv_real,
                force_charge: false,
                no_discharge: false,
                force_charge_at_or_below_czk: None,
                export_min_soc_pct: None,
                export_allowed: true,
            };
            let out = simulate_block(
                first.mode,
                soc / 100.0 * capacity,
                &real,
                first.floor_kwh,
                &battery,
            );
            let grid_charge = out.battery_charge_kwh.min(out.grid_import_kwh);
            charge += out.battery_charge_kwh;
            grid_kwh += grid_charge;
            grid_czk += grid_charge * s.buy;
            import_kwh += out.grid_import_kwh;
            import_czk += out.grid_import_kwh * s.buy;
            export_kwh += out.grid_export_kwh;
            export_czk += out.grid_export_kwh * s.sell;
            let soc_end = out.energy_end_kwh / capacity * 100.0;
            if k < 12 {
                let tag = if floor_last >= emax - 0.01 {
                    " (=emax)"
                } else {
                    ""
                };
                eprintln!(
                    "H20 {label} k={k:2} {} {:?} SOC {soc:5.1}->{soc_end:5.1} | floor(last) {floor_last:6.2}{tag} | plan charge {planned_charge:5.2} | real pv {pv_real:5.2} load {load_real:4.2} | charge {:5.2} import {:5.2} export {:5.2}",
                    s.start.format("%H:%M"),
                    first.mode,
                    out.battery_charge_kwh,
                    out.grid_import_kwh,
                    out.grid_export_kwh,
                );
            }
            applied = Some(first.mode);
            soc = soc_end;
        }
        eprintln!(
            "H20 {label} TOTAL: charge {charge:.2} kWh | grid charge {grid_kwh:.2} kWh = {grid_czk:.2} CZK | import {import_kwh:.2} kWh = {import_czk:.2} CZK | export {export_kwh:.2} kWh = {export_czk:.2} CZK | final SOC {soc:.1} %"
        );
    }

    // Diagnostic printout of the real fixture, not an assertion. Run with:
    //   cargo test -p fluxion-core --lib real_fixture -- --ignored --nocapture
    #[test]
    #[ignore = "diagnostic printout of the real fixture; see the comment above"]
    fn real_fixture_overbuy_numbers() {
        let f = overbuy_fixture();
        assert_eq!(f.blocks.len(), 75);
        assert_eq!(f.load_mean_kwh_per_h_utc.len(), 24);
        assert_eq!(f.load_p75_kwh_per_h_utc.len(), 24);
        overbuy_report("A old single-view", &f, OverbuyPv::Forecast, true, false);
        overbuy_report("B new split", &f, OverbuyPv::Forecast, false, false);
        // Expected load is the mean ×1.0 already, so this is B again by construction.
        overbuy_report(
            "B-i expected load mean x1.0",
            &f,
            OverbuyPv::Forecast,
            false,
            false,
        );
        overbuy_report(
            "C new split, PV x1.44",
            &f,
            OverbuyPv::Recalibrated,
            false,
            false,
        );
        overbuy_report(
            "D new split, actual PV else x1.44",
            &f,
            OverbuyPv::ActualElseRecalibrated,
            false,
            false,
        );
        overbuy_report(
            "B-ii B + next-day copy",
            &f,
            OverbuyPv::Forecast,
            false,
            true,
        );
    }

    // Diagnostic printout of the real fixture, not an assertion. Run with:
    //   cargo test -p fluxion-core --lib real_fixture -- --ignored --nocapture
    #[test]
    #[ignore = "diagnostic printout of the real fixture; see the comment above"]
    fn real_fixture_closed_loop_numbers() {
        let f = overbuy_fixture();
        overbuy_closed_loop("A old single-view", &f, false, true);
        overbuy_closed_loop("B new split", &f, false, false);
        overbuy_closed_loop("B2 new split + next day", &f, true, false);
    }

    /// Head 03:15 UTC (05:15 local) on the real fixture, with the forecast PV and the mean and p75
    /// profiles, SOC 19.5 %. With the pre-horizon night credit the morning is recognised and the
    /// head floor stays low.
    /// The fixture does NOT reproduce the live 44 kWh grid charge: the replay plans one 7.5 kWh
    /// block. The live cause is under investigation via the `WinterReserve horizon:` diagnostics and
    /// `/data/winter_reserve_last_plan.json`.
    #[test]
    fn real_fixture_head_0315_recognises_the_morning() {
        let f = overbuy_fixture();
        let slots = overbuy_slots(&f, false);
        let blocks = overbuy_horizon_from(&f, &slots, 0, OverbuyPv::Forecast, false);
        let tz = chrono_tz::Europe::Prague;
        assert!(morning_blocks(&blocks, tz).iter().any(|m| *m));
        let plan = plan_from(
            &blocks,
            19.5,
            tz,
            &battery_params(&overbuy_control(&f)),
            &WinterReserveParams::default(),
            None,
        );
        let head_floor = plan.blocks[0].floor_kwh;
        assert!(head_floor < 40.0, "head floor {head_floor:.2} kWh");
    }

    #[test]
    fn guard_export_falls_back_to_selfuse_at_floor() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        // Live SOC 41.6% = 39.9 kWh, floor 40 kWh: 39.9 <= 40.5, so the export is held
        seed(&s, &blocks, entry(PlanMode::Export, 45.0, 40.0, 40.0));
        let eval = s.evaluate(&ctx(&blocks, 0, &ctl, 41.6));
        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert!(eval.reason.starts_with("WinterReserve"));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn export_kept_above_floor() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        // Live 41.6% = 39.9 kWh, floor 30 kWh: well above the floor, export stays
        seed(&s, &blocks, entry(PlanMode::Export, 45.0, 40.0, 30.0));
        let eval = s.evaluate(&ctx(&blocks, 0, &ctl, 41.6));
        assert_eq!(eval.mode, InverterOperationMode::ForceDischarge);
    }

    #[test]
    fn guard_charge_falls_back_to_selfuse_near_max() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        seed(&s, &blocks, entry(PlanMode::Charge, 95.0, 99.6, 9.6));
        let eval = s.evaluate(&ctx(&blocks, 0, &ctl, 99.5));
        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        assert!(eval.reason.starts_with("WinterReserve"));
    }

    #[test]
    fn zero_load_no_sun_block_before_sunrise_keeps_night_run() {
        // Deficit 18:00-06:00 UTC, then a zero-load block with no sun at 05:30 UTC (block 54),
        // sunrise at 06:00 UTC (block 56). The no-sun block must not restart the 4 h run.
        let blocks: Vec<PlanBlock> = (0..60)
            .map(|i| {
                let (load, pv) = if i == 54 {
                    (0.0, 0.0)
                } else if i >= 56 {
                    (0.75, 2.0)
                } else {
                    (0.75, 0.0)
                };
                pblock(i, 3.0, 2.0, load, pv)
            })
            .collect();
        let flags = morning_blocks(&blocks, chrono_tz::Europe::Prague);
        assert!(flags[56], "sunrise must still be morning");
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
    }

    #[test]
    fn zero_load_block_at_night_is_not_morning() {
        // Night 18:00-06:00 UTC with pv 0 and load 0.75. Block 28 (23:00 UTC) has load 0, pv 0.
        // Real sunrise at 06:00 UTC (block 56) covers the load.
        let blocks: Vec<PlanBlock> = (0..72)
            .map(|i| {
                let (load, pv) = if i == 28 {
                    (0.0, 0.0)
                } else if i >= 56 {
                    (0.75, 2.0)
                } else {
                    (0.75, 0.0)
                };
                pblock(i, 3.0, 2.0, load, pv)
            })
            .collect();
        let flags = morning_blocks(&blocks, chrono_tz::Europe::Prague);
        assert!(!flags[28], "zero-load night block must not be morning");
        assert!(flags[56]);
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
        // The floor at 20:00 UTC (block 8) still covers the deficits up to real sunrise
        let floors = night_floors(
            &blocks,
            &battery(),
            3.0,
            &WinterReserveParams::default(),
            chrono_tz::Europe::Prague,
        );
        let expected: f32 = battery().bounds_kwh().0
            + blocks[9..56]
                .iter()
                .map(|x| (x.load_kwh - x.pv_kwh).max(0.0))
                .sum::<f32>();
        assert!(approx(floors[8], expected), "{} vs {expected}", floors[8]);
    }

    #[test]
    fn planner_holds_mid_blocks_before_peak_when_battery_is_short() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        // 3 kWh above the floor (10 kWh battery at 40 %, floor 10 %). 2 h at 6 CZK, then a 2 h
        // peak at 8 CZK. Load 3 kW = 0.75 kWh per 15-min block, no PV.
        let b = BatteryParams {
            capacity_kwh: 10.0,
            ..battery()
        };
        let mut blocks: Vec<PlanBlock> = (0..8).map(|i| pblock(i, 6.0, 2.0, 0.75, 0.0)).collect();
        blocks.extend((8..16).map(|i| pblock(i, 8.0, 2.0, 0.75, 0.0)));
        let p = plan(&blocks, 40.0, tz, &b, &params);
        let modes: Vec<PlanMode> = p.blocks.iter().map(|x| x.mode).collect();
        assert!(modes[..8].contains(&PlanMode::Hold), "{modes:?}");
        assert!(modes[8..].iter().any(|m| *m != PlanMode::Hold), "{modes:?}");
    }

    fn mode_changes(modes: &[PlanMode]) -> usize {
        modes.windows(2).filter(|w| w[0] != w[1]).count()
    }

    #[test]
    fn floor_flat_prices_do_not_churn_modes() {
        // Battery at the floor (energy = Emin), 8 h horizon of flat-ish prices (3.0 / 3.2 CZK), no
        // PV. SelfUse and Hold leave the battery identical here, so the mode must not flip.
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            ..battery()
        };
        let blocks: Vec<PlanBlock> = (0..32)
            .map(|i| pblock(i, if i % 2 == 0 { 3.0 } else { 3.2 }, 2.0, 0.4, 0.0))
            .collect();
        let p = plan(&blocks, 10.0, tz, &b, &params);
        let modes: Vec<PlanMode> = p.blocks.iter().map(|x| x.mode).collect();
        assert!(mode_changes(&modes) <= 1, "{modes:?}");
    }

    #[test]
    fn floor_keeps_the_current_mode_when_outcomes_are_identical() {
        // At the floor SelfUse and Hold give the same flows, so the applied mode is kept.
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            ..battery()
        };
        let blocks: Vec<PlanBlock> = (0..4).map(|i| pblock(i, 3.0, 2.0, 0.4, 0.0)).collect();
        let kept = plan_from(&blocks, 10.0, tz, &b, &params, Some(PlanMode::Hold));
        assert_eq!(kept.blocks[0].mode, PlanMode::Hold);
        let default = plan_from(&blocks, 10.0, tz, &b, &params, None);
        assert_eq!(default.blocks[0].mode, PlanMode::SelfUse);
    }

    #[test]
    fn floor_with_pv_forecast_does_not_keep_hold() {
        // Hold blocks PV charging on SolaX; with any PV forecast a tie must go to SelfUse.
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            ..battery()
        };
        let blocks: Vec<PlanBlock> = (0..4).map(|i| pblock(i, 3.0, 2.0, 0.4, 0.1)).collect();
        let p = plan_from(&blocks, 10.0, tz, &b, &params, Some(PlanMode::Hold));
        assert_eq!(p.blocks[0].mode, PlanMode::SelfUse);
    }

    /// Block 0 has PV 0.3 kWh, then 15 blocks without PV; `load` is the load of every block.
    fn pv_block_zero_plan(load: f32, previous: PlanMode) -> PlanMode {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 10.0,
            ..battery()
        };
        let mut blocks: Vec<PlanBlock> = (0..8).map(|i| pblock(i, 6.0, 2.0, load, 0.0)).collect();
        blocks.extend((8..16).map(|i| pblock(i, 8.0, 2.0, load, 0.0)));
        blocks[0].pv_kwh = 0.3;
        let p = plan_from(&blocks, 40.0, tz, &b, &params, Some(previous));
        p.blocks[0].mode
    }

    #[test]
    fn pv_block_keeps_applied_selfuse_when_hold_gain_is_small() {
        // Block 0 has PV. Hold gains 0.95 CZK at block 0, under the 1.0 CZK switch threshold:
        // SelfUse is kept, since keeping SelfUse never blocks PV charging.
        assert_eq!(
            pv_block_zero_plan(1.5, PlanMode::SelfUse),
            PlanMode::SelfUse
        );
    }

    #[test]
    fn pv_block_switches_to_hold_when_gain_is_above_threshold() {
        // Block 0 has PV. Hold gains 1.19 CZK at block 0, above the 1.0 CZK switch threshold
        assert_eq!(pv_block_zero_plan(2.0, PlanMode::SelfUse), PlanMode::Hold);
    }

    #[test]
    fn small_hold_benefit_keeps_applied_selfuse() {
        // The H8 scenario with the previous block SelfUse. Hold gains only 0.59 CZK at block 0,
        // under the 1.0 CZK switch threshold, so the applied SelfUse is kept.
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 10.0,
            ..battery()
        };
        let mut blocks: Vec<PlanBlock> = (0..8).map(|i| pblock(i, 6.0, 2.0, 0.75, 0.0)).collect();
        blocks.extend((8..16).map(|i| pblock(i, 8.0, 2.0, 0.75, 0.0)));
        let p = plan_from(&blocks, 40.0, tz, &b, &params, Some(PlanMode::SelfUse));
        let modes: Vec<PlanMode> = p.blocks.iter().map(|x| x.mode).collect();
        assert_eq!(modes[0], PlanMode::SelfUse, "{modes:?}");
    }

    #[test]
    fn hold_still_chosen_over_previous_selfuse_when_better_by_more_than_switch_threshold() {
        // Load 2.0 kWh per block, buy 6.0 then 8.0: Hold gains 1.58 CZK at block 0 over the applied
        // SelfUse, above the 1.0 CZK switch threshold, so Hold is chosen
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 10.0,
            ..battery()
        };
        let mut blocks: Vec<PlanBlock> = (0..8).map(|i| pblock(i, 6.0, 2.0, 2.0, 0.0)).collect();
        blocks.extend((8..16).map(|i| pblock(i, 8.0, 2.0, 2.0, 0.0)));
        let p = plan_from(&blocks, 40.0, tz, &b, &params, Some(PlanMode::SelfUse));
        let modes: Vec<PlanMode> = p.blocks.iter().map(|x| x.mode).collect();
        assert!(modes[..8].contains(&PlanMode::Hold), "{modes:?}");
    }

    /// Candidate with a physical outcome of its own, so the same-outcome rule never applies.
    fn cand(mode: PlanMode, value: f32) -> (PlanMode, f32, BlockOutcome) {
        let outcome = BlockOutcome {
            grid_import_kwh: mode as u8 as f32,
            ..BlockOutcome::default()
        };
        (mode, value, outcome)
    }

    #[test]
    fn mode_switch_keeps_selfuse_when_hold_is_better_by_less_than_one_czk() {
        let evals = [cand(PlanMode::SelfUse, 10.0), cand(PlanMode::Hold, 9.5)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::SelfUse));
        assert_eq!(mode, PlanMode::SelfUse);
    }

    #[test]
    fn mode_switch_takes_hold_when_better_by_more_than_one_czk() {
        let evals = [cand(PlanMode::SelfUse, 10.0), cand(PlanMode::Hold, 8.5)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::SelfUse));
        assert_eq!(mode, PlanMode::Hold);
    }

    #[test]
    fn mode_switch_keeps_hold_when_selfuse_is_better_by_less_than_one_czk() {
        let evals = [cand(PlanMode::SelfUse, 9.5), cand(PlanMode::Hold, 10.0)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::Hold));
        assert_eq!(mode, PlanMode::Hold);
    }

    #[test]
    fn mode_switch_takes_selfuse_when_better_by_more_than_one_czk() {
        let evals = [cand(PlanMode::SelfUse, 8.5), cand(PlanMode::Hold, 10.0)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::Hold));
        assert_eq!(mode, PlanMode::SelfUse);
    }

    #[test]
    fn mode_switch_never_holds_charge_over_selfuse() {
        // Charge is 0.6 CZK better (above the 0.10 floor by 0.5): it wins despite the hysteresis
        let evals = [cand(PlanMode::SelfUse, 10.0), cand(PlanMode::Charge, 9.4)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::SelfUse));
        assert_eq!(mode, PlanMode::Charge);
    }

    #[test]
    fn mode_switch_hysteresis_is_zero_after_block_zero() {
        // The forward pass passes the switch benefit only for block 0; later blocks pass 0.
        // Previous Hold, SelfUse 0.5 CZK better: the switch happens with 0, and Hold stays with 1.0.
        let evals = [cand(PlanMode::SelfUse, 9.5), cand(PlanMode::Hold, 10.0)];
        let (mode, _, _) = select_mode(&evals, 0.10, 0.0, Some(PlanMode::Hold));
        assert_eq!(mode, PlanMode::SelfUse);
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, Some(PlanMode::Hold));
        assert_eq!(mode, PlanMode::Hold);
    }

    #[test]
    fn mode_switch_without_keep_is_unchanged() {
        let evals = [cand(PlanMode::SelfUse, 10.0), cand(PlanMode::Hold, 9.5)];
        let (mode, _, _) = select_mode(&evals, 0.10, 1.0, None);
        assert_eq!(mode, PlanMode::Hold);
        let (mode, _, _) = select_mode(&evals, 0.10, 0.0, Some(PlanMode::SelfUse));
        assert_eq!(mode, PlanMode::Hold);
    }

    #[test]
    fn missing_spot_sell_uses_export_fee_not_price_minus_fee() {
        let mut blocks = price_blocks();
        for b in &mut blocks {
            // Spot 8 CZK, no spot sell price. Buy is cheap so that the export gate is the only blocker.
            b.price_czk_per_kwh = 8.0;
            b.effective_price_czk_per_kwh = 1.0;
            b.spot_sell_price_czk_per_kwh = None;
        }
        let ctl = control();
        let horizon = strategy().horizon_blocks(&ctx(&blocks, 0, &ctl, 90.0));
        assert!(horizon.iter().all(|b| approx(b.sell_czk, 0.5)));
        let p = plan(
            &horizon,
            90.0,
            chrono_tz::Europe::Prague,
            &battery(),
            &WinterReserveParams::default(),
        );
        assert!(p.blocks.iter().all(|x| x.mode != PlanMode::Export));
    }

    #[test]
    fn insane_guard_needs_positive_cheap_ref() {
        // Free power everywhere: cheap_ref is 0, so no sell price counts as insane.
        let blocks: Vec<PlanBlock> = (0..60).map(|i| pblock(i, 0.0, 0.5, 0.5, 0.0)).collect();
        let b = battery();
        let floors = night_floors(
            &blocks,
            &b,
            0.0,
            &WinterReserveParams::default(),
            chrono_tz::Europe::Prague,
        );
        // Open night (last block is a deficit): the floor is Emax, not Emin
        assert!(approx(floors[0], b.bounds_kwh().1), "floor {}", floors[0]);
    }

    #[test]
    fn replan_when_buy_prices_change_without_block_change() {
        let ctl = control();
        let s = strategy();
        let blocks = price_blocks();
        s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 1);
        // HDO arrives: night buy prices drop by 1.3 CZK, same blocks and same horizon end
        let mut cheaper = price_blocks();
        for b in &mut cheaper[16..48] {
            b.price_czk_per_kwh -= 1.3;
            b.effective_price_czk_per_kwh -= 1.3;
        }
        s.evaluate(&ctx(&cheaper, 0, &ctl, 50.0));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn export_guard_margin_limits_simulated_export() {
        let b = BatteryParams {
            export_guard_margin_kwh: 2.5,
            ..battery()
        };
        // Floor 40 + margin 2.5 = 42.5: discharge stops there, 1.5 kWh exported
        let o = simulate_block(
            PlanMode::Export,
            44.0,
            &pblock(0, 3.0, 7.0, 0.0, 0.0),
            40.0,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 42.5));
        assert!(approx(o.grid_export_kwh, 1.5));
    }

    #[test]
    fn head_guard_holds_export_inside_margin() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        // Floor 40 kWh, margin 2.5 kWh (30 kW * 300 s): live 42.0 kWh is inside the margin
        seed(&s, &blocks, entry(PlanMode::Export, 45.0, 40.0, 40.0));
        let eval = s.evaluate(&ctx(&blocks, 0, &ctl, 42.0 / 96.0 * 100.0));
        assert_eq!(eval.mode, InverterOperationMode::SelfUse);
        // Live 43.2 kWh is above floor + margin: export stays
        seed(&s, &blocks, entry(PlanMode::Export, 45.0, 40.0, 40.0));
        let eval = s.evaluate(&ctx(&blocks, 0, &ctl, 45.0));
        assert_eq!(eval.mode, InverterOperationMode::ForceDischarge);
    }

    #[test]
    fn invalid_prices_or_battery_give_selfuse_plan() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let mut blocks: Vec<PlanBlock> = (0..10).map(|i| pblock(i, 3.0, 7.0, 0.5, 0.0)).collect();
        blocks[3].sell_czk = f32::NAN;
        let p = plan(&blocks, 50.0, tz, &battery(), &params);
        assert_eq!(p.blocks.len(), 10);
        assert!(p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse
            && x.reason == "WinterReserve SELF-USE | invalid price data"));

        let good: Vec<PlanBlock> = (0..10).map(|i| pblock(i, 3.0, 7.0, 0.5, 0.0)).collect();
        let bad_range = BatteryParams {
            min_soc_pct: 100.0,
            max_soc_pct: 100.0,
            ..battery()
        };
        let p = plan(&good, 50.0, tz, &bad_range, &params);
        assert!(p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse
            && x.reason == "WinterReserve SELF-USE | invalid battery config"));
        let no_capacity = BatteryParams {
            capacity_kwh: 0.0,
            ..battery()
        };
        let p = plan(&good, 50.0, tz, &no_capacity, &params);
        assert!(p.blocks.iter().all(|x| x.mode == PlanMode::SelfUse
            && x.reason == "WinterReserve SELF-USE | invalid battery config"));
    }

    // ---- User levers ----

    /// Price levers and discharge windows, planned from the wrapper's horizon.
    fn lever_ctx<'a>(
        blocks: &'a [TimeBlockPrice],
        ctl: &'a ControlConfig,
        levers: &'a UserLevers,
    ) -> super::super::EvaluationContext<'a> {
        let mut c = ctx(blocks, 0, ctl, 50.0);
        c.user_levers = Some(levers);
        c.timezone = Some(chrono_tz::Europe::Prague);
        c
    }

    #[test]
    fn empty_levers_plan_the_same_as_no_levers() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let bat = battery_params(&ctl);
        let empty = UserLevers::default();
        let mut with_empty = ctx(&blocks, 0, &ctl, 50.0);
        with_empty.user_levers = Some(&empty);
        with_empty.timezone = Some(tz);
        let mut without = ctx(&blocks, 0, &ctl, 50.0);
        without.timezone = Some(tz);
        let none_plan = plan(&s.horizon_blocks(&without), 50.0, tz, &bat, &params);
        let empty_plan = plan(&s.horizon_blocks(&with_empty), 50.0, tz, &bat, &params);
        assert_eq!(none_plan, empty_plan);
    }

    #[test]
    fn force_charge_lever_charges_blocks_above_median() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        let tz = chrono_tz::Europe::Prague;
        // Evening 7.0 and morning 6.5 are above the median (3.0); the lever at 7.0 covers every block.
        let levers = UserLevers {
            force_charge_at_or_below_czk: Some(7.0),
            ..UserLevers::default()
        };
        let horizon = s.horizon_blocks(&lever_ctx(&blocks, &ctl, &levers));
        assert!(horizon.iter().all(|b| b.force_charge));
        let p = plan(
            &horizon,
            50.0,
            tz,
            &battery_params(&ctl),
            &WinterReserveParams::default(),
        );
        for (i, b) in p.blocks.iter().enumerate() {
            assert_eq!(b.mode, PlanMode::Charge, "block {i}");
            assert!(
                b.reason
                    .starts_with("WinterReserve CHARGE (price lever ≤ 7.00)"),
                "{}",
                b.reason
            );
        }
    }

    #[test]
    fn no_discharge_lever_never_discharges() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        // Cheap blocks 0..20 carry the lever; deficit (load 0.5, no sun) everywhere.
        let blocks: Vec<PlanBlock> = (0..40)
            .map(|i| {
                let buy = if i < 20 { 2.0 } else { 5.0 };
                let mut b = pblock(i, buy, 1.0, 0.5, 0.0);
                b.no_discharge = i < 20;
                b
            })
            .collect();
        let p = plan(&blocks, 50.0, tz, &battery(), &params);
        let capacity = battery().capacity_kwh;
        for (i, b) in p.blocks.iter().enumerate().take(20) {
            assert!(
                matches!(b.mode, PlanMode::NoDischarge | PlanMode::Charge),
                "block {i}: {:?}",
                b.mode
            );
            assert!(b.outcome.battery_discharge_kwh.abs() < 1e-6, "block {i}");
            let start_kwh = b.soc_start_pct / 100.0 * capacity;
            assert!(b.outcome.energy_end_kwh >= start_kwh - 1e-3, "block {i}");
        }
    }

    #[test]
    fn evening_window_one_hour_exports_top_four_blocks_above_min_soc() {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let blocks = prices_rising_evening();
        let ctl = control();
        let levers = UserLevers {
            evening_discharge: Some(DischargeWindowLimit {
                hours: 1,
                min_soc_pct: 30.0,
            }),
            ..UserLevers::default()
        };
        let horizon = WinterReserveStrategy::new(true, params)
            .horizon_blocks(&lever_ctx(&blocks, &ctl, &levers));
        // Evening blocks 0..24 (18:00-24:00); the four highest sells are 20..24.
        let allowed: Vec<usize> = horizon
            .iter()
            .enumerate()
            .filter(|(_, b)| b.export_allowed)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(allowed, vec![20, 21, 22, 23]);
        assert!(
            horizon
                .iter()
                .all(|b| b.export_min_soc_pct.is_none() || b.export_allowed)
        );
        assert!(
            horizon[20..24]
                .iter()
                .all(|b| b.export_min_soc_pct == Some(30.0))
        );

        let p = plan(&horizon, 100.0, tz, &battery_params(&ctl), &params);
        let exports: Vec<&PlannedBlock> = p
            .blocks
            .iter()
            .filter(|b| b.mode == PlanMode::Export)
            .collect();
        assert!(!exports.is_empty());
        for (i, b) in p.blocks.iter().enumerate() {
            if b.mode == PlanMode::Export {
                assert!((20..24).contains(&i), "export at block {i}");
                assert!(
                    b.soc_end_pct >= 30.0 - 1e-3,
                    "block {i} SOC {}",
                    b.soc_end_pct
                );
            }
        }
    }

    #[test]
    fn zero_hours_in_both_windows_means_no_export() {
        let tz = chrono_tz::Europe::Prague;
        let blocks = prices_rising_evening();
        let ctl = control();
        let levers = UserLevers {
            morning_discharge: Some(DischargeWindowLimit {
                hours: 0,
                min_soc_pct: 30.0,
            }),
            evening_discharge: Some(DischargeWindowLimit {
                hours: 0,
                min_soc_pct: 30.0,
            }),
            ..UserLevers::default()
        };
        let horizon = WinterReserveStrategy::new(true, WinterReserveParams::default())
            .horizon_blocks(&lever_ctx(&blocks, &ctl, &levers));
        assert!(horizon.iter().all(|b| !b.export_allowed));
        let p = plan(
            &horizon,
            100.0,
            tz,
            &battery_params(&ctl),
            &WinterReserveParams::default(),
        );
        assert!(p.blocks.iter().all(|b| b.mode != PlanMode::Export));
    }

    #[test]
    fn window_levers_without_timezone_leave_export_unrestricted() {
        let blocks = prices_rising_evening();
        let ctl = control();
        let levers = UserLevers {
            evening_discharge: Some(DischargeWindowLimit {
                hours: 0,
                min_soc_pct: 30.0,
            }),
            ..UserLevers::default()
        };
        let mut c = lever_ctx(&blocks, &ctl, &levers);
        c.timezone = None;
        let horizon =
            WinterReserveStrategy::new(true, WinterReserveParams::default()).horizon_blocks(&c);
        assert!(
            horizon
                .iter()
                .all(|b| b.export_allowed && b.export_min_soc_pct.is_none())
        );
    }

    #[test]
    fn no_discharge_simulator_balances_energy() {
        let bat = battery();
        // Deficit: the grid covers it, the battery stays put.
        let deficit = pblock(0, 3.0, 2.0, 1.0, 0.25);
        let o = simulate_block(PlanMode::NoDischarge, 50.0, &deficit, 9.6, &bat);
        assert!(approx(o.energy_end_kwh, 50.0));
        assert!(approx(o.battery_discharge_kwh, 0.0));
        assert!(approx(o.grid_import_kwh, 0.75));
        assert!(approx(o.grid_export_kwh, 0.0));

        // Surplus: PV charges the battery as in SelfUse; nothing is discharged.
        let surplus = pblock(0, 3.0, 2.0, 0.25, 1.0);
        let o = simulate_block(PlanMode::NoDischarge, 50.0, &surplus, 9.6, &bat);
        assert!(approx(o.battery_charge_kwh, 0.75));
        assert!(approx(o.energy_end_kwh, 50.0 + 0.75 * bat.efficiency));
        assert!(approx(o.grid_export_kwh, 0.0));
        // Energy in (PV + import) equals energy out (load + export + curtailed + charge).
        for (b, o) in [
            (
                deficit,
                simulate_block(PlanMode::NoDischarge, 50.0, &deficit, 9.6, &bat),
            ),
            (surplus, o),
        ] {
            assert!(approx(
                b.pv_kwh + o.grid_import_kwh,
                b.load_kwh + o.grid_export_kwh + o.curtailed_kwh + o.battery_charge_kwh
            ));
        }
    }

    /// Horizon 18:00 to 09:00 local. Evening sell rises from 5.0 by 0.1 per block; buy stays cheap.
    fn prices_rising_evening() -> Vec<TimeBlockPrice> {
        (0..60)
            .map(|i: i64| TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: base() + Duration::minutes(15 * i),
                duration_minutes: 15,
                price_czk_per_kwh: 1.0,
                effective_price_czk_per_kwh: 1.0,
                spot_sell_price_czk_per_kwh: Some(if i < 24 { 5.0 + i as f32 * 0.1 } else { 2.0 }),
            })
            .collect()
    }

    #[test]
    fn export_set_is_the_same_from_any_evening_block() {
        let blocks = prices_rising_evening();
        let ctl = control();
        let tz = chrono_tz::Europe::Prague;
        let levers = UserLevers {
            evening_discharge: Some(DischargeWindowLimit {
                hours: 1,
                min_soc_pct: 30.0,
            }),
            ..UserLevers::default()
        };
        let s = WinterReserveStrategy::new(true, WinterReserveParams::default());
        let sets: Vec<Vec<DateTime<Utc>>> = (0..10)
            .map(|index| {
                let mut c = ctx(&blocks, index, &ctl, 50.0);
                c.user_levers = Some(&levers);
                c.timezone = Some(tz);
                s.horizon_blocks(&c)
                    .iter()
                    .filter(|b| b.export_allowed)
                    .map(|b| b.start)
                    .collect()
            })
            .collect();
        assert_eq!(sets[0].len(), 4);
        assert!(sets.iter().all(|set| set == &sets[0]));
    }

    #[test]
    fn forced_block_takes_no_export_slot() {
        // Block 23 has the highest sell but is forced to charge (buy 0.5 <= 1.0); others buy at 2.0.
        let blocks: Vec<TimeBlockPrice> = (0..60)
            .map(|i: i64| TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: base() + Duration::minutes(15 * i),
                duration_minutes: 15,
                price_czk_per_kwh: if i == 23 { 0.5 } else { 2.0 },
                effective_price_czk_per_kwh: if i == 23 { 0.5 } else { 2.0 },
                spot_sell_price_czk_per_kwh: Some(if i < 24 { 5.0 + i as f32 * 0.1 } else { 2.0 }),
            })
            .collect();
        let ctl = control();
        let levers = UserLevers {
            evening_discharge: Some(DischargeWindowLimit {
                hours: 1,
                min_soc_pct: 30.0,
            }),
            force_charge_at_or_below_czk: Some(1.0),
            ..UserLevers::default()
        };
        let horizon = WinterReserveStrategy::new(true, WinterReserveParams::default())
            .horizon_blocks(&lever_ctx(&blocks, &ctl, &levers));
        assert!(horizon[23].force_charge);
        assert!(!horizon[23].export_allowed);
        let allowed: Vec<usize> = horizon
            .iter()
            .enumerate()
            .filter(|(_, b)| b.export_allowed)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(allowed, vec![19, 20, 21, 22]);
    }

    #[test]
    fn planner_lever_change_replans_but_vpr_and_kill_switch_do_not() {
        let blocks = price_blocks();
        let ctl = control();
        let s = strategy();
        s.evaluate(&ctx(&blocks, 0, &ctl, 50.0));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 1);

        let forced = UserLevers {
            force_charge_at_or_below_czk: Some(7.0),
            ..UserLevers::default()
        };
        let mut c = ctx(&blocks, 0, &ctl, 50.0);
        c.user_levers = Some(&forced);
        s.evaluate(&c);
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 2);

        let kill_switch = UserLevers {
            automation_disabled: true,
            ..forced
        };
        c.user_levers = Some(&kill_switch);
        s.evaluate(&c);
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 2);
    }
}

#[cfg(test)]
mod force_discharge_gate_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn battery(force_discharge_enabled: bool) -> BatteryParams {
        BatteryParams {
            capacity_kwh: 20.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.125,
            max_export_kw: 10.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled,
        }
    }

    /// A block where exporting is clearly profitable (sell 10, cheap reference 1).
    fn profitable_block() -> PlanBlock {
        PlanBlock {
            start: Utc.with_ymd_and_hms(2026, 10, 13, 17, 0, 0).unwrap(),
            duration_minutes: 15,
            buy_czk: 1.0,
            sell_czk: 10.0,
            load_kwh: 0.5,
            pv_kwh: 0.0,
            reserve_load_kwh: 0.5,
            reserve_pv_kwh: 0.0,
            force_charge: false,
            no_discharge: false,
            force_charge_at_or_below_czk: None,
            export_min_soc_pct: None,
            export_allowed: true,
        }
    }

    #[test]
    fn export_is_not_offered_when_force_discharge_is_disabled() {
        let modes = candidate_modes(
            &profitable_block(),
            2.0,
            1.0,
            &battery(false),
            &WinterReserveParams::default(),
        );
        assert!(!modes.contains(&PlanMode::Export));
        assert!(modes.contains(&PlanMode::SelfUse));
    }

    #[test]
    fn export_is_offered_when_force_discharge_is_enabled() {
        let modes = candidate_modes(
            &profitable_block(),
            2.0,
            1.0,
            &battery(true),
            &WinterReserveParams::default(),
        );
        assert!(modes.contains(&PlanMode::Export));
    }

    #[test]
    fn planner_battery_params_follow_control_config() {
        let mut control = ControlConfig::default();
        assert!(!battery_params(&control).force_discharge_enabled);
        control.force_discharge_enabled = true;
        assert!(battery_params(&control).force_discharge_enabled);
    }
}
