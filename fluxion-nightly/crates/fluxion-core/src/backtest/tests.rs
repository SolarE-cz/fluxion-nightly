// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Tests of the backtest day loader, price construction and replay. Fixtures are synthetic.

use super::*;
use crate::savings::RecordedBlock;
use crate::strategy::pricing::{HdoDaySchedule, HdoTimeRange};
use crate::strategy::winter_reserve::{BatteryParams, WinterReserveParams};
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use fluxion_types::config::{ControlConfig, PriceSchedule, PricingConfig, WinterReserveConfigCore};
use fluxion_types::pricing::TimeBlockPrice;
use std::collections::{BTreeMap, HashMap};

const PRAGUE: Tz = chrono_tz::Europe::Prague;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn pricing(buy_sensor: bool) -> PricingConfig {
    PricingConfig {
        spot_price_entity: "sensor.spot_price".to_owned(),
        tomorrow_price_entity: None,
        buy_price_entity: buy_sensor.then(|| "sensor.buy_price".to_owned()),
        use_spot_prices_to_buy: true,
        use_spot_prices_to_sell: true,
        fixed_buy_price_czk: PriceSchedule::Flat(0.0),
        fixed_sell_price_czk: PriceSchedule::Flat(0.0),
        spot_buy_fee_czk: 0.4,
        spot_sell_fee_czk: 0.3,
        hdo_sensor_entity: "sensor.hdo_schedule".to_owned(),
        hdo_low_tariff_czk: 0.5,
        hdo_high_tariff_czk: 1.8,
    }
}

fn site(buy_sensor: bool, pv_extra: bool) -> BacktestSite {
    let mut pv = vec!["sensor.solax_pv_power_total".to_owned()];
    if pv_extra {
        pv.push("sensor.solax_2_pv_power_total".to_owned());
    }
    BacktestSite {
        timezone: PRAGUE,
        entities: BacktestEntities {
            soc: "sensor.solax_battery_capacity".to_owned(),
            house_load: "sensor.solax_house_load".to_owned(),
            pv,
            grid_import: "sensor.solax_grid_import".to_owned(),
            grid_export: "sensor.solax_grid_export".to_owned(),
            spot_price: "sensor.spot_price".to_owned(),
            buy_price: buy_sensor.then(|| "sensor.buy_price".to_owned()),
            hdo: Some("sensor.hdo_schedule".to_owned()),
        },
        pricing: pricing(buy_sensor),
        control: ControlConfig::default(),
        winter_reserve: WinterReserveConfigCore::default(),
    }
}

fn state(at: DateTime<Utc>, value: &str) -> HistoryState {
    HistoryState::from_raw(at, value, None)
}

/// In-memory recorder: returns the state valid at `start` (stamped at `start`) and the changes
/// up to `end`, like HA's history API.
#[derive(Default)]
struct FakeRecorder {
    states: HashMap<String, Vec<HistoryState>>,
    /// HDO discovery result per configured entity (absent: the configured entity)
    hdo_resolution: HashMap<String, Option<String>>,
    /// The HDO entity lookup (`/api/states`) fails with this error
    hdo_lookup_error: Option<HistoryError>,
    calls: std::sync::Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl HistoryReader for FakeRecorder {
    async fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        with_attributes: bool,
    ) -> Result<Vec<HistoryState>, HistoryError> {
        self.calls
            .lock()
            .unwrap()
            .push((entity_id.to_owned(), with_attributes));
        let all = self.states.get(entity_id).cloned().unwrap_or_default();
        let mut out = Vec::new();
        if let Some(before) = all.iter().rfind(|s| s.at <= start) {
            out.push(HistoryState {
                at: start,
                ..before.clone()
            });
        }
        out.extend(all.into_iter().filter(|s| s.at > start && s.at <= end));
        Ok(out)
    }

