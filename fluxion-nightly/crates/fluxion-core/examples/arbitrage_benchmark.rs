// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Reproducible arbitrage & TCO benchmark for the `WinterReserve` planner.
//!
//! Covers:
//! 1. Replay fixture `crates/fluxion-core/tests/fixtures/backtest_replay_days.json` (17–27 Oct)
//!    and, when given, a recorded owner replay (`--owner-replay <path>` or the
//!    `FLUXION_BENCH_OWNER_REPLAY` environment variable; skipped otherwise).
//! 2. Multi-day closed-loop synthetic scenarios (replan every 15 min, horizon growing at 13:00
//!    local when tomorrow's prices arrive, `BatteryCostBasis` tracking) on:
//!    - Owner-like site: 23 kWh, 10 kW charge/discharge, 9 kW export cap, fixed buy 3.22 low (19 h)
//!      / 4.15 high (07–09, 16–19), spot sell, `min_arbitrage_profit_czk = 1.5`,
//!      `export_floor_until_cheap = true`, with and without 4 h 25 % morning/evening discharge
//!      window levers.
//!    - Jakub-like site: 96 kWh, 30 kW charge/discharge, 27.5 kW export cap,
//!      buy = spot × 1.21 + HDO (0.36 low / 1.0 high) + 0.5 trade fee, sell = spot − 0.5,
//!      with `force_discharge_enabled = true` and `false`.
//!    - The owner-like site also runs with `force_discharge_enabled = false`.
//!
//!    Across four price day shapes:
//!    - Two peaks/day (morning + evening peak)
//!    - Flat days
//!    - Negative-price days (negative midday spot, negative sell, cheap/negative buy)
//!    - Missing tomorrow prices: on 1 day in 6 tomorrow's prices never arrive (frozen horizon)
//!
//!    Forecast noise (default on): the planner sees the forecast, the battery and the grid see the
//!    actual flows. Actual PV = forecast × a per-day lognormal factor (weather), actual load =
//!    forecast × a per-block lognormal factor, both with σ = 0.25 and mean 1, from a fixed seed per
//!    day shape. `--perfect-foresight` makes actual = forecast.
//!
//! Run from `fluxion/`:
//! `cargo run --release -p fluxion-core --example arbitrage_benchmark -- [--perfect-foresight]
//!  [--seed <n>] [--owner-replay <path>]`

use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Timelike, Utc};
use chrono_tz::Europe::Prague;
use chrono_tz::Tz;
use fluxion_core::backtest::{ProfileSource, ReplayOptions, ReplayRow, replay};
use fluxion_core::battery_cost_basis::{
    BatteryCostBasis, CostBasisBattery, CostBasisStep, one_way_efficiency,
};
use fluxion_core::savings::RecordedBlock;
use fluxion_core::strategy::winter_reserve::{
    BatteryParams, BlockOutcome, PlanBlock, PlanMode, WinterReserveParams, WinterReserveStrategy,
    min_charge_kwh, simulate_block,
};
use fluxion_core::strategy::{EconomicStrategy, EvaluationContext};
use fluxion_plugins::protocol::BlockForecast;
use fluxion_types::config::ControlConfig;
use fluxion_types::inverter::InverterOperationMode;
use fluxion_types::pricing::TimeBlockPrice;
use fluxion_types::{CautiousPvSignal, DischargeWindowLimit, UserLevers};
use serde::Deserialize;

#[derive(Deserialize)]
struct Dataset {
    site: Site,
    blocks: Vec<RawBlock>,
}

#[derive(Deserialize)]
struct Site {
    timezone: String,
    battery: BatteryParams,
}

