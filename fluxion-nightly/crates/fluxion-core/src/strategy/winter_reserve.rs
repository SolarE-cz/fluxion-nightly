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
            soc_resolution_pct: 0.5,
        }
    }
}

/// One block of planner input. Forecasts here are already adjusted
/// (load * load_safety_factor, pv * pv_confidence); `plan` uses them as-is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlanBlock {
    pub start: DateTime<Utc>,
    pub duration_minutes: u32,
    /// Effective buy price incl. VAT, operator fee and HDO fee (CZK/kWh)
    pub buy_czk: f32,
    /// Sell price after operator fee (CZK/kWh)
    pub sell_czk: f32,
    pub load_kwh: f32,
    pub pv_kwh: f32,
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
        && block.sell_czk > 0.0
        && export_profit >= params.min_arbitrage_profit_czk
    {
        modes.push(PlanMode::Export);
    }
    modes
}

/// Anti-churn choice (PRD): SelfUse unless another mode is better by more than `min_benefit`.
/// `modes[0]` must be SelfUse. Returns (mode, value, outcome).
fn choose(
    dp: &DpStep,
    modes: &[PlanMode],
    energy_kwh: f32,
    min_benefit: f32,
) -> (PlanMode, f32, BlockOutcome) {
    let evals: Vec<(PlanMode, f32, BlockOutcome)> = modes
        .iter()
        .map(|&mode| {
            let (value, outcome) = dp.value(mode, energy_kwh);
            (mode, value, outcome)
        })
        .collect();
    let best = evals.iter().map(|e| e.1).fold(f32::INFINITY, f32::min);
    if evals[0].1 <= best + min_benefit {
        return evals[0];
    }
    evals[1..]
        .iter()
        .copied()
        .reduce(|a, b| if b.1 < a.1 { b } else { a })
        .unwrap_or(evals[0])
}

