// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Replay recorded site history block by block with the Winter Reserve planner.
//!
//! Input: the JSON written by `fluxion/tools/winter_reserve/export_ha_history.py`.
//! For every block the planner sees the horizon (today, plus tomorrow after 13:00 local)
//! with a forecast (load: mean of the same local quarter-hour over the previous 7 local days,
//! else the mean of all earlier blocks, times `load_safety_factor`; never the block's own
//! actual value. PV: the actual value times `pv_confidence`, a perfect forecast scaled down).
//! The horizon stops just before a missing block. The first planned mode is then applied to
//! the *actual* block. The end state is valued at `cheap_ref` of the last day (see the totals).
//!
//! `--from <UTC timestamp>` starts the simulation at the first block at or after it, with the
//! battery set to that block's recorded SOC; earlier blocks are skipped.
//!
//! `--profile import|house` replaces that forecast with the live hourly UTC profile, rebuilt for
//! every decision day from the 7 local days before it: `import` is the old source (grid import,
//! mean times `load_safety_factor`); `house` is the house-load profile (max of mean and p75 times
//! the planner margin, falling back to `import` below 3 days), as the planner runs it now.
//!
//! Usage:
//! `cargo run -p fluxion-core --example winter_reserve_replay -- data.json [--baseline-only]
//!  [--csv out.csv] [--pv-noise] [--profile forecast|import|house] [--from 2026-10-14T14:30:00Z]
//!  [--param key=value]...`

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use fluxion_core::strategy::winter_reserve::{
    BatteryParams, PlanBlock, PlanMode, WinterReserveParams, plan, simulate_block,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::process::ExitCode;

/// Planner load margin of the house-load profile (`control.planner_load_margin` default)
const PLANNER_LOAD_MARGIN: f32 = 1.10;
/// Days of house-load data before the house-load profile replaces the import one
const MIN_HOUSE_LOAD_DAYS: usize = 3;

/// Which load forecast the planner sees.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ProfileSource {
    /// Mean of the same local minute over the 7 days before (default, unchanged)
    Forecast,
    /// Hourly UTC profile of grid import (the old `consumption_entity` source)
    Import,
    /// Hourly UTC profile of house load: max(mean, p75) times the planner margin
    House,
}

/// Hourly UTC load profile (kWh per hour) built from the 7 local days before a decision day.
struct HourlyLoad {
    mean: [f32; 24],
    p75: [f32; 24],
    /// Days with at least one full UTC hour of data
    days: usize,
}