#[derive(Deserialize)]
struct RawBlock {
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

#[derive(Debug, Clone, Copy, Default)]
struct BenchMetrics {
    days: f32,
    su_adj_czk: f32,
    import_kwh: f32,
    export_kwh: f32,
    import_czk: f32,
    export_czk: f32,
    wear_czk: f32,
    raw_net_czk: f32,
    adj_net_czk: f32,
    gain_vs_su_czk: f32,
    charged_kwh: f32,
    discharged_kwh: f32,
    throughput_kwh: f32,
    cycles_per_day: f32,
    mode_flips: u32,
    flips_per_day: f32,
    self_use_blocks: u32,
    charge_blocks: u32,
    hold_blocks: u32,
    export_blocks: u32,
    end_soc_pct: f32,
}

#[derive(Debug, Clone, Copy)]
enum SiteKind {
    OwnerFixedBuy {
        with_window_levers: bool,
        force_discharge: bool,
    },
    JakubSpotBuy {
        force_discharge: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum DayShape {
    TwoPeaks,
    Flat,
    NegativePrices,
    MissingTomorrow,
}

impl DayShape {
    fn label(self) -> &'static str {
        match self {
            Self::TwoPeaks => "two_peaks",
            Self::Flat => "flat",
            Self::NegativePrices => "negative_prices",
            Self::MissingTomorrow => "missing_tomorrow",
        }
    }
}

impl SiteKind {
    fn label(self) -> &'static str {
        match self {
            Self::OwnerFixedBuy {
                with_window_levers: false,
                force_discharge: true,
            } => "Owner (23kWh fixed-buy, FD=on, no-levers)",
            Self::OwnerFixedBuy {
                with_window_levers: true,
                force_discharge: true,
            } => "Owner (23kWh fixed-buy, FD=on, 4h-25%-levers)",
            Self::OwnerFixedBuy {
                with_window_levers: false,
                force_discharge: false,
            } => "Owner (23kWh fixed-buy, FD=off, no-levers)",
            Self::OwnerFixedBuy {
                with_window_levers: true,
                force_discharge: false,
            } => "Owner (23kWh fixed-buy, FD=off, 4h-25%-levers)",
            Self::JakubSpotBuy {
                force_discharge: true,
            } => "Jakub (96kWh spot-buy, FD=on)",
            Self::JakubSpotBuy {
                force_discharge: false,
            } => "Jakub (96kWh spot-buy, FD=off)",
        }
    }

    fn control_config(self) -> ControlConfig {
        match self {
            Self::OwnerFixedBuy {
                force_discharge, ..
            } => ControlConfig {
                battery_capacity_kwh: 23.0,
                min_battery_soc: 10.0,
                max_battery_soc: 100.0,
                max_battery_charge_rate_kw: 10.0,
                battery_efficiency: 0.90,
                battery_wear_cost_czk_per_kwh: 0.125,
                maximum_export_power_w: 9_000,
                grid_export_fee_czk_per_kwh: 0.50,
                min_mode_change_interval_secs: 300,
                force_discharge_enabled: force_discharge,
                hold_pv_to_battery: false,
                ..ControlConfig::default()
            },
            Self::JakubSpotBuy { force_discharge } => ControlConfig {
                battery_capacity_kwh: 96.0,
                min_battery_soc: 10.0,
                max_battery_soc: 100.0,
                max_battery_charge_rate_kw: 30.0,
                battery_efficiency: 0.90,
                battery_wear_cost_czk_per_kwh: 0.125,
                maximum_export_power_w: 27_500,
                grid_export_fee_czk_per_kwh: 0.50,
                min_mode_change_interval_secs: 300,
                force_discharge_enabled: force_discharge,
                hold_pv_to_battery: false,
                ..ControlConfig::default()
            },
        }
    }

    fn wr_params(self) -> WinterReserveParams {
        match self {
            Self::OwnerFixedBuy { .. } => WinterReserveParams {
                min_arbitrage_profit_czk: 1.5,
                export_floor_until_cheap: true,
                ..WinterReserveParams::default()
            },
            Self::JakubSpotBuy { .. } => WinterReserveParams {
                min_arbitrage_profit_czk: 2.0,
                export_floor_until_cheap: true,
                ..WinterReserveParams::default()
            },
        }
    }

