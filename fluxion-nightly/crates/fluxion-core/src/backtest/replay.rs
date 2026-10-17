// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Block-by-block Winter Reserve replay on recorded blocks, with plain self-use alongside.
//!
//! For every block the planner sees the horizon (today, plus tomorrow after 13:00 local) with a
//! forecast built only from earlier data (load: mean of the same local quarter-hour over the
//! previous 7 local days, else the mean of all earlier blocks; or the live hourly profile, see
//! [`ProfileSource`]). The planner plans buys on the expected flows (load as is, PV times
//! `pv_forecast_bias`) and sizes the night reserve on the conservative ones. The horizon stops
//! just before a missing block. The first planned mode is then applied to the *actual* block.
//!
//! Shared by the `winter_reserve_replay` example and the add-on backtest page.

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::components::{
    HourlyConsumptionProfile, aggregate_hourly_house_load, pick_hourly_profile,
};
use crate::savings::RecordedBlock;
use crate::strategy::winter_reserve::{
    BatteryParams, BlockOutcome, PlanBlock, PlanMode, WinterReserveParams, cheap_reference,
    plan_from, simulate_block,
};
use crate::traits::HistoryDataPoint;

/// Which load forecast the planner sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProfileSource {
    /// Mean of the same local minute over the 7 days before (the example's default)
    Forecast,
    /// Hourly UTC profile of grid import (the old `consumption_entity` source)
    Import,
    /// Hourly UTC profile of house load: max(mean, p75) times the planner margin, as the live
    /// planner runs it (falls back to `Import` below 3 days)
    House,
}

/// One usable recorded block: flows and prices, the recorded SOC at its start, and whether a
/// missing block directly follows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayRow {
    pub block: RecordedBlock,
    pub soc_pct: f32,
    pub gap_after: bool,
}

/// Replay settings.
#[derive(Debug, Clone, Copy)]
pub struct ReplayOptions {
    pub tz: Tz,
    pub battery: BatteryParams,
    pub params: WinterReserveParams,
    /// `control.planner_load_margin`, used with the p75 house-load profile
    pub planner_load_margin: f32,
    pub profile: ProfileSource,
    /// Multiplies the forecast PV the planner sees (the simulated reality uses actual PV)
    pub pv_forecast_bias: f32,
    /// Future PV forecasts times a deterministic factor in [0.5, 1.3]
    pub pv_noise: bool,
    /// Run self-use only
    pub baseline_only: bool,
    /// First simulated block: the first at or after this instant (default: the first row)
    pub from: Option<DateTime<Utc>>,
    /// Simulate only blocks starting before this instant; later rows still feed the horizon
    pub until: Option<DateTime<Utc>>,
}

impl ReplayOptions {
    /// Defaults of the example: 7-day forecast, unbiased PV, no noise, full run.
    #[must_use]
    pub fn new(tz: Tz, battery: BatteryParams, params: WinterReserveParams) -> Self {
        Self {
            tz,
            battery,
            params,
            planner_load_margin: fluxion_types::ControlConfig::default().planner_load_margin,
            profile: ProfileSource::Forecast,
            pv_forecast_bias: 1.0,
            pv_noise: false,
            baseline_only: false,
            from: None,
            until: None,
        }
    }
}

/// Grid and battery result of one strategy on one block.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyStep {
    pub mode: PlanMode,
    pub energy_start_kwh: f32,
    pub energy_end_kwh: f32,
    pub floor_kwh: f32,
    pub outcome: BlockOutcome,
    pub reason: String,
}

/// One simulated block.
#[derive(Debug, Clone, PartialEq)]
pub struct SimulatedBlock {
    pub row: ReplayRow,
    pub local: DateTime<Tz>,
    pub selfuse: StrategyStep,
    /// `None` with `baseline_only`
    pub winter_reserve: Option<StrategyStep>,
}