    async fn resolve_hdo_entity(&self, configured: &str) -> Result<Option<String>, HistoryError> {
        if let Some(e) = &self.hdo_lookup_error {
            return Err(e.clone());
        }
        Ok(self
            .hdo_resolution
            .get(configured)
            .cloned()
            .unwrap_or_else(|| Some(configured.to_owned())))
    }
}

/// HDO sensor state with a schedule attribute: low tariff 00:00-06:00 local on `day`.
fn hdo_state(at: DateTime<Utc>, day: NaiveDate) -> HistoryState {
    let d = day.format("%Y-%m-%d");
    let offset = PRAGUE
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .format("%:z");
    HistoryState {
        at,
        state: Some("on".to_owned()),
        attributes: Some(serde_json::json!({
            "schedule": [
                {"start": format!("{d}T00:00:00{offset}"), "end": format!("{d}T06:00:00{offset}"), "tariff": "NT", "value": 1},
                {"start": format!("{d}T06:00:00{offset}"), "end": format!("{d}T23:59:59{offset}"), "tariff": "VT", "value": 0}
            ]
        })),
    }
}

/// A recorder with constant sensors from `from` on: load 1000 W, PV 400 W (+200 W extra),
/// import 600 W, export 0 W, SOC 50 %, spot 2.0, buy sensor 2.42, HDO schedule for `days`.
fn constant_recorder(from: DateTime<Utc>, days: &[NaiveDate]) -> FakeRecorder {
    let mut rec = FakeRecorder::default();
    let mut put = |entity: &str, value: &str| {
        rec.states
            .insert(entity.to_owned(), vec![state(from, value)]);
    };
    put("sensor.solax_battery_capacity", "50");
    put("sensor.solax_house_load", "1000");
    put("sensor.solax_pv_power_total", "400");
    put("sensor.solax_2_pv_power_total", "200");
    put("sensor.solax_grid_import", "600");
    put("sensor.solax_grid_export", "0");
    put("sensor.spot_price", "2.0");
    put("sensor.buy_price", "2.42");
    rec.states.insert(
        "sensor.hdo_schedule".to_owned(),
        days.iter()
            .map(|d| hdo_state(local_midnight_utc(*d, PRAGUE) - Duration::hours(1), *d))
            .collect(),
    );
    rec
}

// ---------- day blocks and DST ----------

#[test]
fn dst_days_have_92_96_100_blocks() {
    assert_eq!(day_block_starts(date(2026, 3, 29), PRAGUE).len(), 92);
    assert_eq!(day_block_starts(date(2026, 10, 25), PRAGUE).len(), 100);
    assert_eq!(day_block_starts(date(2026, 10, 20), PRAGUE).len(), 96);
    // Autumn day starts at local midnight (22:00 UTC the day before, CEST)
    assert_eq!(
        day_block_starts(date(2026, 10, 25), PRAGUE)[0],
        Utc.with_ymd_and_hms(2026, 10, 24, 22, 0, 0).unwrap()
    );
}

#[tokio::test]
async fn loader_builds_dst_days_and_sums_pv_master_and_extra() {
    for (day, expected) in [(date(2026, 10, 25), 100usize), (date(2026, 3, 29), 92)] {
        let from = local_midnight_utc(day - Duration::days(8), PRAGUE);
        let rec = constant_recorder(from, &[day, day + Duration::days(1)]);
        let site = site(false, true);
        let now = local_midnight_utc(day + Duration::days(3), PRAGUE);
        let loaded = load_day(&rec, &site, day, now).await.unwrap();
        let blocks: Vec<_> = loaded.day_blocks().collect();
        assert_eq!(blocks.len(), expected, "{day}");
        for b in &blocks {
            // (400 + 200) W over 15 min = 0.15 kWh
            assert!((b.pv_kwh.unwrap() - 0.15).abs() < 1e-6, "{b:?}");
            assert!((b.load_kwh.unwrap() - 0.25).abs() < 1e-6);
            assert!(b.missing_fields(false).is_empty(), "{b:?}");
        }
        // The forecast history (7 days before) and the next day are loaded too
        assert_eq!(
            loaded.blocks[0].start,
            local_midnight_utc(day - Duration::days(7), PRAGUE)
        );
        assert!(loaded.blocks.last().unwrap().start >= loaded.day_end);

        let report = build_report(&loaded, &site, None).unwrap();
        assert_eq!(report.floor.source, "control_min_battery_soc");
        assert_eq!(report.labels.len(), expected);
        assert_eq!(report.data.usable_blocks, expected);
        assert_eq!(report.data.history_days, 7);
        assert!(report.data.degraded.is_none());
    }
}