    fn levers(self) -> Option<UserLevers> {
        match self {
            Self::OwnerFixedBuy {
                with_window_levers: true,
                ..
            } => Some(UserLevers {
                morning_discharge: Some(DischargeWindowLimit {
                    hours: 4,
                    min_soc_pct: 25.0,
                }),
                evening_discharge: Some(DischargeWindowLimit {
                    hours: 4,
                    min_soc_pct: 25.0,
                }),
                ..UserLevers::default()
            }),
            _ => None,
        }
    }
}

/// Spot price (CZK/kWh before VAT/fees) for a given day index (0..days), local hour, and shape.
fn spot_price_czk(day: usize, hour: u32, quarter: u32, shape: DayShape) -> f32 {
    let sub = (quarter as f32 - 1.5) * 0.04;
    let day_mod = (day % 3) as f32 * 0.15;
    match shape {
        DayShape::TwoPeaks | DayShape::MissingTomorrow => {
            let base = match hour {
                0..=1 => 2.10,
                2..=4 => 1.75,
                5 => 2.30,
                6 => 4.20,
                7 => 6.80,
                8 => 7.40,
                9 => 5.60,
                10..=11 => 3.40,
                12..=14 => 2.60,
                15 => 3.50,
                16 => 5.20,
                17 => 8.10,
                18 => 9.00,
                19 => 8.40,
                20 => 6.20,
                21 => 4.10,
                _ => 2.60,
            };
            base + day_mod + sub
        }
        DayShape::Flat => 3.20 + sub * 0.5,
        DayShape::NegativePrices => {
            let base = match hour {
                0..=4 => 1.50,
                5..=6 => 2.80,
                7..=8 => 6.20,
                9 => 2.50,
                10 => 0.20,
                11 => -0.80,
                12..=13 => -1.60,
                14 => -0.60,
                15 => 1.20,
                16 => 4.50,
                17..=19 => 7.80,
                20 => 5.40,
                _ => 2.20,
            };
            base + sub
        }
    }
}

/// Effective buy and sell prices (CZK/kWh) for the site and spot price.
fn site_prices(site: SiteKind, hour: u32, spot: f32, shape: DayShape) -> (f32, f32) {
    let high_hdo = (7..9).contains(&hour) || (16..19).contains(&hour);
    match site {
        SiteKind::OwnerFixedBuy { .. } => {
            let buy = match shape {
                DayShape::Flat => 3.22,
                _ => {
                    if high_hdo {
                        4.15
                    } else {
                        3.22
                    }
                }
            };
            let sell = match shape {
                DayShape::Flat => 2.50,
                _ => spot - 0.35,
            };
            (buy, sell)
        }
        SiteKind::JakubSpotBuy { .. } => {
            let buy = match shape {
                DayShape::Flat => 4.80,
                _ => {
                    let hdo = if high_hdo { 1.00 } else { 0.36 };
                    spot * 1.21 + hdo + 0.50
                }
            };
            let sell = match shape {
                DayShape::Flat => 2.70,
                _ => spot - 0.50,
            };
            (buy, sell)
        }
    }
}

/// Expected PV and household load (kWh per 15-min block) for the site and day shape.
fn site_flows(site: SiteKind, day: usize, hour: u32, shape: DayShape) -> (f32, f32) {
    let scale = match site {
        SiteKind::OwnerFixedBuy { .. } => 1.0,
        SiteKind::JakubSpotBuy { .. } => 3.2,
    };
    let load_base = match hour {
        0..=5 => 0.25,
        6..=8 => 0.50,
        9..=15 => 0.32,
        16..=20 => 0.55,
        _ => 0.30,
    };
    let pv_peak = match shape {
        DayShape::NegativePrices => 1.20,
        DayShape::Flat => 0.25,
        _ => {
            if day.is_multiple_of(2) {
                0.55
            } else {
                0.20
            }
        }
    };
    let pv_base = if (9..16).contains(&hour) {
        let phase = (hour as f32 - 9.0 + 0.5) / 7.0 * std::f32::consts::PI;
        pv_peak * phase.sin().max(0.0)
    } else {
        0.0
    };
    (load_base * scale, pv_base * scale)
}

/// Lognormal forecast error σ (actual = forecast × factor, mean factor 1)
const FORECAST_SIGMA: f32 = 0.25;

/// Seeded `SplitMix64`, so every run (and every commit) sees the same forecast errors.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1)
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box–Muller)
    fn normal(&mut self) -> f32 {
        let (u1, u2) = (self.unit(), self.unit());
        ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
    }

    /// Lognormal factor with mean 1: exp(σ·z − σ²/2)
    fn lognormal(&mut self, sigma: f32) -> f32 {
        (sigma * self.normal() - sigma * sigma / 2.0).exp()
    }
}

/// Default base of the noise seeds (`--seed` changes it)
const DEFAULT_SEED: u64 = 0x5EED_0000;

/// Fixed noise seed per day shape from `base` (the same errors for every site and every commit)
fn noise_seed(base: u64, shape: DayShape) -> u64 {
    base + match shape {
        DayShape::TwoPeaks => 1,
        DayShape::Flat => 2,
        DayShape::NegativePrices => 3,
        DayShape::MissingTomorrow => 4,
    }
}