/// Money and energy totals of one strategy over a day.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RunTotals {
    pub cost: f32,
    pub wear_cost: f32,
    pub export_kwh: f32,
    /// Battery charge the grid supplied, per block min(charge, import): kWh and its cost (CZK).
    pub grid_charge_kwh: f32,
    pub grid_charge_czk: f32,
    pub min_night_soc: Option<f32>,
}

/// Totals of one local day.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DayTotals {
    pub actual: f32,
    pub su: RunTotals,
    pub wr: RunTotals,
    /// Winter Reserve blocks per mode: SelfUse, Charge, Hold, Export
    pub modes: [u32; 4],
    pub blocks: u32,
}

/// Result of a replay.
#[derive(Debug, Clone)]
pub struct ReplayRun {
    pub blocks: Vec<SimulatedBlock>,
    pub days: BTreeMap<NaiveDate, DayTotals>,
    /// Days whose first block had no earlier data at all: Winter Reserve fell back to SelfUse
    pub skipped_days: BTreeSet<NaiveDate>,
    /// Rows given (usable blocks)
    pub usable_blocks: usize,
    pub planned_blocks: usize,
    /// Forecast lookups without 7-day history (mean of earlier blocks used)
    pub fallback_blocks: usize,
    pub energy_end_selfuse_kwh: f32,
    pub energy_end_wr_kwh: f32,
    /// `cheap_ref` over the buy prices of the last simulated day
    pub cheap_ref_last_day: f32,
}

impl ReplayRun {
    /// End state: energy Winter Reserve left above self-use, valued at the last day's
    /// `cheap_ref` times efficiency.
    #[must_use]
    pub fn end_soc_adjustment_czk(&self, battery: &BatteryParams) -> f32 {
        (self.energy_end_wr_kwh - self.energy_end_selfuse_kwh)
            * self.cheap_ref_last_day
            * battery.efficiency
    }
}

/// Index of a Winter Reserve mode in [`DayTotals::modes`].
#[must_use]
pub fn mode_index(mode: PlanMode) -> usize {
    match mode {
        // The replay passes no user levers, so the planner never produces NoDischarge.
        PlanMode::SelfUse | PlanMode::NoDischarge => 0,
        PlanMode::Charge => 1,
        PlanMode::Hold => 2,
        PlanMode::Export => 3,
    }
}

/// Mode letters in [`DayTotals::modes`] order.
pub const MODE_LETTERS: [char; 4] = ['S', 'C', 'H', 'E'];

/// Night = 18:00-08:00 local.
#[must_use]
pub fn is_night(hour: u32) -> bool {
    !(8..18).contains(&hour)
}