#[derive(Deserialize)]
struct Dataset {
    site: Site,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Site {
    timezone: String,
    battery: BatteryParams,
}

#[derive(Deserialize)]
struct Block {
    start_utc: String,
    missing: bool,
    buy: Option<f32>,
    sell: Option<f32>,
    soc_pct: Option<f32>,
    load_kwh: Option<f32>,
    pv_kwh: Option<f32>,
    import_kwh: Option<f32>,
    export_kwh: Option<f32>,
}

/// Linear-interpolated percentile `q` of a non-empty slice. Sorts the slice.
fn percentile(values: &mut [f32], q: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    let pos = q * (values.len() - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    values[lo] + (values[hi] - values[lo]) * (pos - lo as f32)
}

/// Builds the hourly UTC profile of `value` (kWh per block) from the 7 local days before
/// `decision_day`. Only full UTC hours (4 blocks) count, as the live fetcher works on hours.
fn hourly_load(rows: &[Row], decision_day: NaiveDate, value: fn(&Row) -> f32) -> HourlyLoad {
    let mut hours: BTreeMap<(NaiveDate, u32), (f32, u32)> = BTreeMap::new();
    for row in rows {
        let day = row.local.date_naive();
        if day >= decision_day || day < decision_day - Duration::days(7) {
            continue;
        }
        let entry = hours
            .entry((row.start.date_naive(), row.start.hour()))
            .or_insert((0.0, 0));
        entry.0 += value(row);
        entry.1 += 1;
    }
    let mut per_day: BTreeMap<NaiveDate, [Option<f32>; 24]> = BTreeMap::new();
    for ((day, hour), (kwh, blocks)) in hours {
        if blocks == 4 {
            per_day.entry(day).or_insert([None; 24])[hour as usize] = Some(kwh);
        }
    }
    let mut out = HourlyLoad {
        mean: [0.0; 24],
        p75: [0.0; 24],
        days: per_day.len(),
    };
    for h in 0..24 {
        let mut values: Vec<f32> = per_day.values().filter_map(|d| d[h]).collect();
        if values.is_empty() {
            continue;
        }
        out.mean[h] = values.iter().sum::<f32>() / values.len() as f32;
        out.p75[h] = percentile(&mut values, 0.75);
    }
    out
}

/// One block after parsing, with local time resolved.
struct Row {
    start: DateTime<Utc>,
    local: DateTime<Tz>,
    buy: f32,
    sell: f32,
    load: f32,
    pv: f32,
    soc0_pct: f32,
    import: f32,
    export: f32,
    /// A missing block directly follows this one in the data.
    gap_after: bool,
}

#[derive(Default, Clone, Copy)]
struct Acc {
    cost: f32,
    wear_cost: f32,
    export_kwh: f32,
    min_night_soc: Option<f32>,
}

#[derive(Default)]
struct DayAcc {
    actual: f32,
    su: Acc,
    wr: Acc,
    modes: [u32; 4],
    blocks: u32,
}

fn mode_index(mode: PlanMode) -> usize {
    match mode {
        PlanMode::SelfUse => 0,
        PlanMode::Charge => 1,
        PlanMode::Hold => 2,
        PlanMode::Export => 3,
        // The replay passes no user levers, so the planner never produces NoDischarge.
        PlanMode::NoDischarge => unreachable!("replay runs without user levers"),
    }
}

const MODE_LETTERS: [char; 4] = ['S', 'C', 'H', 'E'];

fn is_night(hour: u32) -> bool {
    !(8..18).contains(&hour)
}

/// Mean of the lowest `ceil(percentile * n)` buy prices, the same reference as the planner's `cheap_ref`.
fn cheap_reference(buys: &mut [f32], percentile: f32) -> f32 {
    buys.sort_by(f32::total_cmp);
    let n = buys.len();
    let n_cheap = ((f64::from(percentile) * n as f64 - 1e-6).ceil() as usize).clamp(1, n);
    buys[..n_cheap].iter().sum::<f32>() / n_cheap as f32
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

fn set_param(params: &mut WinterReserveParams, assignment: &str) -> Result<(), String> {
    let (key, value) = assignment
        .split_once('=')
        .ok_or_else(|| format!("--param expects key=value, got {assignment}"))?;
    let value: f32 = value
        .parse()
        .map_err(|_| format!("--param {key}: not a number: {value}"))?;
    let mut map = match serde_json::to_value(*params) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => return Err("cannot serialise WinterReserveParams".into()),
    };
    if !map.contains_key(key) {
        return Err(format!("unknown parameter {key}"));
    }
    map.insert(key.to_string(), serde_json::json!(value));
    *params = serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|err| format!("--param {key}: {err}"))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input: Option<String> = None;
    let mut baseline_only = false;
    let mut csv_path: Option<String> = None;
    let mut pv_noise = false;
    let mut profile = ProfileSource::Forecast;
    let mut from: Option<DateTime<Utc>> = None;
    let mut params = WinterReserveParams::default();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baseline-only" => baseline_only = true,
            "--pv-noise" => pv_noise = true,
            "--from" => {
                let text = iter.next().ok_or("--from needs a UTC timestamp")?;
                let parsed = DateTime::parse_from_rfc3339(text)
                    .map_err(|err| format!("--from {text}: {err}"))?;
                from = Some(parsed.with_timezone(&Utc));
            }
            "--profile" => {
                profile = match iter.next().map(String::as_str) {
                    Some("forecast") => ProfileSource::Forecast,
                    Some("import") => ProfileSource::Import,
                    Some("house") => ProfileSource::House,
                    _ => return Err("--profile needs forecast, import or house".into()),
                }
            }
            "--csv" => csv_path = Some(iter.next().ok_or("--csv needs a path")?.clone()),
            "--param" => set_param(&mut params, iter.next().ok_or("--param needs key=value")?)?,
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => input = Some(other.to_string()),
        }
    }
    let input = input.ok_or("usage: winter_reserve_replay data.json [--baseline-only] [--csv out.csv] [--param key=value]...")?;

    let text = fs::read_to_string(&input).map_err(|err| format!("{input}: {err}"))?;
    let dataset: Dataset = serde_json::from_str(&text).map_err(|err| format!("{input}: {err}"))?;
    let tz: Tz = dataset
        .site
        .timezone
        .parse()
        .map_err(|_| format!("unknown timezone {}", dataset.site.timezone))?;
    let battery = dataset.site.battery;
    let capacity = battery.capacity_kwh;
    let e_min = battery.min_soc_pct / 100.0 * capacity;
    let e_max = battery.max_soc_pct / 100.0 * capacity;

    // Non-missing blocks only. Missing blocks are skipped: no cost, no simulation, no forecast data.
    let mut rows: Vec<Row> = Vec::new();
    let mut missing = 0usize;
    for raw in &dataset.blocks {
        if raw.missing {
            missing += 1;
            if let Some(last) = rows.last_mut() {
                last.gap_after = true;
            }
            continue;
        }
        let start: DateTime<Utc> = raw
            .start_utc
            .parse()
            .map_err(|err| format!("bad start_utc {}: {err}", raw.start_utc))?;
        let (Some(buy), Some(sell), Some(load), Some(pv), Some(soc), Some(import), Some(export)) = (
            raw.buy,
            raw.sell,
            raw.load_kwh,
            raw.pv_kwh,
            raw.soc_pct,
            raw.import_kwh,
            raw.export_kwh,
        ) else {
            return Err(format!(
                "block {} is not missing but has null fields",
                raw.start_utc
            ));
        };
        rows.push(Row {
            start,
            local: start.with_timezone(&tz),
            buy,
            sell,
            load,
            pv,
            soc0_pct: soc,
            import,
            export,
            gap_after: false,
        });
    }
    if rows.is_empty() {
        return Err("no usable blocks".into());
    }

    // Historical load per (local date, local minute-of-day), for the 7-day forecast.
    let mut load_hist: HashMap<(NaiveDate, u32), f32> = HashMap::new();
    for row in &rows {
        let key = (
            row.local.date_naive(),
            row.local.hour() * 60 + row.local.minute(),
        );
        load_hist.insert(key, row.load);
    }

    // First simulated block: the first one at or after `--from` (default: the first block).
    let first = match from {
        Some(t) => rows
            .iter()
            .position(|r| r.start >= t)
            .ok_or("--from is after the last usable block")?,
        None => 0,
    };
    let soc_start = rows[first].soc0_pct / 100.0 * capacity;
    let e0_su = soc_start.clamp(e_min, e_max);
    let mut e_su = e0_su;
    let mut e_wr = e0_su;
    let mut days: BTreeMap<NaiveDate, DayAcc> = BTreeMap::new();
    let mut csv_rows: Vec<String> = Vec::new();
    // Days whose first block had no earlier data at all: excluded from the totals.
    let mut skipped_days: BTreeSet<NaiveDate> = BTreeSet::new();
    // Load forecast for horizon block `j` as seen from decision block `i`, using history only:
    // the mean of the same local minute over the 7 local days before the *decision* day; without
    // that history, the mean of all blocks before `i`. No earlier data at all -> None.
    let mut prefix_load = vec![0.0f32; rows.len() + 1];
    for (i, row) in rows.iter().enumerate() {
        prefix_load[i + 1] = prefix_load[i] + row.load;
    }
    let mut fallback_blocks = 0usize;
    let forecast_load = |i: usize, j: usize, fallbacks: &mut usize| -> Option<f32> {
        let decision_day = rows[i].local.date_naive();
        let minute = rows[j].local.hour() * 60 + rows[j].local.minute();
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

    for (i, row) in rows.iter().enumerate().skip(first) {
        let day = row.local.date_naive();
        let acc = days.entry(day).or_default();
        acc.actual += row.import * row.buy - row.export * row.sell;
        acc.blocks += 1;

        // Baseline: SelfUse on the actual block.
        let actual_block = PlanBlock {
            start: row.start,
            duration_minutes: 15,
            buy_czk: row.buy,
            sell_czk: row.sell,
            load_kwh: row.load,
            pv_kwh: row.pv,
            force_charge: false,
            no_discharge: false,
            force_charge_at_or_below_czk: None,
            export_min_soc_pct: None,
            export_allowed: true,
        };
        let su_out = simulate_block(PlanMode::SelfUse, e_su, &actual_block, e_min, &battery);
        acc.su.cost += su_out.grid_import_kwh * row.buy - su_out.grid_export_kwh * row.sell;
        acc.su.wear_cost += su_out.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
        acc.su.export_kwh += su_out.grid_export_kwh;
        if is_night(row.local.hour()) {
            let soc = e_su / capacity * 100.0;
            acc.su.min_night_soc = Some(acc.su.min_night_soc.map_or(soc, |m| m.min(soc)));
        }
        let su_start = e_su;
        e_su = su_out.energy_end_kwh;

        if baseline_only {
            csv_rows.push(format!(
                "{},{:.4},{:.4},S,{:.2},{:.2},{:.2},{:.4},{:.4},baseline SelfUse",
                row.local.format("%Y-%m-%dT%H:%M"),
                row.buy,
                row.sell,
                su_start / capacity * 100.0,
                e_su / capacity * 100.0,
                e_min,
                su_out.grid_import_kwh,
                su_out.grid_export_kwh,
            ));
            continue;
        }

        // Winter Reserve: horizon, forecasts, plan, then apply the first mode to the actual block.
        let soc_now = e_wr / capacity * 100.0;
        let (mode, floor_kwh, reason) = match forecast_load(i, i, &mut fallback_blocks) {
            None => {
                skipped_days.insert(day);
                (
                    PlanMode::SelfUse,
                    e_min,
                    "no earlier data, SelfUse".to_string(),
                )
            }
            Some(_) => {
                let horizon_end_day = if row.local.hour() >= 13 {
                    day.succ_opt().ok_or("date overflow")?
                } else {
                    day
                };
                let import_prof = hourly_load(&rows, day, |r| r.import);
                let house_prof = hourly_load(&rows, day, |r| r.load);
                let mut horizon: Vec<PlanBlock> = Vec::new();
                for (j, later) in rows.iter().enumerate().skip(i) {
                    if later.local.date_naive() > horizon_end_day {
                        break;
                    }
                    let Some(load_fc) = forecast_load(i, j, &mut fallback_blocks) else {
                        break;
                    };
                    let hour = later.start.hour() as usize;
                    let fallback_kwh = load_fc * params.load_safety_factor;
                    let import_kwh = if import_prof.days > 0 {
                        import_prof.mean[hour] * 0.25 * params.load_safety_factor
                    } else {
                        fallback_kwh
                    };
                    let load_kwh = match profile {
                        ProfileSource::Forecast => fallback_kwh,
                        ProfileSource::Import => import_kwh,
                        ProfileSource::House if house_prof.days >= MIN_HOUSE_LOAD_DAYS => {
                            house_prof.mean[hour].max(house_prof.p75[hour])
                                * 0.25
                                * PLANNER_LOAD_MARGIN
                        }
                        ProfileSource::House => import_kwh,
                    };
                    let noise = if pv_noise && j > i {
                        pv_noise_factor(j)
                    } else {
                        1.0
                    };
                    horizon.push(PlanBlock {
                        start: later.start,
                        duration_minutes: 15,
                        buy_czk: later.buy,
                        sell_czk: later.sell,
                        load_kwh,
                        pv_kwh: later.pv * params.pv_confidence * noise,
                        force_charge: false,
                        no_discharge: false,
                        force_charge_at_or_below_czk: None,
                        export_min_soc_pct: None,
                        export_allowed: true,
                    });
                    if later.gap_after {
                        break;
                    }
                }

                let result = plan(&horizon, soc_now, tz, &battery, &params);
                let first = result
                    .blocks
                    .first()
                    .ok_or("planner returned an empty plan")?;
                planned_blocks += 1;
                (first.mode, first.floor_kwh, first.reason.clone())
            }
        };
        acc.modes[mode_index(mode)] += 1;
        let wr_out = simulate_block(mode, e_wr, &actual_block, floor_kwh, &battery);
        acc.wr.cost += wr_out.grid_import_kwh * row.buy - wr_out.grid_export_kwh * row.sell;
        acc.wr.wear_cost += wr_out.battery_discharge_kwh * battery.wear_cost_czk_per_kwh;
        acc.wr.export_kwh += wr_out.grid_export_kwh;
        if is_night(row.local.hour()) {
            acc.wr.min_night_soc = Some(acc.wr.min_night_soc.map_or(soc_now, |m| m.min(soc_now)));
        }
        csv_rows.push(format!(
            "{},{:.4},{:.4},{},{:.2},{:.2},{:.2},{:.4},{:.4},{}",
            row.local.format("%Y-%m-%dT%H:%M"),
            row.buy,
            row.sell,
            MODE_LETTERS[mode_index(mode)],
            soc_now,
            wr_out.energy_end_kwh / capacity * 100.0,
            floor_kwh,
            wr_out.grid_import_kwh,
            wr_out.grid_export_kwh,
            reason,
        ));
        e_wr = wr_out.energy_end_kwh;
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "PV forecast = actual PV × pv_confidence (perfect-shape forecast): savings are an upper bound."
    );
    if pv_noise {
        let _ = writeln!(
            out,
            "--pv-noise: future PV forecasts × deterministic factor in [0.5, 1.3] (seeded by block index); robustness check."
        );
    }
    let counted = || {
        days.iter()
            .filter(|(day, _)| !skipped_days.contains(day))
            .map(|(_, d)| d)
    };
    let total_actual: f32 = counted().map(|d| d.actual).sum();
    let total_su: f32 = counted().map(|d| d.su.cost).sum();
    let total_su_wear: f32 = counted().map(|d| d.su.wear_cost).sum();
    let total_wr: f32 = counted().map(|d| d.wr.cost).sum();
    let total_wr_wear: f32 = counted().map(|d| d.wr.wear_cost).sum();
    let wr_cols = !baseline_only;

    // End state: energy left above SelfUse's, valued at the last day's cheap_ref times efficiency.
    let cheap_ref_last_day = rows.last().map_or(0.0, |last| {
        let last_day = last.local.date_naive();
        let mut buys: Vec<f32> = rows
            .iter()
            .filter(|r| r.local.date_naive() == last_day)
            .map(|r| r.buy)
            .collect();
        cheap_reference(&mut buys, params.cheap_percentile)
    });
    let end_soc_adjustment_czk = (e_wr - e_su) * cheap_ref_last_day * battery.efficiency;
    let total_wr_adjusted = total_wr - end_soc_adjustment_czk;

    let _ = writeln!(
        out,
        "{:<11} {:>9} {:>9} {:>8} {:>9} {:>8} {:>9} {:>7} {:>9} {:>9}",
        "day",
        "actual",
        "selfuse",
        "su_wear",
        "wr",
        "wr_wear",
        "S/C/H/E",
        "minSOC",
        "wr_exp",
        "su_exp"
    );
    for (day, d) in &days {
        let modes = if wr_cols {
            format!(
                "{}/{}/{}/{}",
                d.modes[0], d.modes[1], d.modes[2], d.modes[3]
            )
        } else {
            "-".into()
        };
        let min_soc = if wr_cols {
            d.wr.min_night_soc
        } else {
            d.su.min_night_soc
        };
        let _ = writeln!(
            out,
            "{:<11} {:>9.2} {:>9.2} {:>8.2} {:>9} {:>8} {:>9} {:>7} {:>9} {:>9.2}",
            day,
            d.actual,
            d.su.cost,
            d.su.wear_cost,
            if wr_cols {
                format!("{:.2}", d.wr.cost)
            } else {
                "-".into()
            },
            if wr_cols {
                format!("{:.2}", d.wr.wear_cost)
            } else {
                "-".into()
            },
            modes,
            min_soc.map_or("-".into(), |v| format!("{v:.1}")),
            if wr_cols {
                format!("{:.2}", d.wr.export_kwh)
            } else {
                "-".into()
            },
            d.su.export_kwh,
        );
    }
    let _ = writeln!(
        out,
        "totals: actual {total_actual:.2} CZK | SelfUse {total_su:.2} CZK (wear {total_su_wear:.2}) | {}",
        if wr_cols {
            format!("WinterReserve {total_wr:.2} CZK (wear {total_wr_wear:.2})")
        } else {
            "WinterReserve not run (--baseline-only)".into()
        }
    );
    if wr_cols {
        let _ = writeln!(
            out,
            "end state: WinterReserve {e_wr:.2} kWh vs SelfUse {e_su:.2} kWh, cheap_ref_last_day {cheap_ref_last_day:.3} CZK/kWh, end_soc_adjustment_czk {end_soc_adjustment_czk:.2} (efficiency {:.2}) | WinterReserve raw {total_wr:.2} CZK, adjusted {total_wr_adjusted:.2} CZK (adjusted = raw - end_soc_adjustment_czk)",
            battery.efficiency,
        );
    }
    if !skipped_days.is_empty() {
        let days_text: Vec<String> = skipped_days.iter().map(|d| d.to_string()).collect();
        let _ = writeln!(
            out,
            "skipped from totals (first block has no earlier data): {}",
            days_text.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "blocks: {} usable, {missing} missing (skipped), planned {planned_blocks}, load-forecast fallbacks (no 7-day history, mean of earlier blocks used) {fallback_blocks}",
        rows.len(),
    );
    let _ = writeln!(
        out,
        "costs exclude wear; wear is discharged kWh x wear_cost, shown separately. Night = 18:00-08:00 local, minSOC = SOC at block start of the simulated run."
    );
    print!("{out}");

    if let Some(path) = csv_path {
        let mut text = String::from(
            "start_local,buy,sell,mode,soc_start,soc_end,floor,import,export,reason\n",
        );
        for line in &csv_rows {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&path, text).map_err(|err| format!("{path}: {err}"))?;
    }

    if ![total_actual, total_su, total_su_wear]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err("non-finite totals".into());
    }
    Ok(())
}
