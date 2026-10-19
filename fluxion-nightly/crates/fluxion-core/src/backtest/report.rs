// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Day report of the backtest page: what happened, plain self-use and the Winter Reserve
//! replay on one local day, per block and in total.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;

use super::day::{BacktestSite, FORECAST_HISTORY_DAYS, Field, LoadedDay};
use super::replay::{MODE_LETTERS, ProfileSource, ReplayOptions, ReplayRow, mode_index, replay};
use crate::savings::{RecordedBlock, actual_wear, day_cheap_ref};
use crate::strategy::winter_reserve::{BatteryParams, WinterReserveParams, battery_params};
use fluxion_types::config::WinterReserveConfigCore;

/// Usable blocks a previous day needs to count as history for the forecast (half a day).
const MIN_HISTORY_DAY_BLOCKS: usize = 48;

/// Winter Reserve tunables from the live `strategies.winter_reserve` section (the same mapping
/// as the plugin registration).
#[must_use]
pub fn winter_reserve_params(config: &WinterReserveConfigCore) -> WinterReserveParams {
    WinterReserveParams {
        min_arbitrage_profit_czk: config.min_arbitrage_profit_czk,
        insane_price_multiplier: config.insane_price_multiplier,
        cheap_percentile: config.cheap_percentile,
        load_safety_factor: config.load_safety_factor,
        pv_confidence: config.pv_confidence,
        min_mode_benefit_czk: config.min_mode_benefit_czk,
        mode_switch_min_benefit_czk: config.mode_switch_min_benefit_czk,
        soc_resolution_pct: config.soc_resolution_pct,
    }
}

/// Per-block series of one strategy. `None` where the block is missing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StrategySeries {
    /// SOC at the block start (%)
    pub soc_pct: Vec<Option<f32>>,
    pub import_kwh: Vec<Option<f32>>,
    pub export_kwh: Vec<Option<f32>>,
    /// Mode letter (S/C/H/E); empty for the recorded day (the inverter mode is not read)
    pub mode: Vec<Option<char>>,
    /// Planner reason per block (Winter Reserve only)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reason: Vec<Option<String>>,
}

/// Totals of one strategy over the usable blocks of the day.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct StrategyTotals {
    /// Grid cost: Σ import·buy − export·sell (CZK)
    pub cost_czk: f32,
    /// Discharged kWh × wear cost (CZK), not in `cost_czk`
    pub wear_czk: f32,
    pub import_kwh: f32,
    pub export_kwh: f32,
    pub soc_start_pct: f32,
    pub soc_end_pct: f32,
    /// End battery difference to self-use valued at `cheap_ref × efficiency` (CZK)
    pub end_adjustment_czk: f32,
    /// `self-use cost − cost + end_adjustment` (CZK); 0 for self-use itself
    pub savings_vs_selfuse_czk: f32,
}

/// Minimum SOC of the simulated runs and where it comes from: `control_min_battery_soc`, or
/// `back_up_default_mode` (the dashboard's current default mode is Back Up: the higher of the
/// control minimum and the Back Up minimum, capped at max - 5, as the live planner uses).
#[derive(Debug, Clone, Serialize)]
pub struct FloorUsed {
    pub min_soc_pct: f32,
    pub source: String,
}

/// Fees and price rules the costs were computed with (the live configuration).
#[derive(Debug, Clone, Serialize)]
pub struct FeesUsed {
    pub use_spot_prices_to_buy: bool,
    pub use_spot_prices_to_sell: bool,
    pub buy_price_sensor: bool,
    pub spot_buy_fee_czk: f32,
    pub spot_sell_fee_czk: f32,
    pub hdo_low_tariff_czk: f32,
    pub hdo_high_tariff_czk: f32,
    /// Sell price when not selling at spot
    pub export_price_czk: f32,
}