#[tokio::test]
async fn loader_reads_only_history_and_hdo_with_attributes() {
    let day = date(2026, 10, 20);
    let rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    let site = site(true, true);
    let now = local_midnight_utc(day + Duration::days(3), PRAGUE);
    load_day(&rec, &site, day, now).await.unwrap();
    let calls = rec.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 9, "{calls:?}");
    for (entity, attrs) in calls {
        assert_eq!(attrs, entity == "sensor.hdo_schedule", "{entity}");
    }
}

#[tokio::test]
async fn today_is_refused() {
    let day = date(2026, 10, 20);
    let rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    let now = local_midnight_utc(day, PRAGUE) + Duration::hours(10);
    let err = load_day(&rec, &site(false, false), day, now)
        .await
        .unwrap_err();
    assert_eq!(err, LoadError::NotFinished);
}

#[test]
fn missing_data_is_counted_never_filled() {
    let day = date(2026, 10, 20);
    let start = local_midnight_utc(day, PRAGUE);
    let end = local_midnight_utc(day + Duration::days(1), PRAGUE);
    let at = |q: i64| start + Duration::minutes(15 * q);
    let tl = |states: Vec<HistoryState>| Timeline::from_states(&states);
    let history = SiteHistory {
        soc: tl(vec![state(at(0), "50")]),
        // Load unavailable for blocks 4 and 5
        house_load: tl(vec![
            state(at(0), "1000"),
            state(at(4), "unavailable"),
            state(at(6), "1000"),
        ]),
        // Extra PV has no data before block 10: there PV is the master alone, flagged partial
        // (as live). The master is unavailable in block 30: no PV there.
        pv: vec![
            tl(vec![
                state(at(0), "400"),
                state(at(30), "unavailable"),
                state(at(31), "400"),
            ]),
            tl(vec![state(at(10), "200")]),
        ],
        grid_import: tl(vec![state(at(0), "600")]),
        grid_export: tl(vec![state(at(0), "0")]),
        // No spot price in block 20 (state unknown from 20 to 21)
        spot: tl(vec![
            state(at(0), "2.0"),
            state(at(20), "unknown"),
            state(at(21), "2.0"),
        ]),
        buy: None,
        hdo_schedules: Vec::new(),
    };
    let blocks = build_blocks(
        &history,
        start,
        end,
        &pricing(false),
        &ControlConfig::default(),
        PRAGUE,
    );
    assert_eq!(blocks.len(), 96);
    let mut counts: BTreeMap<Field, usize> = BTreeMap::new();
    for b in &blocks {
        for f in b.missing_fields(false) {
            *counts.entry(f).or_default() += 1;
        }
    }
    assert_eq!(counts.get(&Field::Load), Some(&2));
    assert_eq!(counts.get(&Field::Pv), Some(&1));
    assert_eq!(counts.get(&Field::PvPartial), Some(&10));
    assert_eq!(counts.get(&Field::Spot), Some(&1));
    assert_eq!(counts.get(&Field::Hdo), Some(&96));
    // The report serialises the counts with snake_case field names
    let json = serde_json::to_value(&counts).unwrap();
    assert_eq!(json["load"], 2);
    assert_eq!(json["pv"], 1);
    assert_eq!(json["pv_partial"], 10);
    assert!(blocks[4].load_kwh.is_none() && blocks[4].recorded().is_none());
    // 400 W alone = 0.1 kWh, usable; with the extra 0.15 kWh
    assert!((blocks[9].pv_kwh.unwrap() - 0.1).abs() < 1e-6 && blocks[9].pv_partial);
    assert!(blocks[9].recorded().is_some());
    assert!((blocks[10].pv_kwh.unwrap() - 0.15).abs() < 1e-6 && !blocks[10].pv_partial);
    assert!(blocks[30].pv_kwh.is_none() && !blocks[30].pv_partial);
    assert!(blocks[20].buy_czk.is_none() && blocks[20].recorded().is_none());
    let usable = blocks.iter().filter(|b| b.recorded().is_some()).count();
    // Load gaps 4-5, spot gap 20, master PV gap 30; partial PV blocks stay usable
    assert_eq!(usable, 96 - 2 - 1 - 1);
}