/// Deterministic factor in [0.5, 1.3] for the block at `index` (SplitMix64 of the index).
fn pv_noise_factor(index: usize) -> f32 {
    let mut z = (index as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let unit = (z >> 11) as f64 / (1u64 << 53) as f64;
    (0.5 + 0.8 * unit) as f32
}

/// One row with its local time resolved.
struct Row {
    start: DateTime<Utc>,
    local: DateTime<Tz>,
    load: f32,
    import: f32,
}

/// The hourly profile the live fetcher builds on `day` from the 7 local days before it, with the
/// shared functions. House load: `pick_hourly_profile` of the house-load profile and the import one.
/// `Import`: the import profile without p75, as the old source had (mean only).
fn live_load_profile(
    rows: &[Row],
    day: NaiveDate,
    source: ProfileSource,
) -> Option<HourlyConsumptionProfile> {
    if source == ProfileSource::Forecast {
        return None;
    }
    let window_start = day - Duration::days(7);
    // Each block is one sample at its start: its mean power in W (kWh per 15 min x 4000). The
    // sample holds until the next block, so the time-weighted hourly mean equals kWh/h. The first
    // block of `day` closes the last block before it.
    let samples = |value: fn(&Row) -> f32| -> Vec<HistoryDataPoint> {
        let mut points: Vec<HistoryDataPoint> = rows
            .iter()
            .filter(|r| {
                let d = r.local.date_naive();
                d >= window_start && d < day
            })
            .map(|r| HistoryDataPoint {
                timestamp: r.start,
                value: value(r) * 4000.0,
            })
            .collect();
        if let Some(first) = rows.iter().find(|r| r.local.date_naive() == day) {
            points.push(HistoryDataPoint {
                timestamp: first.start,
                value: value(first) * 4000.0,
            });
        }
        points
    };
    let import = aggregate_hourly_house_load(&samples(|r| r.import)).map(|mut p| {
        p.hourly_p75_kwh = None;
        p
    });
    match source {
        ProfileSource::House => {
            let house = aggregate_hourly_house_load(&samples(|r| r.load));
            pick_hourly_profile(house, import)
        }
        ProfileSource::Import | ProfileSource::Forecast => import,
    }
}

/// Load of one planned block (kWh), as the live planner computes it from the profile: with p75,
/// max(mean, p75) times the planner margin; otherwise mean times the safety factor.
fn planner_block_load(
    profile: &HourlyConsumptionProfile,
    hour: usize,
    load_safety_factor: f32,
    planner_load_margin: f32,
) -> f32 {
    match profile.hourly_p75_kwh {
        Some(p75) => profile.hourly_avg_kwh[hour].max(p75[hour]) * 0.25 * planner_load_margin,
        None => profile.hourly_avg_kwh[hour] * 0.25 * load_safety_factor,
    }
}

/// The actual block as the simulator sees it: recorded load and PV, no levers.
fn actual_plan_block(block: &RecordedBlock) -> PlanBlock {
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
        force_charge_at_or_below_czk: None,
        export_min_soc_pct: None,
        export_allowed: true,
    }
}

fn add_outcome(acc: &mut RunTotals, out: &BlockOutcome, block: &RecordedBlock, wear: f32) {
    acc.cost += out.grid_import_kwh * block.buy_czk - out.grid_export_kwh * block.sell_czk;
    acc.wear_cost += out.battery_discharge_kwh * wear;
    acc.export_kwh += out.grid_export_kwh;
}