/// Data coverage of the day.
#[derive(Debug, Clone, Serialize)]
pub struct DataQuality {
    /// Blocks of the local day (92, 96 or 100)
    pub blocks: usize,
    pub usable_blocks: usize,
    /// Local start times (HH:MM) of the blocks left out for lack of data
    pub missing_blocks: Vec<String>,
    /// Blocks of the day without data, per field
    pub missing_per_field: BTreeMap<Field, usize>,
    /// Previous days (of 7) with history for the load forecast
    pub history_days: u32,
    /// Set when the replay ran on less than the full forecast history
    pub degraded: Option<String>,
    /// Where the HDO tariff came from
    pub hdo_source: String,
}

/// Everything the backtest page shows for one day.
#[derive(Debug, Clone, Serialize)]
pub struct DayReport {
    pub date: NaiveDate,
    pub timezone: String,
    /// Local HH:MM per block (a repeated hour on the autumn DST day shows twice)
    pub labels: Vec<String>,
    pub block_starts_utc: Vec<DateTime<Utc>>,
    pub spot_czk: Vec<Option<f32>>,
    pub buy_czk: Vec<Option<f32>>,
    pub sell_czk: Vec<Option<f32>>,
    pub hdo_low: Vec<Option<bool>>,
    pub load_kwh: Vec<Option<f32>>,
    pub pv_kwh: Vec<Option<f32>>,
    pub actual: StrategySeries,
    pub self_use: StrategySeries,
    pub winter_reserve: StrategySeries,
    pub totals_actual: StrategyTotals,
    pub totals_self_use: StrategyTotals,
    pub totals_winter_reserve: StrategyTotals,
    /// Planner `cheap_ref` of the day's buy prices, used to value the end battery
    pub cheap_ref_czk: f32,
    /// Winter Reserve blocks per mode: S, C, H, E
    pub winter_reserve_modes: [u32; 4],
    pub fees: FeesUsed,
    /// Minimum SOC both simulated runs respect
    pub floor: FloorUsed,
    pub data: DataQuality,
    pub notes: Vec<String>,
    pub summary: String,
}

fn fees_used(site: &BacktestSite) -> FeesUsed {
    let p = &site.pricing;
    FeesUsed {
        use_spot_prices_to_buy: p.use_spot_prices_to_buy,
        use_spot_prices_to_sell: p.use_spot_prices_to_sell,
        buy_price_sensor: site.entities.buy_price.is_some(),
        spot_buy_fee_czk: p.spot_buy_fee_czk,
        spot_sell_fee_czk: p.spot_sell_fee_czk,
        hdo_low_tariff_czk: p.hdo_low_tariff_czk,
        hdo_high_tariff_czk: p.hdo_high_tariff_czk,
        export_price_czk: site.control.grid_export_fee_czk_per_kwh,
    }
}

/// Number of the `FORECAST_HISTORY_DAYS` local days before `date` with enough usable blocks.
fn history_days(rows: &[ReplayRow], date: NaiveDate, site: &BacktestSite) -> u32 {
    let mut per_day: BTreeMap<NaiveDate, usize> = BTreeMap::new();
    for row in rows {
        *per_day
            .entry(row.block.start.with_timezone(&site.timezone).date_naive())
            .or_default() += 1;
    }
    (1..=FORECAST_HISTORY_DAYS)
        .filter(|back| {
            per_day
                .get(&(date - Duration::days(*back)))
                .is_some_and(|n| *n >= MIN_HISTORY_DAY_BLOCKS)
        })
        .count() as u32
}

fn label(start: DateTime<Utc>, site: &BacktestSite) -> String {
    start
        .with_timezone(&site.timezone)
        .format("%H:%M")
        .to_string()
}