#[test]
fn power_mean_is_time_weighted() {
    let t0 = Utc.with_ymd_and_hms(2026, 10, 20, 10, 0, 0).unwrap();
    let timeline = Timeline::from_states(&[
        state(t0 - Duration::minutes(5), "1000"),
        state(t0 + Duration::minutes(5), "4000"),
    ]);
    // 5 min at 1000 W, 10 min at 4000 W
    let mean = timeline.mean(t0, t0 + Duration::minutes(15)).unwrap();
    assert!((mean - 3000.0).abs() < 1e-3, "{mean}");
    assert_eq!(timeline.value_at(t0), Some(1000.0));
    assert_eq!(timeline.value_at(t0 - Duration::minutes(6)), None);
}

// ---------- prices ----------

/// What the live planner computes for one block: `calculate_effective_prices` with the HDO
/// cache, and Winter Reserve's sell rule.
fn planner_prices(
    spot: f32,
    buy_sensor: Option<f32>,
    start: DateTime<Utc>,
    schedules: &[HdoDaySchedule],
    pricing: &PricingConfig,
) -> (f32, f32) {
    let cache = crate::resources::GlobalHdoCache::default();
    cache.set_timezone(PRAGUE);
    cache.update(schedules.to_vec());
    let mut blocks = [TimeBlockPrice {
        block_start: start,
        duration_minutes: 15,
        price_czk_per_kwh: spot,
        effective_price_czk_per_kwh: spot,
        spot_sell_price_czk_per_kwh: Some(spot - pricing.spot_sell_fee_czk),
        buy_price_czk_per_kwh: buy_sensor,
    }];
    crate::scheduling::calculate_effective_prices(&mut blocks, Some(&cache), pricing);
    (
        blocks[0].effective_price_czk_per_kwh,
        blocks[0].spot_sell_price_czk_per_kwh.unwrap(),
    )
}

fn loaded_block(start: DateTime<Utc>, spot: f32, buy_sensor: Option<f32>) -> LoadedBlock {
    LoadedBlock {
        start,
        spot_czk: Some(spot),
        buy_sensor_czk: buy_sensor,
        hdo_low: None,
        load_kwh: Some(0.2),
        pv_kwh: Some(0.0),
        pv_partial: false,
        soc_pct: Some(50.0),
        import_kwh: Some(0.2),
        export_kwh: Some(0.0),
        buy_czk: None,
        sell_czk: None,
    }
}

