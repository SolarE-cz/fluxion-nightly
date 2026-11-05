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

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use fluxion_types::config::{ControlConfig, ExportSelection};
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use fluxion_types::{CautiousPvSignal, DischargeWindowLimit, UserLevers};
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
    /// HOLD as an EEPROM SolaX "Stop Charge and Discharge" runs it (`control.hold_pv_to_battery`,
    /// measured on Jakub's Ultra pair): PV charges the battery first, the rest serves the house,
    /// then the grid. False (default): HOLD means the house is not served from the battery: PV
    /// serves the house, the surplus charges the battery, the rest is exported.
    #[serde(default)]
    pub hold_pv_to_battery: bool,
    /// Smallest stored energy (kWh) a Charge block must be able to add before the runtime guard
    /// stops it (max SOC − 1 %): max_charge_kw × one mode-change interval, at least 0.5 kWh (the
    /// export guard margin's rule). Stored at η × max_charge_kw, that takes longer than the
    /// interval, so a planned charge never trips the guard within one debounce. A smaller top-up
    /// ends within minutes and the guard flips the mode mid-block (brief H30). 0 disables the
    /// check. Price-lever (forced) charges are exempt.
    #[serde(default)]
    pub min_charge_kwh: f32,
    /// Measured cost of charge of the energy in the battery, per kWh delivered (CZK/kWh, losses
    /// and wear included; brief H33). Sets the export floor and the value of the energy left at
    /// the end of the horizon. `None` = unknown: both fall back to `cheap_ref`.
    #[serde(default)]
    pub charge_cost_czk_per_kwh: Option<f32>,
}

/// Tunables of the strategy (`strategies.winter_reserve` config section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WinterReserveParams {
    pub min_arbitrage_profit_czk: f32,
    pub insane_price_multiplier: f32,
    pub cheap_percentile: f32,
    pub load_safety_factor: f32,
    pub pv_confidence: f32,
    pub min_mode_benefit_czk: f32,
    /// Extra benefit (CZK) a SelfUse/Hold switch needs before a kept mode is dropped, counted
    /// over the whole run of blocks the switch is made for (brief H25)
    pub mode_switch_min_benefit_czk: f32,
    /// Planning cost (CZK) of a force-direction reversal: a Charge after an Export or an Export
    /// after a Charge, with any non-force blocks between them (see `ForceDirection`). It groups
    /// charge and export into runs and is never part of the plan cost; 0 = off (owner 2026-11-05)
    #[serde(default = "default_direction_reversal_cost_czk")]
    pub direction_reversal_cost_czk: f32,
    /// Run continuity of the discharge-window levers (%, owner 2026-11-05): a block the lever left
    /// out, in a gap of one or two blocks between two chosen blocks of the same window and day,
    /// may export when it sells at most this percentage below its chosen neighbour (see
    /// `fill_export_gaps`). It fills gaps only, never extends a run outward; 0 = off
    #[serde(default = "default_export_window_continuity_pct")]
    pub export_window_continuity_pct: f32,
    /// How the blocks Export may run in are chosen (owner 2026-11-05): `Windows` = the morning and
    /// evening discharge-hour levers; `DailyTopN` = the best `daily_export_hours` of each local
    /// day 00-24 by sell price, plus the continuity gap fills (`apply_daily_top_n`). The levers'
    /// hours are then ignored; their max-discharge % still applies.
    #[serde(default)]
    pub export_selection: ExportSelection,
    /// `DailyTopN`: export hours per local day (×4 blocks), 1-24
    #[serde(default = "default_daily_export_hours")]
    pub daily_export_hours: u8,
    pub soc_resolution_pct: f32,
    /// Export floor: the reserve deficit only until the next cheap block (buy at or below the
    /// horizon median) when it comes before the next morning (`night_floors`)
    #[serde(default)]
    pub export_floor_until_cheap: bool,
    /// Local months (1-12) planned on cautious PV (forecast times `pv_confidence`) when the
    /// forecast error is unknown (no `CautiousPvSignal`); empty = never
    #[serde(default = "default_cautious_pv_fallback_months")]
    pub cautious_pv_fallback_months: Vec<u32>,
}

fn default_cautious_pv_fallback_months() -> Vec<u32> {
    vec![10, 11, 12, 1, 2, 3]
}

fn default_direction_reversal_cost_czk() -> f32 {
    2.0
}

fn default_export_window_continuity_pct() -> f32 {
    5.0
}

fn default_daily_export_hours() -> u8 {
    6
}

/// Fewest valid days in the 7-day window before the forecast error decides cautious PV; with
/// fewer the fallback months decide (owner decision 2026-11-02, review #652 D-2)
pub const CAUTIOUS_PV_MIN_DAYS: usize = 5;

/// Hysteresis of cautious PV (`strategies.winter_reserve.cautious_pv_mae_pct` and
/// `cautious_pv_off_pct`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CautiousPvThresholds {
    /// Turns on at this forecast error (%) or more; 100 or more = never
    pub on_pct: f32,
    /// Once on, stays on while the error is at or above this (%)
    pub off_pct: f32,
}

/// Cautious PV state after a new forecast error (0-100 %): on at `on_pct` or more, and once on it
/// stays on until the error drops below `off_pct`. An `on_pct` of 100 or more turns the trigger
/// off.
#[must_use]
pub fn cautious_pv_next(was_on: bool, mae_pct: f32, t: CautiousPvThresholds) -> bool {
    if t.on_pct >= 100.0 {
        false
    } else if was_on {
        mae_pct >= t.off_pct
    } else {
        mae_pct >= t.on_pct
    }
}

/// Cautious-PV signal from the forecast error of the H34 window (`CorrectionResult::
/// forecast_error_pct` over `valid_days`) and the stored state: `None` when the error is unknown
/// or rests on fewer than `CAUTIOUS_PV_MIN_DAYS` valid days, so the fallback months decide
#[must_use]
pub fn cautious_pv_signal(
    error_pct: Option<f32>,
    valid_days: usize,
    was_on: bool,
    t: CautiousPvThresholds,
) -> Option<CautiousPvSignal> {
    error_pct
        .filter(|_| valid_days >= CAUTIOUS_PV_MIN_DAYS)
        .map(|mae_pct| CautiousPvSignal {
            mae_pct,
            cautious: cautious_pv_next(was_on, mae_pct, t),
        })
}

/// Whether the block starting at `start` plans on cautious PV: the forecast-error signal when there
/// is one, else whether the block's local month (in `tz`) is in `fallback_months`
#[must_use]
pub fn cautious_pv_at(
    start: DateTime<Utc>,
    signal: Option<CautiousPvSignal>,
    fallback_months: &[u32],
    tz: Tz,
) -> bool {
    match signal {
        Some(signal) => signal.cautious,
        None => fallback_months.contains(&start.with_timezone(&tz).month()),
    }
}

/// The timezone the planner reads local time in (nights, mornings, months)
fn planner_tz(context: &super::EvaluationContext) -> Tz {
    context.timezone.unwrap_or(chrono_tz::Europe::Prague)
}

