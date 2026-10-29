// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Daily savings against plain self-use, from recorded site history.
//!
//! Input: the JSON written by `fluxion/tools/winter_reserve/export_ha_history.py`.
//! Each local day: `savings = cost_selfuse - cost_actual + adj`, with
//! `adj = (soc_end_actual - soc_end_selfuse) x cheap_ref x efficiency`. Definitions:
//! `docs/legacy-levers/brief-H6-savings.md`. The same functions run live in the add-on.
//!
//! Usage: `cargo run -p fluxion-core --example savings_report -- data.json`

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use fluxion_core::savings::{DaySavings, RecordedBlock, day_cheap_ref, day_savings};
use fluxion_core::strategy::winter_reserve::{BatteryParams, WinterReserveParams};
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

/// One usable block with its local date and recorded SOC.
struct Row {
    day: NaiveDate,
    block: RecordedBlock,
    soc_pct: f32,
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
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: savings_report data.json")?;
    let text = fs::read_to_string(&input).map_err(|err| format!("{input}: {err}"))?;
    let dataset: Dataset = serde_json::from_str(&text).map_err(|err| format!("{input}: {err}"))?;
    let tz: Tz = dataset
        .site
        .timezone
        .parse()
        .map_err(|_| format!("unknown timezone {}", dataset.site.timezone))?;
    let battery = dataset.site.battery;
    let cheap_percentile = WinterReserveParams::default().cheap_percentile;

    // Non-missing blocks only. Missing blocks carry no data and are left out of the sums.
    let mut rows: Vec<Row> = Vec::new();
    let mut missing = 0usize;
    for raw in &dataset.blocks {
        if raw.missing {
            missing += 1;
            continue;
        }
        let start: DateTime<Utc> = raw
            .start_utc
            .parse()
            .map_err(|err| format!("bad start_utc {}: {err}", raw.start_utc))?;
        let (Some(buy), Some(sell), Some(soc), Some(load), Some(pv), Some(import), Some(export)) = (
            raw.buy,
            raw.sell,
            raw.soc_pct,
            raw.load_kwh,
            raw.pv_kwh,
            raw.import_kwh,
            raw.export_kwh,
        ) else {
            return Err(format!(
                "block {} is not missing but has null fields",
                raw.start_utc
            ));
        };
        rows.push(Row {
            day: start.with_timezone(&tz).date_naive(),
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
        });
    }
    if rows.is_empty() {
        return Err("no usable blocks".into());
    }

    // Consecutive rows per local day, with the SOC that closes each day: the next day's first
    // block, or the last known block for the final day.
    let mut days: Vec<(NaiveDate, Vec<RecordedBlock>, f32, f32)> = Vec::new();
    for row in &rows {
        match days.last_mut() {
            Some((day, blocks, _, _)) if *day == row.day => blocks.push(row.block),
            _ => days.push((row.day, vec![row.block], row.soc_pct, row.soc_pct)),
        }
    }
    let day_starts: Vec<f32> = days.iter().map(|(_, _, soc, _)| *soc).collect();
    for (index, day) in days.iter_mut().enumerate() {
        day.3 = day_starts.get(index + 1).copied().unwrap_or(day.3);
    }

    let mut results: Vec<(NaiveDate, usize, DaySavings)> = Vec::new();
    for (day, blocks, soc_start, soc_end) in &days {
        let cheap_ref = day_cheap_ref(blocks, cheap_percentile);
        let result = day_savings(blocks, *soc_start, *soc_end, cheap_ref, &battery);
        results.push((*day, blocks.len(), result));
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<11} {:>6} {:>10} {:>10} {:>8} {:>10} {:>10} {:>10}",
        "day", "blocks", "actual", "selfuse", "adj", "savings", "wear_act", "wear_su"
    );
    let mut total = DaySavings::default();
    for (day, count, result) in &results {
        let _ = writeln!(
            out,
            "{:<11} {:>6} {:>10.2} {:>10.2} {:>8.2} {:>10.2} {:>10.2} {:>10.2}",
            day,
            count,
            result.cost_actual,
            result.cost_selfuse,
            result.adj,
            result.savings,
            result.wear_actual,
            result.wear_selfuse,
        );
        total.cost_actual += result.cost_actual;
        total.cost_selfuse += result.cost_selfuse;
        total.adj += result.adj;
        total.savings += result.savings;
        total.wear_actual += result.wear_actual;
        total.wear_selfuse += result.wear_selfuse;
    }
    let _ = writeln!(
        out,
        "{:<11} {:>6} {:>10.2} {:>10.2} {:>8.2} {:>10.2} {:>10.2} {:>10.2}",
        "total",
        rows.len(),
        total.cost_actual,
        total.cost_selfuse,
        total.adj,
        total.savings,
        total.wear_actual,
        total.wear_selfuse,
    );
    let _ = writeln!(
        out,
        "CZK. savings = selfuse - actual + adj; wear is reported apart and not in savings. {missing} missing blocks skipped."
    );
    print!("{out}");

    if ![total.cost_actual, total.cost_selfuse, total.savings]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err("non-finite totals".into());
    }
    Ok(())
}