/// Morning blocks (PRD "Mornings and floor"): pv >= load after a deficit run of at least 4 h.
/// A horizon that starts inside a deficit counts that run from block 0. Timezone-free.
fn morning_blocks(blocks: &[PlanBlock]) -> Vec<bool> {
    const MIN_NIGHT_MINUTES: u32 = 4 * 60;
    let mut run_minutes = 0_u32; // deficit run ending just before the current block
    blocks
        .iter()
        .map(|b| {
            let real_sun = b.pv_kwh > 0.05 * (b.duration_minutes as f32 / 60.0);
            let covered = real_sun && b.pv_kwh >= b.load_kwh;
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

/// Night reserve floor per block (kWh), see PRD "Mornings and floor".
fn night_floors(
    blocks: &[PlanBlock],
    battery: &BatteryParams,
    cheap_ref: f32,
    params: &WinterReserveParams,
) -> Vec<f32> {
    let (emin, emax) = battery.bounds_kwh();
    let morning = morning_blocks(blocks);
    // prefix[i] = sum of max(net, 0) over blocks[..i]
    let prefix: Vec<f32> = std::iter::once(0.0)
        .chain(blocks.iter().scan(0.0_f32, |acc, b| {
            *acc += (b.load_kwh - b.pv_kwh).max(0.0);
            Some(*acc)
        }))
        .collect();
    let n = blocks.len();
    // A horizon that ends in an unfinished night (no morning, last block a deficit) reserves all of Emax.
    // Same "covered" rule as `morning_blocks`: a block without real sun is night, even at zero load.
    let night_open = blocks.last().is_some_and(|b| {
        let real_sun = b.pv_kwh > 0.05 * (b.duration_minutes as f32 / 60.0);
        !(real_sun && b.pv_kwh >= b.load_kwh)
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
    _tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
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
    let floors: Vec<f32> = night_floors(blocks, battery, cheap_ref, params)
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
            .map(|&e| choose(&dp, &modes, e, params.min_mode_benefit_czk).1)
            .collect();
    }

    // Forward pass with the exact simulator, starting from the live SOC.
    let mut energy = (soc_pct / 100.0 * capacity).clamp(emin, emax);
    let mut planned = Vec::with_capacity(n);
    let mut expected_cost = 0.0;
    for (k, block) in blocks.iter().enumerate() {
        let modes = candidate_modes(block, median_buy, cheap_ref, battery, params);
        let dp = DpStep {
            block,
            floor_kwh: floors[k],
            battery,
            grid: &grid,
            next_values: &values[k + 1],
        };
        let (mode, _, outcome) = choose(&dp, &modes, energy, params.min_mode_benefit_czk);
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
    /// Load: non-empty UTC-hour profile, else aligned flat forecast, else average load.
    /// PV: aligned forecast, else 0. Forecasts are used only when aligned with `all_price_blocks`.
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
        let mut blocks: Vec<PlanBlock> = all
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let hours = b.duration_minutes as f32 / 60.0;
                // Load: UTC-hour profile if it has data, else flat block forecast, else average load.
                let base_load = match profile {
                    Some(p) => p[b.block_start.hour() as usize] * hours,
                    None => match forecasts {
                        Some(f) => f[i].consumption_kwh,
                        None => control.average_household_load_kw * hours,
                    },
                };
                let load = base_load * self.params.load_safety_factor;
                let pv = match forecasts {
                    Some(f) => f[i].solar_kwh * self.params.pv_confidence,
                    None => 0.0,
                };
                PlanBlock {
                    start: b.block_start,
                    duration_minutes: b.duration_minutes,
                    buy_czk: b.effective_price_czk_per_kwh,
                    sell_czk: b
                        .spot_sell_price_czk_per_kwh
                        .unwrap_or(control.grid_export_fee_czk_per_kwh),
                    load_kwh: load,
                    pv_kwh: pv,
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
    ) -> Plan {
        self.plan_count.fetch_add(1, Ordering::Relaxed);
        plan(horizon, soc_pct, tz, battery, &self.params)
    }

    /// Planned entry for `horizon[0]`. Head calls reuse or refresh the cache; projection
    /// calls read the cache and, on a miss, plan without caching. `horizon` is never empty.
    /// `fingerprint` is `price_fingerprint` of the horizon and levers.
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
    ) -> PlannedBlock {
        let first = horizon[0].start;
        let last = horizon[horizon.len() - 1].start;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());

        if !is_head {
            let hit = cache
                .as_ref()
                .and_then(|c| c.plan.blocks.iter().find(|b| b.start == first))
                .cloned();
            return hit.unwrap_or_else(|| {
                self.run_plan(horizon, soc_pct, tz, battery)
                    .blocks
                    .swap_remove(0)
            });
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
            return entry;
        }

        let plan = self.run_plan(horizon, soc_pct, tz, battery);
        let entry = plan.blocks[0].clone();
        *cache = Some(CachedPlan {
            plan,
            planned_at: now,
            horizon_last_start: last,
            price_fingerprint: fingerprint,
        });
        entry
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
        let entry = self.entry_for(now, &horizon, live_soc, tz, &battery, is_head, fingerprint);
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
        assert!(morning_blocks(&blocks).iter().all(|m| !m));
    }

    #[test]
    fn thirteen_hour_night_gives_morning_block() {
        // 13 h deficit (52 blocks), then pv covers load from 08:00
        let blocks: Vec<PlanBlock> = (0..70)
            .map(|i| pblock(i, 3.0, 2.0, 0.75, if i >= 52 { 2.0 } else { 0.0 }))
            .collect();
        let flags = morning_blocks(&blocks);
        assert!(flags[52]);
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
        // 2.0 kWh/h * 0.25 h * load_safety_factor 1.10
        assert!(approx(horizon[0].load_kwh, 2.0 * 0.25 * 1.10));
        assert!(approx(horizon[4].load_kwh, 0.5 * 0.25 * 1.10)); // 17:00 UTC
        assert!(approx(horizon[0].pv_kwh, 0.0));
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
        let flags = morning_blocks(&blocks);
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
        let flags = morning_blocks(&blocks);
        assert!(!flags[28], "zero-load night block must not be morning");
        assert!(flags[56]);
        assert_eq!(flags.iter().filter(|m| **m).count(), 1);
        // The floor at 20:00 UTC (block 8) still covers the deficits up to real sunrise
        let floors = night_floors(&blocks, &battery(), 3.0, &WinterReserveParams::default());
        let expected: f32 = battery().bounds_kwh().0
            + blocks[9..56]
                .iter()
                .map(|x| (x.load_kwh - x.pv_kwh).max(0.0))
                .sum::<f32>();
        assert!(approx(floors[8], expected), "{} vs {expected}", floors[8]);
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
        let floors = night_floors(&blocks, &b, 0.0, &WinterReserveParams::default());
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