#[test]
fn prices_match_the_planner_for_spot_only_and_buy_sensor_sites() {
    let day = date(2026, 10, 20);
    let schedules = vec![HdoDaySchedule {
        date: day,
        low_tariff_ranges: vec![HdoTimeRange {
            start: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
        }],
    }];
    let low = local_midnight_utc(day, PRAGUE) + Duration::hours(1);
    let high = local_midnight_utc(day, PRAGUE) + Duration::hours(12);

    // Spot-only site: buy = spot + trade fee + HDO fee, sell = spot - trade fee
    let p = pricing(false);
    let mut blocks = vec![loaded_block(low, 2.0, None), loaded_block(high, 3.0, None)];
    apply_planner_prices(
        &mut blocks,
        &schedules,
        &p,
        &ControlConfig::default(),
        PRAGUE,
    );
    assert_eq!(blocks[0].hdo_low, Some(true));
    assert_eq!(blocks[1].hdo_low, Some(false));
    for (b, spot) in blocks.iter().zip([2.0, 3.0]) {
        let (buy, sell) = planner_prices(spot, None, b.start, &schedules, &p);
        assert!((b.buy_czk.unwrap() - buy).abs() < 1e-6);
        assert!((b.sell_czk.unwrap() - sell).abs() < 1e-6);
    }
    assert!((blocks[0].buy_czk.unwrap() - (2.0 + 0.4 + 0.5)).abs() < 1e-6);
    assert!((blocks[1].buy_czk.unwrap() - (3.0 + 0.4 + 1.8)).abs() < 1e-6);
    assert!((blocks[1].sell_czk.unwrap() - (3.0 - 0.3)).abs() < 1e-6);

    // Buy sensor site (spot x 1.21): the sensor replaces spot for buying, sell stays spot-based.
    // A block the sensor missed gets spot x the median buy/spot ratio, as live.
    let p = pricing(true);
    let mut blocks = vec![
        loaded_block(low, 2.0, Some(2.42)),
        loaded_block(high, 3.0, Some(3.63)),
        loaded_block(high + Duration::minutes(15), 4.0, None),
    ];
    apply_planner_prices(
        &mut blocks,
        &schedules,
        &p,
        &ControlConfig::default(),
        PRAGUE,
    );
    for (b, (spot, sensor)) in blocks.iter().zip([
        (2.0, Some(2.42)),
        (3.0, Some(3.63)),
        (4.0, Some(4.0 * 1.21)),
    ]) {
        let (buy, sell) = planner_prices(spot, sensor, b.start, &schedules, &p);
        assert!((b.buy_czk.unwrap() - buy).abs() < 1e-5, "{b:?} {buy}");
        assert!((b.sell_czk.unwrap() - sell).abs() < 1e-6);
    }
    assert!((blocks[0].buy_czk.unwrap() - (2.42 + 0.4 + 0.5)).abs() < 1e-5);
    assert!((blocks[0].sell_czk.unwrap() - (2.0 - 0.3)).abs() < 1e-6);

    // Unknown tariff (no schedule that day): high tariff, as the planner defaults
    let p = pricing(false);
    let other = local_midnight_utc(day + Duration::days(1), PRAGUE) + Duration::hours(1);
    let mut blocks = vec![loaded_block(other, 2.0, None)];
    apply_planner_prices(
        &mut blocks,
        &schedules,
        &p,
        &ControlConfig::default(),
        PRAGUE,
    );
    assert_eq!(blocks[0].hdo_low, None);
    assert!((blocks[0].buy_czk.unwrap() - (2.0 + 0.4 + 1.8)).abs() < 1e-6);
}

#[test]
fn hdo_schedule_comes_from_the_recorded_sensor_states() {
    let day = date(2026, 10, 20);
    let states = vec![hdo_state(
        local_midnight_utc(day, PRAGUE) - Duration::hours(1),
        day,
    )];
    let schedules = hdo_schedules_from_history(&states, PRAGUE);
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].date, day);
    // A state without attributes (minimal response) gives nothing
    let bare = vec![state(local_midnight_utc(day, PRAGUE), "on")];
    assert!(hdo_schedules_from_history(&bare, PRAGUE).is_empty());
}

// ---------- replay on the example fixture ----------

#[derive(serde::Deserialize)]
struct Fixture {
    site: FixtureSite,
    blocks: Vec<FixtureBlock>,
}

#[derive(serde::Deserialize)]
struct FixtureSite {
    timezone: String,
    battery: BatteryParams,
}