/// What the inverter does in `mode` with the actual flows of `actual`, from `energy` stored.
/// SelfUse/Hold/Backup follow the actual house and PV. ForceCharge and ForceDischarge run the
/// planned battery power, limited by the actual SOC; the grid takes the rest.
fn realize(
    mode: InverterOperationMode,
    planned_charge_kwh: f32,
    planned_discharge_kwh: f32,
    energy: f32,
    actual: &PlanBlock,
    bat: &BatteryParams,
) -> BlockOutcome {
    let emin = bat.min_soc_pct / 100.0 * bat.capacity_kwh;
    let emax = bat.max_soc_pct / 100.0 * bat.capacity_kwh;
    let one_way = one_way_efficiency(bat.efficiency);
    let xmax = bat.max_export_kw * actual.duration_minutes as f32 / 60.0;
    let (load, pv) = (actual.load_kwh, actual.pv_kwh.max(0.0));
    let mut o = BlockOutcome::default();
    let balance = match mode {
        InverterOperationMode::ForceCharge => {
            let c = planned_charge_kwh.min(((emax - energy) / one_way).max(0.0));
            o.battery_charge_kwh = c;
            o.energy_end_kwh = energy + c * one_way;
            load + c - pv
        }
        InverterOperationMode::ForceDischarge => {
            let q = planned_discharge_kwh
                .min((energy - emin).max(0.0) * one_way)
                .min(xmax + (load - pv).max(0.0));
            o.battery_discharge_kwh = q;
            o.energy_end_kwh = energy - q / one_way;
            load - pv - q
        }
        other => {
            let plan_mode = match other {
                InverterOperationMode::NoChargeNoDischarge => PlanMode::Hold,
                InverterOperationMode::BackUpMode => PlanMode::NoDischarge,
                _ => PlanMode::SelfUse,
            };
            return simulate_block(plan_mode, energy, actual, emin, bat);
        }
    };
    if balance >= 0.0 {
        o.grid_import_kwh = balance;
    } else {
        o.grid_export_kwh = (-balance).min(xmax);
        o.curtailed_kwh = -balance - o.grid_export_kwh;
    }
    o
}

