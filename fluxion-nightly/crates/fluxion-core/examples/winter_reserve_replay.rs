// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Replay recorded site history block by block with the Winter Reserve planner.
//!
//! Input: the JSON written by `fluxion/tools/winter_reserve/export_ha_history.py`. The replay
//! itself is `fluxion_core::backtest::replay`, shared with the add-on's backtest page.
//! For every block the planner sees the horizon (today, plus tomorrow after 13:00 local)
//! with a forecast (load: mean of the same local quarter-hour over the previous 7 local days,
//! else the mean of all earlier blocks; never the block's own actual value. The planner plans
//! buys on the expected flows (load as is, PV times `--pv-forecast-bias`) and sizes the night
//! reserve on the conservative ones (load times `load_safety_factor`, PV times `pv_confidence`).
//! The horizon stops just before a missing block. The first planned mode is then applied to
//! the *actual* block. The end state is valued at `cheap_ref` of the last day (see the totals).
//!
//! `--pv-forecast-bias <f>` (default 1.0) multiplies the forecast PV the planner sees, in both
//! views. The simulated reality always uses the actual PV, so a value below 1.0 replays a
//! pessimistic forecast, the case where the planner used to overbuy from the grid.
//! The totals include the grid-charge kWh and its cost (charge the grid had to import).
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
//!  [--csv out.csv] [--pv-noise] [--pv-forecast-bias 0.5] [--profile forecast|import|house]
//!  [--from 2026-10-14T14:30:00Z] [--force-discharge] [--cost-basis] [--param key=value]...`
//!
//! `--force-discharge` sets `force_discharge_enabled` (export to the grid allowed) on the battery.
//! `--cost-basis` tracks the cost of charge of the simulated battery and plans with it (brief
//! H33); without it the planner sees "cost unknown".

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fluxion_core::backtest::{
    MODE_LETTERS, ProfileSource, ReplayOptions, ReplayRow, mode_index, replay,
};
use fluxion_core::savings::RecordedBlock;
use fluxion_core::strategy::winter_reserve::{BatteryParams, WinterReserveParams, min_charge_kwh};
use serde::Deserialize;
use std::fmt::Write as _;
use std::fs;
use std::process::ExitCode;

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
    let mut pv_forecast_bias = 1.0_f32;
    let mut profile = ProfileSource::Forecast;
    let mut from: Option<DateTime<Utc>> = None;
    let mut params = WinterReserveParams::default();
    let mut force_discharge = false;
    let mut cost_basis = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baseline-only" => baseline_only = true,
            "--pv-noise" => pv_noise = true,
            "--force-discharge" => force_discharge = true,
            "--cost-basis" => cost_basis = true,
            "--pv-forecast-bias" => {
                let text = iter.next().ok_or("--pv-forecast-bias needs a factor")?;
                pv_forecast_bias = text
                    .parse()
                    .map_err(|err| format!("--pv-forecast-bias {text}: {err}"))?;
            }
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
    let mut battery = dataset.site.battery;
    // `control.force_discharge_enabled` (export to the grid); off unless the flag is given.
    battery.force_discharge_enabled |= force_discharge;
    // The live planner derives the minimum useful charge from the charge power and the
    // mode-change debounce (brief H30); a dataset without it gets the live default.
    if battery.min_charge_kwh <= 0.0 {
        battery.min_charge_kwh = min_charge_kwh(
            battery.max_charge_kw,
            fluxion_types::config::ControlConfig::default().min_mode_change_interval_secs,
        );
    }
    eprintln!("min_charge_kwh {:.3}", battery.min_charge_kwh);
    let capacity = battery.capacity_kwh;
    let e_min = battery.min_soc_pct / 100.0 * capacity;

    // Non-missing blocks only. Missing blocks are skipped: no cost, no simulation, no forecast data.
    let mut rows: Vec<ReplayRow> = Vec::new();
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
        rows.push(ReplayRow {
            block: RecordedBlock {
                start,
                buy_czk: buy,
                sell_czk: sell,
                load_kwh: load,
                pv_kwh: pv,
                import_kwh: import,
                export_kwh: export,
            },
            soc_pct: soc,
            gap_after: false,
        });
    }

    let mut options = ReplayOptions::new(tz, battery, params);
    options.profile = profile;
    options.pv_forecast_bias = pv_forecast_bias;
    options.pv_noise = pv_noise;
    options.baseline_only = baseline_only;
    options.from = from;
    options.track_cost_basis = cost_basis;
    let run = replay(&rows, &options)?;
    let days = &run.days;
    let skipped_days = &run.skipped_days;
    let (e_su, e_wr) = (run.energy_end_selfuse_kwh, run.energy_end_wr_kwh);
    let planned_blocks = run.planned_blocks;
    let fallback_blocks = run.fallback_blocks;

    let mut csv_rows: Vec<String> = Vec::new();
    for block in &run.blocks {
        let recorded = &block.row.block;
        let su = &block.selfuse;
        match &block.winter_reserve {
            None => csv_rows.push(format!(
                "{},{:.4},{:.4},S,{:.2},{:.2},{:.2},{:.4},{:.4},baseline SelfUse",
                block.local.format("%Y-%m-%dT%H:%M"),
                recorded.buy_czk,
                recorded.sell_czk,
                su.energy_start_kwh / capacity * 100.0,
                su.energy_end_kwh / capacity * 100.0,
                e_min,
                su.outcome.grid_import_kwh,
                su.outcome.grid_export_kwh,
            )),
            Some(wr) => csv_rows.push(format!(
                "{},{:.4},{:.4},{},{:.2},{:.2},{:.2},{:.4},{:.4},{}",
                block.local.format("%Y-%m-%dT%H:%M"),
                recorded.buy_czk,
                recorded.sell_czk,
                MODE_LETTERS[mode_index(wr.mode)],
                wr.energy_start_kwh / capacity * 100.0,
                wr.energy_end_kwh / capacity * 100.0,
                wr.floor_kwh,
                wr.outcome.grid_import_kwh,
                wr.outcome.grid_export_kwh,
                wr.reason,
            )),
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "PV forecast = actual PV × pv_forecast_bias {pv_forecast_bias:.2} (expected) and × pv_confidence (reserve), perfect-shape forecast: savings are an upper bound."
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
    let total_wr_grid_charge_kwh: f32 = counted().map(|d| d.wr.grid_charge_kwh).sum();
    let total_wr_grid_charge_czk: f32 = counted().map(|d| d.wr.grid_charge_czk).sum();
    let wr_cols = !baseline_only;

    // End state: energy left above SelfUse's, valued at the last day's cheap_ref times efficiency.
    let cheap_ref_last_day = run.cheap_ref_last_day;
    let end_soc_adjustment_czk = run.end_soc_adjustment_czk(&battery);
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
    for (day, d) in days {
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
            "grid charge: WinterReserve {total_wr_grid_charge_kwh:.2} kWh = {total_wr_grid_charge_czk:.2} CZK (pv_forecast_bias {pv_forecast_bias:.2})"
        );
        let _ = writeln!(
            out,
            "end state: WinterReserve {e_wr:.2} kWh vs SelfUse {e_su:.2} kWh, cheap_ref_last_day {cheap_ref_last_day:.3} CZK/kWh, end_soc_adjustment_czk {end_soc_adjustment_czk:.2} (efficiency {:.2}) | WinterReserve raw {total_wr:.2} CZK, adjusted {total_wr_adjusted:.2} CZK (adjusted = raw - end_soc_adjustment_czk)",
            battery.efficiency,
        );
    }
    if wr_cols {
        // Counted days only, as the totals above.
        let counted_blocks = || {
            run.blocks.iter().filter(|b| {
                !skipped_days.contains(&b.local.date_naive()) && b.winter_reserve.is_some()
            })
        };
        let curtailed: f32 = counted_blocks()
            .filter_map(|b| b.winter_reserve.as_ref())
            .map(|wr| wr.outcome.curtailed_kwh)
            .sum();
        let exported: f32 = counted().map(|d| d.wr.export_kwh).sum();
        let mut mode_changes = 0usize;
        let mut previous = None;
        for wr in counted_blocks().filter_map(|b| b.winter_reserve.as_ref()) {
            if previous.is_some_and(|m| m != wr.mode) {
                mode_changes += 1;
            }
            previous = Some(wr.mode);
        }
        let _ = writeln!(
            out,
            "flows: WinterReserve exported {exported:.2} kWh, curtailed {curtailed:.2} kWh, mode changes {mode_changes} (force_discharge_enabled {}, cost_basis {cost_basis})",
            battery.force_discharge_enabled,
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
        run.usable_blocks,
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