/// Replay `rows` (usable blocks in time order; missing blocks are left out and flagged with
/// `gap_after` on the row before them). Both runs start from the recorded SOC of the first
/// simulated block, clamped to the battery bounds.
#[expect(clippy::too_many_lines)]
pub fn replay(rows: &[ReplayRow], opts: &ReplayOptions) -> Result<ReplayRun, String> {
    if rows.is_empty() {
        return Err("no usable blocks".into());
    }
    let tz = opts.tz;
    let battery = opts.battery;
    let params = opts.params;
    let capacity = battery.capacity_kwh;
    let e_min = battery.min_soc_pct / 100.0 * capacity;
    let e_max = battery.max_soc_pct / 100.0 * capacity;

    let resolved: Vec<Row> = rows
        .iter()
        .map(|r| Row {
            start: r.block.start,
            local: r.block.start.with_timezone(&tz),
            load: r.block.load_kwh,
            import: r.block.import_kwh,
        })
        .collect();

    // Historical load per (local date, local minute-of-day), for the 7-day forecast.
    let mut load_hist: HashMap<(NaiveDate, u32), f32> = HashMap::new();
    for row in &resolved {
        let key = (
            row.local.date_naive(),
            row.local.hour() * 60 + row.local.minute(),
        );
        load_hist.insert(key, row.load);
    }

    // First simulated block: the first one at or after `from` (default: the first block).
    let first = match opts.from {
        Some(t) => resolved
            .iter()
            .position(|r| r.start >= t)
            .ok_or("--from is after the last usable block")?,
        None => 0,
    };
    let soc_start = rows[first].soc_pct / 100.0 * capacity;
    let e0_su = soc_start.clamp(e_min, e_max);
    let mut e_su = e0_su;
    let mut e_wr = e0_su;
    let mut days: BTreeMap<NaiveDate, DayTotals> = BTreeMap::new();
    let mut blocks_out: Vec<SimulatedBlock> = Vec::new();
    let mut skipped_days: BTreeSet<NaiveDate> = BTreeSet::new();
    // Load forecast for horizon block `j` as seen from decision block `i`, using history only:
    // the mean of the same local minute over the 7 local days before the *decision* day; without
    // that history, the mean of all blocks before `i`. No earlier data at all -> None.
    let mut prefix_load = vec![0.0f32; resolved.len() + 1];
    for (i, row) in resolved.iter().enumerate() {
        prefix_load[i + 1] = prefix_load[i] + row.load;
    }
    let mut fallback_blocks = 0usize;
    let forecast_load = |i: usize, j: usize, fallbacks: &mut usize| -> Option<f32> {
        let decision_day = resolved[i].local.date_naive();
        let minute = resolved[j].local.hour() * 60 + resolved[j].local.minute();
        let (sum, count) = (1..=7i64)
            .filter_map(|k| load_hist.get(&(decision_day - Duration::days(k), minute)))
            .fold((0.0f32, 0u32), |(s, c), v| (s + v, c + 1));
        if count > 0 {
            Some(sum / count as f32)
        } else if i > 0 {
            *fallbacks += 1;
            Some(prefix_load[i] / i as f32)
        } else {
            None
        }
    };
    let mut planned_blocks = 0usize;
    // Mode applied to the previous block: the planner keeps it while it is within the benefit
    let mut applied: Option<PlanMode> = None;
    // The live profile depends only on the day: build it once per day.
    let mut profiles: HashMap<NaiveDate, Option<HourlyConsumptionProfile>> = HashMap::new();
    let mut last_day: Option<NaiveDate> = None;

    for (i, (input, row)) in rows.iter().zip(&resolved).enumerate().skip(first) {
        if opts.until.is_some_and(|until| row.start >= until) {
            break;
        }
        let recorded = &input.block;
        let day = row.local.date_naive();
        last_day = Some(day);
        let acc = days.entry(day).or_default();
        acc.actual +=
            recorded.import_kwh * recorded.buy_czk - recorded.export_kwh * recorded.sell_czk;
        acc.blocks += 1;

        // Baseline: SelfUse on the actual block.
        let actual_block = actual_plan_block(recorded);
        let su_out = simulate_block(PlanMode::SelfUse, e_su, &actual_block, e_min, &battery);
        add_outcome(
            &mut acc.su,
            &su_out,
            recorded,
            battery.wear_cost_czk_per_kwh,
        );
        if is_night(row.local.hour()) {
            let soc = e_su / capacity * 100.0;
            acc.su.min_night_soc = Some(acc.su.min_night_soc.map_or(soc, |m| m.min(soc)));
        }
        let selfuse = StrategyStep {
            mode: PlanMode::SelfUse,
            energy_start_kwh: e_su,
            energy_end_kwh: su_out.energy_end_kwh,
            floor_kwh: e_min,
            outcome: su_out,
            reason: "baseline SelfUse".into(),
        };
        e_su = su_out.energy_end_kwh;

        if opts.baseline_only {
            blocks_out.push(SimulatedBlock {
                row: *input,
                local: row.local,
                selfuse,
                winter_reserve: None,
            });
            continue;
        }

        // Winter Reserve: horizon, forecasts, plan, then apply the first mode to the actual block.
        let soc_now = e_wr / capacity * 100.0;
        let (mode, floor_kwh, reason) = if forecast_load(i, i, &mut fallback_blocks).is_none() {
            skipped_days.insert(day);
            (
                PlanMode::SelfUse,
                e_min,
                "no earlier data, SelfUse".to_string(),
            )
        } else {
            let horizon_end_day = if row.local.hour() >= 13 {
                day.succ_opt().ok_or("date overflow")?
            } else {
                day
            };
            let live_profile = profiles
                .entry(day)
                .or_insert_with(|| {
                    live_load_profile(&resolved, day, opts.profile)
                        .filter(|p| p.hourly_avg_kwh.iter().sum::<f32>() > 0.0)
                })
                .clone();
            let mut horizon: Vec<PlanBlock> = Vec::new();
            for (j, (later_input, later)) in rows.iter().zip(&resolved).enumerate().skip(i) {
                if later.local.date_naive() > horizon_end_day {
                    break;
                }
                let Some(load_fc) = forecast_load(i, j, &mut fallback_blocks) else {
                    break;
                };
                // Expected load is the hourly mean (as production); the reserve load is
                // max(mean, p75) times the planner margin, or mean times the safety factor.
                let (load_kwh, reserve_load_kwh) = match &live_profile {
                    Some(p) => (
                        p.hourly_avg_kwh[later.start.hour() as usize] * 0.25,
                        planner_block_load(
                            p,
                            later.start.hour() as usize,
                            params.load_safety_factor,
                            opts.planner_load_margin,
                        ),
                    ),
                    None => (load_fc, load_fc * params.load_safety_factor),
                };
                let noise = if opts.pv_noise && j > i {
                    pv_noise_factor(j)
                } else {
                    1.0
                };
                let pv_kwh = later_input.block.pv_kwh * opts.pv_forecast_bias * noise;
                horizon.push(PlanBlock {
                    start: later.start,
                    duration_minutes: 15,
                    buy_czk: later_input.block.buy_czk,
                    sell_czk: later_input.block.sell_czk,
                    load_kwh,
                    pv_kwh,
                    reserve_load_kwh,
                    reserve_pv_kwh: pv_kwh * params.pv_confidence,
                    force_charge: false,
                    no_discharge: false,
                    force_charge_at_or_below_czk: None,
                    export_min_soc_pct: None,
                    export_allowed: true,
                });
                if later_input.gap_after {
                    break;
                }
            }

            let result = plan_from(&horizon, soc_now, tz, &battery, &params, applied);
            let first = result
                .blocks
                .first()
                .ok_or("planner returned an empty plan")?;
            planned_blocks += 1;
            applied = Some(first.mode);
            (first.mode, first.floor_kwh, first.reason.clone())
        };
        acc.modes[mode_index(mode)] += 1;
        let wr_out = simulate_block(mode, e_wr, &actual_block, floor_kwh, &battery);
        add_outcome(
            &mut acc.wr,
            &wr_out,
            recorded,
            battery.wear_cost_czk_per_kwh,
        );
        let grid_charge = wr_out.battery_charge_kwh.min(wr_out.grid_import_kwh);
        acc.wr.grid_charge_kwh += grid_charge;
        acc.wr.grid_charge_czk += grid_charge * recorded.buy_czk;
        if is_night(row.local.hour()) {
            acc.wr.min_night_soc = Some(acc.wr.min_night_soc.map_or(soc_now, |m| m.min(soc_now)));
        }
        blocks_out.push(SimulatedBlock {
            row: *input,
            local: row.local,
            selfuse,
            winter_reserve: Some(StrategyStep {
                mode,
                energy_start_kwh: e_wr,
                energy_end_kwh: wr_out.energy_end_kwh,
                floor_kwh,
                outcome: wr_out,
                reason,
            }),
        });
        e_wr = wr_out.energy_end_kwh;
    }

    // cheap_ref of the last simulated day (the last row's day when everything is simulated)
    let cheap_ref_last_day = last_day.map_or(0.0, |last| {
        let buys: Vec<f32> = rows
            .iter()
            .zip(&resolved)
            .filter(|(_, r)| r.local.date_naive() == last)
            .map(|(input, _)| input.block.buy_czk)
            .collect();
        cheap_reference(&buys, params.cheap_percentile)
    });

    Ok(ReplayRun {
        blocks: blocks_out,
        days,
        skipped_days,
        usable_blocks: rows.len(),
        planned_blocks,
        fallback_blocks,
        energy_end_selfuse_kwh: e_su,
        energy_end_wr_kwh: e_wr,
        cheap_ref_last_day,
    })
}