/// Run a multi-day closed-loop simulation using `WinterReserveStrategy::evaluate` (testing the
/// full wrapper, horizon slicing at 13:00 local, levers, cost-basis tracking, and runtime guards).
fn run_synthetic_closed_loop(
    site: SiteKind,
    shape: DayShape,
    num_days: usize,
    perfect_foresight: bool,
    seed: u64,
) -> BenchMetrics {
    let ctl = site.control_config();
    let wr_params = site.wr_params();
    let levers = site.levers();
    let total_blocks = (num_days + 1) * 96; // +1 day so the last simulated day has tomorrow's prices after 13:00
    let sim_blocks = num_days * 96;

    // Start at 2026-10-12 00:00 Europe/Prague (CEST)
    let start_local = Prague.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap();
    let start_utc = start_local.with_timezone(&Utc);

    let mut all_prices = Vec::with_capacity(total_blocks);
    let mut all_forecasts = Vec::with_capacity(total_blocks);
    let mut all_actuals: Vec<(f32, f32)> = Vec::with_capacity(total_blocks);
    let mut rng = Rng(noise_seed(seed, shape));
    let mut pv_day_factor = 1.0_f32;
    let mut hourly_sum = [0.0_f32; 24];
    let mut hourly_cnt = [0usize; 24];

    for i in 0..total_blocks {
        let block_start = start_utc + Duration::minutes(15 * i as i64);
        let local = block_start.with_timezone(&Prague);
        let day = i / 96;
        let hour = local.hour();
        let quarter = local.minute() / 15;
        let spot = spot_price_czk(day, hour, quarter, shape);
        let (buy, sell) = site_prices(site, hour, spot, shape);
        let (load_kwh, pv_kwh) = site_flows(site, day, hour, shape);
        // Draw every factor even with perfect foresight, so both runs share one sequence.
        if i % 96 == 0 {
            pv_day_factor = rng.lognormal(FORECAST_SIGMA);
        }
        let load_factor = rng.lognormal(FORECAST_SIGMA);
        all_actuals.push(if perfect_foresight {
            (load_kwh, pv_kwh)
        } else {
            (load_kwh * load_factor, pv_kwh * pv_day_factor)
        });

        let utc_hour = block_start.hour() as usize;
        hourly_sum[utc_hour] += load_kwh * 4.0;
        hourly_cnt[utc_hour] += 1;

        all_prices.push(TimeBlockPrice {
            buy_price_czk_per_kwh: Some(buy),
            block_start,
            duration_minutes: 15,
            price_czk_per_kwh: spot,
            effective_price_czk_per_kwh: buy,
            spot_sell_price_czk_per_kwh: Some(sell),
        });
        all_forecasts.push(BlockForecast {
            solar_kwh: pv_kwh,
            consumption_kwh: load_kwh,
        });
    }

    let mut mean_profile = [0.0_f32; 24];
    for h in 0..24 {
        if hourly_cnt[h] > 0 {
            mean_profile[h] = hourly_sum[h] / hourly_cnt[h] as f32;
        }
    }

    let bat = BatteryParams {
        capacity_kwh: ctl.battery_capacity_kwh,
        min_soc_pct: ctl.min_battery_soc,
        max_soc_pct: ctl.max_battery_soc,
        max_charge_kw: ctl.max_battery_charge_rate_kw,
        max_discharge_kw: ctl.max_battery_charge_rate_kw,
        efficiency: ctl.battery_efficiency,
        wear_cost_czk_per_kwh: ctl.battery_wear_cost_czk_per_kwh,
        max_export_kw: ctl.maximum_export_power_w as f32 / 1000.0,
        export_guard_margin_kwh: (ctl.max_battery_charge_rate_kw
            * ctl.min_mode_change_interval_secs as f32
            / 3600.0)
            .max(0.5),
        force_discharge_enabled: ctl.force_discharge_enabled,
        hold_pv_to_battery: ctl.hold_pv_to_battery,
        min_charge_kwh: min_charge_kwh(
            ctl.max_battery_charge_rate_kw,
            ctl.min_mode_change_interval_secs,
        ),
        charge_cost_czk_per_kwh: None,
    };

    let cb_bat = CostBasisBattery {
        capacity_kwh: bat.capacity_kwh,
        min_soc_pct: bat.min_soc_pct,
        max_soc_pct: bat.max_soc_pct,
        efficiency: bat.efficiency,
        wear_czk_per_kwh: bat.wear_cost_czk_per_kwh,
    };

    let start_soc_pct = 50.0_f32;
    let start_energy = start_soc_pct / 100.0 * bat.capacity_kwh;
    let mut energy = start_energy;
    let mut su_energy = start_energy;
    let mut su_raw_czk = 0.0_f32;
    let mut cost_basis = BatteryCostBasis {
        usable_kwh: cb_bat.usable_kwh(start_soc_pct),
        grid_kwh: cb_bat.usable_kwh(start_soc_pct),
        unknown_until_reset: true,
        ..BatteryCostBasis::default()
    };
    let mut prev_mode: Option<InverterOperationMode> = None;

    let mut m = BenchMetrics {
        days: num_days as f32,
        ..BenchMetrics::default()
    };

    for i in 0..sim_blocks {
        let block_start = all_prices[i].block_start;
        let local = block_start.with_timezone(&Prague);
        let day_start_idx = (i / 96) * 96;

        // Horizon rules:
        // Before 13:00 local (or on even days in MissingTomorrow): horizon ends at 23:45 today.
        // From 13:00 local: tomorrow's prices are known (horizon ends at 23:45 tomorrow).
        let tomorrow_known = match shape {
            // Tomorrow's prices never arrive on 1 day in 6
            DayShape::MissingTomorrow => local.hour() >= 13 && (i / 96) % 6 != 2,
            _ => local.hour() >= 13,
        };
        let end_idx = if tomorrow_known {
            (day_start_idx + 192).min(total_blocks)
        } else {
            (day_start_idx + 96).min(total_blocks)
        };

        let slice_prices = &all_prices[day_start_idx..end_idx];
        let slice_forecasts = &all_forecasts[day_start_idx..end_idx];
        let in_slice_idx = i - day_start_idx;

        let soc_now = energy / bat.capacity_kwh * 100.0;
        let charge_cost = cost_basis.cost_czk_per_kwh;

        static CLOCK_SECS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        fn sim_clock() -> DateTime<Utc> {
            DateTime::from_timestamp(CLOCK_SECS.load(std::sync::atomic::Ordering::Relaxed), 0)
                .unwrap()
        }
        CLOCK_SECS.store(
            (block_start + Duration::minutes(1)).timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );

        thread_local! {
            static STRAT: std::cell::RefCell<Option<WinterReserveStrategy>> = const { std::cell::RefCell::new(None) };
        }
        if i == 0 {
            STRAT.with(|cell| {
                *cell.borrow_mut() =
                    Some(WinterReserveStrategy::new(true, wr_params.clone()).with_clock(sim_clock));
            });
        }

        let ctx = EvaluationContext {
            price_block: &slice_prices[in_slice_idx],
            control_config: &ctl,
            current_battery_soc: soc_now,
            solar_forecast_kwh: slice_forecasts[in_slice_idx].solar_kwh,
            consumption_forecast_kwh: slice_forecasts[in_slice_idx].consumption_kwh,
            grid_export_price_czk_per_kwh: ctl.grid_export_fee_czk_per_kwh,
            all_price_blocks: Some(slice_prices),
            ranking_price_blocks: None,
            backup_discharge_min_soc: ctl.min_battery_soc,
            grid_import_today_kwh: None,
            consumption_today_kwh: None,
            solar_forecast_total_today_kwh: 0.0,
            solar_forecast_remaining_today_kwh: 0.0,
            solar_forecast_tomorrow_kwh: 0.0,
            battery_avg_charge_price_czk_per_kwh: 3.0,
            battery_charge_cost_czk_per_kwh: charge_cost,
            hourly_consumption_profile: Some(&mean_profile),
            hourly_consumption_p75: None,
            block_forecasts: Some(slice_forecasts),
            timezone: Some(Prague),
            user_levers: levers.as_ref(),
            cautious_pv: Some(CautiousPvSignal {
                mae_pct: 10.0,
                cautious: false,
            }),
        };

        let eval = STRAT.with(|cell| cell.borrow().as_ref().unwrap().evaluate(&ctx));
        let mode = eval.mode;
        if prev_mode.is_some_and(|prev| prev != mode) {
            m.mode_flips += 1;
        }
        prev_mode = Some(mode);

        match mode {
            InverterOperationMode::SelfUse => m.self_use_blocks += 1,
            InverterOperationMode::ForceCharge => m.charge_blocks += 1,
            InverterOperationMode::NoChargeNoDischarge | InverterOperationMode::BackUpMode => {
                m.hold_blocks += 1;
            }
            InverterOperationMode::ForceDischarge => m.export_blocks += 1,
            _ => {}
        }

        let buy = slice_prices[in_slice_idx].effective_price_czk_per_kwh;
        let sell = slice_prices[in_slice_idx]
            .spot_sell_price_czk_per_kwh
            .unwrap_or(0.0);
        let (load, pv) = all_actuals[i];

        let su_block = PlanBlock {
            start: block_start,
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
        };
        let su_out = simulate_block(
            PlanMode::SelfUse,
            su_energy,
            &su_block,
            bat.min_soc_pct / 100.0 * bat.capacity_kwh,
            &bat,
        );
        su_raw_czk += su_out.grid_import_kwh * buy - su_out.grid_export_kwh * sell
            + su_out.battery_discharge_kwh * bat.wear_cost_czk_per_kwh;
        su_energy = su_out.energy_end_kwh;

        let flows = realize(
            mode,
            eval.energy_flows.battery_charge_kwh,
            eval.energy_flows.battery_discharge_kwh,
            energy,
            &su_block,
            &bat,
        );
        let energy_after = flows.energy_end_kwh.clamp(0.0, bat.capacity_kwh);
        let soc_after = energy_after / bat.capacity_kwh * 100.0;

        cost_basis.update(
            &CostBasisStep {
                soc_pct: soc_after,
                battery_charge_kwh: flows.battery_charge_kwh,
                grid_import_kwh: flows.grid_import_kwh,
                export_headroom_kwh: Some(
                    (bat.max_export_kw * 0.25 - flows.grid_export_kwh).max(0.0),
                ),
                sell_czk: sell,
                buy_czk: buy,
                at: block_start + Duration::minutes(15),
            },
            &cb_bat,
        );

        energy = energy_after;
        m.import_kwh += flows.grid_import_kwh;
        m.export_kwh += flows.grid_export_kwh;
        m.import_czk += flows.grid_import_kwh * buy;
        m.export_czk += flows.grid_export_kwh * sell;
        m.wear_czk += flows.battery_discharge_kwh * bat.wear_cost_czk_per_kwh;
        m.charged_kwh += flows.battery_charge_kwh;
        m.discharged_kwh += flows.battery_discharge_kwh;
    }

    m.raw_net_czk = m.import_czk - m.export_czk + m.wear_czk;
    let mut buys: Vec<f32> = all_prices[..sim_blocks]
        .iter()
        .map(|p| p.effective_price_czk_per_kwh)
        .collect();
    buys.sort_by(f32::total_cmp);
    let n_cheap = ((0.20 * buys.len() as f64).ceil() as usize).clamp(1, buys.len());
    let cheap_ref: f32 = buys[..n_cheap].iter().sum::<f32>() / n_cheap as f32;
    let terminal_val = (cheap_ref - bat.wear_cost_czk_per_kwh).max(0.0);
    m.adj_net_czk = m.raw_net_czk - (energy - start_energy) * terminal_val;
    m.su_adj_czk = su_raw_czk - (su_energy - start_energy) * terminal_val;
    m.gain_vs_su_czk = m.su_adj_czk - m.adj_net_czk;
    m.throughput_kwh = (m.charged_kwh + m.discharged_kwh) * 0.5;
    let usable_cap = (bat.max_soc_pct - bat.min_soc_pct) / 100.0 * bat.capacity_kwh;
    m.cycles_per_day = m.discharged_kwh / usable_cap.max(1e-6) / m.days;
    m.flips_per_day = m.mode_flips as f32 / m.days;
    m.end_soc_pct = energy / bat.capacity_kwh * 100.0;
    m
}