#[derive(serde::Deserialize)]
struct FixtureBlock {
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

fn fixture_rows() -> (Tz, BatteryParams, Vec<ReplayRow>) {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../tests/fixtures/backtest_replay_days.json"
    ))
    .unwrap();
    let mut rows: Vec<ReplayRow> = Vec::new();
    for b in &fixture.blocks {
        if b.missing {
            if let Some(last) = rows.last_mut() {
                last.gap_after = true;
            }
            continue;
        }
        rows.push(ReplayRow {
            block: RecordedBlock {
                start: b.start_utc.parse().unwrap(),
                buy_czk: b.buy.unwrap(),
                sell_czk: b.sell.unwrap(),
                load_kwh: b.load_kwh.unwrap(),
                pv_kwh: b.pv_kwh.unwrap(),
                import_kwh: b.import_kwh.unwrap(),
                export_kwh: b.export_kwh.unwrap(),
            },
            soc_pct: b.soc_pct.unwrap(),
            gap_after: false,
        });
    }
    (
        fixture.site.timezone.parse().unwrap(),
        fixture.site.battery,
        rows,
    )
}

/// Totals as the example prints them (skipped days left out).
fn printed_totals(run: &ReplayRun) -> String {
    let counted = || {
        run.days
            .iter()
            .filter(|(d, _)| !run.skipped_days.contains(d))
            .map(|(_, t)| t)
    };
    let actual: f32 = counted().map(|d| d.actual).sum();
    let su: f32 = counted().map(|d| d.su.cost).sum();
    let su_wear: f32 = counted().map(|d| d.su.wear_cost).sum();
    let wr: f32 = counted().map(|d| d.wr.cost).sum();
    let wr_wear: f32 = counted().map(|d| d.wr.wear_cost).sum();
    format!(
        "totals: actual {actual:.2} CZK | SelfUse {su:.2} CZK (wear {su_wear:.2}) | WinterReserve {wr:.2} CZK (wear {wr_wear:.2})"
    )
}

#[test]
fn replay_totals_equal_the_example_on_the_fixture() {
    // Expected lines are the `winter_reserve_replay` example's output on this fixture before the
    // replay moved into the library (default flags, and `--profile house`).
    let (tz, battery, rows) = fixture_rows();
    let options = ReplayOptions::new(tz, battery, WinterReserveParams::default());
    let run = replay(&rows, &options).unwrap();
    assert_eq!(
        printed_totals(&run),
        "totals: actual 196.09 CZK | SelfUse 195.61 CZK (wear 8.13) | WinterReserve 150.67 CZK (wear 13.21)"
    );
    assert_eq!(run.planned_blocks, 863);
    assert_eq!(run.fallback_blocks, 6151);
    assert_eq!(
        format!("{:.2}", run.end_soc_adjustment_czk(&battery)),
        "2.07"
    );

    let mut house = options;
    house.profile = ProfileSource::House;
    let run = replay(&rows, &house).unwrap();
    assert_eq!(
        printed_totals(&run),
        "totals: actual 196.09 CZK | SelfUse 195.61 CZK (wear 8.13) | WinterReserve 155.46 CZK (wear 12.62)"
    );
}

#[test]
fn replay_until_simulates_one_day_with_the_next_in_the_horizon() {
    let (tz, battery, rows) = fixture_rows();
    let day = date(2026, 10, 24);
    let mut options = ReplayOptions::new(tz, battery, WinterReserveParams::default());
    options.from = Some(local_midnight_utc(day, tz));
    options.until = Some(local_midnight_utc(day + Duration::days(1), tz));
    let run = replay(&rows, &options).unwrap();
    assert_eq!(run.days.len(), 1);
    assert!(run.blocks.iter().all(|b| b.local.date_naive() == day));
    assert_eq!(run.blocks.len(), 96);
    let day_buys: Vec<f32> = rows
        .iter()
        .filter(|r| r.block.start.with_timezone(&tz).date_naive() == day)
        .map(|r| r.block.buy_czk)
        .collect();
    let expected = crate::strategy::winter_reserve::cheap_reference(&day_buys, 0.2);
    assert!((run.cheap_ref_last_day - expected).abs() < 1e-6);
}