/// Replay the loaded day and build the report.
#[expect(clippy::too_many_lines)]
///
/// `backup_floor_pct` is the live planner's Back Up floor (`scheduling::backup_planner_floor`
/// of the current user control state), `None` in Self-Use. With it, Winter Reserve plans and
/// self-use runs with that minimum SOC, as the planner and the Back Up inverter mode do live.
pub fn build_report(
    loaded: &LoadedDay,
    site: &BacktestSite,
    backup_floor_pct: Option<f32>,
) -> Result<DayReport, String> {
    let mut control = site.control.clone();
    if let Some(floor) = backup_floor_pct {
        control.min_battery_soc = floor;
    }
    let floor = FloorUsed {
        min_soc_pct: control.min_battery_soc,
        source: if backup_floor_pct.is_some() {
            "back_up_default_mode"
        } else {
            "control_min_battery_soc"
        }
        .to_owned(),
    };
    let battery: BatteryParams = battery_params(&control);
    let params = winter_reserve_params(&site.winter_reserve);
    let capacity = battery.capacity_kwh.max(f32::EPSILON);
    let buy_sensor = site.entities.buy_price.is_some();

    // Usable rows over the whole loaded range; a missing block flags the row before it.
    let mut rows: Vec<ReplayRow> = Vec::new();
    for block in &loaded.blocks {
        match block.recorded() {
            Some((recorded, soc_pct)) => rows.push(ReplayRow {
                block: recorded,
                soc_pct,
                gap_after: false,
            }),
            None => {
                if let Some(last) = rows.last_mut() {
                    last.gap_after = true;
                }
            }
        }
    }

    let day: Vec<_> = loaded.day_blocks().collect();
    let mut missing_per_field: BTreeMap<Field, usize> = BTreeMap::new();
    let mut missing_blocks = Vec::new();
    for block in &day {
        for field in block.missing_fields(buy_sensor) {
            *missing_per_field.entry(field).or_default() += 1;
        }
        if block.recorded().is_none() {
            missing_blocks.push(label(block.start, site));
        }
    }
    let day_rows: Vec<&ReplayRow> = rows
        .iter()
        .filter(|r| r.block.start >= loaded.day_start && r.block.start < loaded.day_end)
        .collect();
    if day_rows.is_empty() {
        return Err(format!(
            "no usable block on {}: the history has no complete data for that day",
            loaded.date
        ));
    }

    let history_days = history_days(&rows, loaded.date, site);
    let mut notes = vec![
        "Winter Reserve replay: the planner sees the actual PV of each block as its forecast, so its result is an upper bound.".to_owned(),
        "Load forecast: the live hourly house-load profile from the 7 days before each block's day (max(mean, p75) x planner margin for the reserve).".to_owned(),
        "Self-use and Winter Reserve start from the recorded SOC at the first usable block and run on the recorded load and PV.".to_owned(),
    ];
    notes.push(match backup_floor_pct {
        Some(pct) => format!(
            "Back Up default mode (current dashboard setting): both simulated runs keep at least {pct:.0} % SOC, the floor the live planner uses; self-use does not discharge below it."
        ),
        None => format!(
            "Minimum SOC of both simulated runs: {:.0} % (control minimum, Self-Use default mode).",
            floor.min_soc_pct
        ),
    });
    let degraded = (history_days < FORECAST_HISTORY_DAYS as u32).then(|| {
        if history_days == 0 {
            "No earlier day has history: the load forecast uses the mean of the earlier blocks of the same day, and the first block runs self-use.".to_owned()
        } else {
            format!(
                "Only {history_days} of {FORECAST_HISTORY_DAYS} previous days have history; the load forecast uses those (below 3 days the import profile or the mean of earlier blocks)."
            )
        }
    });
    let hdo_source = if loaded.hdo_from_sensor {
        format!(
            "schedule of {} recorded on that day",
            loaded.hdo_entity.as_deref().unwrap_or("the HDO sensor")
        )
    } else if let Some(error) = &loaded.hdo_lookup_error {
        notes.push(format!("The HDO sensor could not be looked up ({error}): every block is priced at the high tariff, as the live planner does without HDO data."));
        "lookup failed (high tariff)".to_owned()
    } else if loaded.hdo_entity.is_none() {
        notes.push("No HDO sensor was found (configured entity missing and none found by content): every block is priced at the high tariff, as the live planner does without a schedule.".to_owned());
        "none found (high tariff)".to_owned()
    } else {
        notes.push("No HDO schedule was recorded for this day: every block is priced at the high tariff, as the live planner does without a schedule.".to_owned());
        "none (high tariff)".to_owned()
    };

    let mut options = ReplayOptions::new(site.timezone, battery, params);
    options.planner_load_margin = site.control.planner_load_margin;
    options.profile = ProfileSource::House;
    options.from = Some(loaded.day_start);
    options.until = Some(loaded.day_end);
    let run = replay(&rows, &options)?;

    // Per-block series aligned with every block of the day (missing ones are None).
    let simulated: BTreeMap<DateTime<Utc>, &super::replay::SimulatedBlock> =
        run.blocks.iter().map(|b| (b.row.block.start, b)).collect();
    let mut actual = StrategySeries::default();
    let mut self_use = StrategySeries::default();
    let mut winter_reserve = StrategySeries::default();
    for block in &day {
        let sim = simulated.get(&block.start);
        let recorded = sim.map(|s| s.row.block);
        actual.soc_pct.push(block.soc_pct);
        actual.import_kwh.push(recorded.map(|r| r.import_kwh));
        actual.export_kwh.push(recorded.map(|r| r.export_kwh));
        actual.mode.push(None);
        let su = sim.map(|s| &s.selfuse);
        self_use
            .soc_pct
            .push(su.map(|s| s.energy_start_kwh / capacity * 100.0));
        self_use
            .import_kwh
            .push(su.map(|s| s.outcome.grid_import_kwh));
        self_use
            .export_kwh
            .push(su.map(|s| s.outcome.grid_export_kwh));
        self_use.mode.push(su.map(|_| 'S'));
        let wr = sim.and_then(|s| s.winter_reserve.as_ref());
        winter_reserve
            .soc_pct
            .push(wr.map(|s| s.energy_start_kwh / capacity * 100.0));
        winter_reserve
            .import_kwh
            .push(wr.map(|s| s.outcome.grid_import_kwh));
        winter_reserve
            .export_kwh
            .push(wr.map(|s| s.outcome.grid_export_kwh));
        winter_reserve
            .mode
            .push(wr.map(|s| MODE_LETTERS[mode_index(s.mode)]));
        winter_reserve.reason.push(wr.map(|s| s.reason.clone()));
    }

    // Totals. The day is the only simulated day; recorded blocks for the H6 definitions.
    let totals = run.days.values().next().copied().unwrap_or_default();
    let recorded: Vec<RecordedBlock> = day_rows.iter().map(|r| r.block).collect();
    let cheap_ref = day_cheap_ref(&recorded, params.cheap_percentile);
    let first_sim = run.blocks.first().ok_or("nothing was simulated")?;
    let soc_start_pct = first_sim.selfuse.energy_start_kwh / capacity * 100.0;
    if day_rows
        .first()
        .is_some_and(|r| r.soc_pct + 0.05 < floor.min_soc_pct)
    {
        notes.push(format!(
            "The recorded SOC at the start ({:.0} %) is below the {:.0} % floor: the simulated runs start at the floor.",
            day_rows.first().map_or(0.0, |r| r.soc_pct),
            floor.min_soc_pct
        ));
    }
    // Recorded SOC at the end: the first usable block after the day, else the last of the day
    let actual_end_pct = rows
        .iter()
        .find(|r| r.block.start >= loaded.day_end)
        .or(day_rows.last().copied())
        .map_or(soc_start_pct, |r| r.soc_pct);
    let e_su = run.energy_end_selfuse_kwh;
    let value = |energy_end_kwh: f32| (energy_end_kwh - e_su) * cheap_ref * battery.efficiency;
    let sum = |f: fn(&RecordedBlock) -> f32| recorded.iter().map(f).sum::<f32>();

    let totals_self_use = StrategyTotals {
        cost_czk: totals.su.cost,
        wear_czk: totals.su.wear_cost,
        import_kwh: run
            .blocks
            .iter()
            .map(|b| b.selfuse.outcome.grid_import_kwh)
            .sum(),
        export_kwh: totals.su.export_kwh,
        soc_start_pct,
        soc_end_pct: e_su / capacity * 100.0,
        end_adjustment_czk: 0.0,
        savings_vs_selfuse_czk: 0.0,
    };
    let actual_adj = value(actual_end_pct / 100.0 * capacity);
    let totals_actual = StrategyTotals {
        cost_czk: totals.actual,
        wear_czk: actual_wear(&recorded, battery.wear_cost_czk_per_kwh),
        import_kwh: sum(|b| b.import_kwh),
        export_kwh: sum(|b| b.export_kwh),
        soc_start_pct: day_rows.first().map_or(soc_start_pct, |r| r.soc_pct),
        soc_end_pct: actual_end_pct,
        end_adjustment_czk: actual_adj,
        savings_vs_selfuse_czk: totals.su.cost - totals.actual + actual_adj,
    };
    let wr_adj = value(run.energy_end_wr_kwh);
    let totals_winter_reserve = StrategyTotals {
        cost_czk: totals.wr.cost,
        wear_czk: totals.wr.wear_cost,
        import_kwh: run
            .blocks
            .iter()
            .filter_map(|b| b.winter_reserve.as_ref())
            .map(|s| s.outcome.grid_import_kwh)
            .sum(),
        export_kwh: totals.wr.export_kwh,
        soc_start_pct,
        soc_end_pct: run.energy_end_wr_kwh / capacity * 100.0,
        end_adjustment_czk: wr_adj,
        savings_vs_selfuse_czk: totals.su.cost - totals.wr.cost + wr_adj,
    };
    if !missing_blocks.is_empty() {
        notes.push(format!(
            "{} block(s) without complete data are left out of every total.",
            missing_blocks.len()
        ));
    }

    let summary = format!(
        "On {date} the site paid {actual:.2} CZK for grid energy. Plain self-use would have cost {su:.2} CZK and Winter Reserve with the current settings {wr:.2} CZK. Against self-use, what actually ran saved {sa:.2} CZK and Winter Reserve would have saved {sw:.2} CZK (end battery valued at {cheap:.2} CZK/kWh).",
        date = loaded.date,
        actual = totals_actual.cost_czk,
        su = totals_self_use.cost_czk,
        wr = totals_winter_reserve.cost_czk,
        sa = totals_actual.savings_vs_selfuse_czk,
        sw = totals_winter_reserve.savings_vs_selfuse_czk,
        cheap = cheap_ref,
    );

    Ok(DayReport {
        date: loaded.date,
        timezone: site.timezone.name().to_owned(),
        labels: day.iter().map(|b| label(b.start, site)).collect(),
        block_starts_utc: day.iter().map(|b| b.start).collect(),
        spot_czk: day.iter().map(|b| b.spot_czk).collect(),
        buy_czk: day.iter().map(|b| b.buy_czk).collect(),
        sell_czk: day.iter().map(|b| b.sell_czk).collect(),
        hdo_low: day.iter().map(|b| b.hdo_low).collect(),
        load_kwh: day.iter().map(|b| b.load_kwh).collect(),
        pv_kwh: day.iter().map(|b| b.pv_kwh).collect(),
        actual,
        self_use,
        winter_reserve,
        totals_actual,
        totals_self_use,
        totals_winter_reserve,
        cheap_ref_czk: cheap_ref,
        winter_reserve_modes: totals.modes,
        fees: fees_used(site),
        floor,
        data: DataQuality {
            blocks: day.len(),
            usable_blocks: day_rows.len(),
            missing_blocks,
            missing_per_field,
            history_days,
            degraded,
            hdo_source,
        },
        notes,
        summary,
    })
}