fn run_replay_file(
    label: &str,
    path: &Path,
    force_discharge: bool,
    export_floor_until_cheap: bool,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("Skipping {label} ({} not found)", path.display());
        return;
    };
    let dataset: Dataset = serde_json::from_str(&text).expect("valid replay json");
    let tz: Tz = dataset.site.timezone.parse().expect("valid timezone");
    let mut battery = dataset.site.battery;
    battery.force_discharge_enabled |= force_discharge;
    if battery.min_charge_kwh <= 0.0 {
        battery.min_charge_kwh = min_charge_kwh(
            battery.max_charge_kw,
            ControlConfig::default().min_mode_change_interval_secs,
        );
    }

    let mut rows: Vec<ReplayRow> = Vec::new();
    for raw in &dataset.blocks {
        if raw.missing {
            if let Some(last) = rows.last_mut() {
                last.gap_after = true;
            }
            continue;
        }
        let start: DateTime<Utc> = raw.start_utc.parse().expect("valid start_utc");
        let (Some(buy), Some(sell), Some(load), Some(pv), Some(soc), Some(import), Some(export)) = (
            raw.buy,
            raw.sell,
            raw.load_kwh,
            raw.pv_kwh,
            raw.soc_pct,
            raw.import_kwh,
            raw.export_kwh,
        ) else {
            continue;
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

    let params = WinterReserveParams {
        export_floor_until_cheap,
        ..WinterReserveParams::default()
    };
    let mut options = ReplayOptions::new(tz, battery, params);
    options.profile = ProfileSource::Forecast;
    options.track_cost_basis = true;
    let run = replay(&rows, &options).expect("replay succeeds");
    let counted = || {
        run.days
            .iter()
            .filter(|(day, _)| !run.skipped_days.contains(day))
            .map(|(_, d)| d)
    };
    let n_days = counted().count().max(1) as f32;
    let total_su: f32 = counted().map(|d| d.su.cost).sum();
    let total_wr: f32 = counted().map(|d| d.wr.cost).sum();
    let total_wr_wear: f32 = counted().map(|d| d.wr.wear_cost).sum();
    let total_wr_adj = total_wr - run.end_soc_adjustment_czk(&battery);
    let profit_vs_su = total_su - total_wr_adj;
    let exported: f32 = counted().map(|d| d.wr.export_kwh).sum();
    let c_blocks: u32 = counted().map(|d| d.modes[1]).sum();
    let h_blocks: u32 = counted().map(|d| d.modes[2]).sum();
    let e_blocks: u32 = counted().map(|d| d.modes[3]).sum();

    let counted_blocks = || {
        run.blocks.iter().filter(|b| {
            !run.skipped_days.contains(&b.local.date_naive()) && b.winter_reserve.is_some()
        })
    };
    let mut mode_changes = 0usize;
    let mut previous = None;
    for wr in counted_blocks().filter_map(|b| b.winter_reserve.as_ref()) {
        if previous.is_some_and(|m| m != wr.mode) {
            mode_changes += 1;
        }
        previous = Some(wr.mode);
    }

    println!(
        "| {label:<38} | {:>8.2} | {:>8.2} | {:>7.2} | {:>+8.2} | {:>6.2} | {:>6.2} | {:>5.1} | C:{c_blocks:<3} H:{h_blocks:<3} E:{e_blocks:<3} |",
        total_su,
        total_wr,
        total_wr_adj,
        profit_vs_su,
        exported,
        total_wr_wear,
        mode_changes as f32 / n_days,
    );
}

/// Command line: `--perfect-foresight`, `--seed <u64, 0x.. for hex>`, `--owner-replay <path>`
/// (or `FLUXION_BENCH_OWNER_REPLAY`)
struct Args {
    perfect_foresight: bool,
    seed: u64,
    owner_replay: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        perfect_foresight: false,
        seed: DEFAULT_SEED,
        owner_replay: std::env::var("FLUXION_BENCH_OWNER_REPLAY")
            .ok()
            .filter(|p| !p.is_empty()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--perfect-foresight" => args.perfect_foresight = true,
            "--owner-replay" => args.owner_replay = it.next(),
            "--seed" => {
                let v = it.next().unwrap_or_default();
                let parsed = match v.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
                    None => v.parse(),
                };
                args.seed = parsed.unwrap_or_else(|_| {
                    eprintln!("--seed needs an integer, got {v}");
                    std::process::exit(2);
                });
            }
            other => {
                eprintln!(
                    "unknown argument {other}; usage: [--perfect-foresight] [--seed <n>] \
                     [--owner-replay <path>]"
                );
                std::process::exit(2);
            }
        }
    }
    args
}