#[tokio::test]
async fn back_up_floor_holds_both_simulated_runs() {
    // Constant day: load 1000 W, PV 600 W, no export. Self-use discharges the 0.1 kWh deficit
    // of every block from 50 %; with a 40 % Back Up floor neither run may go below 40 %.
    let day = date(2026, 10, 20);
    let rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    let site = site(false, true);
    let now = local_midnight_utc(day + Duration::days(3), PRAGUE);
    let loaded = load_day(&rec, &site, day, now).await.unwrap();
    let min = |series: &[Option<f32>]| series.iter().flatten().copied().fold(f32::MAX, f32::min);

    let free = build_report(&loaded, &site, None).unwrap();
    assert!((free.floor.min_soc_pct - site.control.min_battery_soc).abs() < 1e-6);
    assert!(
        min(&free.self_use.soc_pct) < 39.0,
        "{}",
        min(&free.self_use.soc_pct)
    );

    let floor = crate::scheduling::backup_planner_floor(
        Some(&fluxion_types::UserControlState {
            default_mode: fluxion_types::DefaultMode::Backup,
            backup_min_soc_pct: 40,
            ..Default::default()
        }),
        site.control.min_battery_soc,
        site.control.max_battery_soc,
    );
    assert_eq!(floor, Some(40.0));
    let backup = build_report(&loaded, &site, floor).unwrap();
    assert_eq!(backup.floor.source, "back_up_default_mode");
    assert!((backup.floor.min_soc_pct - 40.0).abs() < 1e-6);
    assert!(min(&backup.self_use.soc_pct) >= 40.0 - 1e-3);
    assert!(min(&backup.winter_reserve.soc_pct) >= 40.0 - 1e-3);
    assert!(backup.totals_self_use.soc_end_pct >= 40.0 - 1e-3);
    assert!(backup.notes.iter().any(|n| n.contains("Back Up")));
    // Holding the floor costs self-use grid energy it would otherwise take from the battery
    assert!(backup.totals_self_use.cost_czk > free.totals_self_use.cost_czk);
}

#[tokio::test]
async fn hdo_entity_is_resolved_like_live_before_reading_its_history() {
    let day = date(2026, 10, 20);
    let now = local_midnight_utc(day + Duration::days(3), PRAGUE);
    let site = site(false, false);

    // Configured entity missing, a renamed sensor found by content: its history is read
    let mut rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    let schedule = rec.states.remove("sensor.hdo_schedule").unwrap();
    rec.states.insert("sensor.hdo_renamed".to_owned(), schedule);
    rec.hdo_resolution.insert(
        "sensor.hdo_schedule".to_owned(),
        Some("sensor.hdo_renamed".to_owned()),
    );
    let loaded = load_day(&rec, &site, day, now).await.unwrap();
    assert_eq!(loaded.hdo_entity.as_deref(), Some("sensor.hdo_renamed"));
    assert!(loaded.hdo_from_sensor);
    assert!(
        rec.calls
            .lock()
            .unwrap()
            .contains(&("sensor.hdo_renamed".to_owned(), true))
    );
    let report = build_report(&loaded, &site, None).unwrap();
    assert!(report.data.hdo_source.contains("sensor.hdo_renamed"));

    // No HDO entity at all: no attribute read, high tariff, said so
    let mut rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    rec.hdo_resolution
        .insert("sensor.hdo_schedule".to_owned(), None);
    let loaded = load_day(&rec, &site, day, now).await.unwrap();
    assert_eq!(loaded.hdo_entity, None);
    assert!(rec.calls.lock().unwrap().iter().all(|(_, attrs)| !attrs));
    let report = build_report(&loaded, &site, None).unwrap();
    assert_eq!(report.data.hdo_source, "none found (high tariff)");
    assert_eq!(report.data.missing_per_field.get(&Field::Hdo), Some(&96));
}