impl Default for WinterReserveParams {
    fn default() -> Self {
        Self {
            min_arbitrage_profit_czk: 2.0,
            insane_price_multiplier: 3.0,
            cheap_percentile: 0.20,
            load_safety_factor: 1.10,
            pv_confidence: 0.70,
            min_mode_benefit_czk: 0.10,
            mode_switch_min_benefit_czk: 1.0,
            direction_reversal_cost_czk: default_direction_reversal_cost_czk(),
            export_window_continuity_pct: default_export_window_continuity_pct(),
            export_selection: ExportSelection::Windows,
            daily_export_hours: default_daily_export_hours(),
            soc_resolution_pct: 0.5,
            export_floor_until_cheap: false,
            cautious_pv_fallback_months: default_cautious_pv_fallback_months(),
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

/// The last force direction of the battery: a grid Charge or an Export (Force Discharge). Other
/// modes keep it, so Charge, SelfUse, Export is one reversal. The DP plans each direction as its
/// own state and charges `direction_reversal_cost_czk` for a reversal (owner 2026-11-05: no
/// Charge/Export flip-flop, sell in the best contiguous blocks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForceDirection {
    #[default]
    None,
    Charging,
    Exporting,
}

impl ForceDirection {
    /// The directions the DP keeps a value table for. A start with no direction plans from the
    /// one of them that values the start best: its first force mode is then free, as from none.
    const TABLES: [Self; 2] = [Self::Charging, Self::Exporting];

    /// The direction of an applied mode (`None` = unknown, or not a force mode).
    #[must_use]
    pub fn of(mode: Option<PlanMode>) -> Self {
        mode.map_or(Self::None, |m| Self::None.after(m))
    }

    /// The direction after a block in `mode`.
    #[must_use]
    pub fn after(self, mode: PlanMode) -> Self {
        match mode {
            PlanMode::Charge => Self::Charging,
            PlanMode::Export => Self::Exporting,
            PlanMode::SelfUse | PlanMode::Hold | PlanMode::NoDischarge => self,
        }
    }

    /// A block in `mode` reverses this direction.
    #[must_use]
    pub fn reversed_by(self, mode: PlanMode) -> bool {
        matches!(
            (self, mode),
            (Self::Charging, PlanMode::Export) | (Self::Exporting, PlanMode::Charge)
        )
    }
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
    /// Stored energy (kWh) a planned grid charge may reach in this block: Emax less the room
    /// left for the expected solar surplus (brief H33)
    pub charge_cap_kwh: f32,
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

/// The runtime guard stops a planned Charge once the live SOC reaches `max_soc_pct` minus this.
const CHARGE_GUARD_MARGIN_PCT: f32 = 1.0;

impl BatteryParams {
    /// Stored energy (kWh) at which the runtime guard stops a Charge.
    fn guard_cutoff_kwh(&self) -> f32 {
        (self.max_soc_pct - CHARGE_GUARD_MARGIN_PCT) / 100.0 * self.capacity_kwh
    }

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
    /// Stored energy a Charge may reach (see `charge_caps`)
    charge_cap_kwh: f32,
    battery: &'a BatteryParams,
    grid: &'a [f32],
    /// Value of the following step per force direction (`ForceDirection::index`), or a single
    /// entry when reversals are free
    next_values: &'a [Vec<f32>],
    /// Force direction before this block
    direction: ForceDirection,
    /// Planning cost of a reversal (0 when `next_values` has a single entry)
    reversal_cost: f32,
}

impl DpStep<'_> {
    /// A Charge from `energy_kwh` would store less than `min_charge_kwh` before the runtime
    /// guard stops it (at `max_soc_pct - CHARGE_GUARD_MARGIN_PCT`, or at the solar headroom cap),
    /// counted in stored energy and capped at what one block stores at full power (a whole-block
    /// charge is always allowed). A charge that cannot store anything below the cap is too small.
    fn charge_too_small(&self, energy_kwh: f32) -> bool {
        let b = self.battery;
        let (_, emax) = b.bounds_kwh();
        let eta = b.efficiency.max(1e-6);
        let block_stored = b.max_charge_kw * self.block.duration_minutes as f32 / 60.0 * eta;
        let guard_kwh = b.guard_cutoff_kwh().min(emax).min(self.charge_cap_kwh);
        let room = guard_kwh - energy_kwh.clamp(0.0, emax);
        if room <= 1e-6 && self.charge_cap_kwh < emax {
            return true;
        }
        let before_guard = block_stored.min(room.max(0.0));
        before_guard + 1e-6 < b.min_charge_kwh.min(block_stored)
    }

    /// Simulated outcome of `mode` from `energy_kwh` and its block cost (the same in every
    /// force direction).
    fn block_cost(&self, mode: PlanMode, energy_kwh: f32) -> (f32, BlockOutcome) {
        let outcome = simulate_block_capped(
            mode,
            energy_kwh,
            self.block,
            self.floor_kwh,
            self.battery,
            self.charge_cap_kwh,
        );
        let cost = outcome_cost(&outcome, self.block, self.battery.wear_cost_czk_per_kwh);
        (cost, outcome)
    }

    /// `cost` plus the value of the energy `outcome` leaves, in the direction after `mode`, plus
    /// the reversal cost when `mode` reverses `direction`.
    fn value_after(
        &self,
        direction: ForceDirection,
        mode: PlanMode,
        cost: f32,
        outcome: &BlockOutcome,
    ) -> f32 {
        self.with_reversal(
            direction,
            mode,
            cost + self.future(direction.after(mode), outcome),
        )
    }

    /// Value of the energy `outcome` leaves, in the force direction `after` the block.
    fn future(&self, after: ForceDirection, outcome: &BlockOutcome) -> f32 {
        let next = &self.next_values[direction_slot(self.next_values.len(), after)];
        interpolate(self.grid, next, outcome.energy_end_kwh)
    }

    /// `value` plus the reversal cost when `mode` reverses `direction`. Steers the plan only:
    /// `outcome_cost` (the plan cost) never includes it.
    fn with_reversal(&self, direction: ForceDirection, mode: PlanMode, value: f32) -> f32 {
        if self.next_values.len() > 1 && direction.reversed_by(mode) {
            value + self.reversal_cost
        } else {
            value
        }
    }
}

/// Slot of `direction` in a value table with `slots` directions (1 = reversals are free; else
/// `ForceDirection::TABLES`, never planned from `None`).
fn direction_slot(slots: usize, direction: ForceDirection) -> usize {
    usize::from(slots > 1 && direction == ForceDirection::Exporting)
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
    let export_profit = block.sell_czk - export_unit_cost(battery, cheap_ref);
    if block.export_allowed
        && battery.force_discharge_enabled
        && block.sell_czk > 0.0
        && export_profit >= params.min_arbitrage_profit_czk
    {
        modes.push(PlanMode::Export);
    }
    modes
}

/// Owner ruling 2026-11-02, "no battery for the house when it is worth more elsewhere": in a
/// block where an Export candidate from this block on sells above the buy price now, Hold becomes
/// the default mode instead of SelfUse. The house then runs from the grid and the battery keeps
/// its energy for the export. SelfUse stays a candidate and must beat Hold by
/// `min_mode_benefit_czk`, as any mode must beat the default.
///
/// The cost of charge alone (stored energy dearer than the grid, no export ahead) does not change
/// the default: an inflated cost would then hold the battery (H33 frozen-horizon check).
fn house_from_grid_defaults(blocks: &[PlanBlock], modes: &mut [Vec<PlanMode>]) {
    let mut best_export_ahead = f32::NEG_INFINITY;
    for (block, modes) in blocks.iter().zip(modes.iter_mut()).rev() {
        if modes.contains(&PlanMode::Export) {
            best_export_ahead = best_export_ahead.max(block.sell_czk);
        }
        if best_export_ahead > block.buy_czk && modes.first() == Some(&PlanMode::SelfUse) {
            modes.retain(|m| *m != PlanMode::Hold);
            modes.insert(0, PlanMode::Hold);
        }
    }
}

/// Cost (CZK) of one kWh of battery energy sold to the grid: its cost of charge, which already
/// includes the losses and the wear (brief H33). Without a known cost: `cheap_ref / efficiency
/// + wear`, the rule before H33. Export needs `sell − this ≥ min_arbitrage_profit_czk`.
fn export_unit_cost(battery: &BatteryParams, cheap_ref: f32) -> f32 {
    battery
        .charge_cost_czk_per_kwh
        .unwrap_or_else(|| cheap_ref / battery.efficiency.max(1e-6) + battery.wear_cost_czk_per_kwh)
}

/// Value (CZK/kWh) of the energy left at the end of the horizon: its cost of charge (brief H33),
/// capped at `cheap_ref − wear`, the value before H33 (what it saves later, minus the wear of
/// using it). Never negative.
///
/// The cap: energy at the horizon end can be bought again at about `cheap_ref`, so it is worth no
/// more than that. Uncapped, a cost of charge above the horizon's cheap prices makes buying at
/// the end pay on paper (a plan that ends full), and when the horizon end stops moving (prices of
/// tomorrow missing) those charges execute and raise the cost further. A cap of
/// `cheap_ref / efficiency` would still make every block below `cheap_ref` (the mean of the
/// cheapest blocks) such a buy. With this cap, a known cost never raises the end value above the
/// no-cost plan's, so it never ends a plan fuller; a cheap (solar) cost lowers it.
fn terminal_price(battery: &BatteryParams, cheap_ref: f32) -> f32 {
    let before_h33 = cheap_ref - battery.wear_cost_czk_per_kwh;
    battery
        .charge_cost_czk_per_kwh
        .filter(|c| c.is_finite())
        .map_or(before_h33, |c| c.min(before_h33))
        .max(0.0)
}

/// Solar headroom per block (brief H33): the stored energy (kWh) a planned grid charge may reach.
///
/// The battery is a buffer. From the first PV block of a local day (in the horizon) until "late",
/// a Charge may only fill to `Emax − room`, where `room` = Σ max(0, pv − load) over the PV blocks
/// of that day from this block to the day's last PV block (expected flows). "Late" = the remaining
/// expected PV over those blocks is below their expected load: then no cap. Night (after the last
/// PV block, before the first): no cap, nothing is kept for tomorrow's sun. A price-lever charge
/// is exempt.
fn charge_caps(blocks: &[PlanBlock], battery: &BatteryParams, tz: Tz) -> Vec<f32> {
    let (emin, emax) = battery.bounds_kwh();
    let day = |b: &PlanBlock| b.start.with_timezone(&tz).date_naive();
    let mut caps = vec![emax; blocks.len()];
    let mut i = 0;
    while i < blocks.len() {
        let d = day(&blocks[i]);
        let end = blocks[i..]
            .iter()
            .position(|b| day(b) != d)
            .map_or(blocks.len(), |p| i + p);
        let sunny: Vec<usize> = (i..end).filter(|&k| has_real_sun(&blocks[k])).collect();
        if let (Some(&first), Some(&last)) = (sunny.first(), sunny.last()) {
            for k in first..=last {
                if blocks[k].force_charge {
                    continue;
                }
                let rest = &blocks[k..=last];
                let pv: f32 = rest.iter().map(|b| b.pv_kwh).sum();
                let load: f32 = rest.iter().map(|b| b.load_kwh).sum();
                if pv < load {
                    continue; // late: the rest of the day's sun does not cover the house
                }
                let room: f32 = rest.iter().map(|b| (b.pv_kwh - b.load_kwh).max(0.0)).sum();
                caps[k] = (emax - room).max(emin);
            }
        }
        i = end;
    }
    caps
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
/// `modes[0]` is the default: SelfUse, or Hold where `house_from_grid_defaults` makes it the
/// default. Returns (mode, value, outcome).
///
/// `keep` is the mode applied to the previous block. When the chosen mode leaves the battery and
/// grid exactly as `keep` would (for example SelfUse and Hold at the SOC floor), `keep` is returned:
/// the inverter write would change nothing, and the planned cost is identical.
///
/// Between SelfUse and Hold only, `keep` is also returned when it is within
/// `mode_switch_min_benefit` of the chosen mode. Charge, Export and NoDischarge are never held
/// this way. The planner's forward pass passes 0 here and applies the run-based keep rule
/// (`KeepRule::Window`) itself; a per-block margin defers a long switch block after block.
fn choose(
    dp: &DpStep,
    modes: &[PlanMode],
    energy_kwh: f32,
    min_benefit: f32,
    mode_switch_min_benefit: f32,
    keep: Option<PlanMode>,
) -> (PlanMode, f32, BlockOutcome) {
    select_mode(
        &evaluate_modes(dp, modes, energy_kwh),
        min_benefit,
        mode_switch_min_benefit,
        keep,
    )
}

/// Value and outcome of every candidate mode from `energy_kwh`. Charge is left out where it
/// could store less than `min_charge_kwh` before the guard (brief H30), unless a lever forces it.
fn evaluate_modes(
    dp: &DpStep,
    modes: &[PlanMode],
    energy_kwh: f32,
) -> Vec<(PlanMode, f32, BlockOutcome)> {
    block_costs(dp, modes, energy_kwh)
        .into_iter()
        .map(|(mode, cost, outcome)| {
            (
                mode,
                dp.value_after(dp.direction, mode, cost, &outcome),
                outcome,
            )
        })
        .collect()
}

/// Block cost and outcome of every candidate mode from `energy_kwh` (the candidates of
/// `evaluate_modes`), before the value of the energy left.
fn block_costs(
    dp: &DpStep,
    modes: &[PlanMode],
    energy_kwh: f32,
) -> Vec<(PlanMode, f32, BlockOutcome)> {
    modes
        .iter()
        .filter(|&&mode| {
            mode != PlanMode::Charge || dp.block.force_charge || !dp.charge_too_small(energy_kwh)
        })
        .map(|&mode| {
            let (cost, outcome) = dp.block_cost(mode, energy_kwh);
            (mode, cost, outcome)
        })
        .collect()
}

/// The two modes the SelfUse/Hold keep rule applies to.
fn switch_only(mode: PlanMode) -> bool {
    matches!(mode, PlanMode::SelfUse | PlanMode::Hold)
}

/// The selection rule of `choose`, on evaluated candidates (`evals[0]` is the default mode).
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
    // With `export_floor_until_cheap`, the reserve ends at the next cheap block when that comes
    // first: from there the grid serves the house at a price at or below the horizon median and
    // below the midpoint of its cheapest and dearest buy, so energy kept beyond it is worth only a
    // cheap purchase. The midpoint keeps a dear majority at the median (and a flat horizon) from
    // counting as cheap. n = no cheap block ahead. An open night (no morning ahead) also ends at a
    // cheap block.
    let cheap = if params.export_floor_until_cheap && n > 0 {
        let mut buys: Vec<f32> = blocks.iter().map(|b| b.buy_czk).collect();
        buys.sort_by(f32::total_cmp);
        Some((median_of_sorted(&buys), (buys[0] + buys[n - 1]) / 2.0))
    } else {
        None
    };
    let eta = battery.efficiency.max(1e-6);
    let wear = battery.wear_cost_czk_per_kwh;
    let mut next_cheap = n;
    for (k, block) in blocks.iter().enumerate().rev() {
        // With `export_floor_until_cheap`, when no cheap block and no morning are ahead (tomorrow's
        // morning is not in the horizon, e.g. its prices are missing), the reserve ends at the
        // first later block that refills at or below `cheap_ref` with a sell spread and a net gain
        // of `min_arbitrage_profit_czk` against this block's sell (losses and wear included).
        let refill_end = if params.export_floor_until_cheap && next_cheap == n && next_morning == n
        {
            (k + 1..n).find(|&j| {
                let later = &blocks[j];
                later.buy_czk <= cheap_ref
                    && block.sell_czk > later.sell_czk + params.min_mode_benefit_czk
                    && block.sell_czk - (later.buy_czk / eta + wear)
                        >= params.min_arbitrage_profit_czk
            })
        } else {
            None
        };
        let until = if next_cheap < next_morning {
            Some(next_cheap)
        } else if let Some(refill) = refill_end {
            Some(refill)
        } else if next_morning == n && night_open {
            None
        } else {
            Some(next_morning)
        };
        let raw = match until {
            Some(end) => (emin + prefix[end] - prefix[k + 1]).min(emax),
            None => emax,
        };
        // Insane price only counts against a known, positive cheap reference.
        let insane =
            cheap_ref > 0.0 && block.sell_czk >= params.insane_price_multiplier * cheap_ref;
        floors[k] = if insane { emin } else { raw };
        if morning[k] {
            next_morning = k;
        }
        if cheap.is_some_and(|(median, mid)| block.buy_czk <= median && block.buy_czk < mid) {
            next_cheap = k;
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
    simulate_block_capped(mode, energy_kwh, block, floor_kwh, battery, f32::INFINITY)
}

/// `simulate_block` with a Charge stopping at `charge_cap_kwh` stored (solar headroom).
fn simulate_block_capped(
    mode: PlanMode,
    energy_kwh: f32,
    block: &PlanBlock,
    floor_kwh: f32,
    battery: &BatteryParams,
    charge_cap_kwh: f32,
) -> BlockOutcome {
    let h = block.duration_minutes as f32 / 60.0;
    let (emin, emax) = battery.bounds_kwh();
    let eta = battery.efficiency.max(1e-6);
    let cmax = battery.max_charge_kw * h;
    let dmax = battery.max_discharge_kw * h;
    let xmax = battery.max_export_kw * h;
    // Below Emin the battery may still charge but never discharges: discharge headroom is
    // measured from Emin, so the gap below it is not available energy (brief H19 item 4).
    let e = energy_kwh.clamp(0.0, emax);
    // A negative forecast is no PV, not a discharge
    let (load, pv) = (block.load_kwh, block.pv_kwh.max(0.0));
    let mut o = BlockOutcome::default();

    match mode {
        PlanMode::Hold if battery.hold_pv_to_battery => {
            // EEPROM SolaX "Stop Charge and Discharge": PV charges the battery first (it never
            // discharges), the rest serves the house, then the grid.
            let c = pv.min(cmax).min(((emax - e) / eta).max(0.0));
            o.battery_charge_kwh = c;
            o.energy_end_kwh = e + c * eta;
            grid_balance(&mut o, load - (pv - c), xmax);
        }
        // HOLD: the house is not served from the battery. PV serves the house, the surplus charges
        // the battery and the rest is exported, as NoDischarge (SolaX direct VPP mode 7).
        PlanMode::SelfUse | PlanMode::NoDischarge | PlanMode::Hold if pv >= load => {
            let surplus = pv - load;
            let c = surplus.min(cmax).min(((emax - e) / eta).max(0.0));
            o.battery_charge_kwh = c;
            o.energy_end_kwh = e + c * eta;
            o.grid_export_kwh = (surplus - c).min(xmax);
            o.curtailed_kwh = surplus - c - o.grid_export_kwh;
        }
        PlanMode::SelfUse | PlanMode::NoDischarge | PlanMode::Hold => {
            let deficit = load - pv;
            let q = match mode {
                PlanMode::SelfUse => deficit.min(dmax).min(e - emin).max(0.0),
                _ => 0.0,
            };
            o.battery_discharge_kwh = q;
            o.energy_end_kwh = e - q;
            o.grid_import_kwh = deficit - q;
        }
        PlanMode::Charge => {
            let c = cmax.min(((emax.min(charge_cap_kwh) - e) / eta).max(0.0));
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
                charge_cap_kwh: capacity,
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

/// Median of ascending, non-empty `sorted` (mean of the two middle values for an even length).
fn median_of_sorted(sorted: &[f32]) -> f32 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
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

/// Head guard of an Export block: at or below the night floor plus the guard margin the block runs
/// as SelfUse. The live head evaluation and the replay both apply it.
pub fn export_guard_holds(live_kwh: f32, floor_kwh: f32, battery: &BatteryParams) -> bool {
    live_kwh <= floor_kwh + battery.export_guard_margin_kwh
}

/// `plan`, with the mode currently applied to the inverter (`current`, None = unknown). Every
/// block may keep the previous block's mode (the first block keeps `current`): when both leave
/// the battery and grid exactly the same, or, between SelfUse and Hold, when switching saves at
/// most `mode_switch_min_benefit_czk` over the run it is made for. Hold is kept only in blocks
/// without PV. Projected blocks use the same rule as the head, so a block's projected mode is
/// the head decision once it starts (brief H25). With None the first block defaults to SelfUse.
///
/// The force direction before the first block is that of `current` (see `plan_from_direction`).
pub fn plan_from(
    blocks: &[PlanBlock],
    soc_pct: f32,
    tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
    current: Option<PlanMode>,
) -> Plan {
    let direction = ForceDirection::of(current);
    plan_from_direction(blocks, soc_pct, tz, battery, params, current, direction)
}

/// `plan_from` with the force direction before the first block given (`direction`, the last
/// Charge or Export applied, kept through SelfUse/Hold heads). A first-block force mode opposite
/// to it pays `direction_reversal_cost_czk` in the plan choice, as a reversal inside the horizon.
pub fn plan_from_direction(
    blocks: &[PlanBlock],
    soc_pct: f32,
    tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
    current: Option<PlanMode>,
    direction: ForceDirection,
) -> Plan {
    plan_with_rule(
        blocks, soc_pct, tz, battery, params, current, direction, KEEP_RULE,
    )
}

/// How the forward pass compares a kept SelfUse/Hold mode with a switch to the other one.
#[derive(Debug, Clone, Copy, PartialEq)]
enum KeepRule {
    /// Keep when the kept mode costs at most `mode_switch_min_benefit_czk` more than switching,
    /// both followed by the optimal plan. That is the cost of switching one block later, so a
    /// long run of the cheaper mode can be deferred block after block (the H25 Hold lock-in).
    /// Kept only for the closed-loop comparison in the tests.
    #[cfg_attr(not(test), expect(dead_code))]
    PerBlock,
    /// Keep when keeping the mode for the whole run the switch is made for (until the switched
    /// plan returns to the kept mode or leaves SelfUse/Hold) costs at most
    /// `mode_switch_min_benefit_czk` more than switching now.
    Window,
}

/// The keep rule of the planner (brief H25, chosen on the closed-loop table in the report).
const KEEP_RULE: KeepRule = KeepRule::Window;

/// The planned horizon as the forward pass sees it: blocks, floors, value function, candidates.
struct Horizon<'a> {
    blocks: &'a [PlanBlock],
    floors: &'a [f32],
    caps: &'a [f32],
    /// values[k][direction slot][grid point]
    values: &'a [Vec<Vec<f32>>],
    grid: &'a [f32],
    modes: &'a [Vec<PlanMode>],
    battery: &'a BatteryParams,
    min_benefit: f32,
    reversal_cost: f32,
}

impl Horizon<'_> {
    fn step(&self, k: usize, direction: ForceDirection) -> DpStep<'_> {
        DpStep {
            block: &self.blocks[k],
            floor_kwh: self.floors[k],
            charge_cap_kwh: self.caps[k],
            battery: self.battery,
            grid: self.grid,
            next_values: &self.values[k + 1],
            direction,
            reversal_cost: self.reversal_cost,
        }
    }

    /// Extra cost (CZK) of keeping `kept` at block `k` instead of switching to `chosen`.
    /// The switch run is every block from `k` on in which the optimal plan after the switch stays
    /// in the other SelfUse/Hold mode; it ends where that plan returns to `kept`, leaves
    /// SelfUse/Hold, where `kept` is not allowed (not a candidate, or Hold in a PV block), or at
    /// the horizon end. The kept path runs `kept` through the run, then continues optimally.
    /// SelfUse/Hold keep the force direction, so the whole run plans from `direction`.
    fn keep_run_extra_cost(
        &self,
        k: usize,
        energy_kwh: f32,
        kept: PlanMode,
        chosen: &(PlanMode, f32, BlockOutcome),
        direction: ForceDirection,
    ) -> f32 {
        let n = self.blocks.len();
        let mut len = 0;
        let mut cur = *chosen;
        while k + len < n {
            let j = k + len;
            let keep_allowed = self.modes[j].contains(&kept)
                && (kept != PlanMode::Hold || self.blocks[j].pv_kwh <= 0.0);
            if cur.0 == kept || !switch_only(cur.0) || !keep_allowed {
                break;
            }
            len += 1;
            if k + len < n {
                cur = choose(
                    &self.step(k + len, direction),
                    &self.modes[k + len],
                    cur.2.energy_end_kwh,
                    self.min_benefit,
                    0.0,
                    None,
                );
            }
        }
        if len == 0 {
            return f32::INFINITY;
        }
        let mut energy = energy_kwh;
        let mut cost = 0.0;
        for j in k..k + len {
            let block = &self.blocks[j];
            let outcome = simulate_block(kept, energy, block, self.floors[j], self.battery);
            cost += outcome_cost(&outcome, block, self.battery.wear_cost_czk_per_kwh);
            energy = outcome.energy_end_kwh;
        }
        let next = &self.values[k + len];
        cost + interpolate(
            self.grid,
            &next[direction_slot(next.len(), direction)],
            energy,
        ) - chosen.1
    }
}

/// Most steps of the stored-energy grid. Steps = (max - min) / resolution, whatever the capacity;
/// 1000 is the finest resolution the web settings allow (0.1 %) over the full 0..100 % span.
const MAX_SOC_GRID_STEPS: usize = 1000;

/// Energy grid from Emin to Emax; the last point is exactly Emax. A resolution finer than
/// `MAX_SOC_GRID_STEPS` is coarsened to the cap, so an input cannot size the grid.
fn soc_grid(emin: f32, emax: f32, capacity: f32, resolution_pct: f32) -> Vec<f32> {
    let span = (emax - emin).max(0.0);
    let mut grid_step = resolution_pct / 100.0 * capacity;
    if grid_step > 0.0 && span / grid_step > MAX_SOC_GRID_STEPS as f32 {
        grid_step = span / MAX_SOC_GRID_STEPS as f32;
    }
    let steps = if span > 0.0 && grid_step > 0.0 {
        (span / grid_step).ceil() as usize
    } else {
        0
    };
    (0..=steps)
        .map(|i| {
            if i == steps {
                emin + span
            } else {
                emin + (i as f32 * grid_step).min(span)
            }
        })
        .collect()
}

#[expect(clippy::too_many_arguments)]
fn plan_with_rule(
    blocks: &[PlanBlock],
    soc_pct: f32,
    tz: Tz,
    battery: &BatteryParams,
    params: &WinterReserveParams,
    current: Option<PlanMode>,
    start_direction: ForceDirection,
    rule: KeepRule,
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
    if battery.min_soc_pct >= battery.max_soc_pct
        || !(0.0..=100.0).contains(&battery.min_soc_pct)
        || !(0.0..=100.0).contains(&battery.max_soc_pct)
        || capacity.is_nan()
        || capacity <= 0.0
    {
        return self_use_plan(blocks, soc_pct, capacity, "invalid battery config");
    }

    let mut buys: Vec<f32> = blocks.iter().map(|b| b.buy_czk).collect();
    buys.sort_by(f32::total_cmp);
    let median_buy = median_of_sorted(&buys);
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

    let caps = charge_caps(blocks, battery, tz);

    let grid = soc_grid(emin, emax, capacity, params.soc_resolution_pct);

    // Candidate modes per block (`values` below holds the value tables).
    let mut all_modes: Vec<Vec<PlanMode>> = blocks
        .iter()
        .map(|b| candidate_modes(b, median_buy, cheap_ref, battery, params))
        .collect();
    house_from_grid_defaults(blocks, &mut all_modes);
    // One value table per force direction when a reversal has a cost and can happen (a Charge and
    // an Export candidate, or one opposite to the start direction). Otherwise the direction
    // changes nothing and a single table gives the plan without the reversal cost.
    let has = |mode: PlanMode| all_modes.iter().any(|m| m.contains(&mode));
    let (has_charge, has_export) = (has(PlanMode::Charge), has(PlanMode::Export));
    let reversal_possible = (has_charge
        && (has_export || start_direction == ForceDirection::Exporting))
        || (has_export && start_direction == ForceDirection::Charging);
    let reversal_cost = if reversal_possible && params.direction_reversal_cost_czk > 0.0 {
        params.direction_reversal_cost_czk
    } else {
        0.0
    };
    let directions: &[ForceDirection] = if reversal_cost > 0.0 {
        &ForceDirection::TABLES
    } else {
        &[ForceDirection::None]
    };
    // values[k][slot][i] = V_k at grid[i] with the force direction of `slot` before block k;
    // values[n] is the terminal value.
    let mut values: Vec<Vec<Vec<f32>>> = vec![Vec::new(); n + 1];
    // Leftover energy is worth its cost of charge (else a cheap purchase minus the wear).
    let terminal_price = terminal_price(battery, cheap_ref);
    let terminal: Vec<f32> = grid.iter().map(|&e| -e * terminal_price).collect();
    values[n] = vec![terminal; directions.len()];
    for (k, block) in blocks.iter().enumerate().rev() {
        let (current, later) = values.split_at_mut(k + 1);
        let modes = &all_modes[k];
        let dp = DpStep {
            block,
            floor_kwh: floors[k],
            charge_cap_kwh: caps[k],
            battery,
            grid: &grid,
            next_values: &later[0],
            direction: ForceDirection::None,
            reversal_cost,
        };
        let mut tables = vec![Vec::with_capacity(grid.len()); directions.len()];
        let mut forced: Vec<Option<f32>> = Vec::with_capacity(modes.len());
        let mut evals: Vec<(PlanMode, f32, BlockOutcome)> = Vec::with_capacity(modes.len());
        for &e in &grid {
            // The block is simulated once; each direction differs only in the value after it.
            // After a Charge or an Export that value is the same from every direction.
            let costs = block_costs(&dp, modes, e);
            forced.clear();
            forced.extend(costs.iter().map(|&(mode, cost, outcome)| {
                matches!(mode, PlanMode::Charge | PlanMode::Export)
                    .then(|| cost + dp.future(ForceDirection::None.after(mode), &outcome))
            }));
            for (table, &direction) in tables.iter_mut().zip(directions) {
                evals.clear();
                evals.extend(
                    costs
                        .iter()
                        .zip(&forced)
                        .map(|(&(mode, cost, outcome), forced)| {
                            let value = forced.unwrap_or_else(|| {
                                cost + dp.future(direction.after(mode), &outcome)
                            });
                            (mode, dp.with_reversal(direction, mode, value), outcome)
                        }),
                );
                let (_, value, _) = select_mode(
                    &evals,
                    params.min_mode_benefit_czk,
                    params.mode_switch_min_benefit_czk,
                    None,
                );
                table.push(value);
            }
        }
        current[k] = tables;
    }

    // Forward pass with the exact simulator, starting from the real live SOC. A SOC below Emin is
    // not raised to the floor: it can charge but not discharge (brief H19 item 4).
    let mut energy = (soc_pct / 100.0 * capacity).clamp(0.0, emax);
    let mut planned = Vec::with_capacity(n);
    let mut expected_cost = 0.0;
    let horizon = Horizon {
        blocks,
        floors: &floors,
        caps: &caps,
        values: &values,
        grid: &grid,
        modes: &all_modes,
        battery,
        min_benefit: params.min_mode_benefit_czk,
        reversal_cost,
    };
    let mut previous = current;
    // With a reversal cost, a start with no direction plans from the table that values the start
    // best (`ForceDirection::TABLES`); the direction then follows the planned force modes.
    let mut direction = if directions.len() > 1 && start_direction == ForceDirection::None {
        let start_value = |d: ForceDirection| {
            interpolate(
                &grid,
                &values[0][direction_slot(directions.len(), d)],
                energy,
            )
        };
        if start_value(ForceDirection::Exporting) < start_value(ForceDirection::Charging) {
            ForceDirection::Exporting
        } else {
            ForceDirection::Charging
        }
    } else {
        start_direction
    };
    for (k, block) in blocks.iter().enumerate() {
        let modes = &all_modes[k];
        let dp = horizon.step(k, direction);
        // Keep the applied mode in blocks without PV, or when it is SelfUse at any PV. The simulator
        // models Hold as never charging from PV, so a kept Hold could block real PV charging.
        // SelfUse charges from PV, so keeping it is safe.
        let keep = previous.filter(|m| block.pv_kwh <= 0.0 || *m == PlanMode::SelfUse);
        // Every block gets the same keep rule, with `keep` = the previous planned block (block 0:
        // the applied mode). A later block is re-decided by this rule when it becomes block 0
        // (`current` = the previous head), so with unchanged inputs its projected mode is the
        // head decision (brief H25).
        let (mode, _, outcome) = match rule {
            KeepRule::PerBlock => choose(
                &dp,
                modes,
                energy,
                params.min_mode_benefit_czk,
                params.mode_switch_min_benefit_czk,
                keep,
            ),
            KeepRule::Window => {
                let evals = evaluate_modes(&dp, modes, energy);
                let chosen = select_mode(&evals, params.min_mode_benefit_czk, 0.0, keep);
                match keep.and_then(|m| evals.iter().find(|e| e.0 == m)) {
                    Some(kept)
                        if kept.0 != chosen.0
                            && switch_only(kept.0)
                            && switch_only(chosen.0)
                            && horizon
                                .keep_run_extra_cost(k, energy, kept.0, &chosen, direction)
                                <= params.mode_switch_min_benefit_czk =>
                    {
                        *kept
                    }
                    _ => chosen,
                }
            }
        };
        previous = Some(mode);
        direction = direction.after(mode);
        let cost = outcome_cost(&outcome, block, battery.wear_cost_czk_per_kwh);
        let soc_start_pct = soc_pct_of(energy, capacity);
        let soc_end_pct = soc_pct_of(outcome.energy_end_kwh, capacity);
        planned.push(PlannedBlock {
            start: block.start,
            mode,
            soc_start_pct,
            soc_end_pct,
            floor_kwh: floors[k],
            charge_cap_kwh: caps[k],
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
    /// Forecast-error signal of the plan (`None` = fallback months)
    cautious_pv: Option<CautiousPvSignal>,
    /// Some horizon block plans on cautious PV
    cautious_any: bool,
}

/// `yes (mae=39.2%)` or `no (fallback month)`: whether any horizon block plans on cautious PV,
/// and what decided it
fn cautious_pv_log(signal: Option<CautiousPvSignal>, any_cautious: bool) -> String {
    let state = if any_cautious { "yes" } else { "no" };
    match signal {
        Some(s) => format!("{state} (mae={:.1}%)", s.mae_pct),
        None => format!("{state} (fallback month)"),
    }
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
    /// Mode of the previous head the replan kept to (`plan_from_direction`'s `current`)
    current: Option<PlanMode>,
    /// Force direction the replan started from (`plan_from_direction`'s `direction`)
    start_direction: ForceDirection,
    blocks: Vec<PlanDumpBlock<'a>>,
}

/// A head replan with the inputs it started from besides the horizon and SOC, so it can be
/// reproduced offline (`plan_from_direction(horizon, soc, tz, battery, params, current,
/// start_direction)`)
struct Replan {
    plan: Plan,
    current: Option<PlanMode>,
    start_direction: ForceDirection,
}

/// One INFO line per head replan, and the full planner input as JSON to `LAST_PLAN_PATH`.
fn log_replan(horizon: &[PlanBlock], replan: &Replan, soc_pct: f32, inputs: PlanInputs, tz: Tz) {
    let plan = &replan.plan;
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
         first_morning={first_morning} head_floor_kwh={:.2} head_mode={:?} soc_pct={soc_pct:.1} \
         cautious_pv={} current={:?} start_direction={:?}",
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
        cautious_pv_log(inputs.cautious_pv, inputs.cautious_any),
        replan.current,
        replan.start_direction,
    );
    dump_last_plan(horizon, replan, soc_pct);
}

/// The planner input of a head replan as `LAST_PLAN_PATH` stores it.
fn plan_dump<'a>(horizon: &'a [PlanBlock], replan: &Replan, soc_pct: f32) -> PlanDump<'a> {
    PlanDump {
        soc_pct,
        current: replan.current,
        start_direction: replan.start_direction,
        blocks: horizon
            .iter()
            .zip(&replan.plan.blocks)
            .map(|(block, planned)| PlanDumpBlock {
                block,
                floor_kwh: planned.floor_kwh,
            })
            .collect(),
    }
}

fn dump_last_plan(horizon: &[PlanBlock], replan: &Replan, soc_pct: f32) {
    if !std::path::Path::new(LAST_PLAN_DIR).is_dir() {
        return;
    }
    let dump = plan_dump(horizon, replan, soc_pct);
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
    /// Effective minimum SOC (%) the plan used; a change, such as the Back Up floor, replans
    min_soc_pct: f32,
    /// Force direction after the plan's head block: the last Charge or Export the planner
    /// applied, kept through SelfUse/Hold heads; the next replan starts from it
    direction: ForceDirection,
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
    /// The INFO that `daily_top_n` ignores the discharge-hour levers was logged
    top_n_levers_logged: std::sync::atomic::AtomicBool,
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
            top_n_levers_logged: std::sync::atomic::AtomicBool::new(false),
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

/// Smallest useful charge (stored kWh) for `max_charge_kw` and the executor's mode-change
/// debounce: max_charge_kw × one debounce interval, at least 0.5 kWh. The same rule
/// as the export guard margin (PRD review fixes), on the charge side (brief H30).
pub fn min_charge_kwh(max_charge_kw: f32, mode_change_interval_secs: u64) -> f32 {
    (max_charge_kw * mode_change_interval_secs as f32 / 3600.0).max(0.5)
}

/// The HOLD routing the planner uses: `control.hold_pv_to_battery`, except on the direct VPP path
/// (the decided path, see [`crate::vpp::active_path`]; the option itself without a path state),
/// which writes HOLD as VPP mode 7 (house from solar then grid, never the battery), so the default
/// model applies. INFO once per `logged` flag when the option is overridden.
pub fn planner_hold_pv_to_battery(
    control: &ControlConfig,
    path: Option<&crate::vpp::VppPathState>,
    logged: &std::sync::atomic::AtomicBool,
) -> bool {
    if !control.hold_pv_to_battery {
        return false;
    }
    if crate::vpp::active_path(control, path) != crate::vpp::ActivePath::Vpp {
        return true;
    }
    if !logged.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "HOLD planned with the default model (house from PV, surplus to the battery): control.hold_pv_to_battery is ignored on the direct VPP path, which runs HOLD as VPP mode 7"
        );
    }
    false
}

/// The once-per-run flag of [`planner_hold_pv_to_battery`]'s INFO for the live planner
static HOLD_ON_VPP_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Battery limits the live planner derives from the control configuration.
pub fn battery_params(control: &ControlConfig) -> BatteryParams {
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
        // Off on the direct VPP path: HOLD runs as VPP mode 7, the default model
        hold_pv_to_battery: planner_hold_pv_to_battery(
            control,
            crate::vpp::served_path().as_ref(),
            &HOLD_ON_VPP_LOGGED,
        ),
        min_charge_kwh: min_charge_kwh(
            control.max_battery_charge_rate_kw,
            control.min_mode_change_interval_secs,
        ),
        // Set per evaluation from the measured cost of charge (`EvaluationContext`)
        charge_cost_czk_per_kwh: None,
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

/// How much export time a local day gets in `export_eligible`, counted in block minutes (so
/// hourly prices take `hours` blocks, quarter-hour prices `hours` × 4).
#[derive(Clone, Copy, Debug)]
enum DayBudget {
    /// `hours` per window and day (the discharge-window levers)
    Hours(u8),
    /// `hours` of each complete local day 00-24 (`daily_top_n`; a DST day of 23 or 25 h is
    /// complete too). A partial day (known blocks shorter than the local day) gets its share:
    /// min(`hours`, ceil(known × `hours` / 24 h)), with no jump at any threshold (coordinator
    /// 2026-11-05). Today is complete with the ranking context, so a partial day is a future day
    /// the price feed has only partly published.
    ShareOfDay(u8),
}

impl DayBudget {
    /// (numerator, denominator) in minutes: a block is taken while `taken × den < num`, so the
    /// last block may cross a fractional budget (the ceil). `day_minutes` is the local day's
    /// length.
    fn minutes(self, known_minutes: u32, day_minutes: u32) -> (u32, u32) {
        match self {
            DayBudget::Hours(hours) => (u32::from(hours) * 60, 1),
            DayBudget::ShareOfDay(hours) => {
                let full = u32::from(hours) * 60;
                if known_minutes >= day_minutes {
                    (full, 1)
                } else {
                    ((known_minutes * u32::from(hours)).min(full * 24), 24)
                }
            }
        }
    }
}

/// Length of the local day `date` in minutes (1380 or 1500 on a DST change day)
fn local_day_minutes(date: NaiveDate, tz: Tz) -> u32 {
    use chrono::TimeZone;
    let midnight = |d: NaiveDate| {
        tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
    };
    match (midnight(date), date.succ_opt().and_then(midnight)) {
        (Some(start), Some(end)) => u32::try_from((end - start).num_minutes()).unwrap_or(1440),
        _ => 1440,
    }
}

/// Indices of the blocks in `in_window` that are among the highest sell prices of their local day
/// (ties: earlier block first) up to the day's `budget`, plus the gap blocks `fill_export_gaps`
/// adds for `continuity_pct`. Blocks that are forced to charge or must not discharge cannot
/// export, so they take no slot. `blocks` is every known block, not only the horizon: the
/// started blocks of today come in as ranking context (`EvaluationContext::ranking_price_blocks`).
fn export_eligible(
    blocks: &[PlanBlock],
    tz: Tz,
    in_window: fn(u32) -> bool,
    budget: DayBudget,
    continuity_pct: f32,
) -> Vec<usize> {
    let mut known_minutes: std::collections::HashMap<NaiveDate, u32> =
        std::collections::HashMap::new();
    for b in blocks {
        *known_minutes
            .entry(b.start.with_timezone(&tz).date_naive())
            .or_default() += b.duration_minutes;
    }
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
    let mut limit = (0, 1);
    for (date, i) in ranked {
        if day != Some(date) {
            day = Some(date);
            taken = 0;
            limit = budget.minutes(
                known_minutes.get(&date).copied().unwrap_or(0),
                local_day_minutes(date, tz),
            );
        }
        if taken * limit.1 < limit.0 {
            eligible.push(i);
            taken += blocks[i].duration_minutes;
        }
    }
    if continuity_pct > 0.0 && continuity_pct.is_finite() {
        let filled = fill_export_gaps(blocks, tz, in_window, &eligible, continuity_pct);
        eligible.extend(filled);
    }
    eligible
}

/// Run continuity of a discharge window (owner 2026-11-05: "continuity can be accepted within
/// 5 % margin"; a Hold now and then is fine, an Export/Hold flip on every block is not). Returns
/// the blocks that fill a gap of one or two blocks between two `chosen` blocks of the same window
/// and local day, so a run is never extended outward:
/// - a one-block gap fills when its sell price is at most `pct` % below the sell price of the
///   chosen block before it or of the one after it (either neighbour);
/// - a two-block gap fills only as a whole, when each block sells at most `pct` % below the
///   chosen block it touches;
/// - longer gaps stay as the lever chose: a longer pause in a run is not a flip-flop.
///
/// Only the lever's own choice counts as a neighbour, so filled blocks never chain into further
/// gaps. A gap block must be able to export (no forced charge, no no-discharge rule). The margin
/// is `|neighbour| × pct / 100` below the neighbour, so a negative price is never loosened past
/// itself the wrong way.
fn fill_export_gaps(
    blocks: &[PlanBlock],
    tz: Tz,
    in_window: fn(u32) -> bool,
    chosen: &[usize],
    pct: f32,
) -> Vec<usize> {
    let mut sorted = chosen.to_vec();
    sorted.sort_unstable();
    let local = |i: usize| blocks[i].start.with_timezone(&tz);
    let within = |gap: usize, neighbour: usize| {
        let reference = blocks[neighbour].sell_czk;
        blocks[gap].sell_czk >= reference - reference.abs() * pct / 100.0
    };
    let can_fill = |i: usize, date: NaiveDate| {
        let b = &blocks[i];
        let at = local(i);
        !b.force_charge && !b.no_discharge && in_window(at.hour()) && at.date_naive() == date
    };
    let mut filled = Vec::new();
    for pair in sorted.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        let gap = after - before - 1;
        let date = local(before).date_naive();
        if !(1..=2).contains(&gap) || local(after).date_naive() != date {
            continue;
        }
        let gap_blocks: Vec<usize> = (before + 1..after).collect();
        if !gap_blocks.iter().all(|&i| can_fill(i, date)) {
            continue;
        }
        let fills = match gap_blocks[..] {
            [only] => within(only, before) || within(only, after),
            [first, second] => within(first, before) && within(second, after),
            // Longer gaps are skipped above
            _ => false,
        };
        if fills {
            filled.extend(gap_blocks);
        }
    }
    filled
}

/// A discharge window: its lever limit and its local-hour test.
type DischargeWindow = (Option<DischargeWindowLimit>, fn(u32) -> bool);

/// Price and discharge rules of the user levers (CONTRACT "Price levers", "Discharge levers").
/// `None` levers, or levers with no rule set, leave the blocks as the plain planner has them.
/// Discharge windows need a timezone; without one, export is unrestricted.
fn apply_user_levers(
    blocks: &mut [PlanBlock],
    levers: Option<&UserLevers>,
    tz: Option<Tz>,
    continuity_pct: f32,
) {
    let Some(levers) = levers else { return };
    apply_price_levers(blocks, levers);

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
        let budget = DayBudget::Hours(limit.hours);
        for i in export_eligible(blocks, tz, in_window, budget, continuity_pct) {
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

/// Any local hour: the `daily_top_n` export selection ranks the whole local day 00-24.
fn any_hour(_hour: u32) -> bool {
    true
}

/// Export selection `daily_top_n` (owner 2026-11-05): Export may run only in the best-selling
/// `hours` of each local day 00-24 (`hours` × 4 quarter-hour blocks), wherever they fall
/// (`export_eligible` over the whole day, started blocks included, ties to the earlier block),
/// plus the continuity gap fills. A partly published future day gets its share
/// (`DayBudget::ShareOfDay`). The price levers apply as
/// in `apply_user_levers`; a forced-charge or no-discharge block takes no slot. The morning and
/// evening discharge-hour levers are ignored; their max-discharge % is honoured: allowed blocks
/// never export below the higher SOC limit of the set windows. With neither window lever set (both
/// lever entities unavailable) there is no such limit and Export runs down to the planner minimum
/// SOC, as the windows mode does without levers. Without a timezone export is
/// unrestricted, as with the window levers. The DP still decides by money within these blocks.
fn apply_daily_top_n(
    blocks: &mut [PlanBlock],
    levers: Option<&UserLevers>,
    tz: Option<Tz>,
    hours: u8,
    continuity_pct: f32,
) {
    if let Some(levers) = levers {
        apply_price_levers(blocks, levers);
    }
    let Some(tz) = tz else { return };
    let min_soc_pct = levers.and_then(|l| {
        [l.morning_discharge, l.evening_discharge]
            .into_iter()
            .flatten()
            .map(|limit| limit.min_soc_pct)
            .reduce(f32::max)
    });
    for block in blocks.iter_mut() {
        block.export_allowed = false;
        block.export_min_soc_pct = None;
    }
    let budget = DayBudget::ShareOfDay(hours.clamp(1, 24));
    for i in export_eligible(blocks, tz, any_hour, budget, continuity_pct) {
        blocks[i].export_allowed = true;
        blocks[i].export_min_soc_pct = min_soc_pct;
    }
}

/// The export rules of `params.export_selection`: the window levers (`apply_user_levers`) or the
/// daily top N (`apply_daily_top_n`).
fn apply_export_rules(
    blocks: &mut [PlanBlock],
    levers: Option<&UserLevers>,
    tz: Option<Tz>,
    params: &WinterReserveParams,
) {
    match params.export_selection {
        ExportSelection::Windows => {
            apply_user_levers(blocks, levers, tz, params.export_window_continuity_pct);
        }
        ExportSelection::DailyTopN => apply_daily_top_n(
            blocks,
            levers,
            tz,
            params.daily_export_hours,
            params.export_window_continuity_pct,
        ),
    }
}

/// Ranking-only blocks for the export rules: the `ranking` blocks before `first` on the local day
/// of `first`, oldest first. They carry prices only; `apply_export_rules_ranked` drops them after
/// ranking, so they are never planned.
fn ranking_prefix(
    ranking: &[TimeBlockPrice],
    first: DateTime<Utc>,
    tz: Tz,
    export_fee_czk: f32,
) -> Vec<PlanBlock> {
    let day = first.with_timezone(&tz).date_naive();
    let mut prefix: Vec<PlanBlock> = ranking
        .iter()
        .filter(|b| b.block_start < first && b.block_start.with_timezone(&tz).date_naive() == day)
        .map(|b| PlanBlock {
            start: b.block_start,
            duration_minutes: b.duration_minutes,
            buy_czk: b.effective_price_czk_per_kwh,
            sell_czk: b.spot_sell_price_czk_per_kwh.unwrap_or(export_fee_czk),
            load_kwh: 0.0,
            pv_kwh: 0.0,
            reserve_load_kwh: 0.0,
            reserve_pv_kwh: 0.0,
            force_charge: false,
            no_discharge: false,
            force_charge_at_or_below_czk: None,
            export_min_soc_pct: None,
            export_allowed: true,
        })
        .collect();
    prefix.sort_by_key(|b| b.start);
    prefix.dedup_by_key(|b| b.start);
    prefix
}

/// `apply_export_rules` over `prefix` followed by `blocks`; returns `blocks` only. `prefix` is
/// today's started blocks (`ranking_prefix`): the per-day ranking sees the whole local day, so the
/// export set does not move as the day passes.
fn apply_export_rules_ranked(
    blocks: Vec<PlanBlock>,
    prefix: Vec<PlanBlock>,
    levers: Option<&UserLevers>,
    tz: Option<Tz>,
    params: &WinterReserveParams,
) -> Vec<PlanBlock> {
    let known = prefix.len();
    let mut all = prefix;
    all.extend(blocks);
    apply_export_rules(&mut all, levers, tz, params);
    all.split_off(known)
}

/// The price levers (CONTRACT "Price levers"): force charge at or below one buy price, no
/// discharge at or below another.
fn apply_price_levers(blocks: &mut [PlanBlock], levers: &UserLevers) {
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
    /// times `load_safety_factor`. Expected PV: aligned forecast, else 0, times `pv_confidence` in
    /// a cautious-PV block (`cautious_pv_at`). Reserve PV: the forecast times `pv_confidence`.
    /// Forecasts are used only when aligned with `all_price_blocks`.
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
        let tz = planner_tz(context);
        let blocks: Vec<PlanBlock> = all
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
                // Cautious PV: the plan itself uses the reserve view of the sun
                let cautious = cautious_pv_at(
                    b.block_start,
                    context.cautious_pv,
                    &self.params.cautious_pv_fallback_months,
                    tz,
                );
                PlanBlock {
                    start: b.block_start,
                    duration_minutes: b.duration_minutes,
                    buy_czk: b.effective_price_czk_per_kwh,
                    sell_czk: b
                        .spot_sell_price_czk_per_kwh
                        .unwrap_or(control.grid_export_fee_czk_per_kwh),
                    load_kwh: expected_load,
                    pv_kwh: if cautious {
                        pv * self.params.pv_confidence
                    } else {
                        pv
                    },
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
        // Levers rank over every known block of the day, the started ones included, so the export
        // set does not move as the day passes.
        if self.params.export_selection == ExportSelection::DailyTopN
            && context
                .user_levers
                .is_some_and(|l| l.morning_discharge.is_some() || l.evening_discharge.is_some())
            && !self.top_n_levers_logged.swap(true, Ordering::Relaxed)
        {
            tracing::info!(
                "WinterReserve: export_selection daily_top_n: the morning and evening discharge-hour levers are ignored, Export may run in the best {} h of each local day; the max-discharge % lever still applies",
                self.params.daily_export_hours.clamp(1, 24)
            );
        }
        let prefix = match (context.timezone, context.ranking_price_blocks, all.first()) {
            (Some(tz), Some(ranking), Some(first)) => ranking_prefix(
                ranking,
                first.block_start,
                tz,
                control.grid_export_fee_czk_per_kwh,
            ),
            _ => Vec::new(),
        };
        let mut blocks = apply_export_rules_ranked(
            blocks,
            prefix,
            context.user_levers,
            context.timezone,
            &self.params,
        );

        blocks.split_off(start)
    }

    fn run_plan(
        &self,
        horizon: &[PlanBlock],
        soc_pct: f32,
        tz: Tz,
        battery: &BatteryParams,
        current: Option<PlanMode>,
        direction: ForceDirection,
    ) -> Plan {
        self.plan_count.fetch_add(1, Ordering::Relaxed);
        plan_from_direction(
            horizon,
            soc_pct,
            tz,
            battery,
            &self.params,
            current,
            direction,
        )
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
    ) -> (PlannedBlock, Option<Replan>) {
        let first = horizon[0].start;
        let last = horizon[horizon.len() - 1].start;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());

        if !is_head {
            let hit = cache
                .as_ref()
                .and_then(|c| c.plan.blocks.iter().find(|b| b.start == first))
                .cloned();
            let entry = hit.unwrap_or_else(|| {
                self.run_plan(horizon, soc_pct, tz, battery, None, ForceDirection::None)
                    .blocks
                    .swap_remove(0)
            });
            return (entry, None);
        }

        let stale = cache.as_ref().is_none_or(|c| {
            c.plan.blocks.first().map(|b| b.start) != Some(first)
                || c.horizon_last_start != last
                || c.price_fingerprint != fingerprint
                || (c.min_soc_pct - battery.min_soc_pct).abs() > 1e-6
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
        let recent = cache
            .as_ref()
            .filter(|c| now - c.planned_at <= Duration::minutes(REPLAN_MAX_AGE_MIN * 2));
        let current = recent.and_then(|c| c.plan.blocks.first()).map(|b| b.mode);
        // The force direction comes from the same plan as `current`, kept through SelfUse/Hold
        // heads, so a block projected after a reversal is decided the same way as head.
        let direction = recent.map_or(ForceDirection::None, |c| c.direction);
        let plan = self.run_plan(horizon, soc_pct, tz, battery, current, direction);
        let entry = plan.blocks[0].clone();
        let replanned = Replan {
            plan: plan.clone(),
            current,
            start_direction: direction,
        };
        *cache = Some(CachedPlan {
            plan,
            planned_at: now,
            horizon_last_start: last,
            price_fingerprint: fingerprint,
            min_soc_pct: battery.min_soc_pct,
            direction: direction.after(entry.mode),
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
        let tz = planner_tz(context);
        let mut battery = battery_params(context.control_config);
        battery.charge_cost_czk_per_kwh = context.battery_charge_cost_czk_per_kwh;
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
        if let Some(replan) = &replanned {
            let inputs = PlanInputs {
                block_forecasts: context.block_forecasts.is_some(),
                profile: context.hourly_consumption_profile.is_some(),
                cautious_pv: context.cautious_pv,
                cautious_any: horizon.iter().any(|b| {
                    cautious_pv_at(
                        b.start,
                        context.cautious_pv,
                        &self.params.cautious_pv_fallback_months,
                        tz,
                    )
                }),
            };
            log_replan(&horizon, replan, live_soc, inputs, tz);
        }
        let head = &horizon[0];

        // Runtime guards, head calls only.
        let mut mode = entry.mode;
        let mut reason = entry.reason.clone();
        if is_head
            && mode == PlanMode::Export
            && export_guard_holds(live_kwh, entry.floor_kwh, &battery)
        {
            mode = PlanMode::SelfUse;
            reason = format!(
                "WinterReserve SELF-USE | guard: export held at night floor {:.1} kWh, SOC {live_soc:.0}%",
                entry.floor_kwh
            );
        } else if is_head
            && mode == PlanMode::Charge
            && live_soc >= battery.max_soc_pct - CHARGE_GUARD_MARGIN_PCT
        {
            mode = PlanMode::SelfUse;
            reason = format!("WinterReserve SELF-USE | guard: charge held at SOC {live_soc:.0}%");
        } else if is_head
            && mode == PlanMode::Charge
            && !head.force_charge
            && live_kwh >= entry.charge_cap_kwh
        {
            mode = PlanMode::SelfUse;
            reason = format!(
                "WinterReserve SELF-USE | guard: charge held at the solar headroom {:.1} kWh, SOC {live_soc:.0}%",
                entry.charge_cap_kwh
            );
        }

        let start_kwh = if is_head {
            live_kwh
        } else {
            entry.soc_start_pct / 100.0 * capacity
        };
        let outcome = simulate_block_capped(
            mode,
            start_kwh,
            head,
            entry.floor_kwh,
            &battery,
            if head.force_charge {
                f32::INFINITY
            } else {
                entry.charge_cap_kwh
            },
        );

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
            hold_pv_to_battery: false,
            min_charge_kwh: 0.0,
            charge_cost_czk_per_kwh: None,
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
    fn sim_hold_solax_charges_battery_from_pv_first() {
        let b = BatteryParams {
            hold_pv_to_battery: true,
            ..battery()
        };
        // PV 5, load 2: the battery takes all 5 kWh of PV, the house is served from the grid
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 2.0, 5.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 5.0));
        assert!(approx(o.energy_end_kwh, 50.0 + 5.0 * 0.9));
        assert!(approx(o.grid_import_kwh, 2.0));
        assert!(approx(o.grid_export_kwh, 0.0));
        assert!(approx(o.battery_discharge_kwh, 0.0));
        // PV 12, load 2: charge limited to 7.5 kWh, 4.5 kWh serves the house, 2.5 exported
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 2.0, 12.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 7.5));
        assert!(approx(o.grid_import_kwh, 0.0));
        assert!(approx(o.grid_export_kwh, 2.5));
        // No PV: the battery is idle and the house imports, as before
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 2.0, 0.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 50.0));
        assert!(approx(o.grid_import_kwh, 2.0));
    }

    #[test]
    fn sim_hold_default_serves_house_from_pv_then_charges_surplus() {
        let b = battery();
        assert!(!b.hold_pv_to_battery);
        // PV 5, load 2: the house takes 2, the battery the 3 kWh surplus, nothing exported
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 2.0, 5.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 3.0));
        assert!(approx(o.energy_end_kwh, 50.0 + 3.0 * 0.9));
        assert!(approx(o.grid_import_kwh, 0.0));
        assert!(approx(o.grid_export_kwh, 0.0));
        // Full battery: the surplus is exported
        let (_, emax) = b.bounds_kwh();
        let o = simulate_block(
            PlanMode::Hold,
            emax,
            &pblock(0, 3.0, 2.0, 2.0, 5.0),
            9.6,
            &b,
        );
        assert!(approx(o.battery_charge_kwh, 0.0));
        assert!(approx(o.grid_export_kwh, 2.5));
        // PV 1, load 2: the battery never discharges, the grid covers the deficit
        let o = simulate_block(
            PlanMode::Hold,
            50.0,
            &pblock(0, 3.0, 2.0, 2.0, 1.0),
            9.6,
            &b,
        );
        assert!(approx(o.energy_end_kwh, 50.0));
        assert!(approx(o.battery_discharge_kwh, 0.0));
        assert!(approx(o.grid_import_kwh, 1.0));
    }

    #[test]
    fn sim_hold_negative_pv_never_discharges() {
        let b = BatteryParams {
            hold_pv_to_battery: true,
            ..battery()
        };
        let (_, emax) = b.bounds_kwh();
        for e in [50.0, emax] {
            let o = simulate_block(PlanMode::Hold, e, &pblock(0, 3.0, 2.0, 2.0, -1.0), 9.6, &b);
            assert!(approx(o.energy_end_kwh, e), "{e}: {}", o.energy_end_kwh);
            assert!(approx(o.battery_charge_kwh, 0.0));
            assert!(approx(o.grid_import_kwh, 2.0));
        }
    }

    /// #648: on the direct VPP path HOLD runs as mode 7, so the option is ignored (the default
    /// HOLD model), with one INFO; on the EEPROM path the option stands
    #[test]
    fn hold_pv_to_battery_is_off_on_the_direct_vpp_path() {
        use crate::vpp::{ModePath, VppPathState};
        use std::sync::atomic::AtomicBool;
        let control = ControlConfig {
            hold_pv_to_battery: true,
            vpp_direct_enabled: true,
            ..ControlConfig::default()
        };
        // The live planner params: no path registered, so the option decides (direct VPP)
        assert!(!battery_params(&control).hold_pv_to_battery);
        let direct = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        let eeprom = VppPathState::new(ModePath::Eeprom("test".to_owned()));
        let logged = AtomicBool::new(false);
        let logs = crate::continuous_systems::levers_execution_tests::captured_logs(|| {
            for _ in 0..3 {
                assert!(!planner_hold_pv_to_battery(
                    &control,
                    Some(&direct),
                    &logged
                ));
            }
            assert!(!planner_hold_pv_to_battery(&control, None, &logged));
        });
        assert_eq!(
            logs.matches("HOLD planned with the default model").count(),
            1,
            "{logs}"
        );
        assert!(logs.contains(" INFO "), "{logs}");
        // The EEPROM path (decided, or the option off) keeps the option
        let logged = AtomicBool::new(false);
        assert!(planner_hold_pv_to_battery(&control, Some(&eeprom), &logged));
        let off = ControlConfig {
            vpp_direct_enabled: false,
            ..control.clone()
        };
        assert!(planner_hold_pv_to_battery(&off, None, &logged));
        assert!(battery_params(&off).hold_pv_to_battery);
        assert!(!logged.load(Ordering::Relaxed));
    }

    #[test]
    fn hold_routing_defaults_follow_the_control_config() {
        let b = battery_params(&ControlConfig::default());
        assert!(!b.hold_pv_to_battery);
        assert!(!WinterReserveParams::default().export_floor_until_cheap);
    }

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
        // The battery is never sold; an Export in the sunny morning sells only PV (HOLD and
        // SelfUse store the surplus, so Export is how PV reaches the grid at 5.5)
        for (i, b) in p.blocks.iter().enumerate() {
            if b.mode == PlanMode::Export {
                assert!(i >= 48, "export at night block {i}");
                assert!(
                    approx(b.outcome.battery_discharge_kwh, 0.0),
                    "battery sold at {i}"
                );
            }
        }
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
        // Evening buy 8.5, night 4, morning 6.5: cheap_ref 4, median 4, cost unknown: export
        // needs sell ≥ 4 / 0.9 + 0.125 + 2.0 ≈ 6.57
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

        // 6.0 - 4 / 0.9 - 0.125 = 1.43 < 2.0 (min_arbitrage_profit_czk)
        let fail = night_blocks((8.5, 6.0), (4.0, 2.0), (6.5, 5.5), 0.75);
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
    fn floor_until_cheap_ends_the_reserve_at_the_next_cheap_block() {
        let tz = chrono_tz::Europe::Prague;
        let b = battery();
        // Load 0.75 everywhere, PV from 08:00 (block 56). Evening 18-22 dear (buy 8), night from
        // 22:00 (block 16) cheap (buy 3), morning dear again.
        let blocks: Vec<PlanBlock> = (0..60)
            .map(|i| {
                let buy = if (16..48).contains(&i) { 3.0 } else { 8.0 };
                pblock(i, buy, buy - 1.0, 0.75, if i >= 56 { 2.0 } else { 0.0 })
            })
            .collect();
        let deficit = |r: std::ops::Range<usize>| -> f32 {
            blocks[r]
                .iter()
                .map(|x| (x.load_kwh - x.pv_kwh).max(0.0))
                .sum()
        };
        let emin = b.bounds_kwh().0;
        let mut params = WinterReserveParams::default();
        let old = night_floors(&blocks, &b, 3.0, &params, tz);
        params.export_floor_until_cheap = true;
        let new = night_floors(&blocks, &b, 3.0, &params, tz);
        // 20:00 (block 8): until the morning without the flag, until 22:00 (block 16) with it
        assert!(approx(old[8], emin + deficit(9..56)), "{}", old[8]);
        assert!(approx(new[8], emin + deficit(9..16)), "{}", new[8]);
        // In the cheap night the next cheap block is the following one: the floor is Emin
        assert!(approx(new[20], emin), "{}", new[20]);
        // 06:00 (block 48) is dear and no cheap block follows before the morning: unchanged
        assert!(approx(new[48], old[48]), "{} vs {}", new[48], old[48]);
    }

    #[test]
    fn floor_until_cheap_ends_an_open_night_at_a_cheap_block() {
        let tz = chrono_tz::Europe::Prague;
        let b = battery();
        // No PV at all (open night): the plain floor is Emax; a cheap block at 22:00 ends it
        let blocks: Vec<PlanBlock> = (0..40)
            .map(|i| {
                let buy = if i >= 16 { 3.0 } else { 8.0 };
                pblock(i, buy, buy - 1.0, 0.75, 0.0)
            })
            .collect();
        let (emin, emax) = b.bounds_kwh();
        let params = WinterReserveParams {
            export_floor_until_cheap: true,
            ..WinterReserveParams::default()
        };
        let old = night_floors(&blocks, &b, 3.0, &WinterReserveParams::default(), tz);
        let new = night_floors(&blocks, &b, 3.0, &params, tz);
        assert!(approx(old[8], emax));
        assert!(approx(new[8], emin + 7.0 * 0.75), "{}", new[8]);
    }

    #[test]
    fn floor_until_cheap_ignores_a_dear_majority_at_the_median_and_a_flat_horizon() {
        let tz = chrono_tz::Europe::Prague;
        let b = battery();
        let (emin, emax) = b.bounds_kwh();
        let params = WinterReserveParams {
            export_floor_until_cheap: true,
            ..WinterReserveParams::default()
        };
        // Open night, 32 blocks at 4 then 8 at 2: the median is the dear 4, not cheap
        let blocks: Vec<PlanBlock> = (0..40)
            .map(|i| {
                let buy = if i >= 32 { 2.0 } else { 4.0 };
                pblock(i, buy, 5.0, 0.75, 0.0)
            })
            .collect();
        let new = night_floors(&blocks, &b, 2.0, &params, tz);
        assert!(approx(new[0], (emin + 31.0 * 0.75).min(emax)), "{}", new[0]);
        // Flat prices: nothing is cheap, the floor stays as without the option
        let flat: Vec<PlanBlock> = (0..40).map(|i| pblock(i, 5.0, 8.0, 0.75, 0.0)).collect();
        let old = night_floors(&flat, &b, 5.0, &WinterReserveParams::default(), tz);
        let new = night_floors(&flat, &b, 5.0, &params, tz);
        assert_eq!(old, new);
        assert!(approx(new[0], emax));
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
        // Without the reversal cost: one value table, the budget before it (200 ms). With it (the
        // default 2.0, Charge and Export both possible here): two tables, about twice the work
        // (2026-11-05, debug build: 116-204 ms against 56-128 ms before, single test run), so
        // twice the budget.
        for (cost, budget_ms) in [(0.0, 200), (2.0, 400)] {
            let params = WinterReserveParams {
                direction_reversal_cost_czk: cost,
                ..WinterReserveParams::default()
            };
            // The fastest of 3 runs, so a loaded host (parallel test threads) does not fail it
            // while a slower planner still does.
            let fastest = (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let p = plan(
                        &blocks,
                        50.0,
                        chrono_tz::Europe::Prague,
                        &battery(),
                        &params,
                    );
                    let elapsed = start.elapsed();
                    assert_eq!(p.blocks.len(), 192);
                    elapsed
                })
                .min()
                .unwrap();
            assert!(
                fastest < std::time::Duration::from_millis(budget_ms),
                "cost {cost}: fastest of 3 plans took {fastest:?}"
            );
        }
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
            ranking_price_blocks: None,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 3.0,
            battery_charge_cost_czk_per_kwh: None,
            hourly_consumption_profile: None,
            hourly_consumption_p75: None,
            block_forecasts: None,
            timezone: None,
            user_levers: None,
            cautious_pv: None,
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
    fn raised_min_soc_floor_replans_at_once() {
        let blocks = price_blocks();
        let s = strategy();
        s.evaluate(&ctx(&blocks, 0, &control(), 50.0));
        // The Back Up floor changes the effective minimum SOC: the cached plan must not be reused
        let mut raised = control();
        raised.min_battery_soc = 40.0;
        s.evaluate(&ctx(&blocks, 0, &raised, 50.0));
        assert_eq!(s.plan_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn soc_below_floor_is_planned_from_real_soc_and_never_discharged() {
        let blocks = price_blocks();
        let mut ctl = control();
        ctl.min_battery_soc = 40.0;
        let s = strategy();
        let horizon = s.horizon_blocks(&ctx(&blocks, 0, &ctl, 20.0));
        let battery = battery_params(&ctl);
        let plan = s.run_plan(
            &horizon,
            20.0,
            chrono_tz::UTC,
            &battery,
            None,
            ForceDirection::None,
        );
        // No phantom start at the floor
        assert!(
            (plan.blocks[0].soc_start_pct - 20.0).abs() < 0.01,
            "phantom start: {}",
            plan.blocks[0].soc_start_pct
        );
        // Below the floor the battery may charge but never discharge
        for b in &plan.blocks {
            assert!(
                b.soc_end_pct >= b.soc_start_pct - 0.01,
                "discharge below floor at {}: {:?}",
                b.start,
                b.mode
            );
        }
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
        let direction = ForceDirection::of(Some(entry.mode));
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
            min_soc_pct: control().min_battery_soc,
            direction,
        });
    }

    fn entry(mode: PlanMode, soc_start: f32, soc_end: f32, floor: f32) -> PlannedBlock {
        PlannedBlock {
            start: base(),
            mode,
            soc_start_pct: soc_start,
            soc_end_pct: soc_end,
            floor_kwh: floor,
            charge_cap_kwh: f32::INFINITY,
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
    /// Buy is 6.0 for 8 blocks, then `later`.
    fn pv_block_zero_plan(load: f32, later: f32, previous: PlanMode) -> PlanMode {
        let tz = chrono_tz::Europe::Prague;
        let params = WinterReserveParams::default();
        let b = BatteryParams {
            capacity_kwh: 10.0,
            ..battery()
        };
        let mut blocks: Vec<PlanBlock> = (0..8).map(|i| pblock(i, 6.0, 2.0, load, 0.0)).collect();
        blocks.extend((8..16).map(|i| pblock(i, later, 2.0, load, 0.0)));
        blocks[0].pv_kwh = 0.3;
        let p = plan_from(&blocks, 40.0, tz, &b, &params, Some(previous));
        p.blocks[0].mode
    }

    #[test]
    fn pv_block_keeps_applied_selfuse_when_hold_gain_is_small() {
        // Block 0 has PV. Later buy 6.2: Hold is the free choice, but over its whole run it gains
        // only ~0.6 CZK, under the 1.0 CZK switch threshold: SelfUse is kept, since keeping
        // SelfUse never blocks PV charging. (Before H25: later 8.0, Hold gained 0.95 CZK at
        // block 0 alone; that case now switches, see below.)
        assert_eq!(
            pv_block_zero_plan(1.5, 6.2, PlanMode::SelfUse),
            PlanMode::SelfUse
        );
    }

    #[test]
    fn pv_block_switches_to_hold_when_gain_is_above_threshold() {
        // Block 0 has PV. Hold gains 1.19 CZK at block 0, above the 1.0 CZK switch threshold
        assert_eq!(
            pv_block_zero_plan(2.0, 8.0, PlanMode::SelfUse),
            PlanMode::Hold
        );
        // H25: the switch threshold counts the whole Hold run, not one block of delay. At load 1.5
        // Hold gains 0.95 CZK at block 0 but more than 1.0 CZK over its 7-block run: switch.
        assert_eq!(
            pv_block_zero_plan(1.5, 8.0, PlanMode::SelfUse),
            PlanMode::Hold
        );
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
    fn mode_switch_margin_zero_switches_and_one_keeps() {
        // `select_mode` margin: the forward pass passes 0 and applies the run-based keep rule
        // itself (H25). Previous Hold, SelfUse 0.5 CZK better: the switch happens with 0, and Hold
        // stays with 1.0.
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
        let horizon = WinterReserveStrategy::new(true, params.clone())
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
            min_charge_kwh: 0.0,
            force_discharge_enabled,
            hold_pv_to_battery: false,
            charge_cost_czk_per_kwh: None,
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

/// Keep-rule tests of brief H25: projection == head decision, no Hold lock-in, and the
/// closed-loop comparison of keep rules (ignored; prints the table quoted in the H25 report).
#[cfg(test)]
mod keep_rule_tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    /// 2026-10-17 00:00 Europe/Prague (CEST)
    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 16, 22, 0, 0).unwrap()
    }

    fn blk(i: usize, buy: f32, sell: f32, load: f32, pv: f32) -> PlanBlock {
        PlanBlock {
            start: start() + Duration::minutes(15 * i as i64),
            duration_minutes: 15,
            buy_czk: buy,
            sell_czk: sell,
            load_kwh: load,
            pv_kwh: pv,
            reserve_load_kwh: load * 1.1,
            reserve_pv_kwh: pv * 0.7,
            force_charge: false,
            no_discharge: false,
            force_charge_at_or_below_czk: None,
            export_min_soc_pct: None,
            export_allowed: true,
        }
    }

    fn battery(capacity_kwh: f32) -> BatteryParams {
        BatteryParams {
            capacity_kwh,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.125,
            max_export_kw: 10.0,
            export_guard_margin_kwh: 0.0,
            min_charge_kwh: 0.0,
            force_discharge_enabled: false,
            hold_pv_to_battery: false,
            charge_cost_czk_per_kwh: None,
        }
    }

    const TZ: Tz = chrono_tz::Europe::Prague;

    /// The flat buy price of the 2026-10-17 evidence (CZK/kWh).
    #[expect(clippy::approx_constant, reason = "a price, not TAU")]
    const FLAT_BUY: f32 = 6.28;

    /// The evidence of 2026-10-17: Hold applied at 80 % SOC, flat buy 6.28, no PV, and SelfUse
    /// is the cheaper mode (the rest of the horizon is cheaper, 5.5). Before H25 the head kept
    /// Hold block after block, since switching one block later always cost less than 1 CZK.
    #[test]
    fn applied_hold_at_flat_price_is_released_at_the_next_boundary() {
        let params = WinterReserveParams::default();
        let b = battery(20.0);
        let blocks: Vec<PlanBlock> = (0..72)
            .map(|i| blk(i, if i < 16 { FLAT_BUY } else { 5.5 }, 1.0, 0.4, 0.0))
            .collect();
        let free = plan_from(&blocks, 80.0, TZ, &b, &params, None);
        assert_eq!(free.blocks[0].mode, PlanMode::SelfUse);
        // Head replan with Hold applied: SelfUse at once, and the plan does not return to Hold
        let p = plan_from(&blocks, 80.0, TZ, &b, &params, Some(PlanMode::Hold));
        let modes: Vec<PlanMode> = p.blocks.iter().map(|x| x.mode).collect();
        assert_eq!(modes[0], PlanMode::SelfUse, "{modes:?}");
        assert!(!modes.contains(&PlanMode::Hold), "{modes:?}");
        // The old per-block rule locked Hold in for the whole horizon
        let old = plan_with_rule(
            &blocks,
            80.0,
            TZ,
            &b,
            &params,
            Some(PlanMode::Hold),
            ForceDirection::None,
            KeepRule::PerBlock,
        );
        assert!(old.blocks.iter().all(|x| x.mode == PlanMode::Hold));
        // Full objective: block costs minus the energy left, valued at cheap_ref minus wear
        let objective = |plan: &Plan| {
            let end = plan.blocks.last().map_or(0.0, |x| x.outcome.energy_end_kwh);
            plan.expected_cost_czk - end * (plan.cheap_ref_czk - b.wear_cost_czk_per_kwh)
        };
        assert!(
            objective(&old) > objective(&p) + 3.0,
            "lock-in {:.2} vs released {:.2}",
            objective(&old),
            objective(&p)
        );
    }

    /// A switch that saves less than the threshold over its run keeps the applied mode; one
    /// that saves more switches at once.
    #[test]
    fn switch_needs_the_threshold_over_its_run() {
        let params = WinterReserveParams::default();
        let zero = WinterReserveParams {
            mode_switch_min_benefit_czk: 0.0,
            ..params.clone()
        };
        let b = battery(20.0);
        // Hold applied; block 0 is dearer than the rest of the night, so SelfUse is the free choice
        let plan_at = |head_buy: f32, p: &WinterReserveParams| {
            let mut blocks: Vec<PlanBlock> = (0..40)
                .map(|i| blk(i, if i < 24 { 5.0 } else { 9.0 }, 1.0, 0.4, 0.0))
                .collect();
            blocks[0].buy_czk = head_buy;
            plan_from(&blocks, 30.0, TZ, &b, p, Some(PlanMode::Hold)).blocks[0].mode
        };
        // Small gain (head 9.2): kept; without a threshold it switches
        assert_eq!(plan_at(9.2, &zero), PlanMode::SelfUse);
        assert_eq!(plan_at(9.2, &params), PlanMode::Hold);
        // Large gain (head 11.0): switches
        assert_eq!(plan_at(11.0, &params), PlanMode::SelfUse);
    }

    /// Projection == head decision: the mode projected for block k is the head decision of a
    /// replan at block k with unchanged inputs (projected SOC, previous planned mode applied).
    /// Before H25 the projection had no hysteresis while the head had it, so 36 of these 180
    /// scenarios disagreed (the 04:00 SELF-USE, 04:05 HOLD flap).
    #[test]
    fn projection_matches_head_decision_across_consecutive_replans() {
        let params = WinterReserveParams::default();
        let mut mismatches = Vec::new();
        let mut compared = 0;
        for cap in [10.0f32, 20.0, 40.0] {
            for load in [0.2f32, 0.4, 0.6, 1.0] {
                for later in [5.0f32, 6.0, 6.5, 7.0, 8.0] {
                    for soc in [30.0f32, 50.0, 80.0] {
                        let b = BatteryParams {
                            force_discharge_enabled: true,
                            ..battery(cap)
                        };
                        let blocks: Vec<PlanBlock> = (0..72)
                            .map(|i| blk(i, if i < 16 { FLAT_BUY } else { later }, 1.0, load, 0.0))
                            .collect();
                        let p = plan_from(&blocks, soc, TZ, &b, &params, Some(PlanMode::Hold));
                        for k in 1..8 {
                            // Same inputs: the shorter horizon must keep the cheap reference, the
                            // median and the floor (otherwise the head may rightly differ).
                            if (cheap_reference(
                                &blocks[k..].iter().map(|x| x.buy_czk).collect::<Vec<_>>(),
                                params.cheap_percentile,
                            ) - p.cheap_ref_czk)
                                .abs()
                                > 1e-4
                            {
                                continue;
                            }
                            let head = plan_from(
                                &blocks[k..],
                                p.blocks[k].soc_start_pct,
                                TZ,
                                &b,
                                &params,
                                Some(p.blocks[k - 1].mode),
                            );
                            if (head.median_buy_czk - p.median_buy_czk).abs() > 1e-4
                                || (head.blocks[0].floor_kwh - p.blocks[k].floor_kwh).abs() > 1e-4
                            {
                                continue;
                            }
                            compared += 1;
                            let (projected, decided) = (p.blocks[k].mode, head.blocks[0].mode);
                            // Charge comes from the cheap reference of the shorter horizon, not
                            // from the keep rule; only SelfUse/Hold disagreements count here.
                            if projected != decided
                                && switch_only(projected)
                                && switch_only(decided)
                            {
                                mismatches.push(format!(
                                    "cap {cap} load {load} later {later} soc {soc} k {k}: \
                                     projected {projected:?}, head {decided:?}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(compared >= 600, "only {compared} comparable replans");
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    /// xorshift64*: deterministic noise without a new dependency.
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / (1u64 << 24) as f32
        }

        /// Uniform in [1 - spread, 1 + spread].
        fn factor(&mut self, spread: f32) -> f32 {
            1.0 + spread * (2.0 * self.unit() - 1.0)
        }
    }

    #[derive(Clone, Copy)]
    struct Scenario {
        name: &'static str,
        /// Flat 6.28 today; the next day (after 96 blocks) costs this
        next_day: f32,
        varying: bool,
        pv: bool,
        /// Load multiplier (0.5 = a small house, where one block of delay costs little)
        load_scale: f32,
        soc: f32,
    }

    fn local_hour(t: DateTime<Utc>) -> f32 {
        let l = t.with_timezone(&TZ);
        l.hour() as f32 + l.minute() as f32 / 60.0
    }

    fn truth_block(sc: Scenario, i: usize, rng: &mut Rng) -> PlanBlock {
        let h = local_hour(start() + Duration::minutes(15 * i as i64));
        let buy = if !sc.varying {
            if i < 96 { FLAT_BUY } else { sc.next_day }
        } else {
            let base = match h {
                h if h < 6.0 => 4.6,
                h if h < 9.0 => 7.8,
                h if h < 16.0 => 5.6,
                h if h < 21.0 => 8.9,
                _ => 5.2,
            };
            base + 0.15 * ((i % 4) as f32 - 1.5) / 1.5
        };
        let load = match h {
            h if h < 6.0 => 0.3,
            h if h < 9.0 => 0.55,
            h if h < 16.0 => 0.4,
            h if h < 22.0 => 0.7,
            _ => 0.4,
        } * sc.load_scale
            * rng.factor(0.1);
        let pv = if sc.pv && (8.0..16.0).contains(&h) {
            0.9 * (std::f32::consts::PI * (h - 8.0) / 8.0).sin()
        } else {
            0.0
        };
        blk(i, buy, buy * 0.7, load, pv)
    }

    #[derive(Default, Clone, Copy)]
    struct LoopResult {
        cost: f32,
        writes: u32,
        flips: u32,
        /// A-B-A over three consecutive boundaries
        ping_pong: u32,
        /// A-B-A-B over four consecutive boundaries
        ping_pong4: u32,
    }

    /// One day closed loop: the head replans every 5 minutes on noisy forecasts (load and PV
    /// ±15 % per replan, buy ±0.05 CZK) from the true SOC. Only the replan at the boundary is
    /// written (executor block lock and boundary wait, brief H25 B and C); the planner's
    /// `current` is the last head decision, as in `entry_for`. Reality runs the true flows.
    fn closed_loop(
        sc: Scenario,
        seed: u64,
        params: &WinterReserveParams,
        rule: KeepRule,
        noise: f32,
    ) -> LoopResult {
        let b = battery(20.0);
        let mut truth_rng = Rng(seed);
        let truth: Vec<PlanBlock> = (0..96 + 72)
            .map(|i| truth_block(sc, i, &mut truth_rng))
            .collect();
        let mut rng = Rng(seed ^ 0x9E37_79B9_7F4A_7C15);
        let start_energy = sc.soc / 100.0 * b.capacity_kwh;
        let mut energy = start_energy;
        let mut current = Some(PlanMode::Hold);
        let mut applied: Vec<PlanMode> = vec![PlanMode::Hold];
        let mut result = LoopResult::default();
        for i in 0..96 {
            let mut end_energy = energy;
            for r in 0..3 {
                let horizon: Vec<PlanBlock> = truth[i..i + 72]
                    .iter()
                    .map(|t| {
                        let mut f = *t;
                        f.load_kwh = t.load_kwh * rng.factor(0.15 * noise);
                        f.reserve_load_kwh = f.load_kwh * 1.1;
                        f.pv_kwh = t.pv_kwh * rng.factor(0.15 * noise);
                        f.reserve_pv_kwh = f.pv_kwh * 0.7;
                        f.buy_czk = t.buy_czk + 0.05 * noise * (2.0 * rng.unit() - 1.0);
                        f
                    })
                    .collect();
                let soc_now =
                    (energy + (end_energy - energy) * r as f32 / 3.0) / b.capacity_kwh * 100.0;
                let plan = plan_with_rule(
                    &horizon,
                    soc_now,
                    TZ,
                    &b,
                    params,
                    current,
                    ForceDirection::of(current),
                    rule,
                );
                let head = &plan.blocks[0];
                current = Some(head.mode);
                if r == 0 {
                    let mode = head.mode;
                    let previous = *applied.last().unwrap_or(&mode);
                    if mode != previous {
                        result.writes += 1;
                        if switch_only(mode) && switch_only(previous) {
                            result.flips += 1;
                        }
                    }
                    let n = applied.len();
                    if n >= 2 && applied[n - 2] == mode && previous != mode {
                        result.ping_pong += 1;
                        if n >= 3 && applied[n - 3] == previous {
                            result.ping_pong4 += 1;
                        }
                    }
                    applied.push(mode);
                    let outcome = simulate_block(mode, energy, &truth[i], head.floor_kwh, &b);
                    result.cost += outcome_cost(&outcome, &truth[i], b.wear_cost_czk_per_kwh);
                    end_energy = outcome.energy_end_kwh;
                }
            }
            energy = end_energy;
        }
        // Energy left over is worth what the planner values it at (cheap reference minus wear).
        let buys: Vec<f32> = truth.iter().map(|t| t.buy_czk).collect();
        let terminal =
            (cheap_reference(&buys, params.cheap_percentile) - b.wear_cost_czk_per_kwh).max(0.0);
        result.cost -= (energy - start_energy) * terminal;
        result
    }

    /// Prints the closed-loop keep-rule table of the H25 report (release build, ~1-2 min):
    /// `cargo test -p fluxion-core --release --lib keep_rule_closed_loop_table -- --ignored --nocapture`
    #[test]
    #[ignore = "prints the H25 keep-rule table; slow"]
    fn keep_rule_closed_loop_table() {
        let mut scenarios = Vec::new();
        for (name, next_day, varying, pv, load_scale) in [
            ("flat 6.28, no PV", FLAT_BUY, false, false, 1.0),
            ("flat 6.28, next day 5.5, no PV", 5.5, false, false, 1.0),
            (
                "flat 6.28, next day 5.5, no PV, half load",
                5.5,
                false,
                false,
                0.5,
            ),
            ("flat 6.28, next day 5.5, PV", 5.5, false, true, 1.0),
            ("varying, no PV", 0.0, true, false, 1.0),
            ("varying, no PV, half load", 0.0, true, false, 0.5),
            ("varying, PV", 0.0, true, true, 1.0),
        ] {
            for soc in [80.0, 40.0] {
                scenarios.push(Scenario {
                    name,
                    next_day,
                    varying,
                    pv,
                    load_scale,
                    soc,
                });
            }
        }
        let seeds = [11u64, 23, 37, 41, 59];
        let mut rules: Vec<(String, KeepRule, f32)> = [0.0f32, 0.05, 0.10, 0.25, 0.5, 1.0]
            .iter()
            .map(|&t| (format!("per-block {t:.2}"), KeepRule::PerBlock, t))
            .collect();
        for t in [0.25f32, 0.5, 1.0, 2.0] {
            rules.push((format!("window {t:.2}"), KeepRule::Window, t));
        }
        let run = |rule: KeepRule, t: f32, noise: f32| -> Vec<LoopResult> {
            let params = WinterReserveParams {
                mode_switch_min_benefit_czk: t,
                ..WinterReserveParams::default()
            };
            scenarios
                .iter()
                .flat_map(|&sc| seeds.iter().map(move |&seed| (sc, seed)))
                .map(|(sc, seed)| closed_loop(sc, seed, &params, rule, noise))
                .collect()
        };
        let results: Vec<Vec<LoopResult>> = rules
            .iter()
            .map(|(_, rule, t)| run(*rule, *t, 1.0))
            .collect();
        let noisy_zero = results[0].clone();
        let runs = noisy_zero.len() as f32;
        // Reference: no threshold, planned on the noise-free forecast (the no-threshold optimum)
        let quiet = run(KeepRule::PerBlock, 0.0, 0.0);
        let reference = quiet.clone();
        println!(
            "Without forecast noise (per-block 0.00): writes/24h {:.1}, flips/24h {:.1}, A-B-A {}, A-B-A-B {}",
            quiet.iter().map(|r| r.writes).sum::<u32>() as f32 / runs,
            quiet.iter().map(|r| r.flips).sum::<u32>() as f32 / runs,
            quiet.iter().map(|r| r.ping_pong).sum::<u32>(),
            quiet.iter().map(|r| r.ping_pong4).sum::<u32>(),
        );
        println!(
            "| rule | mean Δcost CZK/day vs optimum | worst Δcost CZK/day | mean Δ vs noisy no-threshold | writes/24h | SelfUse<->Hold flips/24h | A-B-A (total, {runs} days) | A-B-A-B (total) |"
        );
        println!("|---|---|---|---|---|---|---|---|");
        for ((name, _, _), res) in rules.iter().zip(&results) {
            let deltas: Vec<f32> = res
                .iter()
                .zip(&reference)
                .map(|(r, base)| r.cost - base.cost)
                .collect();
            let mean = deltas.iter().sum::<f32>() / runs;
            let worst = deltas.iter().copied().fold(f32::MIN, f32::max);
            let worst_at = deltas.iter().position(|d| *d == worst).unwrap_or(0);
            let worst_sc = scenarios[worst_at / seeds.len()];
            eprintln!(
                "{name}: worst {worst:+.2} in {} SOC {} seed {}",
                worst_sc.name,
                worst_sc.soc,
                seeds[worst_at % seeds.len()]
            );
            let writes = res.iter().map(|r| r.writes).sum::<u32>() as f32 / runs;
            let flips = res.iter().map(|r| r.flips).sum::<u32>() as f32 / runs;
            let pp: u32 = res.iter().map(|r| r.ping_pong).sum();
            let pp4: u32 = res.iter().map(|r| r.ping_pong4).sum();
            let vs_noisy = res
                .iter()
                .zip(&noisy_zero)
                .map(|(r, z)| r.cost - z.cost)
                .sum::<f32>()
                / runs;
            println!(
                "| {name} | {mean:+.2} | {worst:+.2} | {vs_noisy:+.2} | {writes:.1} | {flips:.1} | {pp} | {pp4} |"
            );
        }
        println!();
        println!(
            "Per scenario (mean of {} seeds), reference = per-block 0.00:",
            seeds.len()
        );
        for (si, sc) in scenarios.iter().enumerate() {
            let pick = |v: &[LoopResult]| -> LoopResult {
                let mut acc = LoopResult::default();
                for r in &v[si * seeds.len()..(si + 1) * seeds.len()] {
                    acc.cost += r.cost / seeds.len() as f32;
                    acc.writes += r.writes;
                    acc.flips += r.flips;
                    acc.ping_pong += r.ping_pong;
                    acc.ping_pong4 += r.ping_pong4;
                }
                acc
            };
            let base = pick(&reference);
            let q = pick(&quiet);
            let mut line = format!(
                "{} SOC {}: ref {:.2} CZK, writes {} (no noise: w{} pp{})",
                sc.name, sc.soc, base.cost, base.writes, q.writes, q.ping_pong
            );
            for ((name, _, _), res) in rules.iter().zip(&results) {
                {
                    let r = pick(res);
                    line += &format!(
                        " | {name}: {:+.2} w{} pp{}/{}",
                        r.cost - base.cost,
                        r.writes,
                        r.ping_pong,
                        r.ping_pong4
                    );
                }
            }
            println!("{line}");
        }
    }
}

/// Brief H30: no tiny top-up charges near the maximum SOC.
#[cfg(test)]
mod h30_tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    const TZ: Tz = chrono_tz::Europe::Prague;

    /// 2026-10-19 03:00 UTC (05:00 Europe/Prague, CEST), the first block of the evidence
    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 3, 0, 0).unwrap()
    }

    /// The evidence site class: 23 kWh, Back Up minimum 25 %, 10 kW charge, export disabled.
    fn battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 25.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.125,
            max_export_kw: 10.0,
            export_guard_margin_kwh: 0.0,
            min_charge_kwh: min_charge_kwh(10.0, 300),
            force_discharge_enabled: false,
            hold_pv_to_battery: false,
            charge_cost_czk_per_kwh: None,
        }
    }

    /// Blocks from `first` (index from 03:00 UTC) to the end of the local day: cheap night
    /// (4.86, 5.07, ... like the evidence), morning and evening peaks, a sunny midday.
    fn day_from(first: usize) -> Vec<PlanBlock> {
        (first..76)
            .map(|i| {
                let t = start() + Duration::minutes(15 * i as i64);
                let l = t.with_timezone(&TZ);
                let h = l.hour() as f32 + l.minute() as f32 / 60.0;
                let buy = match h {
                    h if h < 6.0 => [4.86, 5.07, 5.2, 5.3][i % 4],
                    h if h < 9.0 => 6.9,
                    h if h < 16.0 => 6.0,
                    h if h < 21.0 => 7.6,
                    _ => 6.2,
                };
                let pv = if (9.0..16.0).contains(&h) {
                    0.6 * (std::f32::consts::PI * (h - 9.0) / 7.0).sin()
                } else {
                    0.0
                };
                let load = 0.35;
                PlanBlock {
                    start: t,
                    duration_minutes: 15,
                    buy_czk: buy,
                    sell_czk: buy - 3.5,
                    load_kwh: load,
                    pv_kwh: pv,
                    reserve_load_kwh: load * 1.1,
                    reserve_pv_kwh: pv * 0.7,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                }
            })
            .collect()
    }

    fn head(first: usize, soc: f32) -> PlannedBlock {
        let params = WinterReserveParams::default();
        plan_from(&day_from(first), soc, TZ, &battery(), &params, None)
            .blocks
            .swap_remove(0)
    }

    /// 2026-10-19 evidence: 03:00 CHARGE 98→100, guard at 03:07; 03:15 CHARGE 98→100 again,
    /// guard at 03:20; 03:30 HOLD at 97. A top-up of 2 % (0.46 kWh) is not a Charge block, and
    /// neither is the block after a guard stop (SOC 99).
    #[test]
    fn tiny_top_up_near_full_is_not_planned() {
        for (first, soc) in [(0, 98.0), (1, 98.0), (1, 99.0), (2, 97.0), (0, 97.0)] {
            let b = head(first, soc);
            assert_ne!(
                b.mode,
                PlanMode::Charge,
                "block {first} SOC {soc}: {}",
                b.reason
            );
        }
    }

    /// A real charge is chosen as before: from 90 % the cheap 03:00 block charges a full block
    /// (2.25 kWh stored, to 99.8 %), and from 95 % it tops up to 100 % (0.92 kWh to the guard
    /// at 99 %, above the 0.83 kWh minimum).
    #[test]
    fn meaningful_charge_is_unchanged() {
        for soc in [90.0, 95.0] {
            let b = head(0, soc);
            assert_eq!(b.mode, PlanMode::Charge, "SOC {soc}: {}", b.reason);
            assert!(b.soc_end_pct > 99.5, "{}", b.reason);
        }
    }

    /// No planned (unforced) Charge block anywhere in the horizon stores less than the minimum.
    #[test]
    fn no_planned_charge_block_is_below_the_minimum() {
        let bat = battery();
        let params = WinterReserveParams::default();
        for first in 0..4 {
            for soc in (60..=100).map(|s| s as f32) {
                let p = plan_from(&day_from(first), soc, TZ, &bat, &params, None);
                for b in p.blocks.iter().filter(|b| b.mode == PlanMode::Charge) {
                    let stored = (b.soc_end_pct - b.soc_start_pct) / 100.0 * bat.capacity_kwh;
                    assert!(
                        stored + 1e-4 >= bat.min_charge_kwh,
                        "first {first} SOC {soc}: {} stores {stored:.3} kWh",
                        b.reason
                    );
                }
            }
        }
    }

    /// Review H30 #1: the minimum is stored energy, so low-power sites (0.5 kWh floor) do not
    /// top up 98→100 either. 2 % of 23 kWh is 0.46 kWh stored (0.51 kWh drawn).
    #[test]
    fn low_power_sites_do_not_top_up_two_percent() {
        let params = WinterReserveParams::default();
        for kw in [5.0f32, 3.0] {
            let bat = BatteryParams {
                max_charge_kw: kw,
                max_discharge_kw: kw,
                min_charge_kwh: min_charge_kwh(kw, 300),
                ..battery()
            };
            for (first, soc) in [(0, 98.0), (1, 98.0), (0, 99.0), (0, 97.0)] {
                let b = plan_from(&day_from(first), soc, TZ, &bat, &params, None)
                    .blocks
                    .swap_remove(0);
                assert_ne!(b.mode, PlanMode::Charge, "{kw} kW SOC {soc}: {}", b.reason);
            }
        }
    }

    /// Review H30 #2: on both site shapes (23 kWh / 10 kW, 96 kWh / 30 kW), no planned Charge
    /// block reaches the guard (max SOC − 1 %) within one 300 s debounce. A charge that stays
    /// below the guard for the whole block never trips it.
    #[test]
    fn no_planned_charge_trips_the_guard_within_one_debounce() {
        let params = WinterReserveParams::default();
        for (capacity, kw) in [(23.0f32, 10.0f32), (96.0, 30.0)] {
            let bat = BatteryParams {
                capacity_kwh: capacity,
                max_charge_kw: kw,
                max_discharge_kw: kw,
                min_charge_kwh: min_charge_kwh(kw, 300),
                ..battery()
            };
            let cutoff = bat.guard_cutoff_kwh();
            let stored_kw = kw * bat.efficiency;
            let mut charges = 0;
            for first in 0..4 {
                for half_pct in 120..=200 {
                    let soc = half_pct as f32 / 2.0;
                    let p = plan_from(&day_from(first), soc, TZ, &bat, &params, None);
                    for b in p.blocks.iter().filter(|b| b.mode == PlanMode::Charge) {
                        charges += 1;
                        let start = b.soc_start_pct / 100.0 * capacity;
                        if b.outcome.energy_end_kwh < cutoff {
                            continue;
                        }
                        let secs = (cutoff - start).max(0.0) / stored_kw * 3600.0;
                        assert!(
                            secs >= 300.0,
                            "{capacity} kWh / {kw} kW, head {first} SOC {soc}: {} trips the guard after {secs:.0} s",
                            b.reason
                        );
                    }
                }
            }
            assert!(charges > 0, "{capacity} kWh: no charge planned at all");
        }
    }

    /// The price lever still forces a charge, however small.
    #[test]
    fn forced_charge_is_exempt() {
        let mut blocks = day_from(0);
        blocks[0].force_charge = true;
        let p = plan(
            &blocks,
            99.0,
            TZ,
            &battery(),
            &WinterReserveParams::default(),
        );
        assert_eq!(p.blocks[0].mode, PlanMode::Charge);
    }

    /// The live minimum: one debounce interval at full charge power, at least 0.5 kWh.
    #[test]
    fn live_min_charge_follows_charge_power_and_debounce() {
        let mut control = ControlConfig {
            max_battery_charge_rate_kw: 10.0,
            min_mode_change_interval_secs: 300,
            ..ControlConfig::default()
        };
        assert!((battery_params(&control).min_charge_kwh - 10.0 * 300.0 / 3600.0).abs() < 1e-6);
        control.max_battery_charge_rate_kw = 3.0;
        assert!((battery_params(&control).min_charge_kwh - 0.5).abs() < 1e-6);
    }
}

#[cfg(test)]
mod h33_tests {
    //! Brief H33: the battery as a buffer (solar headroom), cost of charge as the export floor
    //! and the end value.
    use super::*;
    use chrono::TimeZone;

    const TZ: Tz = chrono_tz::Europe::Prague;

    fn battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 10.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 5.0,
            max_discharge_kw: 5.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 5.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: false,
            hold_pv_to_battery: false,
            min_charge_kwh: 0.0,
            charge_cost_czk_per_kwh: None,
        }
    }

    /// 15-minute block `i` counted from `local_hour`:00 on 2026-10-20 (Europe/Prague, CEST).
    fn blk(local_hour: u32, i: i64, buy: f32, load: f32, pv: f32) -> PlanBlock {
        let start = TZ
            .with_ymd_and_hms(2026, 10, 20, local_hour, 0, 0)
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(15 * i);
        PlanBlock {
            start,
            duration_minutes: 15,
            buy_czk: buy,
            sell_czk: 0.5,
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

    fn local_hour(b: &PlanBlock) -> u32 {
        b.start.with_timezone(&TZ).hour()
    }

    /// 08:00-20:00: PV 0.3 kWh/block until 16:00 over a 0.15 kWh load (4.8 kWh surplus in
    /// all), evening load 0.5 kWh/block. Grid is cheap (1.0) from 08:00 to 09:00, else 6.0.
    fn sunny_day(from_hour: u32) -> Vec<PlanBlock> {
        (0..((20 - from_hour) * 4) as i64)
            .map(|i| {
                let b = blk(from_hour, i, 6.0, 0.0, 0.0);
                let h = local_hour(&b);
                let buy = if h < 9 { 1.0 } else { 6.0 };
                let (load, pv) = if h < 16 { (0.15, 0.3) } else { (0.5, 0.0) };
                blk(from_hour, i, buy, load, pv)
            })
            .collect()
    }

    #[test]
    fn sunny_morning_grid_charge_stops_at_max_minus_room() {
        let b = battery();
        let blocks = sunny_day(8);
        let caps = charge_caps(&blocks, &b, TZ);
        // 08:00: room = 32 blocks × 0.15 = 4.8 kWh
        assert!((caps[0] - (10.0 - 4.8)).abs() < 1e-3, "{}", caps[0]);
        let p = plan(&blocks, 10.0, TZ, &b, &WinterReserveParams::default());
        let charges: Vec<&PlannedBlock> = p
            .blocks
            .iter()
            .filter(|x| x.mode == PlanMode::Charge)
            .collect();
        assert!(
            !charges.is_empty(),
            "cheap morning grid should still charge"
        );
        for x in &charges {
            assert!(
                x.outcome.energy_end_kwh <= x.charge_cap_kwh + 1e-3,
                "charged to {} over cap {}",
                x.outcome.energy_end_kwh,
                x.charge_cap_kwh
            );
        }
        let at_nine = p.blocks[4].soc_start_pct;
        assert!(at_nine < 60.0, "grid-charged to {at_nine}% before the sun");
    }

    #[test]
    fn after_late_the_same_day_may_charge_to_full() {
        // From 14:00 the sun is weak: 0.1 kWh PV over 0.15 kWh load until 16:00, cheap grid
        // until 16:00, then a dear evening with 0.7 kWh load per block.
        let b = battery();
        let blocks: Vec<PlanBlock> = sunny_day(14)
            .into_iter()
            .map(|x| {
                let h = local_hour(&x);
                let (buy, pv, load) = if h < 16 {
                    (1.0, 0.1, 0.15)
                } else {
                    (6.0, 0.0, 0.7)
                };
                PlanBlock {
                    buy_czk: buy,
                    pv_kwh: pv,
                    reserve_pv_kwh: pv,
                    load_kwh: load,
                    reserve_load_kwh: load,
                    ..x
                }
            })
            .collect();
        let caps = charge_caps(&blocks, &b, TZ);
        assert!(caps.iter().all(|&c| (c - 10.0).abs() < 1e-6), "{caps:?}");
        let p = plan(&blocks, 10.0, TZ, &b, &WinterReserveParams::default());
        let peak = p
            .blocks
            .iter()
            .map(|x| x.outcome.energy_end_kwh)
            .fold(0.0_f32, f32::max);
        assert!(peak > 9.9, "late day should fill to 100 %, peak {peak}");
    }

    #[test]
    fn night_may_charge_to_full_before_a_sunny_day() {
        // 22:00 tonight to 20:00 tomorrow (local): cheap night, sunny tomorrow.
        let b = battery();
        let blocks: Vec<PlanBlock> = (0..88)
            .map(|i| {
                let x = blk(22, i, 1.0, 0.1, 0.0);
                let h = local_hour(&x);
                let pv = if (9..16).contains(&h) { 0.6 } else { 0.0 };
                let buy = if (22..24).contains(&h) || h < 6 {
                    1.0
                } else {
                    6.0
                };
                // Dear 06:00-09:00 with a high load: worth a full battery
                let load = if (6..9).contains(&h) { 0.8 } else { 0.4 };
                PlanBlock {
                    buy_czk: buy,
                    pv_kwh: pv,
                    reserve_pv_kwh: pv,
                    load_kwh: load,
                    reserve_load_kwh: load,
                    ..x
                }
            })
            .collect();
        let caps = charge_caps(&blocks, &b, TZ);
        for (x, cap) in blocks.iter().zip(&caps) {
            if local_hour(x) >= 22 || local_hour(x) < 9 {
                assert!((cap - 10.0).abs() < 1e-6, "night cap {cap} at {}", x.start);
            }
        }
        assert!(caps.iter().any(|&c| c < 10.0), "daytime is capped");
        let p = plan(&blocks, 10.0, TZ, &b, &WinterReserveParams::default());
        let night_peak = p
            .blocks
            .iter()
            .zip(&blocks)
            .filter(|(_, x)| local_hour(x) < 9 || local_hour(x) >= 22)
            .map(|(pb, _)| pb.outcome.energy_end_kwh)
            .fold(0.0_f32, f32::max);
        assert!(
            night_peak > 9.9,
            "night should fill to 100 %, peak {night_peak}"
        );
    }

    #[test]
    fn cloudy_day_is_not_capped() {
        // PV below the load all day: always "late", no room kept.
        let b = battery();
        let blocks: Vec<PlanBlock> = (0..48)
            .map(|i| {
                let x = blk(8, i, 1.0, 0.3, 0.0);
                let pv = if local_hour(&x) < 16 { 0.2 } else { 0.0 };
                PlanBlock {
                    pv_kwh: pv,
                    reserve_pv_kwh: pv,
                    ..x
                }
            })
            .collect();
        let caps = charge_caps(&blocks, &b, TZ);
        assert!(caps.iter().all(|&c| (c - 10.0).abs() < 1e-6), "{caps:?}");
    }

    #[test]
    fn forced_charge_is_exempt_from_the_headroom() {
        let b = battery();
        let mut blocks = sunny_day(8);
        blocks[0].force_charge = true;
        let caps = charge_caps(&blocks, &b, TZ);
        assert!((caps[0] - 10.0).abs() < 1e-6);
        assert!(caps[1] < 10.0);
    }

    fn export_block(sell: f32) -> PlanBlock {
        PlanBlock {
            sell_czk: sell,
            ..blk(18, 0, 6.0, 0.3, 0.0)
        }
    }

    fn offers_export(sell: f32, cost: Option<f32>, flag: bool) -> bool {
        let b = BatteryParams {
            force_discharge_enabled: flag,
            charge_cost_czk_per_kwh: cost,
            ..battery()
        };
        candidate_modes(
            &export_block(sell),
            6.0,
            1.0,
            &b,
            &WinterReserveParams::default(),
        )
        .contains(&PlanMode::Export)
    }

    #[test]
    fn export_only_when_sell_covers_cost_of_charge_plus_min_profit() {
        // min_arbitrage_profit_czk default 2.0; the cost of charge already includes the wear.
        assert!(offers_export(5.0, Some(2.5), true));
        assert!(offers_export(4.6, Some(2.5), true));
        assert!(!offers_export(4.4, Some(2.5), true));
        assert!(!offers_export(5.0, Some(3.5), true));
        // Never with force_discharge_enabled off
        assert!(!offers_export(50.0, Some(0.1), false));
        // Cost unknown: cheap_ref 1.0 / 0.9 + 0.1 + 2.0 = 3.21
        assert!(offers_export(3.3, None, true));
        assert!(!offers_export(3.1, None, true));
    }

    #[test]
    fn export_never_goes_below_the_night_reserve() {
        let b = BatteryParams {
            force_discharge_enabled: true,
            charge_cost_czk_per_kwh: Some(1.0),
            ..battery()
        };
        // 18:00 dear hour, a night with load, sun from 07:00 next day (16 h horizon)
        let blocks: Vec<PlanBlock> = (0..64)
            .map(|i| {
                let x = blk(18, i, 3.0, 0.1, 0.0);
                let sell = if i < 4 { 8.0 } else { 1.0 };
                let pv = if i >= 52 { 1.0 } else { 0.0 };
                PlanBlock {
                    sell_czk: sell,
                    pv_kwh: pv,
                    reserve_pv_kwh: pv,
                    ..x
                }
            })
            .collect();
        let p = plan(&blocks, 90.0, TZ, &b, &WinterReserveParams::default());
        let exports: Vec<&PlannedBlock> = p
            .blocks
            .iter()
            .filter(|x| x.mode == PlanMode::Export)
            .collect();
        assert!(!exports.is_empty());
        for x in exports {
            assert!(x.outcome.energy_end_kwh >= x.floor_kwh - 1e-3);
        }
    }

    #[test]
    fn terminal_value_is_the_cost_of_charge_capped_at_cheap_ref_minus_wear() {
        let b = battery();
        assert!((terminal_price(&b, 3.0) - 2.9).abs() < 1e-6);
        let with = |cost: f32| BatteryParams {
            charge_cost_czk_per_kwh: Some(cost),
            ..b
        };
        // A cheap (solar) cost lowers the end value
        assert!((terminal_price(&with(1.2), 3.0) - 1.2).abs() < 1e-6);
        // A dear cost is capped at the value before H33
        assert!((terminal_price(&with(5.0), 3.0) - 2.9).abs() < 1e-6);
        assert!((terminal_price(&with(f32::NAN), 3.0) - 2.9).abs() < 1e-6);
        // Flat 3.0 night, nothing to cover: a dear cost of charge does not make buying pay
        let blocks: Vec<PlanBlock> = (0..8).map(|i| blk(1, i, 3.0, 0.0, 0.0)).collect();
        let params = WinterReserveParams::default();
        let unknown = plan(&blocks, 50.0, TZ, &b, &params);
        assert!(unknown.blocks.iter().all(|x| x.mode != PlanMode::Charge));
        let dear = plan(&blocks, 50.0, TZ, &with(5.0), &params);
        assert!(dear.blocks.iter().all(|x| x.mode != PlanMode::Charge));
    }

    /// Reviewer probe: 23 kWh / 10 kW, 18:00 start, 30 h horizon; evening (17-22) 7.5, night
    /// (22-06) 3.5, day 5.0; 0.3 kWh load per block, no PV.
    fn probe_blocks(from: usize) -> Vec<PlanBlock> {
        (0..120)
            .map(|i| {
                let h = local_hour(&blk(18, i, 0.0, 0.0, 0.0));
                let buy = match h {
                    17..=21 => 7.5,
                    22..=23 | 0..=5 => 3.5,
                    _ => 5.0,
                };
                blk(18, i, buy, 0.3, 0.0)
            })
            .skip(from)
            .collect()
    }

    fn probe_battery(cost: Option<f32>) -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            max_export_kw: 10.0,
            efficiency: 0.95,
            charge_cost_czk_per_kwh: cost,
            ..battery()
        }
    }

    #[test]
    fn a_dear_cost_of_charge_does_not_fill_the_battery_at_the_horizon_end() {
        let params = WinterReserveParams::default();
        let blocks = probe_blocks(0);
        let none = plan(&blocks, 50.0, TZ, &probe_battery(None), &params);
        let none_end = none.blocks.last().unwrap().soc_end_pct;
        for cost in [4.0, 8.0] {
            let p = plan(&blocks, 50.0, TZ, &probe_battery(Some(cost)), &params);
            let end = p.blocks.last().unwrap().soc_end_pct;
            assert!(
                end <= none_end + 0.5,
                "cost {cost}: end SOC {end:.1} % vs {none_end:.1} % without a cost"
            );
            let last_night_charge: f32 = p.blocks[112..]
                .iter()
                .map(|x| x.outcome.battery_charge_kwh)
                .sum();
            let none_last_night_charge: f32 = none.blocks[112..]
                .iter()
                .map(|x| x.outcome.battery_charge_kwh)
                .sum();
            assert!(last_night_charge <= none_last_night_charge + 1e-3);
        }
    }

    #[test]
    fn cost_of_charge_cannot_ratchet_with_a_frozen_horizon_end() {
        // Tomorrow's prices missing: every replan ends at the same block. Each replan executes
        // its first block and feeds the measured cost back. Start from a cost inflated by a SOC
        // rise priced at the evening buy (8.0). The executed run must match the run without a
        // cost, and the cost must not climb.
        use crate::battery_cost_basis::{BatteryCostBasis, CostBasisBattery, CostBasisStep};
        let params = WinterReserveParams::default();
        let cb = CostBasisBattery {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            efficiency: 0.95,
            wear_czk_per_kwh: 0.1,
        };
        let run = |start_cost: Option<f32>| {
            let mut basis = BatteryCostBasis {
                cost_czk_per_kwh: start_cost,
                usable_kwh: cb.usable_kwh(50.0),
                ..BatteryCostBasis::default()
            };
            let mut soc = 50.0_f32;
            let mut modes = Vec::new();
            let mut costs = Vec::new();
            for from in 0..120 {
                let blocks = probe_blocks(from);
                let p = plan(
                    &blocks,
                    soc,
                    TZ,
                    &probe_battery(basis.cost_czk_per_kwh),
                    &params,
                );
                let head = &p.blocks[0];
                modes.push(head.mode);
                soc = head.soc_end_pct;
                if start_cost.is_some() {
                    basis.update(
                        &CostBasisStep {
                            soc_pct: soc,
                            battery_charge_kwh: head.outcome.battery_charge_kwh,
                            grid_import_kwh: head.outcome.grid_import_kwh,
                            export_headroom_kwh: None,
                            sell_czk: blocks[0].sell_czk,
                            buy_czk: blocks[0].buy_czk,
                            at: blocks[0].start,
                        },
                        &cb,
                    );
                    costs.push(basis.cost_czk_per_kwh);
                }
            }
            (modes, soc, costs)
        };
        let (none_modes, none_end, _) = run(None);
        let (modes, end, costs) = run(Some(8.0));
        assert_eq!(modes, none_modes);
        assert!((end - none_end).abs() < 0.5, "{end} vs {none_end}");
        let known: Vec<f32> = costs.into_iter().flatten().collect();
        assert!(known.windows(2).all(|w| w[1] <= w[0] + 1e-3), "{known:?}");
    }

    #[test]
    fn head_guard_stops_a_charge_at_the_solar_headroom() {
        let b = battery();
        let blocks = sunny_day(8);
        let cap = charge_caps(&blocks, &b, TZ)[0];
        let at_cap = simulate_block_capped(PlanMode::Charge, cap, &blocks[0], 0.0, &b, cap);
        assert!(at_cap.battery_charge_kwh.abs() < 1e-6);
        let below = simulate_block_capped(PlanMode::Charge, cap - 0.5, &blocks[0], 0.0, &b, cap);
        assert!((below.energy_end_kwh - cap).abs() < 1e-4);
    }
}

/// Owner rules for a fixed-buy, spot-sell site (owner ruling 2026-11-02): an owner-like site
/// with a 23 kWh battery, fixed buy 3.22 (low tariff) / 4.15 (high tariff) and spot sell with
/// a morning and an evening peak.
#[cfg(test)]
mod owner_fixed_buy_tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use chrono_tz::Europe::Prague;

    const LOW: f32 = 3.22;
    const HIGH: f32 = 4.15;

    /// Local midnight 2026-11-03 (CET)
    fn day_start() -> DateTime<Utc> {
        Prague
            .with_ymd_and_hms(2026, 11, 3, 0, 0, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn battery(charge_cost: Option<f32>) -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.95,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 9.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: true,
            hold_pv_to_battery: false,
            min_charge_kwh: 0.0,
            charge_cost_czk_per_kwh: charge_cost,
        }
    }

    fn params() -> WinterReserveParams {
        WinterReserveParams {
            min_arbitrage_profit_czk: 1.5,
            export_floor_until_cheap: true,
            ..WinterReserveParams::default()
        }
    }

    fn levers() -> UserLevers {
        let window = DischargeWindowLimit {
            hours: 4,
            min_soc_pct: 25.0,
        };
        UserLevers {
            morning_discharge: Some(window),
            evening_discharge: Some(window),
            ..UserLevers::default()
        }
    }

    /// High tariff 07–09 and 16–19 local, else low
    fn buy_at(hour: u32) -> f32 {
        if (7..9).contains(&hour) || (16..19).contains(&hour) {
            HIGH
        } else {
            LOW
        }
    }

    /// Net spot sell 4.5–8.5: morning peak 07–09, evening peak 17–20
    fn sell_at(hour: u32) -> f32 {
        match hour {
            7 => 7.6,
            8 => 8.0,
            9 => 6.2,
            10..=15 => 4.5,
            16 => 6.0,
            17 => 8.2,
            18 => 8.5,
            19 => 8.3,
            20..=23 => 5.8,
            _ => 5.0,
        }
    }

    /// Two local days from `day_start() + skip` blocks; `price` maps (local hour) to (buy, sell)
    fn blocks_with(skip: usize, price: impl Fn(u32) -> (f32, f32)) -> Vec<PlanBlock> {
        (skip as i64..192)
            .map(|i| {
                let start = day_start() + Duration::minutes(15 * i);
                let hour = start.with_timezone(&Prague).hour();
                let (buy, sell) = price(hour);
                let pv = if (10..14).contains(&hour) { 0.4 } else { 0.0 };
                PlanBlock {
                    start,
                    duration_minutes: 15,
                    buy_czk: buy,
                    sell_czk: sell,
                    load_kwh: 0.3,
                    pv_kwh: pv,
                    reserve_load_kwh: 0.3 * 1.1,
                    reserve_pv_kwh: pv * 0.7,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                }
            })
            .collect()
    }

    fn owner_plan(skip: usize, soc: f32, cost: Option<f32>) -> (Vec<PlanBlock>, Plan) {
        owner_plan_with(skip, soc, cost, |h| (buy_at(h), sell_at(h)))
    }

    fn owner_plan_with(
        skip: usize,
        soc: f32,
        cost: Option<f32>,
        price: impl Fn(u32) -> (f32, f32),
    ) -> (Vec<PlanBlock>, Plan) {
        let mut blocks = blocks_with(skip, price);
        apply_user_levers(&mut blocks, Some(&levers()), Some(Prague), 0.0);
        let plan = plan_from(&blocks, soc, Prague, &battery(cost), &params(), None);
        (blocks, plan)
    }

    fn hour(b: &PlannedBlock) -> u32 {
        b.start.with_timezone(&Prague).hour()
    }

    /// Without an export ahead, a cost of charge above the buy price keeps SelfUse the default
    #[test]
    fn rule1_cost_of_charge_alone_keeps_self_use() {
        let blocks = blocks_with(0, |_| (LOW, 3.0));
        let b = battery(Some(4.0));
        let mut modes: Vec<Vec<PlanMode>> = blocks
            .iter()
            .map(|x| candidate_modes(x, LOW, LOW, &b, &params()))
            .collect();
        house_from_grid_defaults(&blocks, &mut modes);
        assert!(modes.iter().all(|m| m[0] == PlanMode::SelfUse));
    }

    /// Review S-2a: the best export ahead sells below this block's buy price, so covering the
    /// house now is worth more: SelfUse stays the default everywhere.
    #[test]
    fn rule1_export_ahead_below_buy_keeps_self_use() {
        let blocks = blocks_with(0, |h| (9.0, sell_at(h)));
        let b = battery(None);
        let mut modes: Vec<Vec<PlanMode>> = blocks
            .iter()
            .map(|x| candidate_modes(x, LOW, LOW, &b, &params()))
            .collect();
        assert!(modes.iter().any(|m| m.contains(&PlanMode::Export)));
        house_from_grid_defaults(&blocks, &mut modes);
        assert!(modes.iter().all(|m| m[0] == PlanMode::SelfUse));
    }

    /// Review S-2b: a high sell ahead that may not be exported (`export_allowed` false, e.g. a
    /// discharge lever) is no reason to hold: SelfUse stays the default everywhere.
    #[test]
    fn rule1_sell_ahead_without_export_keeps_self_use() {
        let mut blocks = blocks_with(0, |h| (LOW, sell_at(h)));
        for block in &mut blocks {
            block.export_allowed = false;
        }
        let b = battery(None);
        let mut modes: Vec<Vec<PlanMode>> = blocks
            .iter()
            .map(|x| candidate_modes(x, LOW, LOW, &b, &params()))
            .collect();
        assert!(modes.iter().all(|m| !m.contains(&PlanMode::Export)));
        house_from_grid_defaults(&blocks, &mut modes);
        assert!(modes.iter().all(|m| m[0] == PlanMode::SelfUse));
    }

    /// Review R3: a lever block (force charge `[Charge]`, no discharge `[NoDischarge, ..]`)
    /// keeps its candidates even with a dearer export ahead: never `[Hold, Charge]`.
    #[test]
    fn rule1_never_touches_lever_blocks() {
        let mut blocks = blocks_with(0, |h| (LOW, sell_at(h)));
        blocks[0].force_charge = true;
        blocks[0].force_charge_at_or_below_czk = Some(LOW);
        blocks[1].no_discharge = true;
        let b = battery(None);
        let mut modes: Vec<Vec<PlanMode>> = blocks
            .iter()
            .map(|x| candidate_modes(x, LOW, LOW, &b, &params()))
            .collect();
        assert_eq!(modes[0], vec![PlanMode::Charge]);
        assert_eq!(modes[1], vec![PlanMode::NoDischarge, PlanMode::Charge]);
        house_from_grid_defaults(&blocks, &mut modes);
        assert_eq!(modes[0], vec![PlanMode::Charge]);
        assert_eq!(modes[1], vec![PlanMode::NoDischarge, PlanMode::Charge]);
        // The guard is what keeps them: a plain block with the same export ahead gets Hold first
        assert_eq!(modes[2][0], PlanMode::Hold);
    }

    /// Rule 1b: the battery is kept for a later export that earns more than covering the house
    /// now (sell − cost ≥ 1.5 net): the night blocks before the morning peak run from the grid.
    #[test]
    fn rule1_energy_worth_more_as_export_is_kept_from_the_house() {
        let (_, plan) = owner_plan(0, 80.0, None);
        for b in plan.blocks.iter().filter(|b| hour(b) < 7) {
            assert!(
                b.outcome.battery_discharge_kwh < 1e-6 || b.mode == PlanMode::Export,
                "battery serves the house before the peak: {} {:?}",
                b.start,
                b.mode
            );
        }
    }

    /// Rule 2: export goes into the most profitable blocks. Between two charges the battery
    /// holds a fixed amount of energy, so no export-allowed block that stays idle may sell
    /// higher than a block that exports in the same stretch. Exports stay within the 9 kW cap and
    /// stop at the 25 % discharge lever.
    #[test]
    fn rule2_export_takes_the_highest_sell_blocks() {
        for (soc, cost) in [(50.0, None), (100.0, None), (80.0, Some(4.0))] {
            let (blocks, plan) = owner_plan(0, soc, cost);
            let mut stretch: Vec<usize> = Vec::new();
            let check = |stretch: &[usize]| {
                let exported = stretch
                    .iter()
                    .filter(|&&k| plan.blocks[k].mode == PlanMode::Export)
                    .map(|&k| blocks[k].sell_czk)
                    .fold(f32::INFINITY, f32::min);
                for &k in stretch {
                    let b = &plan.blocks[k];
                    assert!(
                        b.mode == PlanMode::Export
                            || !blocks[k].export_allowed
                            || blocks[k].sell_czk <= exported,
                        "soc {soc}: {} idle at sell {} while {exported} exports",
                        b.start,
                        blocks[k].sell_czk
                    );
                }
            };
            for (k, b) in plan.blocks.iter().enumerate() {
                if b.mode == PlanMode::Charge {
                    check(&stretch);
                    stretch.clear();
                } else {
                    stretch.push(k);
                }
            }
            check(&stretch);
            assert!(plan.blocks.iter().any(|b| b.mode == PlanMode::Export));
            for (b, block) in plan.blocks.iter().zip(&blocks) {
                assert!(b.outcome.grid_export_kwh <= 9.0 * 0.25 + 1e-4);
                if b.mode == PlanMode::Export {
                    assert!(
                        block.export_allowed,
                        "{} exports outside the lever",
                        b.start
                    );
                    assert!(b.soc_end_pct >= 25.0 - 1e-3, "{} below the lever", b.start);
                }
            }
        }
    }

    /// Rule 3: a morning and an evening peak each get a charge before them (two cycles a day)
    #[test]
    fn rule3_charges_before_each_peak() {
        let (_, plan) = owner_plan(0, 30.0, None);
        let day1 = |b: &&PlannedBlock| b.start < day_start() + Duration::hours(24);
        let in_hours = |lo: u32, hi: u32, mode: PlanMode| {
            plan.blocks
                .iter()
                .filter(day1)
                .any(|b| (lo..hi).contains(&hour(b)) && b.mode == mode)
        };
        assert!(
            in_hours(0, 7, PlanMode::Charge),
            "no charge before the morning peak"
        );
        assert!(in_hours(7, 10, PlanMode::Export), "no morning export");
        assert!(
            in_hours(10, 17, PlanMode::Charge),
            "no charge between the peaks"
        );
        assert!(in_hours(17, 20, PlanMode::Export), "no evening export");
    }

    /// Rule 4: the 1.5 CZK margin is net of losses and wear: sell − (buy / 0.95 + 0.1)
    #[test]
    fn rule4_minimum_profit_is_net_of_losses_and_wear() {
        let unit = LOW / 0.95 + 0.1;
        for (net, cycles) in [(1.4, false), (1.6, true)] {
            let price = |h: u32| {
                (
                    LOW,
                    if (17..20).contains(&h) {
                        unit + net
                    } else {
                        3.0
                    },
                )
            };
            let (_, plan) = owner_plan_with(0, 30.0, None, price);
            let exports = plan.blocks.iter().any(|b| b.mode == PlanMode::Export);
            let charges = plan.blocks.iter().any(|b| b.mode == PlanMode::Charge);
            assert_eq!(exports, cycles, "net {net}: export");
            assert_eq!(charges, cycles, "net {net}: charge");
        }
    }

    /// When a horizon starts at 19:00 (after the 16:00–19:00 high-tariff window ends) and ends
    /// at 23:45 (tomorrow's prices missing), all remaining buy prices are flat at 3.22 while
    /// 19:00–20:00 still has the evening sell peak (8.30 CZK) followed by cheap refill hours
    /// (buy 3.22, sell 5.80). `export_floor_until_cheap` must recognize the later 20:00 block as
    /// a cheap refill so the 19:00–20:00 blocks can export instead of clamping floor_kwh to Emax.
    #[test]
    fn floor_until_cheap_allows_evening_export_into_flat_low_tariff_tail() {
        // Start at 19:00 of day 2 (skip = 96 + 76 = 172), leaving 20 blocks (19:00..24:00)
        // where buy is flat LOW (3.22), sell is 8.30 at 19:00..20:00 and 5.80 at 20:00..24:00.
        let (_, p) = owner_plan(172, 90.0, None);
        assert!(
            p.blocks[0].floor_kwh < 10.0,
            "19:00 floor should end at the 20:00 low-tariff refill, got {} kWh",
            p.blocks[0].floor_kwh
        );
        assert_eq!(
            p.blocks[0].mode,
            PlanMode::Export,
            "19:00 peak block should export before refilling in the low-tariff tail: {:?}",
            p.blocks[0].mode
        );
    }

    /// With no morning in the horizon, the refill ends the reserve only when selling now and buying back later nets
    /// `min_arbitrage_profit_czk`, losses and wear included: 4.60 − (3.22 / 0.95 + 0.1) = 1.11 and
    /// 4.90 − (3.22 / 0.95 + 0.1) = 1.41 are below 1.5 (4.90 would clear it without the losses:
    /// 4.90 − 3.22 − 0.1 = 1.58). Solar energy at a cost of 2.00 passes the export gate, but the
    /// house would buy it back for more than the margin, so the 19:00 block keeps the night reserve
    /// and does not export.
    #[test]
    fn floor_until_cheap_refill_must_clear_the_net_margin() {
        for peak in [4.6, 4.9] {
            let (_, p) = owner_plan_with(172, 90.0, Some(2.0), |h| {
                (buy_at(h), if h == 19 { peak } else { 2.0 })
            });
            let (_, emax) = battery(None).bounds_kwh();
            assert!(
                p.blocks[0].floor_kwh >= emax - 1e-3,
                "sell {peak}: a refill below the net margin must not end the night reserve, \
                 floor {} kWh",
                p.blocks[0].floor_kwh
            );
            assert!(
                p.blocks[..4].iter().all(|b| b.mode != PlanMode::Export),
                "sell {peak}: 19:00 must not sell night energy it would buy back at a loss: {:?}",
                p.blocks[..4].iter().map(|b| b.mode).collect::<Vec<_>>()
            );
        }
    }

    /// The refill end applies only while tomorrow's morning is not in the horizon. From 19:00 on
    /// day 1 with flat buy 3.22 (no cheap block), the 19:00 peak sells 8.30 and a 3.22 refill would
    /// clear the margin, but day 2's sunny 10:00 morning is in the horizon: the floor keeps the
    /// night's house load until then, and 19:00 does not sell it.
    #[test]
    fn floor_until_cheap_refill_waits_for_a_known_morning() {
        let (blocks, p) =
            owner_plan_with(76, 90.0, None, |h| (LOW, if h == 19 { 8.3 } else { 5.8 }));
        let (emin, _) = battery(None).bounds_kwh();
        let night: f32 = blocks
            .iter()
            .skip(1)
            .take_while(|b| b.start.with_timezone(&Prague).hour() != 10)
            .map(|b| (b.reserve_load_kwh - b.reserve_pv_kwh).max(0.0))
            .sum();
        assert!(
            p.blocks[0].floor_kwh >= emin + night - 1e-3,
            "the floor runs to the 10:00 morning ({:.1} kWh), got {} kWh",
            emin + night,
            p.blocks[0].floor_kwh
        );
        assert_ne!(p.blocks[0].mode, PlanMode::Export);
    }

    /// With no morning in the horizon, the refill must be at or below `cheap_ref`. From 18:00 (tomorrow's prices
    /// missing): 18:00 buy 2.00, the 19:00 peak sells 6.50 at buy 4.00 (below the insane-price
    /// 3 × cheap_ref), the night after buys 3.80. Selling at 6.50 and buying back at 3.80 would
    /// clear the 1.5 margin, but no later block is cheap (3.80 is above the 3.00 midpoint) and
    /// 3.80 is above cheap_ref (2.36), so the 19:00 peak keeps the whole night reserve.
    #[test]
    fn floor_until_cheap_refill_must_be_at_or_below_cheap_ref() {
        let (_, p) = owner_plan_with(168, 90.0, None, |h| match h {
            18 => (2.0, 3.0),
            19 => (4.0, 6.5),
            _ => (3.8, 3.0),
        });
        assert!(p.cheap_ref_czk < 3.8, "cheap_ref {}", p.cheap_ref_czk);
        let (_, emax) = battery(None).bounds_kwh();
        for b in &p.blocks[4..8] {
            assert!(
                b.floor_kwh >= emax - 1e-3,
                "a refill at 3.80 above cheap_ref {:.2} must not end the reserve, floor {} kWh",
                p.cheap_ref_czk,
                b.floor_kwh
            );
            assert_ne!(b.mode, PlanMode::Export, "19:00 peak must keep the reserve");
        }
    }
}

/// Cautious PV (owner ruling 2026-11-02): while the forecast error is high, or in a fallback month
/// when too few days are measured, the plan itself uses the forecast PV times `pv_confidence`.
#[cfg(test)]
mod cautious_pv_tests {
    use super::*;
    use chrono::TimeZone;
    use fluxion_plugins::protocol::BlockForecast;
    use fluxion_types::pricing::TimeBlockPrice;

    const PRAGUE: Tz = chrono_tz::Europe::Prague;

    fn thresholds(on_pct: f32) -> CautiousPvThresholds {
        CautiousPvThresholds {
            on_pct,
            off_pct: 20.0,
        }
    }

    fn signal(mae_pct: f32, threshold_pct: f32, was_on: bool) -> CautiousPvSignal {
        CautiousPvSignal {
            mae_pct,
            cautious: cautious_pv_next(was_on, mae_pct, thresholds(threshold_pct)),
        }
    }

    fn control(load_kw: f32) -> ControlConfig {
        ControlConfig {
            force_discharge_enabled: false,
            battery_capacity_kwh: 96.0,
            min_battery_soc: 10.0,
            max_battery_soc: 100.0,
            max_battery_charge_rate_kw: 30.0,
            battery_efficiency: 0.9,
            battery_wear_cost_czk_per_kwh: 0.125,
            maximum_export_power_w: 10_000,
            grid_export_fee_czk_per_kwh: 0.5,
            average_household_load_kw: load_kw,
            ..ControlConfig::default()
        }
    }

    fn prices(starts: &[DateTime<Utc>], buy: impl Fn(DateTime<Utc>) -> f32) -> Vec<TimeBlockPrice> {
        starts
            .iter()
            .map(|&s| TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: s,
                duration_minutes: 15,
                price_czk_per_kwh: buy(s),
                effective_price_czk_per_kwh: buy(s),
                spot_sell_price_czk_per_kwh: Some(1.0),
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn context<'a>(
        blocks: &'a [TimeBlockPrice],
        index: usize,
        control: &'a ControlConfig,
        forecasts: &'a [BlockForecast],
        soc: f32,
        tz: Tz,
        cautious_pv: Option<CautiousPvSignal>,
    ) -> super::super::EvaluationContext<'a> {
        super::super::EvaluationContext {
            price_block: &blocks[index],
            control_config: control,
            current_battery_soc: soc,
            solar_forecast_kwh: 0.0,
            consumption_forecast_kwh: 0.0,
            grid_export_price_czk_per_kwh: 0.5,
            all_price_blocks: Some(blocks),
            ranking_price_blocks: None,
            backup_discharge_min_soc: 10.0,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 0.0,
            battery_charge_cost_czk_per_kwh: None,
            hourly_consumption_profile: None,
            hourly_consumption_p75: None,
            block_forecasts: Some(forecasts),
            timezone: Some(tz),
            user_levers: None,
            cautious_pv,
        }
    }

    fn params(confidence: f32) -> WinterReserveParams {
        WinterReserveParams {
            pv_confidence: confidence,
            ..WinterReserveParams::default()
        }
    }

    /// One 10 kWh block at local noon on `day` (y, m, d) in Prague, planned with `signal`
    fn noon_block(
        ymd: (i32, u32, u32),
        p: WinterReserveParams,
        cautious_pv: Option<CautiousPvSignal>,
    ) -> PlanBlock {
        let start = PRAGUE
            .with_ymd_and_hms(ymd.0, ymd.1, ymd.2, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let blocks = prices(&[start], |_| 5.0);
        let forecasts = [BlockForecast {
            solar_kwh: 10.0,
            consumption_kwh: 0.5,
        }];
        let ctl = control(1.0);
        WinterReserveStrategy::new(true, p).horizon_blocks(&context(
            &blocks,
            0,
            &ctl,
            &forecasts,
            50.0,
            PRAGUE,
            cautious_pv,
        ))[0]
    }

    #[test]
    fn high_forecast_error_plans_on_cautious_pv_low_error_does_not() {
        // MAE 39 % with the default threshold 25 %: cautious, PV 10 -> 7.0 (reserve 7.0)
        let high = signal(39.0, 25.0, false);
        assert!(high.cautious);
        let b = noon_block((2026, 6, 15), params(0.7), Some(high));
        assert!((b.pv_kwh - 7.0).abs() < 1e-5, "{}", b.pv_kwh);
        assert!((b.reserve_pv_kwh - 7.0).abs() < 1e-5);
        // MAE 10 %: not cautious, even in November (the signal wins over the months)
        let low = signal(10.0, 25.0, false);
        assert!(!low.cautious);
        let b = noon_block((2026, 11, 15), params(0.7), Some(low));
        assert!((b.pv_kwh - 10.0).abs() < 1e-5, "{}", b.pv_kwh);
        assert!((b.reserve_pv_kwh - 7.0).abs() < 1e-5);
    }

    #[test]
    fn hysteresis_holds_the_state_between_20_and_25_percent() {
        // Threshold 100 is off, even at the largest error the bounded metric gives
        assert!(!cautious_pv_next(false, 100.0, thresholds(100.0)));
        assert!(!cautious_pv_next(true, 100.0, thresholds(100.0)));
        // Off: 22 % does not turn it on; 25 % does
        assert!(!cautious_pv_next(false, 22.0, thresholds(25.0)));
        assert!(cautious_pv_next(false, 25.0, thresholds(25.0)));
        let mut on = false;
        let mut states = Vec::new();
        for mae in [30.0, 22.0, 20.0, 19.0, 22.0] {
            on = cautious_pv_next(on, mae, thresholds(25.0));
            states.push(on);
        }
        // 22 stays on after 30, 20 still on, 19 turns off, 22 stays off after 19
        assert_eq!(states, vec![true, true, true, false, false]);
        // The error decides only with at least 5 valid days in the window
        let t = thresholds(25.0);
        assert_eq!(cautious_pv_signal(Some(40.0), 4, false, t), None);
        assert!(
            cautious_pv_signal(Some(40.0), 5, false, t)
                .unwrap()
                .cautious
        );
    }

    #[test]
    fn without_a_signal_the_local_month_decides_at_the_prague_boundary() {
        // Fallback months [10, 11, 12, 1, 2, 3]: 30 Sep 23:45 local is not cautious, 1 Oct 00:00
        // local (30 Sep 22:00 UTC) is
        let sep_last = Utc.with_ymd_and_hms(2026, 9, 30, 21, 45, 0).unwrap();
        let oct_first = Utc.with_ymd_and_hms(2026, 9, 30, 22, 0, 0).unwrap();
        let blocks = prices(&[sep_last, oct_first], |_| 5.0);
        let forecasts = [BlockForecast {
            solar_kwh: 10.0,
            consumption_kwh: 0.5,
        }; 2];
        let ctl = control(1.0);
        let s = WinterReserveStrategy::new(true, params(0.7));
        let h = s.horizon_blocks(&context(&blocks, 0, &ctl, &forecasts, 50.0, PRAGUE, None));
        assert!((h[0].pv_kwh - 10.0).abs() < 1e-5, "{}", h[0].pv_kwh);
        assert!((h[1].pv_kwh - 7.0).abs() < 1e-5, "{}", h[1].pv_kwh);
        // In UTC the second block is still 30 September: the site timezone is what counts
        let h = s.horizon_blocks(&context(
            &blocks,
            0,
            &ctl,
            &forecasts,
            50.0,
            chrono_tz::UTC,
            None,
        ));
        assert!((h[1].pv_kwh - 10.0).abs() < 1e-5, "{}", h[1].pv_kwh);
        // The March/April end: 31 Mar 23:45 local cautious, 1 Apr 00:00 local (CEST) not
        let mar_last = Utc.with_ymd_and_hms(2026, 3, 31, 21, 45, 0).unwrap();
        let apr_first = Utc.with_ymd_and_hms(2026, 3, 31, 22, 0, 0).unwrap();
        let blocks = prices(&[mar_last, apr_first], |_| 5.0);
        let h = s.horizon_blocks(&context(&blocks, 0, &ctl, &forecasts, 50.0, PRAGUE, None));
        assert!((h[0].pv_kwh - 7.0).abs() < 1e-5, "{}", h[0].pv_kwh);
        assert!((h[1].pv_kwh - 10.0).abs() < 1e-5, "{}", h[1].pv_kwh);
    }

    #[test]
    fn empty_months_and_threshold_100_give_the_old_horizon_exactly() {
        let off = WinterReserveParams {
            cautious_pv_fallback_months: Vec::new(),
            ..params(0.7)
        };
        // A 39 % error under a 100 % threshold is not cautious
        let sig = signal(39.0, 100.0, false);
        assert!(!sig.cautious);
        let start = PRAGUE
            .with_ymd_and_hms(2026, 11, 2, 0, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let starts: Vec<_> = (0..96).map(|i| start + Duration::minutes(15 * i)).collect();
        let blocks = prices(&starts, |_| 5.0);
        let forecasts: Vec<BlockForecast> = (0..96)
            .map(|i| BlockForecast {
                solar_kwh: (i as f32 * 0.137).sin().max(0.0) * 1.3,
                consumption_kwh: 0.3,
            })
            .collect();
        let ctl = control(1.0);
        let s = WinterReserveStrategy::new(true, off);
        for cautious_pv in [None, Some(sig)] {
            let h = s.horizon_blocks(&context(
                &blocks,
                0,
                &ctl,
                &forecasts,
                50.0,
                PRAGUE,
                cautious_pv,
            ));
            for (b, f) in h.iter().zip(&forecasts) {
                assert_eq!(b.pv_kwh.to_bits(), f.solar_kwh.to_bits());
                assert_eq!(b.reserve_pv_kwh.to_bits(), (f.solar_kwh * 0.7).to_bits());
            }
        }
    }

    /// Jakub-like evening, 2 Nov 18:00 to 3 Nov 24:00 local: night 7.7, next day 9.5-10.5, 102 kWh
    /// forecast for the next day, `pv_confidence` 0.35, 96 kWh at 17 %, 30 kW charge. Re-planned
    /// every block from the simulated SOC; returns the SOC (%) at 06:00 and the night grid charge.
    const JAKUB_LOAD_KWH: f32 = 0.6;

    fn jakub_closed_loop(cautious_pv: Option<CautiousPvSignal>) -> (f32, f32) {
        let start = PRAGUE
            .with_ymd_and_hms(2026, 11, 2, 18, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let n = 120;
        let starts: Vec<_> = (0..n).map(|i| start + Duration::minutes(15 * i)).collect();
        let local_hour = |s: DateTime<Utc>| s.with_timezone(&PRAGUE).hour();
        let is_next_day = |s: DateTime<Utc>| s.with_timezone(&PRAGUE).day() == 3;
        let blocks = prices(&starts, |s| {
            let h = local_hour(s);
            if !is_next_day(s) && h >= 22 || is_next_day(s) && h < 6 {
                7.7
            } else if !is_next_day(s) {
                10.5
            } else {
                9.5 + (h as f32 - 6.0) / 18.0
            }
        });
        // 102 kWh over 08:00-16:00 local next day, a symmetric bell over 32 blocks
        let weights: Vec<f32> = (0..32)
            .map(|k| (std::f32::consts::PI * (k as f32 + 0.5) / 32.0).sin())
            .collect();
        let total: f32 = weights.iter().sum();
        let forecasts: Vec<BlockForecast> = starts
            .iter()
            .map(|&s| {
                let h = local_hour(s);
                let solar_kwh = if is_next_day(s) && (8..16).contains(&h) {
                    let k = (h - 8) as usize * 4 + s.with_timezone(&PRAGUE).minute() as usize / 15;
                    102.0 * weights[k] / total
                } else {
                    0.0
                };
                BlockForecast {
                    solar_kwh,
                    consumption_kwh: JAKUB_LOAD_KWH,
                }
            })
            .collect();
        let ctl = control(JAKUB_LOAD_KWH * 4.0);
        let s = WinterReserveStrategy::new(true, params(0.35));
        let battery = battery_params(&ctl);
        let mut soc = 17.0_f32;
        let mut current = None;
        let mut night_charge = 0.0_f32;
        let mut soc_at_six = None;
        for i in 0..n as usize {
            if is_next_day(starts[i]) && local_hour(starts[i]) == 6 && soc_at_six.is_none() {
                soc_at_six = Some(soc);
            }
            let horizon = s.horizon_blocks(&context(
                &blocks,
                i,
                &ctl,
                &forecasts,
                soc,
                PRAGUE,
                cautious_pv,
            ));
            let p = s.run_plan(
                &horizon,
                soc,
                PRAGUE,
                &battery,
                current,
                ForceDirection::of(current),
            );
            let head = &p.blocks[0];
            if head.mode == PlanMode::Charge && blocks[i].effective_price_czk_per_kwh < 8.0 {
                night_charge += head.outcome.battery_charge_kwh;
            }
            current = Some(head.mode);
            soc = head.soc_end_pct;
        }
        (soc_at_six.expect("06:00 reached"), night_charge)
    }

    #[test]
    fn jakub_high_forecast_error_charges_at_night_low_error_does_not() {
        // Cautious (MAE 39 %): the plan sees 35.7 kWh of sun and fills the battery at 7.7 overnight.
        // It stops short of 100 % because the charge cap leaves room for that cautious PV, so
        // about 60 % is the most the night can add (59.2 % measured).
        let (soc_six, charged) = jakub_closed_loop(Some(signal(39.0, 25.0, false)));
        assert!(
            soc_six >= 58.0 && charged >= 40.0,
            "MAE 39 %: SOC at 06:00 {soc_six:.1} %, night charge {charged:.1} kWh"
        );
        // Face value (MAE 10 %): 102 kWh of sun covers the day, so the night buys only what the
        // morning before sunrise needs (the reserve), not the day
        let (soc_six, charged) = jakub_closed_loop(Some(signal(10.0, 25.0, false)));
        assert!(
            soc_six < 30.0 && charged < 20.0,
            "MAE 10 %: SOC at 06:00 {soc_six:.1} %, night charge {charged:.1} kWh"
        );
    }
}

#[cfg(test)]
mod soc_bounds_tests {
    use super::*;
    use chrono::TimeZone;

    fn battery(min_soc_pct: f32, max_soc_pct: f32) -> BatteryParams {
        BatteryParams {
            capacity_kwh: 10.0,
            min_soc_pct,
            max_soc_pct,
            max_charge_kw: 5.0,
            max_discharge_kw: 5.0,
            efficiency: 0.9,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 5.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: false,
            hold_pv_to_battery: false,
            min_charge_kwh: 0.0,
            charge_cost_czk_per_kwh: None,
        }
    }

    fn blocks() -> Vec<PlanBlock> {
        let start = Utc.with_ymd_and_hms(2026, 10, 20, 10, 0, 0).unwrap();
        (0..4)
            .map(|i| PlanBlock {
                start: start + Duration::minutes(15 * i),
                duration_minutes: 15,
                buy_czk: 2.0,
                sell_czk: 0.5,
                load_kwh: 0.5,
                pv_kwh: 0.0,
                reserve_load_kwh: 0.5,
                reserve_pv_kwh: 0.0,
                force_charge: false,
                no_discharge: false,
                force_charge_at_or_below_czk: None,
                export_min_soc_pct: None,
                export_allowed: true,
            })
            .collect()
    }

    /// SOC limits outside 0..=100 (or NaN) are an invalid battery config, not a grid to size.
    #[test]
    fn soc_limits_outside_0_to_100_are_rejected() {
        for (min, max) in [
            (-5.0, 100.0),
            (10.0, 150.0),
            (f32::NAN, 100.0),
            (10.0, f32::NAN),
        ] {
            let p = plan(
                &blocks(),
                50.0,
                chrono_tz::Europe::Prague,
                &battery(min, max),
                &WinterReserveParams::default(),
            );
            assert!(
                p.blocks[0].reason.ends_with("invalid battery config"),
                "{min}..{max}: {}",
                p.blocks[0].reason
            );
        }
    }

    /// The grid holds (max - min) / resolution steps; a finer resolution is coarsened to the cap
    /// and the grid still runs from Emin to Emax.
    #[test]
    fn soc_grid_is_capped_and_reaches_emax() {
        let normal = soc_grid(0.0, 8.0, 8.0, 12.5);
        assert_eq!(normal.len(), 9);
        let fine = soc_grid(0.0, 8.0, 8.0, 1e-5);
        assert!(
            fine.len() <= MAX_SOC_GRID_STEPS + 2,
            "{} points",
            fine.len()
        );
        assert_eq!(fine[0], 0.0);
        assert_eq!(*fine.last().unwrap(), 8.0);
    }
}

/// Direction-reversal cost (owner 2026-11-05): no Charge/Export flip-flop, sell in the best
/// contiguous blocks
#[cfg(test)]
mod reversal_tests {
    use super::*;
    use chrono::TimeZone;

    const TZ: Tz = chrono_tz::Europe::Prague;
    /// Owner site evening sell prices (spot − fee, CZK/kWh), 17:00-20:15 local, 2026-11-05 plan
    const OWNER_EVENING_SELL: [f32; 14] = [
        8.30, 8.38, 8.20, 8.05, 8.13, 8.60, 8.01, 8.75, 8.46, 8.28, 7.95, 8.10, 8.14, 7.89,
    ];

    /// Owner-like battery: 23 kWh, 10 kW charge, discharge and export, 25 % Back Up floor
    fn owner_battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 25.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.95,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 10.0,
            export_guard_margin_kwh: 0.0,
            force_discharge_enabled: true,
            hold_pv_to_battery: false,
            min_charge_kwh: 0.5,
            charge_cost_czk_per_kwh: None,
        }
    }

    fn block_at(start: DateTime<Utc>, buy: f32, sell: f32, load: f32, pv: f32) -> PlanBlock {
        PlanBlock {
            start,
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

    /// Fixed buy 3.22 all along. 17:00-20:15 local: the owner's evening sells. Then a night with no
    /// load and sell 2.0 (no export pays, and selling below the end value never pays for a
    /// charge), and a sunny morning, so the night closes and the floor stays at the 25 % minimum,
    /// as on the owner's plan.
    fn owner_evening() -> Vec<PlanBlock> {
        // 2026-11-05 17:00 CET
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 16, 0, 0).unwrap();
        (0..14 + 56 + 12)
            .map(|i| {
                let at = start + Duration::minutes(15 * i as i64);
                if i < 14 {
                    block_at(at, 3.22, OWNER_EVENING_SELL[i], 0.0, 0.0)
                } else if i < 14 + 56 {
                    block_at(at, 3.22, 2.0, 0.0, 0.0)
                } else {
                    block_at(at, 3.22, 2.0, 0.2, 1.0)
                }
            })
            .collect()
    }

    fn params(cost: f32) -> WinterReserveParams {
        WinterReserveParams {
            direction_reversal_cost_czk: cost,
            ..WinterReserveParams::default()
        }
    }

    fn modes(plan: &Plan) -> Vec<PlanMode> {
        plan.blocks.iter().map(|b| b.mode).collect()
    }

    /// Reversals of `modes` starting from `direction`
    fn reversals(mut direction: ForceDirection, modes: &[PlanMode]) -> usize {
        let mut count = 0;
        for &mode in modes {
            if direction.reversed_by(mode) {
                count += 1;
            }
            direction = direction.after(mode);
        }
        count
    }

    fn letters(modes: &[PlanMode]) -> String {
        modes
            .iter()
            .map(|m| match m {
                PlanMode::SelfUse => 'S',
                PlanMode::Charge => 'C',
                PlanMode::Hold => 'H',
                PlanMode::Export => 'E',
                PlanMode::NoDischarge => 'N',
            })
            .collect()
    }

    /// Plan cost plus the energy left valued as the DP values it (`terminal_price`): what the
    /// planner minimises, without the reversal cost
    fn objective(plan: &Plan, battery: &BatteryParams) -> f32 {
        let end = plan.blocks.last().map_or(0.0, |b| b.outcome.energy_end_kwh);
        plan.expected_cost_czk - end * terminal_price(battery, plan.cheap_ref_czk)
    }

    /// Two spreads a day: cheap night (2.0) and midday (2.5) buys, morning (06-09, sell 9.0) and
    /// evening (17-20, sell 10.0) peaks, from 00:00 local to a sunny 10:00 the next day
    fn big_spread() -> Vec<PlanBlock> {
        // 2026-11-06 00:00 CET
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 23, 0, 0).unwrap();
        (0..136)
            .map(|i| {
                let at = start + Duration::minutes(15 * i as i64);
                let hour = i / 4;
                let (buy, sell) = match hour {
                    0..5 => (2.0, 1.5),
                    5 => (4.0, 3.5),
                    6..9 => (7.0, 9.0),
                    9..11 | 14..17 | 20..24 => (5.0, 4.0),
                    11..14 => (2.5, 3.0),
                    17..20 => (7.0, 10.0),
                    _ => (4.0, 3.5),
                };
                let pv = if hour >= 32 { 1.0 } else { 0.0 };
                block_at(at, buy, sell, 0.0, pv)
            })
            .collect()
    }

    /// Evening window of `owner_evening` (17:00-20:15 local)
    const EVENING: usize = 14;

    /// In every stretch of the evening without a Charge, the exported blocks are the dearest
    /// ones: no block left unexported sells for more than an exported one (0.1 CZK/kWh slack,
    /// the planner's `min_mode_benefit_czk` per block)
    fn exports_take_the_best_sells(blocks: &[PlanBlock], modes: &[PlanMode]) -> Result<(), String> {
        let mut start = 0;
        while start < modes.len() {
            let end = (start..modes.len())
                .find(|&k| modes[k] == PlanMode::Charge)
                .unwrap_or(modes.len());
            let sells = |export: bool| {
                (start..end)
                    .filter(move |&k| (modes[k] == PlanMode::Export) == export)
                    .map(|k| blocks[k].sell_czk)
            };
            let lowest_export = sells(true).fold(f32::INFINITY, f32::min);
            let best_skipped = sells(false).fold(f32::NEG_INFINITY, f32::max);
            if best_skipped > lowest_export + 0.1 {
                return Err(format!(
                    "blocks {start}..{end}: sell {best_skipped} skipped, {lowest_export} exported"
                ));
            }
            start = end + 1;
        }
        Ok(())
    }

    /// Owner site 2026-11-05 (fixed buy 3.22, sell 7.9-8.9 all evening): today's planner (cost 0)
    /// alternates Charge and Export in 15-minute blocks, `CECCCECEEECCEE` from 35 % SOC and
    /// `EECCCECEEECCEE` from 46 % and 57 % (6-8 reversals; documented, not asserted). With the
    /// default cost 2.0 the evening runs in contiguous blocks with at most 3 reversals, the plan
    /// pays at least 2 CZK per reversal it keeps, and exports take the dearest blocks. From the
    /// owner's 17:00 state (35 %, after the 15:30-16:45 export run) the plan is one charge run then
    /// one export run.
    #[test]
    fn owner_evening_groups_charge_and_export_into_runs() {
        let b = owner_battery();
        let blocks = owner_evening();
        for soc in [35.0, 46.0, 57.0] {
            for direction in [ForceDirection::None, ForceDirection::Exporting] {
                let free = plan_from_direction(&blocks, soc, TZ, &b, &params(0.0), None, direction);
                let p = plan_from_direction(&blocks, soc, TZ, &b, &params(2.0), None, direction);
                let m = modes(&p);
                let evening = &m[..EVENING];
                let (rev, rev_free) = (
                    reversals(direction, evening),
                    reversals(direction, &modes(&free)[..EVENING]),
                );
                let case = format!(
                    "SOC {soc} from {direction:?}: {} (cost 0: {}, {rev_free} reversals)",
                    letters(evening),
                    letters(&modes(&free)[..EVENING]),
                );
                assert!(rev <= 3, "{case}");
                assert!(rev < rev_free, "{case}");
                // Each reversal it keeps buys at least the reversal cost (DP grid slack 0.1 CZK)
                assert!(
                    objective(&p, &b) + 2.0 * rev as f32
                        <= objective(&free, &b) + 2.0 * rev_free as f32 + 0.1,
                    "{case}: {} vs {}",
                    objective(&p, &b),
                    objective(&free, &b)
                );
                exports_take_the_best_sells(&blocks[..EVENING], evening)
                    .unwrap_or_else(|e| panic!("{case}: {e}"));
                // The reversal cost steers the plan only: the plan cost is its blocks' cost
                let block_sum: f32 = p.blocks.iter().map(|x| x.cost_czk).sum();
                assert!((p.expected_cost_czk - block_sum).abs() < 1e-3, "{case}");
            }
        }
        // The owner's state at 17:00: SOC 35 % after an export run
        let p = plan_from(&blocks, 35.0, TZ, &b, &params(2.0), Some(PlanMode::Export));
        let evening = letters(&modes(&p)[..EVENING]);
        assert_eq!(evening, "ECCCCCCCEEEEEE");
    }

    /// Two spreads a day: the plan still charges at night, exports in the morning peak, charges
    /// at midday and exports in the evening peak (3 reversals, each worth far more than 2 CZK),
    /// and loses nothing against the plan without the reversal cost.
    #[test]
    fn big_spread_still_cycles_twice_a_day() {
        let b = owner_battery();
        let blocks = big_spread();
        let free = plan(&blocks, 25.0, TZ, &b, &params(0.0));
        let p = plan(&blocks, 25.0, TZ, &b, &params(2.0));
        let m = modes(&p);
        // Force runs in order, other modes skipped
        let mut runs: Vec<PlanMode> = Vec::new();
        for &mode in m
            .iter()
            .filter(|m| matches!(m, PlanMode::Charge | PlanMode::Export))
        {
            if runs.last() != Some(&mode) {
                runs.push(mode);
            }
        }
        use PlanMode::{Charge as C, Export as E};
        assert_eq!(runs, vec![C, E, C, E], "{}", letters(&m));
        let hour_of = |k: usize| k / 4;
        for (k, &mode) in m.iter().enumerate() {
            match mode {
                C => assert!(
                    matches!(hour_of(k), 0..5 | 11..14),
                    "C at {k}: {}",
                    letters(&m)
                ),
                E => assert!(
                    matches!(hour_of(k), 6..9 | 17..20),
                    "E at {k}: {}",
                    letters(&m)
                ),
                _ => {}
            }
        }
        let rev = reversals(ForceDirection::None, &m);
        assert_eq!(rev, 3);
        assert!(
            objective(&p, &b) <= objective(&free, &b) + 2.0 * rev as f32,
            "{} vs {}",
            objective(&p, &b),
            objective(&free, &b)
        );
    }

    /// Cost 0 is today's planner: the plans below were printed by the planner before this change
    /// (origin/legacy/winter-strategy 5071b009) for the same inputs and must stay identical.
    #[test]
    fn zero_cost_plans_match_the_planner_before_the_change() {
        let b = owner_battery();
        let night: String = "S".repeat(56 + 12);
        let cases: [(f32, &str, f32); 3] = [
            (35.0, "CECCCECEEECCEE", -88.149994),
            (46.0, "EECCCECEEECCEE", -109.2549),
            (57.0, "EECCCECEEECCEE", -116.69999),
        ];
        for (soc, evening, cost) in cases {
            for current in [None, Some(PlanMode::Charge), Some(PlanMode::Export)] {
                let p = plan_from(&owner_evening(), soc, TZ, &b, &params(0.0), current);
                assert_eq!(
                    letters(&modes(&p)),
                    format!("{evening}{night}"),
                    "{soc} {current:?}"
                );
                assert_eq!(p.expected_cost_czk, cost, "{soc} {current:?}");
            }
        }
        let p = plan(&big_spread(), 25.0, TZ, &b, &params(0.0));
        assert_eq!(
            letters(&modes(&p)),
            format!(
                "{}{}",
                "HHHHHHHHHHHHCCHCCCCCHHHHHHHHHEEEEEEEHHHHHHHHHHHHCCCCCCHCHHHHHHHHHHHHHHHHHEEEEEEE",
                "S".repeat(56)
            )
        );
        assert_eq!(p.expected_cost_czk, -233.79999);
    }

    /// One marginal charge (buy 6.5) that an export at 8.0 repays by about 2.5 CZK, from the
    /// 25 % floor. Charge cost of the stored energy 4.0, so the export clears the profit gap.
    fn marginal_charge() -> Vec<PlanBlock> {
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 16, 0, 0).unwrap();
        (0..8 + 56 + 12)
            .map(|i| {
                let at = start + Duration::minutes(15 * i as i64);
                match i {
                    0 => block_at(at, 6.5, 5.0, 0.0, 0.0),
                    1 | 2 => block_at(at, 9.0, 8.0, 0.0, 0.0),
                    _ if i < 8 + 56 => block_at(at, 9.0, 2.0, 0.0, 0.0),
                    _ => block_at(at, 9.0, 2.0, 0.2, 1.0),
                }
            })
            .collect()
    }

    /// While the battery exports, a charge that gains less than two reversals (export, charge,
    /// export) is not planned next. The direction comes from the applied mode, and from the last
    /// force mode through a Hold head. From no direction, or with no reversal cost, the same
    /// charge is planned, so the test is not vacuous.
    #[test]
    fn exporting_direction_blocks_a_marginal_charge() {
        let b = BatteryParams {
            charge_cost_czk_per_kwh: Some(4.0),
            ..owner_battery()
        };
        let blocks = marginal_charge();
        let head = |p: &Plan| p.blocks[0].mode;
        let exporting = plan_from(&blocks, 25.0, TZ, &b, &params(2.0), Some(PlanMode::Export));
        assert_ne!(
            head(&exporting),
            PlanMode::Charge,
            "{}",
            letters(&modes(&exporting))
        );
        assert!(!modes(&exporting).contains(&PlanMode::Charge));
        let through_hold = plan_from_direction(
            &blocks,
            25.0,
            TZ,
            &b,
            &params(2.0),
            Some(PlanMode::Hold),
            ForceDirection::Exporting,
        );
        assert_ne!(head(&through_hold), PlanMode::Charge);
        // Not vacuous: no direction, or no cost, charges in block 0
        let none = plan_from(&blocks, 25.0, TZ, &b, &params(2.0), Some(PlanMode::Hold));
        assert_eq!(head(&none), PlanMode::Charge, "{}", letters(&modes(&none)));
        let free = plan_from(&blocks, 25.0, TZ, &b, &params(0.0), Some(PlanMode::Export));
        assert_eq!(head(&free), PlanMode::Charge);
        assert!(
            modes(&free)[1..3].contains(&PlanMode::Export),
            "{}",
            letters(&modes(&free))
        );
    }

    #[test]
    fn force_direction_follows_charge_and_export_only() {
        use ForceDirection::{Charging, Exporting, None as NoDir};
        assert_eq!(NoDir.after(PlanMode::Charge), Charging);
        assert_eq!(Charging.after(PlanMode::Hold), Charging);
        assert_eq!(Charging.after(PlanMode::SelfUse), Charging);
        assert_eq!(Exporting.after(PlanMode::NoDischarge), Exporting);
        assert!(Charging.reversed_by(PlanMode::Export));
        assert!(Exporting.reversed_by(PlanMode::Charge));
        assert!(!NoDir.reversed_by(PlanMode::Export));
        assert!(!Charging.reversed_by(PlanMode::Charge));
        // C, S, E is one reversal
        assert_eq!(
            reversals(
                NoDir,
                &[PlanMode::Charge, PlanMode::SelfUse, PlanMode::Export]
            ),
            1
        );
        assert_eq!(ForceDirection::of(Some(PlanMode::Export)), Exporting);
        assert_eq!(ForceDirection::of(Some(PlanMode::Hold)), NoDir);
        assert_eq!(
            WinterReserveParams::default().direction_reversal_cost_czk,
            2.0
        );
    }

    /// The live cache path (`WinterReserveStrategy::entry_for`, as `evaluate` calls it on a head
    /// block) on the `marginal_charge` horizon at 16:00 UTC, replanned at 16:05
    fn replan_head(s: &WinterReserveStrategy, battery: &BatteryParams) -> (PlanMode, Replan) {
        let now = Utc.with_ymd_and_hms(2026, 11, 5, 16, 5, 0).unwrap();
        let (entry, replan) = s.entry_for(now, &marginal_charge(), 25.0, TZ, battery, true, 1);
        (entry.mode, replan.expect("a head replan"))
    }

    fn strategy(cost: f32) -> WinterReserveStrategy {
        WinterReserveStrategy::new(true, params(cost))
    }

    /// Review r1 item 1: the force direction the live strategy carries from one head replan to
    /// the next, through `CachedPlan.direction`.
    #[test]
    fn strategy_cache_carries_the_export_direction_into_the_next_replan() {
        let b = BatteryParams {
            charge_cost_czk_per_kwh: Some(4.0),
            ..owner_battery()
        };
        // (a) The previous head replan (15:45 block, SOC 35 %) exports at 9.0; the next one must
        // not plan the marginal charge.
        let s = strategy(2.0);
        let t0 = Utc.with_ymd_and_hms(2026, 11, 5, 15, 50, 0).unwrap();
        let mut previous = vec![block_at(t0 - Duration::minutes(5), 9.0, 9.0, 0.0, 0.0)];
        previous.extend(marginal_charge());
        let (head, first) = s.entry_for(t0, &previous, 35.0, TZ, &b, true, 0);
        assert_eq!(
            head.mode,
            PlanMode::Export,
            "{}",
            letters(&modes(&first.unwrap().plan))
        );
        let (mode, replan) = replan_head(&s, &b);
        assert_ne!(mode, PlanMode::Charge, "{}", letters(&modes(&replan.plan)));
        assert_eq!(replan.start_direction, ForceDirection::Exporting);
        assert_eq!(replan.current, Some(PlanMode::Export));

        // (b) The previous head is a Hold after an export: the direction is still Exporting.
        let s = strategy(2.0);
        let hold = PlannedBlock {
            start: t0 - Duration::minutes(5),
            mode: PlanMode::Hold,
            soc_start_pct: 25.0,
            soc_end_pct: 25.0,
            floor_kwh: 5.75,
            charge_cap_kwh: 23.0,
            outcome: BlockOutcome::default(),
            cost_czk: 0.0,
            reason: "WinterReserve TEST".into(),
        };
        *s.cache.lock().unwrap() = Some(CachedPlan {
            plan: Plan {
                blocks: vec![hold],
                cheap_ref_czk: 9.0,
                median_buy_czk: 9.0,
                expected_cost_czk: 0.0,
            },
            planned_at: t0,
            horizon_last_start: t0,
            price_fingerprint: 0,
            min_soc_pct: b.min_soc_pct,
            direction: ForceDirection::Exporting,
        });
        let (mode, replan) = replan_head(&s, &b);
        assert_ne!(mode, PlanMode::Charge, "{}", letters(&modes(&replan.plan)));
        assert_eq!(replan.current, Some(PlanMode::Hold));
        assert_eq!(replan.start_direction, ForceDirection::Exporting);

        // (c) Controls: no cached plan, or no reversal cost after the same export, charges.
        let (mode, replan) = replan_head(&strategy(2.0), &b);
        assert_eq!(replan.start_direction, ForceDirection::None);
        assert_eq!(mode, PlanMode::Charge);
        let s = strategy(0.0);
        s.entry_for(t0, &previous, 35.0, TZ, &b, true, 0);
        assert_eq!(replan_head(&s, &b).0, PlanMode::Charge);
    }

    /// Review r1 item 2: the plan dump records the inputs a replan started from.
    #[test]
    fn plan_dump_records_current_and_start_direction() {
        let horizon = marginal_charge();
        let replan = Replan {
            plan: plan(&horizon, 25.0, TZ, &owner_battery(), &params(2.0)),
            current: Some(PlanMode::Hold),
            start_direction: ForceDirection::Exporting,
        };
        let json = serde_json::to_value(plan_dump(&horizon, &replan, 25.0)).unwrap();
        assert_eq!(json["current"], "Hold");
        assert_eq!(json["start_direction"], "Exporting");
        assert_eq!(json["blocks"].as_array().unwrap().len(), horizon.len());
    }
}

/// Run continuity of the discharge-window levers (owner 2026-11-05: "continuity can be accepted
/// within 5 % margin"; the margin compares a gap block with its chosen neighbours)
#[cfg(test)]
mod continuity_tests {
    use super::*;
    use chrono::TimeZone;

    const TZ: Tz = chrono_tz::Europe::Prague;

    // Owner site live horizon at 2026-11-05 12:45Z (13:45 local; fixture of
    // legacy/exp-mode-change-cost 2198ab30, which reproduces the live plan letter for letter), the head replan logged as
    // `n=137 ... load_expected_kwh=93.63 pv_expected_kwh=19.19 ... soc_pct=99.0 current=Some(Hold)
    // start_direction=None` on 0.2.74: buy = `/chart-data` effective price, sell = spot − 0.35,
    // load = the UTC-hour profile / 4, PV = `solar_forecast_kw` / 4 × 0.9 (cautious PV). The
    // arrays start at 11:00Z (12:00 local, the evening lever window) so the levers rank the
    // whole window; the horizon starts at `LIVE_HEAD`.
    const LIVE_BUY: [f32; 144] = [
        3.224, 3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 4.146, 4.146, 4.146, 4.146, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224, 3.224,
        3.224,
    ];
    const LIVE_SELL: [f32; 144] = [
        5.1477, 4.9493, 5.0040, 4.9321, 5.2304, 5.1567, 5.0770, 5.0719, 4.6652, 4.9316, 5.6354,
        7.0405, 6.5776, 7.7912, 8.6420, 8.8516, 8.5098, 8.0036, 8.1913, 8.5173, 8.2341, 8.3795,
        7.9871, 7.9669, 8.1271, 7.8289, 8.0063, 7.7938, 8.4568, 8.2844, 7.8116, 7.8488, 8.1417,
        7.8855, 7.2192, 6.6999, 7.4878, 6.8229, 6.4053, 6.0836, 6.5156, 6.4155, 6.6625, 6.2854,
        6.3160, 6.0173, 5.8393, 5.3077, 5.8464, 5.4752, 5.4650, 5.4360, 5.7759, 5.5111, 5.4829,
        5.5080, 5.5114, 5.3709, 5.3774, 5.3281, 5.3828, 5.3942, 5.3128, 5.3526, 5.3563, 5.4812,
        5.5578, 5.6371, 5.6145, 5.5092, 5.6040, 5.4764, 5.6767, 7.0592, 7.7314, 7.5760, 7.6772,
        7.5726, 7.5442, 7.4384, 7.8945, 7.5816, 7.2887, 6.8100, 7.2104, 6.6999, 6.4792, 5.7294,
        6.5144, 5.8391, 5.5634, 5.3631, 5.5471, 5.4951, 5.3876, 5.4941, 5.5087, 5.3577, 5.4956,
        5.2919, 5.4275, 5.3993, 5.5544, 5.5457, 5.4110, 5.6918, 6.3965, 7.4946, 7.0857, 7.9886,
        8.6095, 9.3322, 8.8166, 8.8115, 8.8968, 8.9444, 8.2409, 8.6520, 8.8178, 8.7704, 8.8416,
        8.6180, 8.7055, 8.3192, 8.4247, 8.1583, 7.9310, 8.1201, 8.4055, 8.1070, 7.5865, 7.1419,
        7.1027, 6.9552, 6.3518, 5.8887, 6.5380, 6.1702, 6.2465, 6.0601, 6.2930, 5.8388, 5.6609,
        5.3684,
    ];
    const LIVE_LOAD: [f32; 144] = [
        0.4339, 0.4339, 0.4339, 0.4339, 1.2261, 1.2261, 1.2261, 1.2261, 0.4348, 0.4348, 0.4348,
        0.4348, 0.6619, 0.6619, 0.6619, 0.6619, 1.0963, 1.0963, 1.0963, 1.0963, 0.8112, 0.8112,
        0.8112, 0.8112, 0.7426, 0.7426, 0.7426, 0.7426, 0.7888, 0.7888, 0.7888, 0.7888, 0.6657,
        0.6657, 0.6657, 0.6657, 0.5318, 0.5318, 0.5318, 0.5318, 0.7339, 0.7339, 0.7339, 0.7339,
        0.4219, 0.4219, 0.4219, 0.4219, 0.4585, 0.4585, 0.4585, 0.4585, 0.4400, 0.4400, 0.4400,
        0.4400, 0.3419, 0.3419, 0.3419, 0.3419, 0.3316, 0.3316, 0.3316, 0.3316, 0.2357, 0.2357,
        0.2357, 0.2357, 0.4293, 0.4293, 0.4293, 0.4293, 0.9282, 0.9282, 0.9282, 0.9282, 0.8392,
        0.8392, 0.8392, 0.8392, 0.9930, 0.9930, 0.9930, 0.9930, 1.0913, 1.0913, 1.0913, 1.0913,
        0.7935, 0.7935, 0.7935, 0.7935, 0.7796, 0.7796, 0.7796, 0.7796, 0.4339, 0.4339, 0.4339,
        0.4339, 1.2261, 1.2261, 1.2261, 1.2261, 0.4348, 0.4348, 0.4348, 0.4348, 0.6619, 0.6619,
        0.6619, 0.6619, 1.0963, 1.0963, 1.0963, 1.0963, 0.8112, 0.8112, 0.8112, 0.8112, 0.7426,
        0.7426, 0.7426, 0.7426, 0.7888, 0.7888, 0.7888, 0.7888, 0.6657, 0.6657, 0.6657, 0.6657,
        0.5318, 0.5318, 0.5318, 0.5318, 0.7339, 0.7339, 0.7339, 0.7339, 0.4219, 0.4219, 0.4219,
        0.4219,
    ];
    const LIVE_PV: [f32; 144] = [
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.5035, 0.4557, 0.4044, 0.3553,
        0.3040, 0.2480, 0.1933, 0.1311, 0.0689, 0.0225, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0478,
        0.0610, 0.1154, 0.1665, 0.2207, 0.2758, 0.3207, 0.3708, 0.4262, 0.4871, 0.5502, 0.5923,
        0.6322, 0.6636, 0.6927, 0.7155, 0.7313, 0.7408, 0.7423, 0.7372, 0.7266, 0.7104, 0.6892,
        0.6609, 0.6294, 0.5938, 0.5511, 0.5032, 0.4571, 0.4030, 0.3533, 0.3001, 0.2447, 0.1866,
        0.1247, 0.0630, 0.0152, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000, 0.0000,
        0.0000,
    ];
    /// The live plan of that replan (`fluxion-ro plan owner`, read 12:47Z)
    const LIVE_PLAN: &str = "HHHHHHHEEEEEECCCCEHEHEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCCEEEEHHHESSSSSSSSSSSSSSS";
    /// 18:00-19:15 local in `LIVE_PLAN` (`EHEHEE`)
    const LIVE_ZIGZAG: std::ops::Range<usize> = 17..23;
    /// 12:45Z in the `LIVE_*` arrays
    const LIVE_HEAD: usize = 7;
    /// Reserve load over expected load of that replan (125.68 / 93.63; p75 profile not read)
    const LIVE_RESERVE_FACTOR: f32 = 125.68 / 93.63;

    /// The owner's discharge-window levers: morning 4 h, evening `evening_hours`, both at
    /// `min_soc_pct` (`fluxion_max_*_vybiti_baterie`)
    fn owner_levers(evening_hours: u8, min_soc_pct: f32) -> UserLevers {
        UserLevers {
            morning_discharge: Some(DischargeWindowLimit {
                hours: 4,
                min_soc_pct,
            }),
            evening_discharge: Some(DischargeWindowLimit {
                hours: evening_hours,
                min_soc_pct,
            }),
            ..UserLevers::default()
        }
    }

    fn block_at(start: DateTime<Utc>, buy: f32, sell: f32, load: f32, pv: f32) -> PlanBlock {
        PlanBlock {
            start,
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

    /// The `LIVE_*` blocks from 11:00Z, before the levers
    fn live_blocks() -> Vec<PlanBlock> {
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 11, 0, 0).unwrap();
        (0..LIVE_BUY.len())
            .map(|i| PlanBlock {
                reserve_load_kwh: LIVE_LOAD[i] * LIVE_RESERVE_FACTOR,
                ..block_at(
                    start + Duration::minutes(15 * i as i64),
                    LIVE_BUY[i],
                    LIVE_SELL[i],
                    LIVE_LOAD[i],
                    LIVE_PV[i],
                )
            })
            .collect()
    }

    /// The owner's battery of that replan: 23 kWh, 10 kW, export 9 kW, Back Up floor 25 %, cost
    /// of charge 4.58 (`battery_cost_basis`)
    fn owner_battery() -> BatteryParams {
        BatteryParams {
            capacity_kwh: 23.0,
            min_soc_pct: 25.0,
            max_soc_pct: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            efficiency: 0.95,
            wear_cost_czk_per_kwh: 0.1,
            max_export_kw: 9.0,
            export_guard_margin_kwh: 10.0 * 300.0 / 3600.0,
            force_discharge_enabled: true,
            hold_pv_to_battery: false,
            min_charge_kwh: min_charge_kwh(10.0, 300),
            charge_cost_czk_per_kwh: Some(4.58),
        }
    }

    /// The owner's add-on options (0.2.74) with `continuity_pct` and `reversal_czk`
    fn owner_params(continuity_pct: f32, reversal_czk: f32) -> WinterReserveParams {
        WinterReserveParams {
            min_arbitrage_profit_czk: 1.5,
            insane_price_multiplier: 3.0,
            cheap_percentile: 0.2,
            load_safety_factor: 1.1,
            pv_confidence: 0.9,
            min_mode_benefit_czk: 0.1,
            mode_switch_min_benefit_czk: 1.0,
            direction_reversal_cost_czk: reversal_czk,
            export_window_continuity_pct: continuity_pct,
            export_selection: ExportSelection::Windows,
            daily_export_hours: 6,
            soc_resolution_pct: 0.5,
            export_floor_until_cheap: true,
            cautious_pv_fallback_months: vec![10, 11, 12, 1, 2, 3],
        }
    }

    /// The owner's live horizon at 12:45Z with the levers at `evening_hours` and `continuity_pct`
    fn owner_live_horizon(evening_hours: u8, continuity_pct: f32) -> Vec<PlanBlock> {
        let mut blocks = live_blocks();
        apply_user_levers(
            &mut blocks,
            Some(&owner_levers(evening_hours, 25.0)),
            Some(TZ),
            continuity_pct,
        );
        blocks.split_off(LIVE_HEAD)
    }

    fn letters(modes: impl IntoIterator<Item = PlanMode>) -> String {
        modes
            .into_iter()
            .map(|m| match m {
                PlanMode::SelfUse => 'S',
                PlanMode::Charge => 'C',
                PlanMode::Hold => 'H',
                PlanMode::Export => 'E',
                PlanMode::NoDischarge => 'N',
            })
            .collect()
    }

    fn plan_letters(plan: &Plan) -> String {
        letters(plan.blocks.iter().map(|b| b.mode))
    }

    fn allowed(blocks: &[PlanBlock]) -> String {
        blocks
            .iter()
            .map(|b| if b.export_allowed { '1' } else { '0' })
            .collect()
    }

    fn live_plan(evening_hours: u8, continuity_pct: f32) -> Plan {
        let blocks = owner_live_horizon(evening_hours, continuity_pct);
        plan_from(
            &blocks,
            99.0,
            TZ,
            &owner_battery(),
            &owner_params(continuity_pct, 2.0),
            Some(PlanMode::Hold),
        )
    }

    /// Index of the local time `hh:mm` on 2026-11-05 in the horizon of `owner_live_horizon`
    fn at(hh: u32, mm: u32) -> usize {
        let local = TZ.with_ymd_and_hms(2026, 11, 5, hh, mm, 0).unwrap();
        let first = Utc.with_ymd_and_hms(2026, 11, 5, 12, 45, 0).unwrap();
        usize::try_from((local.with_timezone(&Utc) - first).num_minutes() / 15).unwrap()
    }

    /// Prints the golden of `zero_continuity_matches_0_2_74` (pct 0) and the plans with
    /// continuity (`--ignored --nocapture`)
    #[test]
    #[ignore = "golden printer, run by hand"]
    fn print_golden() {
        for pct in [0.0f32, 2.5, 5.0, 10.0] {
            for hours in [4u8, 5, 6] {
                let blocks = owner_live_horizon(hours, pct);
                let plan = live_plan(hours, pct);
                println!(
                    "golden|{hours}|{}|{}|{:#010x}|pct {pct}|17:00-21:45 {}",
                    allowed(&blocks),
                    plan_letters(&plan),
                    plan.expected_cost_czk.to_bits(),
                    &plan_letters(&plan)[at(17, 0)..at(22, 0)]
                );
            }
        }
    }
    /// 0.2.74 (e13f01a3) on `owner_live_horizon`: evening hours, export-allowed mask, plan
    /// letters and `expected_cost_czk` bits, printed by `print_golden` on that commit
    const GOLDEN_0_2_74: [(u8, &str, &str, u32); 3] = [
        (
            4,
            "10000001111111111101011001100000000000000000000000000000000000000011111111111111011000000000000000000001111111111111110001000000000000000",
            "HHHHHHHEEEEEECCCCEHEHEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCCEEEEHHHESSSSSSSSSSSSSSS",
            0xc289db47,
        ),
        (
            5,
            "10000001111111111111111111100000000000000000000000000000000000000011111111111111011000000000000000000011111111111111111011100000000000000",
            "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSCEEEHEEECCCCEEEECCCEEESSSSSSSSSSSSSS",
            0xc2bc15ff,
        ),
        (
            6,
            "10001011111111111111111111110100000000000000000000000000000000000011111111111111011000000000000000001011111111111111111111111000000000000",
            "HHHHECEEEEHEECCCCCCEEEECCEEEHEHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSCECEEEHEEECCCCEEEECCCCEEEESSSSSSSSSSSS",
            0xc2fb469a,
        ),
    ];

    #[test]
    fn zero_continuity_matches_0_2_74() {
        for (hours, mask, plan_letters_0_2_74, cost_bits) in GOLDEN_0_2_74 {
            let blocks = owner_live_horizon(hours, 0.0);
            assert_eq!(allowed(&blocks), mask, "{hours} h export mask");
            let plan = live_plan(hours, 0.0);
            assert_eq!(plan_letters(&plan), plan_letters_0_2_74, "{hours} h plan");
            assert_eq!(
                plan.expected_cost_czk.to_bits(),
                cost_bits,
                "{hours} h cost"
            );
        }
        // The 4 h plan is the live one
        assert_eq!(plan_letters(&live_plan(4, 0.0)), LIVE_PLAN);
    }

    /// The owner's evening at 4 h: 18:15 (7.83) and 18:45 (7.79) fell just below the lever's
    /// cut-off, so the live plan read E H E H E E at 18:00-19:15. Within 5 % of a chosen
    /// neighbour (18:15 vs 18:00 8.13 / 18:30 8.01; 18:45 vs 18:30 8.01 / 19:00 8.46) they join
    /// the run.
    #[test]
    fn gap_fill_joins_the_owner_evening_at_4h() {
        let evening = at(18, 0)..=at(19, 15);
        let off = owner_live_horizon(4, 0.0);
        let on = owner_live_horizon(4, 5.0);
        for (hh, mm) in [(18, 15), (18, 45)] {
            assert!(
                !off[at(hh, mm)].export_allowed,
                "{hh}:{mm} without continuity"
            );
            assert!(on[at(hh, mm)].export_allowed, "{hh}:{mm} with 5 %");
        }
        for (hh, mm) in [(18, 0), (18, 30), (19, 0), (19, 15)] {
            assert!(
                off[at(hh, mm)].export_allowed,
                "{hh}:{mm} chosen by the lever"
            );
        }
        let before = plan_letters(&live_plan(4, 0.0))[evening.clone()].to_string();
        assert_eq!(before, &LIVE_PLAN[LIVE_ZIGZAG]);
        assert!(before.contains("EHEH"), "{before}");
        let after = plan_letters(&live_plan(4, 5.0))[evening].to_string();
        assert!(!after.contains("EHEH"), "18:00-19:15 at 5 %: {after}");
        assert!(!after.contains('H'), "18:00-19:15 at 5 %: {after}");
    }

    /// 2026-11-05 local blocks with sell 5.0, except `sells` at local (hh, mm)
    fn day_with(sells: &[((u32, u32), f32)]) -> Vec<PlanBlock> {
        let first = TZ.with_ymd_and_hms(2026, 11, 5, 0, 0, 0).unwrap();
        (0..96)
            .map(|i| {
                let start = first.with_timezone(&Utc) + Duration::minutes(15 * i);
                let local = start.with_timezone(&TZ);
                let sell = sells
                    .iter()
                    .find(|((hh, mm), _)| local.hour() == *hh && local.minute() == *mm)
                    .map_or(5.0, |(_, s)| *s);
                block_at(start, 3.22, sell, 0.5, 0.0)
            })
            .collect()
    }

    /// Local times of the evening blocks `export_eligible` returns for 1 h (4 blocks)
    fn evening_eligible(blocks: &[PlanBlock], pct: f32) -> Vec<(u32, u32)> {
        let mut chosen = export_eligible(blocks, TZ, in_evening_window, DayBudget::Hours(1), pct);
        chosen.sort_unstable();
        chosen
            .into_iter()
            .map(|i| {
                let local = blocks[i].start.with_timezone(&TZ);
                (local.hour(), local.minute())
            })
            .collect()
    }

    /// One-block gaps: in within `pct` of either chosen neighbour, out when more than `pct` below
    /// both
    #[test]
    fn a_gap_block_more_than_pct_below_both_neighbours_stays_out() {
        // Chosen: 18:00, 18:30, 19:15 at 9.0 and 19:00 at 8.9. 18:15 is 5.6 % below both of its
        // neighbours; 18:45 is 5.6 % below 18:30 but 4.5 % below 19:00.
        let blocks = day_with(&[
            ((18, 0), 9.0),
            ((18, 15), 8.5),
            ((18, 30), 9.0),
            ((18, 45), 8.5),
            ((19, 0), 8.9),
            ((19, 15), 9.0),
        ]);
        let lever = vec![(18, 0), (18, 30), (19, 0), (19, 15)];
        assert_eq!(evening_eligible(&blocks, 0.0), lever);
        assert_eq!(
            evening_eligible(&blocks, 5.0),
            vec![(18, 0), (18, 30), (18, 45), (19, 0), (19, 15)],
            "18:15 out (5.6 % below both), 18:45 in (4.5 % below 19:00)"
        );
        assert_eq!(evening_eligible(&blocks, 4.0), lever);
        assert_eq!(
            evening_eligible(&blocks, 6.0),
            vec![(18, 0), (18, 15), (18, 30), (18, 45), (19, 0), (19, 15)]
        );
    }

    /// Continuity fills gaps of one or two blocks between chosen blocks only: never before the
    /// first or after the last chosen block, never a gap of three, and a two-block gap only whole
    #[test]
    fn continuity_never_extends_a_run_outward() {
        // Chosen: 18:00, 18:15, 19:00, 19:15 at 9.0; 17:45 and 19:30 at 8.99 sit just outside.
        let two_block_gap = |second: f32| {
            day_with(&[
                ((17, 45), 8.99),
                ((18, 0), 9.0),
                ((18, 15), 9.0),
                ((18, 30), 8.8),
                ((18, 45), second),
                ((19, 0), 9.0),
                ((19, 15), 9.0),
                ((19, 30), 8.99),
            ])
        };
        assert_eq!(
            evening_eligible(&two_block_gap(8.8), 5.0),
            vec![(18, 0), (18, 15), (18, 30), (18, 45), (19, 0), (19, 15)]
        );
        // 18:45 is 11 % below 19:00: the whole two-block gap stays out, 18:30 too
        assert_eq!(
            evening_eligible(&two_block_gap(8.0), 10.0),
            vec![(18, 0), (18, 15), (19, 0), (19, 15)]
        );
        // A three-block gap stays as the lever chose
        let three_block_gap = day_with(&[
            ((18, 0), 9.0),
            ((18, 15), 9.0),
            ((18, 30), 8.9),
            ((18, 45), 8.9),
            ((19, 0), 8.9),
            ((19, 15), 9.0),
            ((19, 30), 9.0),
        ]);
        assert_eq!(
            evening_eligible(&three_block_gap, 10.0),
            vec![(18, 0), (18, 15), (19, 15), (19, 30)]
        );
        // A gap block that must not discharge is not filled
        let mut blocks = two_block_gap(8.8);
        let i = blocks
            .iter()
            .position(|b| b.start.with_timezone(&TZ).hour() == 18 && b.start.minute() == 30)
            .unwrap();
        blocks[i].no_discharge = true;
        assert_eq!(
            evening_eligible(&blocks, 5.0),
            vec![(18, 0), (18, 15), (19, 0), (19, 15)]
        );
    }

    #[test]
    fn continuity_defaults_to_5_pct() {
        assert!((WinterReserveParams::default().export_window_continuity_pct - 5.0).abs() < 1e-6);
        let mut json = serde_json::to_value(WinterReserveParams::default()).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("export_window_continuity_pct");
        let parsed: WinterReserveParams = serde_json::from_value(json).unwrap();
        assert!((parsed.export_window_continuity_pct - 5.0).abs() < 1e-6);
        let core = fluxion_types::config::WinterReserveConfigCore::default();
        assert!((core.export_window_continuity_pct - 5.0).abs() < 1e-6);
    }

    /// One cell of the closed-loop day grid
    struct Cell {
        label: &'static str,
        evening_hours: u8,
        min_soc_pct: f32,
        continuity_pct: f32,
        reversal_czk: f32,
        levers: bool,
    }

    /// A day to simulate: blocks (expected load and PV as the planner sees them), the simulated
    /// range, the start state and whether tomorrow's prices are known only after 13:00 local
    struct SimDay {
        name: String,
        blocks: Vec<PlanBlock>,
        from: usize,
        to: usize,
        soc_pct: f32,
        current: Option<PlanMode>,
        tomorrow_after_13: bool,
    }

    fn sim_day_from_json(path: &str) -> SimDay {
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let num = |v: &serde_json::Value| v.as_f64().unwrap() as f32;
        let blocks = json["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                let start = DateTime::parse_from_rfc3339(b["start"].as_str().unwrap())
                    .unwrap()
                    .with_timezone(&Utc);
                let load = num(&b["load"]);
                PlanBlock {
                    reserve_load_kwh: load * LIVE_RESERVE_FACTOR,
                    ..block_at(start, num(&b["buy"]), num(&b["sell"]), load, num(&b["pv"]))
                }
            })
            .collect();
        SimDay {
            name: json["name"].as_str().unwrap().to_string(),
            blocks,
            from: json["sim_from"].as_u64().unwrap() as usize,
            to: json["sim_to"].as_u64().unwrap() as usize,
            soc_pct: num(&json["soc_pct"]),
            current: None,
            tomorrow_after_13: true,
        }
    }

    /// Today's remaining horizon: the live fixture from 12:45Z to the end of the local day
    fn sim_day_live() -> SimDay {
        let blocks = live_blocks();
        let end = Utc.with_ymd_and_hms(2026, 11, 5, 23, 0, 0).unwrap();
        let to = blocks.iter().position(|b| b.start >= end).unwrap();
        SimDay {
            name: "2026-11-05 from 13:45".into(),
            blocks,
            from: LIVE_HEAD,
            to,
            soc_pct: 99.0,
            current: Some(PlanMode::Hold),
            tomorrow_after_13: false,
        }
    }

    /// Closed loop as the replay runs it: replan every block from the realised SOC, apply the
    /// head (export guard as live) to the block with PV × `pv_factor` and load × `load_factor`
    fn simulate(day: &SimDay, cell: &Cell, pv_factor: f32, load_factor: f32) -> String {
        let battery = owner_battery();
        let params = owner_params(cell.continuity_pct, cell.reversal_czk);
        let mut blocks = day.blocks.clone();
        let levers = owner_levers(cell.evening_hours, cell.min_soc_pct);
        apply_user_levers(
            &mut blocks,
            cell.levers.then_some(&levers),
            Some(TZ),
            cell.continuity_pct,
        );
        let cap = battery.capacity_kwh;
        let mut energy = (day.soc_pct / 100.0 * cap).clamp(
            battery.min_soc_pct / 100.0 * cap,
            battery.max_soc_pct / 100.0 * cap,
        );
        let mut applied = day.current;
        let mut direction = ForceDirection::of(day.current);
        let (mut cost, mut wear, mut exported) = (0.0f32, 0.0f32, 0.0f32);
        let mut executed = Vec::new();
        let mut evening = String::new();
        let mut evening_allowed = String::new();
        for k in day.from..day.to {
            let local = blocks[k].start.with_timezone(&TZ);
            let horizon_day = if !day.tomorrow_after_13 || local.hour() >= 13 {
                local.date_naive().succ_opt().unwrap()
            } else {
                local.date_naive()
            };
            let end = blocks
                .iter()
                .position(|b| b.start.with_timezone(&TZ).date_naive() > horizon_day)
                .unwrap_or(blocks.len());
            let plan = plan_from_direction(
                &blocks[k..end],
                energy / cap * 100.0,
                TZ,
                &battery,
                &params,
                applied,
                direction,
            );
            let head = &plan.blocks[0];
            applied = Some(head.mode);
            direction = direction.after(head.mode);
            let mode = if head.mode == PlanMode::Export
                && export_guard_holds(energy, head.floor_kwh, &battery)
            {
                PlanMode::SelfUse
            } else {
                head.mode
            };
            let b = &blocks[k];
            let actual = block_at(
                b.start,
                b.buy_czk,
                b.sell_czk,
                b.load_kwh * load_factor,
                b.pv_kwh * pv_factor,
            );
            let out = simulate_block(mode, energy, &actual, head.floor_kwh, &battery);
            cost += out.grid_import_kwh * b.buy_czk - out.grid_export_kwh * b.sell_czk;
            wear += out.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
            exported += out.grid_export_kwh;
            energy = out.energy_end_kwh;
            executed.push(mode);
            if (15..22).contains(&local.hour()) {
                evening.push_str(&letters([mode]));
                evening_allowed.push(if b.export_allowed { '1' } else { '0' });
            }
        }
        let changes = executed.windows(2).filter(|w| w[0] != w[1]).count();
        let mut dir = ForceDirection::None;
        let mut reversals = 0;
        for &m in &executed {
            if dir.reversed_by(m) {
                reversals += 1;
            }
            dir = dir.after(m);
        }
        let runs = executed
            .iter()
            .enumerate()
            .filter(|&(i, &m)| m == PlanMode::Export && (i == 0 || executed[i - 1] != m))
            .count();
        let buys: Vec<f32> = blocks[day.from..day.to].iter().map(|b| b.buy_czk).collect();
        let cheap_ref = cheap_reference(&buys, params.cheap_percentile);
        let end_value = energy * cheap_ref * battery.efficiency;
        let net = cost + wear - end_value;
        format!(
            "sim|{}|pv{pv_factor}/load{load_factor}|{}|{}|{}|{}|{}|net {net:.2}|grid {cost:.2}|wear {wear:.2}|end {:.1}% value {end_value:.2}|exported {exported:.2}|changes {changes}|reversals {reversals}|runs {runs}|15-22 {evening}|allowed {evening_allowed}",
            day.name,
            cell.label,
            if cell.levers {
                cell.evening_hours.to_string()
            } else {
                "-".into()
            },
            cell.min_soc_pct,
            cell.continuity_pct,
            cell.reversal_czk,
            energy / cap * 100.0,
        )
    }

    /// Closed-loop day grid (brief "find the optimal setup"). `CONTINUITY_SIM_DAY` = path of a
    /// day JSON (`.local/work/continuity/tools/build_inputs.py`), or unset for the live fixture.
    /// Run in release with `--ignored --nocapture`.
    #[test]
    #[ignore = "simulation grid, run by hand"]
    fn closed_loop_grid() {
        let day = match std::env::var("CONTINUITY_SIM_DAY") {
            Ok(path) => sim_day_from_json(&path),
            Err(_) => sim_day_live(),
        };
        let mut cells = Vec::new();
        for evening_hours in [4u8, 5, 6] {
            for continuity_pct in [0.0f32, 2.5, 5.0, 10.0] {
                for reversal_czk in [2.0f32, 4.0] {
                    cells.push(Cell {
                        label: "grid",
                        evening_hours,
                        min_soc_pct: 25.0,
                        continuity_pct,
                        reversal_czk,
                        levers: true,
                    });
                }
            }
        }
        for reversal_czk in [2.0f32, 4.0] {
            cells.push(Cell {
                label: "max0",
                evening_hours: 6,
                min_soc_pct: 0.0,
                continuity_pct: 5.0,
                reversal_czk,
                levers: true,
            });
            cells.push(Cell {
                label: "nolever",
                evening_hours: 0,
                min_soc_pct: 0.0,
                continuity_pct: 0.0,
                reversal_czk,
                levers: false,
            });
        }
        let lines: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = cells
                .iter()
                .flat_map(|cell| {
                    let day = &day;
                    [(1.0f32, 1.0f32), (0.8, 1.2)]
                        .map(|(pv, load)| scope.spawn(move || simulate(day, cell, pv, load)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for line in lines {
            println!("{line}");
        }
    }

    #[derive(serde::Deserialize)]
    struct OwnerDayBlock {
        start: DateTime<Utc>,
        buy: f32,
        sell: f32,
        load: f32,
        pv: f32,
    }

    #[derive(serde::Deserialize)]
    struct OwnerDays {
        blocks: Vec<OwnerDayBlock>,
    }

    /// The owner's 2026-11-04 and 2026-11-05 local, 192 blocks from 00:00 local (sell =
    /// spot − 0.35, equal to `LIVE_SELL` where they overlap)
    fn owner_days() -> Vec<OwnerDayBlock> {
        serde_json::from_str::<OwnerDays>(include_str!(
            "../../tests/fixtures/winter_reserve_owner_2026_11_04_05.json"
        ))
        .expect("owner days fixture parses")
        .blocks
    }

    fn price_of(b: &OwnerDayBlock) -> TimeBlockPrice {
        TimeBlockPrice {
            buy_price_czk_per_kwh: None,
            block_start: b.start,
            duration_minutes: 15,
            price_czk_per_kwh: b.buy,
            effective_price_czk_per_kwh: b.buy,
            spot_sell_price_czk_per_kwh: Some(b.sell),
        }
    }

    /// 2026-11-05 00:00-11:45 local of `owner_days`: the started blocks the scheduler drops
    /// before `live_blocks`, as ranking context
    fn live_morning_ranking() -> Vec<TimeBlockPrice> {
        let first = live_blocks()[0].start;
        owner_days()
            .iter()
            .filter(|b| {
                b.start < first
                    && b.start.with_timezone(&TZ).date_naive()
                        == first.with_timezone(&TZ).date_naive()
            })
            .map(price_of)
            .collect()
    }

    /// Daily top-N export selection (owner 2026-11-05)
    mod daily_top_n_tests {
        use super::*;

        /// `print_golden` on 3a984783 (legacy/lever-continuity, before the export selection):
        /// continuity %, evening hours, export-allowed mask, plan letters, `expected_cost_czk` bits
        /// (`.local/work/daily-topn/golden_3a984783.txt`)
        const GOLDEN_3A984783: [(f32, u8, &str, &str, u32); 9] = [
            (
                2.5,
                4,
                "10000001111111111111011001100000000000000000000000000000000000000011111111111111011000000000000000000001111111111111110001000000000000000",
                "HHHHHHHEEEEEECCCCEEECEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCCEEEEHHHESSSSSSSSSSSSSSS",
                0xc298b189,
            ),
            (
                2.5,
                5,
                "10000001111111111111111111100000000000000000000000000000000000000011111111111111011000000000000000000011111111111111111111100000000000000",
                "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSCEEEHEEECCCCEEEECCCEEESSSSSSSSSSSSSS",
                0xc2bc15ff,
            ),
            (
                2.5,
                6,
                "10001011111111111111111111110100000000000000000000000000000000000011111111111111011000000000000000001011111111111111111111111000000000000",
                "HHHHECEEEEHEECCCCCCEEEECCEEEHEHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSCECEEEHEEECCCCEEEECCCCEEEESSSSSSSSSSSS",
                0xc2fb469a,
            ),
            (
                5.0,
                4,
                "10000001111111111111111001100000000000000000000000000000000000000011111111111111011000000000000000000001111111111111110001000000000000000",
                "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCCEEEEHHHESSSSSSSSSSSSSSS",
                0xc29785a1,
            ),
            (
                5.0,
                5,
                "10000001111111111111111111100000000000000000000000000000000000000011111111111111011000000000000000000011111111111111111111100000000000000",
                "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSCEEEHEEECCCCEEEECCCEEESSSSSSSSSSSSSS",
                0xc2bc15ff,
            ),
            (
                5.0,
                6,
                "10001011111111111111111111110100000000000000000000000000000000000011111111111111011000000000000000001011111111111111111111111000000000000",
                "HHHHECEEEEHEECCCCCCEEEECCEEEHEHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSCECEEEHEEECCCCEEEECCCCEEEESSSSSSSSSSSS",
                0xc2fb469a,
            ),
            (
                10.0,
                4,
                "10000001111111111111111111100000000000000000000000000000000000000011111111111111011000000000000000000001111111111111110001000000000000000",
                "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCCEEEEHHHESSSSSSSSSSSSSSS",
                0xc29785a1,
            ),
            (
                10.0,
                5,
                "10000001111111111111111111100000000000000000000000000000000000000011111111111111011000000000000000000011111111111111111111100000000000000",
                "HHHHHHHEEEEEECCCCCEEEEECCEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSSSCEEEHEEECCCCEEEECCCEEESSSSSSSSSSSSSS",
                0xc2bc15ff,
            ),
            (
                10.0,
                6,
                "10001111111111111111111111111100000000000000000000000000000000000011111111111111011000000000000000001111111111111111111111111000000000000",
                "HHHHEECCEEEEEECCCCCCEEECCEEEEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEECCCCEEEEEEHECCCCCCCHHHHSSSSSSCECEEEHEEECCCCEEEECCCCEEEESSSSSSSSSSSS",
                0xc2fc5488,
            ),
        ];

        fn params(
            selection: ExportSelection,
            hours: u8,
            continuity_pct: f32,
        ) -> WinterReserveParams {
            WinterReserveParams {
                export_selection: selection,
                daily_export_hours: hours,
                ..owner_params(continuity_pct, 2.0)
            }
        }

        /// The owner's live horizon at 12:45Z with the export rules of `params` and the owner's
        /// levers (morning 4 h, evening `evening_hours`, 25 %)
        fn horizon(params: &WinterReserveParams, evening_hours: u8) -> Vec<PlanBlock> {
            let mut blocks = live_blocks();
            apply_export_rules(
                &mut blocks,
                Some(&owner_levers(evening_hours, 25.0)),
                Some(TZ),
                params,
            );
            blocks.split_off(LIVE_HEAD)
        }

        /// `horizon` with today's started blocks (00:00-11:45 local) as ranking context, as the
        /// scheduler sends them since review F1
        fn horizon_ranked(params: &WinterReserveParams, evening_hours: u8) -> Vec<PlanBlock> {
            let blocks = live_blocks();
            let prefix = ranking_prefix(&live_morning_ranking(), blocks[0].start, TZ, 0.0);
            assert_eq!(prefix.len(), 48);
            let mut blocks = apply_export_rules_ranked(
                blocks,
                prefix,
                Some(&owner_levers(evening_hours, 25.0)),
                Some(TZ),
                params,
            );
            blocks.split_off(LIVE_HEAD)
        }

        /// Prints `GOLDEN_RANKED` (`--ignored --nocapture`)
        #[test]
        #[ignore = "golden printer, run by hand"]
        fn print_ranked_golden() {
            for (pct, hours, ..) in GOLDEN_3A984783 {
                let p = params(ExportSelection::Windows, 6, pct);
                let blocks = horizon_ranked(&p, hours);
                let plan = plan_from(
                    &blocks,
                    99.0,
                    TZ,
                    &owner_battery(),
                    &p,
                    Some(PlanMode::Hold),
                );
                println!(
                    "ranked|{pct}|{hours}|{}|{}|{:#010x}",
                    allowed(&blocks),
                    plan_letters(&plan),
                    plan.expected_cost_czk.to_bits()
                );
            }
        }

        /// `windows` (the default) plans as 3a984783 did, bit for bit, whatever
        /// `daily_export_hours` says, on the same blocks (the list from 12:00 local, no ranking
        /// context). With today's started blocks as ranking context (review F1) the golden moves
        /// deliberately in one place: 13:45, the last morning-window block, is no longer among the
        /// best 4 h of 04:00-14:00 (the 06:00-10:00 peak is), so its export bit clears. The plan
        /// letters and the cost stay bit-exact: that replan held at 13:45 anyway.
        #[test]
        fn windows_mode_is_bit_exact_against_3a984783() {
            assert_eq!(
                WinterReserveParams::default().export_selection,
                ExportSelection::Windows
            );
            for (pct, hours, mask, golden_letters, cost_bits) in GOLDEN_3A984783 {
                for top_n_hours in [6u8, 1] {
                    let p = params(ExportSelection::Windows, top_n_hours, pct);
                    let blocks = horizon(&p, hours);
                    assert_eq!(allowed(&blocks), mask, "{hours} h, {pct} % mask");
                    let plan = plan_from(
                        &blocks,
                        99.0,
                        TZ,
                        &owner_battery(),
                        &p,
                        Some(PlanMode::Hold),
                    );
                    assert_eq!(plan_letters(&plan), golden_letters, "{hours} h, {pct} %");
                    assert_eq!(
                        plan.expected_cost_czk.to_bits(),
                        cost_bits,
                        "{hours} h, {pct} % cost"
                    );

                    let ranked = horizon_ranked(&p, hours);
                    let moved = format!("0{}", &mask[1..]);
                    assert!(mask.starts_with('1'), "13:45 was allowed on 3a984783");
                    assert_eq!(allowed(&ranked), moved, "{hours} h, {pct} % ranked mask");
                    let plan = plan_from(
                        &ranked,
                        99.0,
                        TZ,
                        &owner_battery(),
                        &p,
                        Some(PlanMode::Hold),
                    );
                    assert_eq!(
                        plan_letters(&plan),
                        golden_letters,
                        "{hours} h, {pct} % ranked"
                    );
                    assert_eq!(
                        plan.expected_cost_czk.to_bits(),
                        cost_bits,
                        "{hours} h, {pct} % ranked cost"
                    );
                }
            }
        }

        /// 2026-11-05: 06:00-10:45 sell 7.0 except 08:00 at 6.8; a midday peak 12:00-12:45 at
        /// 9.0; 21:00 at 8.5; a mediocre evening 19:00 at 6.0; the rest 5.0. The best 6 h (24
        /// blocks): the midday peak, 21:00 and the 19 morning blocks at 7.0.
        fn ranking_day() -> Vec<PlanBlock> {
            let mut sells = vec![((12, 0), 9.0), ((12, 15), 9.0), ((12, 30), 9.0)];
            sells.extend([((12, 45), 9.0), ((21, 0), 8.5), ((19, 0), 6.0)]);
            for hh in 6..11 {
                for mm in [0, 15, 30, 45] {
                    sells.push(((hh, mm), if (hh, mm) == (8, 0) { 6.8 } else { 7.0 }));
                }
            }
            day_with(&sells)
        }

        fn eligible_times(blocks: &[PlanBlock]) -> Vec<(u32, u32)> {
            blocks
                .iter()
                .filter(|b| b.export_allowed)
                .map(|b| {
                    let local = b.start.with_timezone(&TZ);
                    (local.hour(), local.minute())
                })
                .collect()
        }

        fn ranked(
            levers: Option<&UserLevers>,
            selection: ExportSelection,
            pct: f32,
        ) -> Vec<PlanBlock> {
            let mut blocks = ranking_day();
            apply_export_rules(&mut blocks, levers, Some(TZ), &params(selection, 6, pct));
            blocks
        }

        #[test]
        fn daily_top_n_ranks_the_whole_local_day_and_ignores_the_hour_levers() {
            // Evening 1 h and morning 4 h at 30 %: in windows mode they decide the blocks
            let levers = owner_levers(1, 30.0);
            let top = ranked(Some(&levers), ExportSelection::DailyTopN, 0.0);
            let times = eligible_times(&top);
            assert_eq!(times.len(), 24, "{times:?}");
            assert!(
                times.contains(&(12, 0)) && times.contains(&(12, 45)),
                "midday peak"
            );
            assert!(times.contains(&(21, 0)), "evening peak");
            assert!(!times.contains(&(19, 0)), "mediocre evening block");
            assert!(
                !times.contains(&(8, 0)),
                "below the cut-off without continuity"
            );
            // Continuity fills the one-block gap at 08:00 (6.8 within 5 % of 7.0), nothing else
            let filled = eligible_times(&ranked(Some(&levers), ExportSelection::DailyTopN, 5.0));
            let mut expected = times.clone();
            expected.push((8, 0));
            expected.sort_unstable();
            assert_eq!(filled, expected);
            // The levers' hours change nothing; their max-discharge % applies
            for other in [None, Some(owner_levers(6, 30.0))] {
                assert_eq!(
                    eligible_times(&ranked(other.as_ref(), ExportSelection::DailyTopN, 5.0)),
                    filled
                );
            }
            for b in ranked(Some(&levers), ExportSelection::DailyTopN, 5.0) {
                let soc = if b.export_allowed { Some(30.0) } else { None };
                assert_eq!(b.export_min_soc_pct, soc, "{}", b.start);
            }
            // Windows mode with the same levers does not take 21:00
            let windows = eligible_times(&ranked(Some(&levers), ExportSelection::Windows, 5.0));
            assert!(!windows.contains(&(21, 0)), "{windows:?}");
            // The DP sells the midday peak, not the mediocre evening block
            let blocks = ranked(Some(&levers), ExportSelection::DailyTopN, 5.0);
            let plan = plan(
                &blocks,
                80.0,
                TZ,
                &owner_battery(),
                &params(ExportSelection::DailyTopN, 6, 5.0),
            );
            let mode_at = |hh: u32, mm: u32| {
                plan.blocks
                    .iter()
                    .find(|b| {
                        let l = b.start.with_timezone(&TZ);
                        (l.hour(), l.minute()) == (hh, mm)
                    })
                    .unwrap()
                    .mode
            };
            assert_eq!(mode_at(12, 0), PlanMode::Export);
            assert_ne!(mode_at(19, 0), PlanMode::Export);
        }

        /// Exactly hours × 4 blocks per complete local day before the gap fills, on both days of
        /// the owner's horizon (11-05 completed by its morning) and for every N
        #[test]
        fn daily_top_n_takes_exactly_n_hours_per_local_day() {
            for hours in [1u8, 4, 6, 8, 24] {
                let p = params(ExportSelection::DailyTopN, hours, 0.0);
                let live = live_blocks();
                let mut blocks = ranking_prefix(&live_morning_ranking(), live[0].start, TZ, 0.0);
                blocks.extend(live);
                assert_eq!(blocks.len(), 192);
                apply_export_rules(&mut blocks, None, Some(TZ), &p);
                let mut per_day: std::collections::BTreeMap<NaiveDate, (usize, usize)> =
                    std::collections::BTreeMap::new();
                for b in &blocks {
                    let e = per_day
                        .entry(b.start.with_timezone(&TZ).date_naive())
                        .or_default();
                    e.0 += 1;
                    e.1 += usize::from(b.export_allowed);
                }
                for (day, (known, allowed)) in per_day {
                    assert_eq!(
                        allowed,
                        usize::from(hours) * 4,
                        "{hours} h {day}, {known} known"
                    );
                }
            }
        }

        /// One row of the daily top-N day grid
        struct Row {
            label: String,
            params: WinterReserveParams,
            levers: Option<UserLevers>,
        }

        /// Closed loop as `simulate`, sliced as the live scheduler slices (review F1): every
        /// replan applies the export rules of `row.params` to the remaining blocks only, with the
        /// blocks before them as ranking context (`ranking_prefix`, `apply_export_rules_ranked`,
        /// as `horizon_blocks` does). `discharged_before_kwh` is the battery discharge of the first
        /// simulated local day before `day.from` (reported only; no cap on this branch).
        fn simulate_row(
            day: &SimDay,
            row: &Row,
            pv_factor: f32,
            load_factor: f32,
            discharged_before_kwh: f32,
        ) -> String {
            let battery = owner_battery();
            let params = &row.params;
            let blocks = &day.blocks;
            let prices: Vec<TimeBlockPrice> = blocks
                .iter()
                .map(|b| TimeBlockPrice {
                    buy_price_czk_per_kwh: None,
                    block_start: b.start,
                    duration_minutes: b.duration_minutes,
                    price_czk_per_kwh: b.buy_czk,
                    effective_price_czk_per_kwh: b.buy_czk,
                    spot_sell_price_czk_per_kwh: Some(b.sell_czk),
                })
                .collect();
            let cap = battery.capacity_kwh;
            let mut energy = (day.soc_pct / 100.0 * cap).clamp(
                battery.min_soc_pct / 100.0 * cap,
                battery.max_soc_pct / 100.0 * cap,
            );
            let mut applied = day.current;
            let mut direction = ForceDirection::of(day.current);
            let (mut cost, mut wear, mut exported, mut discharged) = (0.0f32, 0.0f32, 0.0, 0.0);
            let mut executed = Vec::new();
            let mut letters_5_22 = String::new();
            let mut allowed_5_22 = String::new();
            let first_local = blocks[day.from].start.with_timezone(&TZ);
            for k in 0..day.to {
                let local = blocks[k].start.with_timezone(&TZ);
                if k < day.from {
                    if local.date_naive() == first_local.date_naive()
                        && (5..22).contains(&local.hour())
                    {
                        letters_5_22.push('.');
                        allowed_5_22.push('.');
                    }
                    continue;
                }
                let horizon_day = if !day.tomorrow_after_13 || local.hour() >= 13 {
                    local.date_naive().succ_opt().unwrap()
                } else {
                    local.date_naive()
                };
                let end = blocks
                    .iter()
                    .position(|b| b.start.with_timezone(&TZ).date_naive() > horizon_day)
                    .unwrap_or(blocks.len());
                // The scheduler sends the 24 h before the block as ranking context
                let from = blocks[..k]
                    .iter()
                    .position(|b| b.start >= blocks[k].start - Duration::hours(24))
                    .unwrap_or(k);
                // `DAILY_TOPN_NO_RANKING` set: no ranking context, as 91c344e7 ran live
                let prefix = if std::env::var_os("DAILY_TOPN_NO_RANKING").is_some() {
                    Vec::new()
                } else {
                    ranking_prefix(&prices[from..k], blocks[k].start, TZ, 0.0)
                };
                let horizon = apply_export_rules_ranked(
                    blocks[k..end].to_vec(),
                    prefix,
                    row.levers.as_ref(),
                    Some(TZ),
                    params,
                );
                let plan = plan_from_direction(
                    &horizon,
                    energy / cap * 100.0,
                    TZ,
                    &battery,
                    params,
                    applied,
                    direction,
                );
                let head = &plan.blocks[0];
                applied = Some(head.mode);
                direction = direction.after(head.mode);
                let mode = if head.mode == PlanMode::Export
                    && export_guard_holds(energy, head.floor_kwh, &battery)
                {
                    PlanMode::SelfUse
                } else {
                    head.mode
                };
                let b = &blocks[k];
                let actual = block_at(
                    b.start,
                    b.buy_czk,
                    b.sell_czk,
                    b.load_kwh * load_factor,
                    b.pv_kwh * pv_factor,
                );
                let out = simulate_block(mode, energy, &actual, head.floor_kwh, &battery);
                cost += out.grid_import_kwh * b.buy_czk - out.grid_export_kwh * b.sell_czk;
                wear += out.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
                exported += out.grid_export_kwh;
                discharged += out.battery_discharge_kwh;
                energy = out.energy_end_kwh;
                executed.push(mode);
                if (5..22).contains(&local.hour()) {
                    letters_5_22.push_str(&letters([mode]));
                    allowed_5_22.push(if horizon[0].export_allowed { '1' } else { '0' });
                }
            }
            let changes = executed.windows(2).filter(|w| w[0] != w[1]).count();
            let mut dir = ForceDirection::None;
            let mut reversals = 0;
            for &m in &executed {
                if dir.reversed_by(m) {
                    reversals += 1;
                }
                dir = dir.after(m);
            }
            let runs = executed
                .iter()
                .enumerate()
                .filter(|&(i, &m)| m == PlanMode::Export && (i == 0 || executed[i - 1] != m))
                .count();
            let buys: Vec<f32> = blocks[day.from..day.to].iter().map(|b| b.buy_czk).collect();
            let cheap_ref = cheap_reference(&buys, params.cheap_percentile);
            let end_value = energy * cheap_ref * battery.efficiency;
            let net = cost + wear - end_value;
            format!(
                "row|{}|pv{pv_factor}/load{load_factor}|{}|net {net:.2}|grid {cost:.2}|wear {wear:.2}|end {:.1}% value {end_value:.2}|exported {exported:.2}|discharged {discharged:.2}|before {discharged_before_kwh:.2}|cycles {:.2}|day cycles {:.2}|changes {changes}|reversals {reversals}|runs {runs}|05-22 {letters_5_22}|allowed {allowed_5_22}",
                day.name,
                row.label,
                energy / cap * 100.0,
                discharged / cap,
                (discharged + discharged_before_kwh) / cap,
            )
        }

        /// Daily top-N day grid with live slicing (review F1). `CONTINUITY_SIM_DAY` = path of a
        /// day JSON, or unset for 2026-11-05 from 13:45 (27.6 kWh discharged before, the owner's
        /// `sensor.solax_battery_output_energy_today` at 12:45Z). `DAILY_TOPN_MORNING` = a day
        /// JSON whose blocks of the same local day before the fixture's first block are
        /// prepended (ranking context only: the simulation starts at the same block).
        /// `DAILY_TOPN_HOURS` = the N rows (default 8,10,12; no cap on this branch).
        /// `DAILY_TOPN_NO_RANKING` = replan without the ranking context (the bug of review F1). Run in
        /// release with `--ignored --nocapture`.
        #[test]
        #[ignore = "simulation grid, run by hand"]
        fn daily_top_n_grid() {
            let (mut day, before) = match std::env::var("CONTINUITY_SIM_DAY") {
                Ok(path) => (sim_day_from_json(&path), 0.0),
                Err(_) => (sim_day_live(), 27.6),
            };
            if let Ok(path) = std::env::var("DAILY_TOPN_MORNING") {
                let first = day.blocks[0].start;
                let date = first.with_timezone(&TZ).date_naive();
                let morning: Vec<PlanBlock> = sim_day_from_json(&path)
                    .blocks
                    .into_iter()
                    .filter(|b| b.start < first && b.start.with_timezone(&TZ).date_naive() == date)
                    .collect();
                day.from += morning.len();
                day.to += morning.len();
                day.blocks.splice(0..0, morning);
                day.name.push_str(" (+morning)");
            }
            let owner = owner_levers(6, 25.0);
            let mut rows = vec![
                Row {
                    label: "live|windows|6h eve/4h morn".into(),
                    params: params(ExportSelection::Windows, 6, 5.0),
                    levers: Some(owner),
                },
                Row {
                    label: "nolever|windows|-".into(),
                    params: params(ExportSelection::Windows, 6, 5.0),
                    levers: None,
                },
            ];
            let hours: Vec<u8> = std::env::var("DAILY_TOPN_HOURS")
                .map(|v| v.split(',').filter_map(|h| h.trim().parse().ok()).collect())
                .unwrap_or_else(|_| vec![8, 10, 12]);
            for n in hours {
                rows.push(Row {
                    label: format!("top{n}|daily_top_n|{n}h"),
                    params: params(ExportSelection::DailyTopN, n, 5.0),
                    levers: Some(owner),
                });
            }
            let lines: Vec<String> = std::thread::scope(|scope| {
                let handles: Vec<_> = rows
                    .iter()
                    .flat_map(|row| {
                        let day = &day;
                        [(1.0f32, 1.0f32), (0.8, 1.2)].map(|(pv, load)| {
                            scope.spawn(move || simulate_row(day, row, pv, load, before))
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for line in lines {
                println!("{line}");
            }
        }

        #[test]
        fn new_options_default_to_windows_and_6_h() {
            let d = WinterReserveParams::default();
            assert_eq!(d.export_selection, ExportSelection::Windows);
            assert_eq!(d.daily_export_hours, 6);
            let mut json = serde_json::to_value(WinterReserveParams::default()).unwrap();
            for key in ["export_selection", "daily_export_hours"] {
                json.as_object_mut().unwrap().remove(key);
            }
            let parsed: WinterReserveParams = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, d);
            let core = fluxion_types::config::WinterReserveConfigCore::default();
            assert_eq!(core.export_selection, ExportSelection::Windows);
            assert_eq!(core.daily_export_hours, 6);
            let wire: ExportSelection = serde_json::from_str("\"daily_top_n\"").unwrap();
            assert_eq!(wire, ExportSelection::DailyTopN);
            assert_eq!(
                serde_json::to_string(&ExportSelection::Windows).unwrap(),
                "\"windows\""
            );
        }
    }

    /// Review F1 of legacy/daily-top-n-ship: the scheduler plans only from now − 15 min, so the
    /// per-day ranking gets today's started blocks as ranking context
    mod ranking_context_tests {
        use super::*;

        /// v0.2.74 (e13f01a3) from 00:00 local on `owner_days` (192 blocks, SOC 37 %, no current
        /// mode, reserve load = load × 1.1): morning and evening hours at 25 %, export-allowed mask,
        /// plan letters, `expected_cost_czk` bits; printed by a one-off test on v0.2.74
        /// (`.local/work/daily-topn/golden_midnight_0_2_74.txt`)
        const GOLDEN_MIDNIGHT_0_2_74: [(u8, u8, &str, &str, u32); 4] = [
            (
                4,
                4,
                "000000000000000000000001011111111111111010000000000000000000000101011111111111111000000000000000000000000000000000000100011111111111111010000000000000000000001111111111101011001100000000000000",
                "HHHHHHHHHHHHHHHHHCCCCCCEHEEEEECCCCCEEEESECCCCCCCHHHHSSSSHHHHHHCEHEHEEECCCCCEEEEEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEEEECCCCEEEHSECCCCCCCHHHHSSSSSSSSSCEEEEEECCCCEHEHEECCEESSSSSSSSSSSSSS",
                0xbf559900,
            ),
            (
                4,
                5,
                "000000000000000000000001011111111111111010000000000000000000000111111111111111111110000000000000000000000000000000000100011111111111111010000000000000000000001111111111111111111100000000000000",
                "HHHHHHHHHHHHHHHHHCCCCCCEHEEEEECCCCCEEEESECCCCCCCHHHHSSSSHHHHHHCEEEEECCCCCEEEEECCEEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEEEECCCCEEEHSECCCCCCCHHHHSSSSHHHHHCEEEEEECCCCCEEEEECCEESSSSSSSSSSSSSS",
                0xc1bc09a0,
            ),
            (
                4,
                6,
                "000000000000000000000001011111111111111010000000000000000001001111111111111111111111100000000000000000000000000000000100011111111111111010000000000000000001011111111111111111111110100000000000",
                "HHHHHHHHHHHHHHHHHCCCCCCEHEEEEECCCCCEEEESECCCCCCCHHHHSSSSSSSECCEEHEEECCCCCEEEECCCEEEEEHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHEEEEEECCCCEEEHSECCCCCCCHHHHSSSSSSCECEEEEHEECCCCCCEEEECCEEEHESSSSSSSSSSS",
                0xc265001a,
            ),
            (
                2,
                3,
                "000000000000000000000000001111111100000000000000000000000000000100001111011111101000000000000000000000000000000000000000001111101110000000000000000000000000001110111100101011001000000000000000",
                "HHHHHHHHHHHHHHHHHHCCCCCCHHEEEEECEEHCSSSSSCCCCCCCHHHHSSSSHHHHHHCEHHHHEECCCEEEEEECEHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHCCCCCCCHHEEEEECEEECSSSSSCCCCCCCHHHHSSSSSSSSSCEEEHHEEECCCCEHEEHHESSSSSSSSSSSSSSS",
                0x425c7e76,
            ),
        ];

        fn levers(morning: u8, evening: u8) -> UserLevers {
            UserLevers {
                morning_discharge: Some(DischargeWindowLimit {
                    hours: morning,
                    min_soc_pct: 25.0,
                }),
                evening_discharge: Some(DischargeWindowLimit {
                    hours: evening,
                    min_soc_pct: 25.0,
                }),
                ..UserLevers::default()
            }
        }

        /// A lever site at 00:00 knows the whole day: the windows mode with continuity 0 plans as
        /// 0.2.74, bit for bit, with yesterday's blocks as ranking context (another local day)
        #[test]
        fn lever_site_at_midnight_is_bit_exact_against_0_2_74() {
            let days = owner_days();
            let yesterday: Vec<TimeBlockPrice> = days[..96]
                .iter()
                .map(|b| TimeBlockPrice {
                    block_start: b.start - Duration::days(1),
                    ..price_of(b)
                })
                .collect();
            let p = owner_params(0.0, 2.0);
            for (morning, evening, mask, golden_letters, cost_bits) in GOLDEN_MIDNIGHT_0_2_74 {
                let blocks: Vec<PlanBlock> = days
                    .iter()
                    .map(|b| PlanBlock {
                        reserve_load_kwh: b.load * 1.1,
                        ..block_at(b.start, b.buy, b.sell, b.load, b.pv)
                    })
                    .collect();
                let prefix = ranking_prefix(&yesterday, blocks[0].start, TZ, 0.0);
                let blocks = apply_export_rules_ranked(
                    blocks,
                    prefix,
                    Some(&levers(morning, evening)),
                    Some(TZ),
                    &p,
                );
                assert_eq!(allowed(&blocks), mask, "{morning}/{evening} h mask");
                let plan = plan_from(&blocks, 37.0, TZ, &owner_battery(), &p, None);
                assert_eq!(plan_letters(&plan), golden_letters, "{morning}/{evening} h");
                assert_eq!(
                    plan.expected_cost_czk.to_bits(),
                    cost_bits,
                    "{morning}/{evening} h cost"
                );
            }
        }

        /// (start, export allowed, export min SOC) of the horizon `horizon_blocks` builds from
        /// local `hh`:00 on 2026-11-05: the known prices from then on as `all_price_blocks`, the
        /// 24 h before as `ranking_price_blocks`, as the scheduler sends them
        fn horizon_from(
            hh: u32,
            params: &WinterReserveParams,
            levers: &UserLevers,
        ) -> Vec<(DateTime<Utc>, bool, Option<f32>)> {
            let prices: Vec<TimeBlockPrice> = owner_days().iter().map(price_of).collect();
            let at = 96 + hh as usize * 4;
            let control = ControlConfig::default();
            let context = super::super::super::EvaluationContext {
                price_block: &prices[at],
                control_config: &control,
                current_battery_soc: 60.0,
                solar_forecast_kwh: 0.0,
                consumption_forecast_kwh: 0.25,
                grid_export_price_czk_per_kwh: 0.5,
                all_price_blocks: Some(&prices[at..]),
                ranking_price_blocks: Some(&prices[at - 96..at]),
                backup_discharge_min_soc: 10.0,
                grid_import_today_kwh: None,
                consumption_today_kwh: None,
                solar_forecast_total_today_kwh: 0.0,
                solar_forecast_remaining_today_kwh: 0.0,
                solar_forecast_tomorrow_kwh: 0.0,
                battery_avg_charge_price_czk_per_kwh: 3.0,
                battery_charge_cost_czk_per_kwh: None,
                hourly_consumption_profile: None,
                hourly_consumption_p75: None,
                block_forecasts: None,
                timezone: Some(TZ),
                user_levers: Some(levers),
                cautious_pv: None,
            };
            WinterReserveStrategy::new(true, params.clone())
                .horizon_blocks(&context)
                .iter()
                .map(|b| (b.start, b.export_allowed, b.export_min_soc_pct))
                .collect()
        }

        /// Price lists that start at 08:00, 12:00, 14:00 and 18:00 local give the same export
        /// set as the one at 00:00, in both export selections
        #[test]
        fn mid_day_horizons_keep_the_midnight_export_set() {
            let mut cases: Vec<(String, WinterReserveParams)> =
                vec![("windows 4/4 h, 5 %".to_owned(), owner_params(5.0, 2.0))];
            for n in [6u8, 8, 10, 12] {
                cases.push((
                    format!("daily_top_n {n} h, 5 %"),
                    WinterReserveParams {
                        export_selection: ExportSelection::DailyTopN,
                        daily_export_hours: n,
                        ..owner_params(5.0, 2.0)
                    },
                ));
            }
            let levers = levers(4, 4);
            for (name, params) in cases {
                let midnight = horizon_from(0, &params, &levers);
                assert_eq!(midnight.len(), 96);
                for hh in [8u32, 12, 14, 18] {
                    let later = horizon_from(hh, &params, &levers);
                    let expected = &midnight[hh as usize * 4..];
                    assert_eq!(later.as_slice(), expected, "{name} from {hh}:00");
                }
            }
        }

        /// Hourly prices: the budget counts block minutes, so 6 h is 6 hourly blocks, not 24
        #[test]
        fn daily_top_n_counts_hourly_blocks_by_duration() {
            let first = TZ
                .with_ymd_and_hms(2026, 11, 5, 0, 0, 0)
                .unwrap()
                .with_timezone(&Utc);
            let mut blocks: Vec<PlanBlock> = (0..24)
                .map(|h| PlanBlock {
                    duration_minutes: 60,
                    ..block_at(
                        first + Duration::hours(h),
                        3.0,
                        5.0 + h as f32 * 0.1,
                        0.5,
                        0.0,
                    )
                })
                .collect();
            let p = WinterReserveParams {
                export_selection: ExportSelection::DailyTopN,
                daily_export_hours: 6,
                ..owner_params(0.0, 2.0)
            };
            apply_export_rules(&mut blocks, None, Some(TZ), &p);
            let allowed: Vec<usize> = (0..24).filter(|&i| blocks[i].export_allowed).collect();
            assert_eq!(allowed, vec![18, 19, 20, 21, 22, 23]);
        }

        /// A future day the feed has published only in part: its known blocks are ranked, and it
        /// gets min(N h, ceil(known × N / 24 h)), with no jump at a threshold (coordinator
        /// 2026-11-05); the complete day before it keeps N h
        #[test]
        fn daily_top_n_partial_day_gets_its_share() {
            let days = owner_days();
            for (n, known, expected) in [
                (8u8, 20usize, 7usize),
                (8, 32, 11),
                (8, 33, 11),
                (8, 40, 14),
                (8, 95, 32),
                (8, 96, 32),
                (6, 24, 6),
                (6, 10, 3),
                (24, 40, 40),
            ] {
                let mut blocks: Vec<PlanBlock> = days[..96 + known]
                    .iter()
                    .map(|b| block_at(b.start, b.buy, b.sell, b.load, b.pv))
                    .collect();
                let p = WinterReserveParams {
                    export_selection: ExportSelection::DailyTopN,
                    daily_export_hours: n,
                    ..owner_params(0.0, 2.0)
                };
                apply_export_rules(&mut blocks, None, Some(TZ), &p);
                let today = blocks[..96].iter().filter(|b| b.export_allowed).count();
                let partial = blocks[96..].iter().filter(|b| b.export_allowed).count();
                assert_eq!(today, usize::from(n) * 4, "N {n} full day");
                assert_eq!(partial, expected, "N {n}, {known} blocks known");
            }
        }

        /// A complete local day is not partial: N h on the 23-hour DST day (2026-03-29) and on the
        /// 25-hour one (2026-10-25), as on a normal day
        #[test]
        fn daily_top_n_complete_dst_days_get_n_hours() {
            for (date, len) in [
                ((2026, 3, 29), 92i64),
                ((2026, 10, 25), 100),
                ((2026, 11, 5), 96),
            ] {
                let first = TZ
                    .with_ymd_and_hms(date.0, date.1, date.2, 0, 0, 0)
                    .unwrap()
                    .with_timezone(&Utc);
                let mut blocks: Vec<PlanBlock> = (0..len)
                    .map(|i| {
                        block_at(
                            first + Duration::minutes(15 * i),
                            3.0,
                            5.0 + i as f32 * 0.01,
                            0.5,
                            0.0,
                        )
                    })
                    .collect();
                assert_eq!(
                    local_day_minutes(first.with_timezone(&TZ).date_naive(), TZ),
                    u32::try_from(len * 15).unwrap()
                );
                let p = WinterReserveParams {
                    export_selection: ExportSelection::DailyTopN,
                    daily_export_hours: 8,
                    ..owner_params(0.0, 2.0)
                };
                apply_export_rules(&mut blocks, None, Some(TZ), &p);
                let allowed = blocks.iter().filter(|b| b.export_allowed).count();
                assert_eq!(allowed, 32, "{date:?}");
            }
        }
    }
}