fn main() {
    let args = parse_args();
    println!(
        "=== 1. Recorded Replays (with BatteryCostBasis; lower cost / higher ProfitSU is better) ==="
    );
    println!(
        "| {:<38} | {:>8} | {:>8} | {:>7} | {:>8} | {:>6} | {:>6} | {:>5} | {:<17} |",
        "Replay Scenario",
        "SelfUse",
        "WR Raw",
        "WR Adj",
        "ProfitSU",
        "ExpkWh",
        "Wear",
        "Flp/d",
        "Modes"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let fixture = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/backtest_replay_days.json"
    ));
    run_replay_file("fixture_9d (FD=off, ef=off)", fixture, false, false);
    run_replay_file("fixture_9d (FD=on,  ef=off)", fixture, true, false);
    run_replay_file("fixture_9d (FD=on,  ef=on)", fixture, true, true);

    if let Some(owner) = &args.owner_replay {
        let owner = Path::new(owner);
        run_replay_file("owner_9d   (FD=off, ef=off)", owner, false, false);
        run_replay_file("owner_9d   (FD=on,  ef=off)", owner, true, false);
        run_replay_file("owner_9d   (FD=on,  ef=on)", owner, true, true);
    } else {
        println!(
            "Skipping owner replay (pass --owner-replay <path> or FLUXION_BENCH_OWNER_REPLAY)"
        );
    }

    println!(
        "\n=== 2. Synthetic Multi-Day Closed-Loop Benchmark (6 days each, 15m replans, 13:00 horizon, {}) ===",
        if args.perfect_foresight {
            "perfect foresight".to_string()
        } else {
            format!("forecast noise σ={FORECAST_SIGMA}, seed {:#x}", args.seed)
        }
    );
    println!(
        "| {:<42} | {:<16} | {:>8} | {:>8} | {:>8} | {:>6} | {:>5} | {:>5} | {:<14} |",
        "Site",
        "Day Shape",
        "Raw Cost",
        "Adj Cost",
        "ProfitSU",
        "ExpkWh",
        "Cyc/d",
        "Flp/d",
        "S/C/H/E"
    );
    println!("|---|---|---|---|---|---|---|---|---|");

    let sites = [
        SiteKind::OwnerFixedBuy {
            with_window_levers: false,
            force_discharge: true,
        },
        SiteKind::OwnerFixedBuy {
            with_window_levers: true,
            force_discharge: true,
        },
        SiteKind::OwnerFixedBuy {
            with_window_levers: false,
            force_discharge: false,
        },
        SiteKind::JakubSpotBuy {
            force_discharge: true,
        },
        SiteKind::JakubSpotBuy {
            force_discharge: false,
        },
    ];
    let shapes = [
        DayShape::TwoPeaks,
        DayShape::Flat,
        DayShape::NegativePrices,
        DayShape::MissingTomorrow,
    ];

    for site in sites {
        let mut sum_adj = 0.0_f32;
        let mut sum_raw = 0.0_f32;
        let mut sum_gain = 0.0_f32;
        let mut sum_exp = 0.0_f32;
        let mut sum_cyc = 0.0_f32;
        let mut sum_flp = 0.0_f32;
        for shape in shapes {
            let m = run_synthetic_closed_loop(site, shape, 6, args.perfect_foresight, args.seed);
            sum_adj += m.adj_net_czk;
            sum_raw += m.raw_net_czk;
            sum_gain += m.gain_vs_su_czk;
            sum_exp += m.export_kwh;
            sum_cyc += m.cycles_per_day / shapes.len() as f32;
            sum_flp += m.flips_per_day / shapes.len() as f32;
            println!(
                "| {:<42} | {:<16} | {:>8.2} | {:>8.2} | {:>+8.2} | {:>6.1} | {:>5.2} | {:>5.1} | {:>3}/{:>2}/{:>2}/{:>2} |",
                site.label(),
                shape.label(),
                m.raw_net_czk,
                m.adj_net_czk,
                m.gain_vs_su_czk,
                m.export_kwh,
                m.cycles_per_day,
                m.flips_per_day,
                m.self_use_blocks,
                m.charge_blocks,
                m.hold_blocks,
                m.export_blocks,
            );
        }
        println!(
            "| {:<42} | {:<16} | {:>8.2} | {:>8.2} | {:>+8.2} | {:>6.1} | {:>5.2} | {:>5.1} | {:<14} |",
            site.label(),
            "TOTAL / MEAN",
            sum_raw,
            sum_adj,
            sum_gain,
            sum_exp,
            sum_cyc,
            sum_flp,
            "-",
        );
    }
}