#[tokio::test]
async fn failed_hdo_lookup_prices_the_day_at_the_high_tariff_and_says_so() {
    let day = date(2026, 10, 20);
    let now = local_midnight_utc(day + Duration::days(3), PRAGUE);
    let site = site(false, false);
    let mut rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    rec.hdo_lookup_error = Some(HistoryError::Timeout);

    // The day loads (no 502) without reading the HDO history
    let loaded = load_day(&rec, &site, day, now).await.unwrap();
    assert_eq!(loaded.hdo_lookup_error, Some(HistoryError::Timeout));
    assert_eq!(loaded.hdo_entity, None);
    assert!(!loaded.hdo_from_sensor);
    assert!(rec.calls.lock().unwrap().iter().all(|(_, attrs)| !attrs));

    // Every block at the high tariff, as live without HDO data; counted like no schedule
    for block in loaded.day_blocks() {
        assert_eq!(block.hdo_low, None);
    }
    let report = build_report(&loaded, &site, None).unwrap();
    assert_eq!(report.data.hdo_source, "lookup failed (high tariff)");
    assert_eq!(report.data.missing_per_field.get(&Field::Hdo), Some(&96));
    assert_eq!(report.data.usable_blocks, 96);
    let note = report
        .notes
        .iter()
        .find(|n| n.contains("could not be looked up"))
        .expect("note");
    assert!(
        note.contains("high tariff") && note.contains("timed out"),
        "{note}"
    );

    // Same prices as a day whose sensor recorded no schedule
    let no_schedule = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[]);
    let reference = load_day(&no_schedule, &site, day, now).await.unwrap();
    let prices = |d: &LoadedDay| d.day_blocks().map(|b| b.buy_czk).collect::<Vec<_>>();
    assert!(prices(&loaded).iter().all(Option::is_some));
    assert_eq!(prices(&loaded), prices(&reference));
    assert_eq!(
        build_report(&reference, &site, None)
            .unwrap()
            .data
            .hdo_source,
        "none (high tariff)"
    );
}

#[tokio::test]
async fn out_of_range_dates_are_refused_without_panicking() {
    let rec = FakeRecorder::default();
    let now = Utc.with_ymd_and_hms(2026, 10, 23, 12, 0, 0).unwrap();
    let site = site(false, false);
    for bad in [
        NaiveDate::MIN,
        NaiveDate::from_ymd_opt(-262143, 1, 1).unwrap(),
        date(2019, 12, 31),
        date(2026, 10, 25),
        NaiveDate::MAX,
    ] {
        assert!(!date_in_range(bad, now), "{bad}");
        assert_eq!(
            load_day(&rec, &site, bad, now).await.unwrap_err(),
            LoadError::OutOfRange,
            "{bad}"
        );
    }
    assert!(date_in_range(date(2020, 1, 1), now));
    assert!(date_in_range(date(2026, 10, 24), now));
    // Tomorrow is in range but not finished
    assert_eq!(
        load_day(&rec, &site, date(2026, 10, 24), now)
            .await
            .unwrap_err(),
        LoadError::NotFinished
    );
}

#[tokio::test]
async fn horizon_cut_at_now_is_flagged() {
    let day = date(2026, 10, 20);
    let rec = constant_recorder(local_midnight_utc(day - Duration::days(8), PRAGUE), &[day]);
    let site = site(false, false);
    // Now inside the next day: the planner horizon (until the end of the 21st) is cut
    let cut = local_midnight_utc(day + Duration::days(1), PRAGUE) + Duration::hours(15);
    assert!(
        !load_day(&rec, &site, day, cut)
            .await
            .unwrap()
            .horizon_complete
    );
    let full = local_midnight_utc(day + Duration::days(2), PRAGUE);
    assert!(
        load_day(&rec, &site, day, full)
            .await
            .unwrap()
            .horizon_complete
    );
}
