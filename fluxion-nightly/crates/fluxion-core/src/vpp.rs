// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided "AS IS", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

//! SolaX direct VPP mode control (`control.vpp_direct_enabled`, owner ruling 2026-10-27).
//!
//! Every block mode and export curtailment is a direct VPP remote-control command (RAM, no
//! EEPROM), re-sent every cycle and verified by the `modbus_power_control` read-back. The only
//! EEPROM write left is the basic work mode (Self Use or Back Up, from the user's default mode).
//! Measured behaviour and the mode table: `docs/reference/solax-vpp-direct-learning-2026-10-27.md`.
//!
//! Parallel sites (owner direction D5, measured on an X3-Ultra pair:
//! `docs/reference/solax-vpp-direct-ultra-parallel-2026-10-28.md`): commands go to the master
//! only and the slaves follow at once; push power is the system total. An inverter without the
//! mode 8 entities (the Ultra) runs modes 1–7 ([`without_mode8`]): mode 8 with an uncapped PV
//! becomes mode 4 with the same push, and a curtailment that needs mode 8 runs on the export
//! limit instead.
//!
//! The executor (`schedule_execution_system`) still decides which FluxION mode is current
//! (`CurrentMode`, with block locks, VPR, fixed slots and the force discharge gate) but writes
//! nothing while VPP is active; [`vpp_direct_system`] turns that mode into the command.

use bevy_ecs::prelude::*;
use chrono::{DateTime, Duration, Utc};
use fluxion_types::user_control::{DefaultMode, UserControlState};
use fluxion_types::{VprMode, VprPhase};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tracing::{debug, error, info, warn};

use crate::components::{BatteryStatus, CurrentMode, Inverter, InverterCommand, RawInverterState};
use crate::continuous_systems::{LeversStartup, lever_startup_wait};
use crate::debug::DebugModeConfig;
use fluxion_types::inverter::InverterOperationMode;

/// Re-send interval of a non-zero command. Modes 4–7 lapse to mode 6 after ~10 min and mode 8
/// carries [`MODE8_DURATION_S`], so this leaves a wide margin.
pub const KEEPALIVE_SECS: i64 = 60;
/// Minimum time between two VPP writes (a stop to mode 0 is never held by it).
pub const MIN_WRITE_GAP_SECS: i64 = 20;
/// Minimum gap for a mode 8 curtailment command (owner ruling R1, 2026-10-27): it follows the
/// load more closely; other commands keep `MIN_WRITE_GAP_SECS`.
pub const CURTAIL_WRITE_GAP_SECS: i64 = 10;
/// Mode 8 PV limit when PV is not limited (force charge/discharge, Feed-in); the adapter clamps
/// it to the entity's maximum.
pub const PV_UNCAPPED_W: u32 = 30_000;
/// After a read-back fall-back to mode 0 the plan is retried this often (owner ruling R4).
pub const FALLBACK_RETRY_SECS: i64 = 900;
/// The read-back (input 0x100) lags a write by one poll; reads this soon after a mode change are
/// not compared.
pub const READBACK_LAG_SECS: i64 = 30;
/// Mode 8 duration (s). Never 0: 0 ends the command at once (measured).
pub const MODE8_DURATION_S: u16 = 180;
const _: () = assert!(
    MODE8_DURATION_S > 0,
    "mode 8 duration 0 ends the command at once"
);
/// Mode 8 NextMotion (the integration's `remotecontrol_timeout_8_9_direct`): 160 = exit to the
/// basic work mode when the duration ends.
pub const MODE8_NEXT_MOTION_EXIT: u16 = 160;
/// Mismatched reads are counted at most once per this interval (state reads arrive every few
/// seconds; one count per executor cycle).
pub const MISMATCH_INTERVAL_SECS: i64 = 60;
/// Counted read-back mismatches before a warning and a forced re-send.
pub const MISMATCH_WARN: u32 = 2;
/// Read-back mismatches before falling back to mode 0 for good (until a stop path resets it).
pub const MISMATCH_FAIL: u32 = 5;
/// Interval between basic work mode (EEPROM) write attempts while the read-back differs.
pub const BASIC_RETRY_SECS: i64 = 300;
/// Basic work mode write attempts before FluxION stops trying (ERROR, until the target changes).
pub const BASIC_MAX_ATTEMPTS: u32 = 3;
/// Power fields are written in steps of 100 W (the entities' step).
const POWER_STEP_W: i32 = 100;
/// A change of push power or PV limit smaller than this waits for the keep-alive.
const MATERIAL_CHANGE_W: i32 = 200;
/// Curtailment mode 8: PV more than this below the limit counts as sun-limited.
const CURTAIL_CLIP_MARGIN_W: i32 = 300;
/// ... or this share of the limit, when larger (the measured clip shortfall is 150 W at 3 kW)
const CURTAIL_CLIP_MARGIN_PCT: i32 = 10;
/// Curtailment mode 8 → mode 7 after this many sun-limited reads in a row.
const CURTAIL_SUN_LIMITED_READS: u8 = 2;
/// Curtailment at full battery: mode 8 holds until the SOC is this far below the maximum.
const CURTAIL_SOC_RELEASE_PCT: f32 = 1.0;
/// Inputs older than this are not used for a power figure (discharge clamp, curtailment).
const STALE_INPUT_SECS: i64 = 300;

/// One direct VPP command (SolaX power control modes; mode 5 and 9 are not used).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VppCommand {
    /// Mode 0: exit remote control, the basic work mode runs
    Exit,
    /// Mode 4: exact battery power, `push_w` > 0 discharge, < 0 charge (grid tops up PV)
    Push { push_w: i32 },
    /// Mode 6: self-consume charge/discharge (Self Use on top of a Back Up basic mode)
    SelfConsume,
    /// Mode 7: self-consume charge only (PV surplus to the battery, never discharges)
    ChargeOnly,
    /// Mode 8: PV capped at `pv_limit_w`, exact battery power `push_w`, duration
    /// [`MODE8_DURATION_S`], then the basic work mode
    PvAndBattery { pv_limit_w: u32, push_w: i32 },
}

impl VppCommand {
    /// SolaX power control mode number (input 0x100 / holding 0x7C or 0xA0)
    #[must_use]
    pub fn mode(&self) -> u8 {
        match self {
            Self::Exit => 0,
            Self::Push { .. } => 4,
            Self::SelfConsume => 6,
            Self::ChargeOnly => 7,
            Self::PvAndBattery { .. } => 8,
        }
    }

    /// Option of `select.<control prefix>_modbus_power_control_direct` (solax-modbus 2026.10.01)
    #[must_use]
    pub fn select_option(&self) -> &'static str {
        match self {
            Self::Exit => "Disabled",
            Self::Push { .. } => "Push Power-Positive/Negative Mode",
            Self::SelfConsume => "Self-Consume - Charge/Discharge Mode",
            Self::ChargeOnly => "Self-Consume - Charge Only Mode",
            Self::PvAndBattery { .. } => "Individual Setting - Duration Mode",
        }
    }

    /// Parameters written before the mode select, as (key, value): the entity is
    /// `number.<control prefix>_remotecontrol_<key>_direct`.
    #[must_use]
    pub fn parameters(&self) -> Vec<(&'static str, i64)> {
        match *self {
            Self::Exit => Vec::new(),
            Self::Push { push_w } => vec![("push_mode_power", i64::from(push_w)), ("timeout", 0)],
            Self::SelfConsume | Self::ChargeOnly => vec![("timeout", 0)],
            Self::PvAndBattery { pv_limit_w, push_w } => vec![
                ("pv_power_limit", i64::from(pv_limit_w)),
                ("push_mode_power_8_9", i64::from(push_w)),
                ("duration_8", i64::from(MODE8_DURATION_S)),
                ("timeout_8_9", i64::from(MODE8_NEXT_MOTION_EXIT)),
            ],
        }
    }

    /// Battery is pushed to charge (grid may top up): the charge run checks apply.
    #[must_use]
    pub fn charges(&self) -> bool {
        matches!(self, Self::Push { push_w } | Self::PvAndBattery { push_w, .. } if *push_w < 0)
    }

    /// A force charge from the grid (mode 8 with an uncapped PV, or mode 4, and a negative
    /// push): the grid-charge check is armed. Curtailment's mode 8 charges from PV and is not.
    #[must_use]
    pub fn grid_charges(&self) -> bool {
        matches!(self, Self::Push { push_w } if *push_w < 0)
            || matches!(self, Self::PvAndBattery { pv_limit_w: PV_UNCAPPED_W, push_w } if *push_w < 0)
    }

    /// Same mode, and export capped lower than under `prev`: a lower push, or on mode 8 a lower
    /// PV limit + push, also when one of them rises (R5-3, R6-4)
    fn lowers_export_from(&self, prev: &Self) -> bool {
        match (*self, *prev) {
            (Self::Push { push_w: a }, Self::Push { push_w: b }) => a < b,
            (
                Self::PvAndBattery {
                    pv_limit_w: la,
                    push_w: a,
                },
                Self::PvAndBattery {
                    pv_limit_w: lb,
                    push_w: b,
                },
            ) => i64::from(la) + i64::from(a) < i64::from(lb) + i64::from(b),
            _ => false,
        }
    }

    /// Different enough to write before the keep-alive: another mode, or a power field moved by
    /// at least `MATERIAL_CHANGE_W`.
    fn materially_differs(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Push { push_w: a }, Self::Push { push_w: b }) => {
                (a - b).abs() >= MATERIAL_CHANGE_W
            }
            (
                Self::PvAndBattery {
                    pv_limit_w: pa,
                    push_w: a,
                },
                Self::PvAndBattery {
                    pv_limit_w: pb,
                    push_w: b,
                },
            ) => {
                (a - b).abs() >= MATERIAL_CHANGE_W
                    || (i64::from(pa) - i64::from(pb)).abs() >= i64::from(MATERIAL_CHANGE_W)
            }
            (a, b) => a.mode() != b.mode(),
        }
    }
}

impl std::fmt::Display for VppCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mode {}", self.mode())?;
        let params = self.parameters();
        if !params.is_empty() {
            let params: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            write!(f, " {{{}}}", params.join(", "))?;
        }
        Ok(())
    }
}

/// Decode `sensor.<prefix>_modbus_power_control` (input 0x100) with the integration's own
/// labels. An unknown label is `None`, never a guessed mode.
#[must_use]
pub fn decode_power_control(label: &str) -> Option<u8> {
    match label {
        "Disabled" => Some(0),
        "Enabled Power Control" => Some(1),
        "Enabled Quantity Control" => Some(2),
        "Enabled SOC Target Control" => Some(3),
        "Push Power - P/N Mode" => Some(4),
        "Push Power - Zero Mode" => Some(5),
        "Self Consume - C/D Mode" => Some(6),
        "Self Consume - Charge Only Mode" => Some(7),
        "Individual Setting - Duration Mode" => Some(8),
        "Individual Setting - Target SOC Mode" => Some(9),
        _ => None,
    }
}

/// The inverter's basic work mode (EEPROM), from the user's default mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasicMode {
    SelfUse,
    Backup { min_soc_pct: u8 },
}

impl BasicMode {
    #[must_use]
    pub fn of(user_control: Option<&UserControlState>) -> Self {
        match user_control {
            Some(uc) if uc.default_mode == DefaultMode::Backup => Self::Backup {
                min_soc_pct: uc.backup_min_soc_pct,
            },
            _ => Self::SelfUse,
        }
    }

    /// The FluxION mode the hardware reads in this basic mode (`select.*_charger_use_mode`)
    #[must_use]
    pub fn operation_mode(&self) -> InverterOperationMode {
        match self {
            Self::SelfUse => InverterOperationMode::SelfUse,
            Self::Backup { .. } => InverterOperationMode::BackUpMode,
        }
    }

    /// The write that sets this basic mode (charger_use_mode, plus the minimum SOC for Back Up)
    #[must_use]
    pub fn command(&self) -> InverterCommand {
        match *self {
            Self::SelfUse => InverterCommand::SetMode(InverterOperationMode::SelfUse),
            Self::Backup { min_soc_pct } => InverterCommand::SetBackupMode { min_soc_pct },
        }
    }
}

/// Live readings used for power figures; `None` when unknown or stale.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VppInputs {
    pub soc: Option<f32>,
    pub pv_w: Option<f32>,
    pub load_w: Option<f32>,
}

/// Site limits for the mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VppLimits {
    /// Battery charge and discharge power (W), from `max_battery_charge_rate_kw`
    pub battery_w: i32,
    /// Export cap at the meter (W): `maximum_export_power_w`, lowered by the hard cap
    pub max_export_w: i32,
    pub max_soc: f32,
    /// SOC at or below which a discharge stops (Back Up minimum in Back Up default)
    pub discharge_floor: f32,
}

fn floor_step(w: f32) -> i32 {
    let w = w.max(0.0) as i32;
    w - w % POWER_STEP_W
}

fn ceil_step(w: f32) -> i32 {
    let w = w.max(0.0).ceil() as i32;
    (w + POWER_STEP_W - 1) / POWER_STEP_W * POWER_STEP_W
}

/// Discharge push (W) with meter export ≤ `max_export_w`: push ≤ cap + load − PV, floor 0.
/// Unknown PV or load gives 0 (no discharge without a clamp).
#[must_use]
pub fn discharge_push_w(inputs: &VppInputs, limits: &VppLimits) -> i32 {
    match (inputs.pv_w, inputs.load_w) {
        (Some(pv), Some(load)) => {
            let headroom = limits.max_export_w as f32 + load - pv;
            floor_step(headroom)
                .min(limits.battery_w.max(0) - limits.battery_w.max(0) % POWER_STEP_W)
        }
        _ => 0,
    }
}

/// Feed-in mode 8 PV limit from one read (the start of [`ExportMemory`]): ceil_100(cap + load)
/// when PV reaches it, else uncapped. Unknown PV or load: uncapped (the inverter's own export
/// limit caps the meter, #649 Q3).
#[must_use]
pub fn feed_in_pv_limit_w(inputs: &VppInputs, limits: &VppLimits) -> u32 {
    match (inputs.pv_w, inputs.load_w) {
        (Some(pv), Some(load)) if pv >= limits.max_export_w as f32 + load => {
            ceil_step(limits.max_export_w as f32 + load) as u32
        }
        _ => PV_UNCAPPED_W,
    }
}

/// Above this residual (W) a PV − load read is a step, not noise: the estimator restarts from it.
/// The threshold is clamp(`NET_STEP_SIGMAS` × σ̂, `NET_STEP_MIN_W`, this)
const NET_STEP_W: f32 = 500.0;
const NET_STEP_MIN_W: f32 = 100.0;
const NET_STEP_SIGMAS: f32 = 4.0;
/// Least gain of the PV − load level (5 s reads; the first reads are averaged)
const NET_GAIN: f32 = 0.07;
/// Prior read noise σ (W) of PV − load, and its weight in reads
const NET_SIGMA_PRIOR_W: f32 = 200.0;
const NET_SIGMA_PRIOR_READS: f32 = 0.5;
/// Least gain of the read noise variance (it follows a change of the noise)
const NET_SIGMA_GAIN: f32 = 0.05;
/// Least residual σ (W) of a slope fit (a perfect line is not infinitely certain)
const NET_FIT_SIGMA_MIN_W: f32 = 5.0;
/// Slope fit windows (reads), shortest first: a fast ramp is seen early, a slow one late
const NET_SLOPE_WINDOWS: [usize; 3] = [6, 12, 24];
/// A slope is a ramp when it is this many 0.9995 t-quantile standard errors from 0
const NET_SLOPE_Z: f32 = 5.0;
/// A slope of the 12-read window this many standard errors from 0 moves the step prediction
const NET_PREDICT_Z: f32 = 3.0;
/// Reads after a restart in which a PV rise seeds the slope (a cloud edge ramps on)
const NET_SEEDED_READS: usize = 4;
/// A rising PV − load is projected this many reads ahead (the read-back lag and one read), so
/// a push written now still fits when it applies (R7-1)
const NET_HORIZON_READS: f32 = 7.0;
/// Gain of the smoothed load of the curtail regime's PV limit
const LOAD_GAIN: f32 = 0.2;
/// The push target keeps this many σ of the PV − load estimate below the cap
const PUSH_MARGIN_SIGMAS: f32 = 1.5;
/// Rounding of the push target: within this of the next step rounds up
const PUSH_ROUND_W: f32 = 50.0;
/// Export over the cap (W) that lowers the push at once (R7-1)
const PUSH_OVER_W: f32 = 100.0;
/// The mean of the last `PUSH_FAST_READS` reads lowers the push when export is `PUSH_OVER_W` +
/// this many σ of that mean over the cap; one read when it is `PUSH_OVER_ONE_SIGMAS` σ over
const PUSH_FAST_READS: usize = 3;
const PUSH_OVER_MEAN_SIGMAS: f32 = 1.5;
const PUSH_OVER_ONE_SIGMAS: f32 = 2.0;
/// The push falls to its target when that is max(1 step, this many σ) lower
const PUSH_DOWN_SIGMAS: f32 = 4.0;
/// The push rises beyond max(min(`PUSH_UP_W`, max(1 step, `PUSH_UP_SIGMAS` σ)), 10 % of the
/// load): noise writes nothing, a quiet read rises by one step (R7-5)
const PUSH_UP_W: f32 = 200.0;
const PUSH_UP_SIGMAS: f32 = 4.0;
const PUSH_UP_LOAD_PCT: f32 = 10.0;
/// Export expected over the cap by this much with the push at 0: the curtail regime at once
const CURTAIL_ENTRY_OVER_W: f32 = 100.0;
/// The curtail regime's limit follows a load rise of one step held this many reads (R7-3)
const CURTAIL_LOAD_UP_READS: u8 = 6;
/// The curtail regime ends when the mean of `CURTAIL_EXIT_READS` settled PV reads is below
/// cap + load by max(`CURTAIL_EXIT_W`, `CURTAIL_EXIT_CLIP_PCT` % of the limit + 1 step) +
/// `CURTAIL_EXIT_SIGMAS` σ of that mean, for `CURTAIL_SUN_LIMITED_READS` reads (R7-4)
const CURTAIL_EXIT_READS: usize = 3;
const CURTAIL_EXIT_W: f32 = 300.0;
const CURTAIL_EXIT_CLIP_PCT: f32 = 5.0;
const CURTAIL_EXIT_SIGMAS: f32 = 2.0;

/// Two-sided 0.9995 t-quantile by degrees of freedom (the next lower tabulated one)
fn t_quantile(dof: usize) -> f32 {
    const TABLE: [(usize, f32); 14] = [
        (1, 3183.0),
        (2, 70.7),
        (3, 22.2),
        (4, 13.0),
        (5, 9.68),
        (6, 8.02),
        (7, 7.06),
        (8, 6.44),
        (9, 6.01),
        (10, 5.69),
        (12, 5.26),
        (15, 4.88),
        (20, 4.54),
        (22, 4.48),
    ];
    TABLE
        .iter()
        .rev()
        .find(|(d, _)| *d <= dof.max(1))
        .map_or(TABLE[0].1, |(_, t)| *t)
}

/// A least-squares line through the last reads
#[derive(Debug, Clone, Copy)]
struct Fit {
    /// W per read
    slope: f32,
    /// Standard error of the slope × t-quantile / `NET_SLOPE_Z` (`f32::MAX` under 3 reads)
    slope_sd: f32,
    /// The line at the last read
    end: f32,
    reads: usize,
}

/// PV − load (W) per read (R7-1): a level that averages read noise, the read noise σ̂ from
/// successive differences, and least-squares slopes over `NET_SLOPE_WINDOWS` reads. A slope is
/// used only when it is significant, so noise is not projected; a step beyond the restart
/// threshold restarts the estimator, and a step with PV rising by more than half of it seeds
/// the slope (a cloud edge ramps on, a load step does not)
#[derive(Debug, Clone, Default, PartialEq)]
struct NetEstimator {
    reads: std::collections::VecDeque<f32>,
    level: Option<f32>,
    level_reads: u32,
    variance: Option<f32>,
    variance_reads: u32,
    /// The 12-read slope, removed from the successive differences of σ̂
    slope: f32,
    seeded: bool,
}

impl NetEstimator {
    fn restart(&mut self, net: f32, seed: Option<f32>) {
        self.reads.clear();
        self.reads.extend(seed);
        self.reads.push_back(net);
        self.seeded = seed.is_some();
        self.level = Some(net);
        self.level_reads = 1;
    }

    fn sigma(&self) -> f32 {
        self.variance
            .unwrap_or(NET_SIGMA_PRIOR_W * NET_SIGMA_PRIOR_W)
            .sqrt()
    }

    fn fit(&self, window: usize) -> Fit {
        let n = window.min(self.reads.len());
        let ys = self.reads.iter().skip(self.reads.len() - n);
        let x_mean = (n as f32 - 1.0) / 2.0;
        let y_mean = ys.clone().sum::<f32>() / n as f32;
        let sxx: f32 = (0..n).map(|i| (i as f32 - x_mean).powi(2)).sum();
        let slope = if sxx > 0.0 {
            ys.clone()
                .enumerate()
                .map(|(i, y)| (i as f32 - x_mean) * (y - y_mean))
                .sum::<f32>()
                / sxx
        } else {
            0.0
        };
        let slope_sd = if n >= 3 {
            let residual = ys
                .enumerate()
                .map(|(i, y)| (y - (y_mean + slope * (i as f32 - x_mean))).powi(2))
                .sum::<f32>()
                / (n - 2) as f32;
            (residual.max(NET_FIT_SIGMA_MIN_W * NET_FIT_SIGMA_MIN_W) / sxx).sqrt()
                * t_quantile(n - 2)
                / NET_SLOPE_Z
        } else {
            f32::MAX
        };
        Fit {
            slope,
            slope_sd,
            end: y_mean + slope * (n as f32 - 1.0 - x_mean),
            reads: n,
        }
    }

    /// A significant rising or falling slope (shortest window first), or the seeded slope
    fn ramp(&self) -> Option<Fit> {
        let n = self.reads.len();
        if n < 2 {
            return None;
        }
        if self.seeded && n <= NET_SEEDED_READS {
            return Some(self.fit(n));
        }
        if n < 3 {
            return None;
        }
        NET_SLOPE_WINDOWS
            .iter()
            .map(|w| self.fit(*w))
            .find(|f| f.slope > 0.0 && f.slope > NET_SLOPE_Z * f.slope_sd)
    }

    fn update(&mut self, net: f32, pv_rise: f32) {
        let Some(level) = self.level else {
            self.restart(net, None);
            return;
        };
        let mid = NET_SLOPE_WINDOWS[1];
        let predicted = match self.ramp() {
            Some(f) if f.slope > 0.0 => f.end + f.slope,
            _ if self.reads.len() >= 3 => {
                let f = self.fit(mid);
                if f.slope.abs() > NET_PREDICT_Z * f.slope_sd {
                    f.end + f.slope
                } else {
                    level
                }
            }
            _ => level,
        };
        self.slope = if self.reads.len() >= 3 {
            self.fit(mid).slope
        } else {
            0.0
        };
        let residual = net - predicted;
        let step = (NET_STEP_SIGMAS * self.sigma()).clamp(NET_STEP_MIN_W, NET_STEP_W);
        if residual.abs() > step {
            let seed = (residual > 0.0 && pv_rise > step.max(residual) / 2.0)
                .then(|| net - pv_rise.min(residual));
            self.restart(net, seed);
            return;
        }
        if self.reads.len() >= 2
            && let Some(last) = self.reads.back()
        {
            let d = net - last - self.slope;
            self.variance_reads += 1;
            let gain =
                (1.0 / (self.variance_reads as f32 + NET_SIGMA_PRIOR_READS)).max(NET_SIGMA_GAIN);
            let variance = self
                .variance
                .unwrap_or(NET_SIGMA_PRIOR_W * NET_SIGMA_PRIOR_W);
            self.variance = Some(variance + gain * (d * d / 2.0 - variance));
        }
        self.reads.push_back(net);
        while self.reads.len() > NET_SLOPE_WINDOWS[2] {
            self.reads.pop_front();
        }
        self.level_reads += 1;
        let gain = (1.0 / self.level_reads as f32).max(NET_GAIN);
        self.level = Some(level + gain * (net - level));
    }

    /// PV − load now, its σ, and the rising slope (W per read, 0 without a ramp)
    fn now(&self) -> (f32, f32, f32) {
        if let Some(f) = self.ramp().filter(|f| f.slope > 0.0) {
            let m = f.reads as f32;
            return (
                f.end,
                self.sigma() * ((4.0 * m - 2.0) / (m * (m + 1.0))).sqrt(),
                f.slope,
            );
        }
        let k = self.level_reads.max(1) as f32;
        let gain = (1.0 / k).max(NET_GAIN);
        (
            self.level.unwrap_or(0.0),
            self.sigma() * (1.0 / k).max(gain / (2.0 - gain)).sqrt(),
            0.0,
        )
    }

    /// The mean of the last `PUSH_FAST_READS` reads, its σ, and its age (reads)
    fn recent(&self) -> (f32, f32, f32) {
        let m = PUSH_FAST_READS.min(self.reads.len()).max(1);
        let mean = self.reads.iter().rev().take(m).sum::<f32>() / m as f32;
        (
            mean,
            self.sigma() / (m as f32).sqrt(),
            (m as f32 - 1.0) / 2.0,
        )
    }

    fn last(&self) -> f32 {
        self.reads.back().copied().unwrap_or(0.0)
    }
}

/// What an [`ExportMemory`] runs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// Force Discharge on mode 8: both regimes
    Discharge,
    /// Force Discharge on mode 4 (no mode 8): the discharge regime only, the inverter's own
    /// export limit caps PV above the cap
    PushOnly,
    /// Feed-in on mode 8: push 0; PV uncapped or capped at cap + load
    FeedIn,
}

/// The regime of an [`ExportMemory`]
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExportRegime {
    /// PV uncapped, so measured PV is the sun; the battery fills export up to the cap. Counts
    /// the reads with PV at or above cap + load
    Discharge { above_reads: u8 },
    /// PV at or above cap + load: PV limit = ceil(cap + `load_w`), push 0
    Curtail { load_w: f32 },
}

impl Default for ExportRegime {
    fn default() -> Self {
        Self::Discharge { above_reads: 0 }
    }
}

/// Force Discharge and Feed-in on mode 8 outside curtailment, stepped once per fresh state
/// read (R6, R7-1..R7-5). Two regimes:
/// - discharge: PV uncapped (`PV_UNCAPPED_W`), push = cap − (PV − load) within the battery, on
///   the [`NetEstimator`] (a significant rise projected `NET_HORIZON_READS` ahead, less
///   `PUSH_MARGIN_SIGMAS` σ). The push falls at once (urgent, not held by the write gap) when
///   the last read or the mean of the last 3 shows export `PUSH_OVER_W` over the cap beyond its
///   noise, and to its target when that is max(1 step, `PUSH_DOWN_SIGMAS` σ) lower; it rises
///   beyond the up band, so read noise writes nothing. Feed-in: push 0;
/// - curtail: once the push is 0 and PV is at or above cap + load for 2 reads (at once when
///   `CURTAIL_ENTRY_OVER_W` over): PV limit = ceil(cap + load), push 0. The limit follows a
///   load drop of 2 steps, a rise beyond max(`CURTAIL_CLIP_MARGIN_W`, 10 %) or of one step held
///   `CURTAIL_LOAD_UP_READS` reads, and a grid import at once (urgent). It ends when settled PV
///   is short of cap + load (the `CURTAIL_EXIT_*` rule), for 2 reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportMemory {
    /// The last command computed, and the read it came from
    last: Option<(DateTime<Utc>, VppCommand)>,
    net: NetEstimator,
    /// Smoothed load (the curtail regime's PV limit)
    load_w: Option<f32>,
    /// PV of the last read
    pv_w: Option<f32>,
    push_w: Option<i32>,
    regime: ExportRegime,
    /// Settled PV reads of the curtail regime (the exit test)
    settled_pv: std::collections::VecDeque<f32>,
    sun_limited_reads: u8,
    load_up_reads: u8,
    /// The PV limit last sent and when it last changed (the curtail regime's settling)
    sent_limit: Option<(u32, DateTime<Utc>)>,
    /// The last command must be written at once (export over the cap, a grid import)
    urgent: bool,
}

impl ExportMemory {
    /// Note a sent command: a changed PV limit starts the settling
    pub fn sent(&mut self, command: VppCommand, now: DateTime<Utc>) {
        if let VppCommand::PvAndBattery { pv_limit_w, .. } = command
            && self.sent_limit.is_none_or(|(old, _)| old != pv_limit_w)
        {
            self.sent_limit = Some((pv_limit_w, now));
        }
        self.urgent = false;
    }

    /// The last command is not held by the minimum write gap nor the material-change band
    #[must_use]
    pub fn urgent(&self) -> bool {
        self.urgent
    }

    /// The command for this read (PV and load known)
    pub fn step(
        &mut self,
        kind: ExportKind,
        pv: f32,
        load: f32,
        limits: &VppLimits,
        read_at: DateTime<Utc>,
    ) -> VppCommand {
        if let Some((at, command)) = self.last
            && at == read_at
        {
            return command;
        }
        self.urgent = false;
        let net = pv - load;
        self.net
            .update(net, self.pv_w.map_or(0.0, |last| pv - last));
        self.pv_w = Some(pv);
        self.load_w = Some(match self.load_w {
            Some(smoothed) if (load - smoothed).abs() <= NET_STEP_W => {
                smoothed + LOAD_GAIN * (load - smoothed)
            }
            _ => load,
        });
        let smoothed_load = self.load_w.unwrap_or(load);
        let cap = limits.max_export_w as f32;
        let curtail_limit = |load_w: f32| ceil_step(cap + load_w) as u32;

        if let ExportRegime::Curtail { load_w: held } = self.regime {
            if let Some(command) = self.curtail_step(pv, load, smoothed_load, held, cap, read_at) {
                self.last = Some((read_at, command));
                return command;
            }
            // Sun-limited: the discharge regime, from this read
            self.regime = ExportRegime::default();
            self.net.restart(net, None);
            self.push_w = None;
        }

        let (now, _, rise) = self.net.now();
        let unclamped = cap - (now + rise * NET_HORIZON_READS);
        if kind != ExportKind::PushOnly
            && let ExportRegime::Discharge { above_reads } = self.regime
        {
            if unclamped <= -CURTAIL_ENTRY_OVER_W || (unclamped <= 0.0 && above_reads >= 1) {
                self.regime = ExportRegime::Curtail {
                    load_w: smoothed_load,
                };
                self.sun_limited_reads = 0;
                self.load_up_reads = 0;
                self.settled_pv.clear();
                self.push_w = Some(0);
                // PV will be capped: its reads no longer show the sun
                self.net.restart(net, None);
                let command = VppCommand::PvAndBattery {
                    pv_limit_w: curtail_limit(smoothed_load),
                    push_w: 0,
                };
                self.last = Some((read_at, command));
                return command;
            }
            self.regime = ExportRegime::Discharge {
                above_reads: if unclamped <= 0.0 { above_reads + 1 } else { 0 },
            };
        }
        let push = self.push_step(kind, load, limits);
        let command = match kind {
            ExportKind::PushOnly => VppCommand::Push { push_w: push },
            ExportKind::Discharge | ExportKind::FeedIn => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: push,
            },
        };
        self.last = Some((read_at, command));
        command
    }

    /// The curtail regime's command, `None` when it ends (sun-limited)
    fn curtail_step(
        &mut self,
        pv: f32,
        load: f32,
        smoothed_load: f32,
        held: f32,
        cap: f32,
        read_at: DateTime<Utc>,
    ) -> Option<VppCommand> {
        let step = POWER_STEP_W as f32;
        let held = if pv < load && load > held {
            // The house imports: the limit follows at once
            self.urgent = true;
            self.load_up_reads = 0;
            load
        } else if smoothed_load <= held - 2.0 * step
            || smoothed_load >= held + (CURTAIL_CLIP_MARGIN_W as f32).max(smoothed_load * 0.1)
        {
            self.load_up_reads = 0;
            smoothed_load
        } else if smoothed_load >= held + step {
            // A slow load rise (R7-3)
            self.load_up_reads += 1;
            if self.load_up_reads >= CURTAIL_LOAD_UP_READS {
                self.load_up_reads = 0;
                smoothed_load
            } else {
                held
            }
        } else {
            self.load_up_reads = 0;
            held
        };
        let target = cap + held;
        let limit = ceil_step(target) as u32;
        let sent = self.sent_limit.map_or(limit, |(limit, _)| limit);
        let settling = self
            .sent_limit
            .is_some_and(|(_, at)| read_at < at + Duration::seconds(READBACK_LAG_SECS));
        if settling {
            self.settled_pv.clear();
        } else {
            self.settled_pv.push_back(pv);
            while self.settled_pv.len() > CURTAIL_EXIT_READS {
                self.settled_pv.pop_front();
            }
        }
        if self.settled_pv.len() >= CURTAIL_EXIT_READS {
            let n = self.settled_pv.len() as f32;
            let mean = self.settled_pv.iter().sum::<f32>() / n;
            let margin = CURTAIL_EXIT_W.max(sent as f32 * CURTAIL_EXIT_CLIP_PCT / 100.0 + step)
                + CURTAIL_EXIT_SIGMAS * self.net.sigma() / n.sqrt();
            if mean < target - margin {
                self.sun_limited_reads += 1;
                if self.sun_limited_reads >= CURTAIL_SUN_LIMITED_READS {
                    self.sun_limited_reads = 0;
                    return None;
                }
            } else {
                self.sun_limited_reads = 0;
            }
        }
        self.regime = ExportRegime::Curtail { load_w: held };
        self.push_w = Some(0);
        Some(VppCommand::PvAndBattery {
            pv_limit_w: limit,
            push_w: 0,
        })
    }

    /// The discharge regime's push (R7-1)
    fn push_step(&mut self, kind: ExportKind, load: f32, limits: &VppLimits) -> i32 {
        let cap = limits.max_export_w as f32;
        let step = POWER_STEP_W as f32;
        let battery = limits.battery_w.max(0) - limits.battery_w.max(0) % POWER_STEP_W;
        let (now, sigma_now, rise) = self.net.now();
        let ahead = now + rise * NET_HORIZON_READS;
        // Within the battery before the bands (R6-3)
        let target = if kind == ExportKind::FeedIn {
            0
        } else {
            floor_step(cap - ahead - PUSH_MARGIN_SIGMAS * sigma_now + PUSH_ROUND_W).min(battery)
        };
        let (recent, sigma_recent, age) = self.net.recent();
        let recent_ahead = recent + rise * (NET_HORIZON_READS + age);
        let last_ahead = self.net.last() + rise * NET_HORIZON_READS;
        let push = match self.push_w {
            None => target,
            Some(push) if kind != ExportKind::FeedIn => {
                let p = push as f32;
                let over_mean =
                    p + recent_ahead - cap > PUSH_OVER_W + PUSH_OVER_MEAN_SIGMAS * sigma_recent;
                let over_one =
                    p + last_ahead - cap > PUSH_OVER_W + PUSH_OVER_ONE_SIGMAS * self.net.sigma();
                let up_band = (PUSH_UP_W.min((PUSH_UP_SIGMAS * sigma_now).max(step)))
                    .max(load * PUSH_UP_LOAD_PCT / 100.0);
                if over_mean || over_one {
                    // Export over the cap: from the unsmoothed reads, at once (R7-1)
                    self.urgent = true;
                    let worst = if over_one {
                        recent_ahead.max(last_ahead)
                    } else {
                        recent_ahead
                    };
                    target.min(floor_step(cap - worst))
                } else if target as f32 <= p - step.max(PUSH_DOWN_SIGMAS * sigma_now) {
                    self.urgent = true;
                    target
                } else if target as f32 >= p + up_band {
                    target
                } else {
                    push
                }
            }
            Some(_) => target,
        };
        self.push_w = Some(push);
        push
    }
}

/// Curtailment state carried between state reads (the mode 8 → 7 hysteresis).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurtailMemory {
    /// Consecutive reads with PV below the mode 8 limit by more than the clip margin
    pub sun_limited_reads: u8,
    /// Set by the caller each read: the PV limit last sent to the inverter (`None`: the limit of
    /// `prev`)
    pub sent_limit_w: Option<u32>,
    /// Set by the caller each read: the sent limit was raised less than `READBACK_LAG_SECS`
    /// before this read (PV still ramping), so the read is not counted as sun-limited
    pub settling: bool,
}

/// Curtailed export with PV for the house and the battery (owner ruling 2026-10-27):
/// - mode 7 (charge only, no discharge) while the battery can take the whole surplus: the
///   inverter's own zero-export loop puts PV surplus into the battery and holds the meter at 0
///   while the load moves;
/// - mode 8 only when mode 7 would export: the SOC at its maximum (and surplus above one
///   step), or the surplus above the charge cap. PV limit = load + charge (rounded up to 100 W),
///   push = −charge. Mode 8 is open-loop, so load swings show on the meter until the next read.
///
/// Mode 8 goes back to mode 7 after `CURTAIL_SUN_LIMITED_READS` settled reads with PV below the
/// last sent limit by more than max(`CURTAIL_CLIP_MARGIN_W`, `CURTAIL_CLIP_MARGIN_PCT` %) (the
/// sun cannot fill it; at full battery: PV below the house load, no surplus to curtail), or, at
/// full battery, once the SOC is `CURTAIL_SOC_RELEASE_PCT` below the maximum. `prev` is the last
/// curtailment command. Call once per fresh state read.
#[must_use]
pub fn curtail_command(
    prev: Option<VppCommand>,
    memory: &mut CurtailMemory,
    pv_w: f32,
    load_w: f32,
    soc: Option<f32>,
    limits: &VppLimits,
) -> VppCommand {
    let cap = limits.battery_w.max(0) - limits.battery_w.max(0) % POWER_STEP_W;
    // Unknown SOC: no charge is assumed
    let full = soc.is_none_or(|soc| soc >= limits.max_soc);
    // Rounded up: a slight export rather than an import (owner ruling R2)
    let mode8 = |charge: i32| VppCommand::PvAndBattery {
        pv_limit_w: ceil_step(load_w + charge as f32) as u32,
        push_w: -charge,
    };

    if let Some(VppCommand::PvAndBattery { pv_limit_w, push_w }) = prev {
        let limit = memory.sent_limit_w.unwrap_or(pv_limit_w) as i32;
        let margin = CURTAIL_CLIP_MARGIN_W.max(limit * CURTAIL_CLIP_MARGIN_PCT / 100);
        let sun_limited = pv_w < (limit - margin) as f32;
        if !memory.settling {
            memory.sun_limited_reads = if sun_limited {
                memory.sun_limited_reads.saturating_add(1)
            } else {
                0
            };
        }
        let held_full = push_w == 0;
        let soc_released =
            held_full && soc.is_some_and(|soc| soc < limits.max_soc - CURTAIL_SOC_RELEASE_PCT);
        if memory.sun_limited_reads < CURTAIL_SUN_LIMITED_READS && !soc_released {
            // At full battery the charge stays 0 until the SOC is released (hysteresis)
            return mode8(if held_full || full { 0 } else { cap });
        }
        memory.sun_limited_reads = 0;
        return VppCommand::ChargeOnly;
    }

    memory.sun_limited_reads = 0;
    let surplus = floor_step(pv_w - load_w);
    match (full, surplus) {
        (true, s) if s > POWER_STEP_W => mode8(0),
        (false, s) if s > cap => mode8(cap),
        _ => VppCommand::ChargeOnly,
    }
}

/// What a mapping wants logged once (behaviour that differs from the EEPROM mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VppNote {
    /// Feed-in Priority runs as mode 8 push 0: PV above the export cap is not stored
    FeedInBatteryIdle,
    /// Back Up block on a Self Use basic mode runs as mode 7 (no discharge, PV charges)
    BackupAsChargeOnly,
    /// Curtailment without PV or house load: hard stop (PV 0, push 0), the house imports
    CurtailHardStop,
    /// Discharge without PV or house load: no push (the export clamp needs both)
    DischargeUnclamped,
    /// No mode 8 on the inverter: a curtailment that needs it runs on the export limit
    NoMode8ExportLimit,
}

/// The command for a FluxION mode on a basic work mode (brief design §2).
#[must_use]
pub fn command_for(
    mode: InverterOperationMode,
    basic: BasicMode,
    curtail: Option<&mut CurtailMemory>,
    inputs: &VppInputs,
    limits: &VppLimits,
    prev: Option<VppCommand>,
) -> (VppCommand, Option<VppNote>) {
    use InverterOperationMode as M;
    let soc_at_max = inputs.soc.is_some_and(|soc| soc >= limits.max_soc);
    let soc_at_floor = inputs.soc.is_some_and(|soc| soc <= limits.discharge_floor);

    if let Some(memory) = curtail {
        let battery_w = limits.battery_w.max(0) - limits.battery_w.max(0) % POWER_STEP_W;
        let (Some(pv), Some(load)) = (inputs.pv_w, inputs.load_w) else {
            memory.sun_limited_reads = 0;
            // Mode 7 needs no figures while the battery has room; otherwise a hard stop
            if inputs.soc.is_some_and(|soc| soc < limits.max_soc) && mode != M::ForceDischarge {
                return (VppCommand::ChargeOnly, None);
            }
            return (
                VppCommand::PvAndBattery {
                    pv_limit_w: 0,
                    push_w: 0,
                },
                Some(VppNote::CurtailHardStop),
            );
        };
        let command = match mode {
            // Force charge at full power, grid tops up PV, no export
            M::ForceCharge if !soc_at_max => {
                memory.sun_limited_reads = 0;
                VppCommand::PvAndBattery {
                    pv_limit_w: ceil_step(load + battery_w as f32) as u32,
                    push_w: -battery_w,
                }
            }
            // No discharge into a curtailed grid: the battery stays idle, PV serves the house
            M::ForceDischarge => {
                memory.sun_limited_reads = 0;
                VppCommand::PvAndBattery {
                    pv_limit_w: ceil_step(load) as u32,
                    push_w: 0,
                }
            }
            _ => curtail_command(prev, memory, pv, load, inputs.soc, limits),
        };
        return (command, None);
    }

    match mode {
        M::SelfUse => match basic {
            BasicMode::SelfUse => (VppCommand::Exit, None),
            BasicMode::Backup { .. } => (VppCommand::SelfConsume, None),
        },
        M::BackUpMode => match basic {
            BasicMode::Backup { .. } => (VppCommand::Exit, None),
            BasicMode::SelfUse => (VppCommand::ChargeOnly, Some(VppNote::BackupAsChargeOnly)),
        },
        // At the SOC maximum no grid push: PV surplus only
        M::ForceCharge if soc_at_max => (VppCommand::ChargeOnly, None),
        // Mode 8 with an uncapped PV (owner ruling R5): its duration lapses to the basic work
        // mode within 3 min if FluxION stops sending (crash safety)
        M::ForceCharge => (
            VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: -(limits.battery_w.max(0) - limits.battery_w.max(0) % POWER_STEP_W),
            },
            None,
        ),
        M::ForceDischarge if soc_at_floor => (VppCommand::ChargeOnly, None),
        // PV uncapped, the battery fills export up to the cap ([`ExportMemory`] per read, R6)
        M::ForceDischarge => {
            let note = (inputs.pv_w.is_none() || inputs.load_w.is_none())
                .then_some(VppNote::DischargeUnclamped);
            (
                VppCommand::PvAndBattery {
                    pv_limit_w: PV_UNCAPPED_W,
                    push_w: discharge_push_w(inputs, limits),
                },
                note,
            )
        }
        M::NoChargeNoDischarge => (VppCommand::ChargeOnly, None),
        M::FeedinPriority => (
            VppCommand::PvAndBattery {
                pv_limit_w: feed_in_pv_limit_w(inputs, limits),
                push_w: 0,
            },
            Some(VppNote::FeedInBatteryIdle),
        ),
    }
}

/// The command for an inverter without mode 8 (owner direction D5: the X3-Ultra has modes 1–7
/// only), from the mode 8 mapping of `command_for`, and whether the export limit must curtail:
/// - mode 8 outside curtailment (force charge, force discharge, Feed-in): mode 4 with the same
///   push (force charge −W, discharge +W with its export clamp, battery idle push 0); its PV
///   limit is dropped (mode 4 has none);
/// - mode 8 during curtailment: no volatile PV limit exists, so the export limit
///   curtails; force charge keeps its mode 4 charge, force discharge holds the battery (mode 4
///   push 0), anything else is mode 7 (PV surplus to the battery);
/// - modes 0, 6 and 7 unchanged.
#[must_use]
pub fn without_mode8(
    command: VppCommand,
    mode: InverterOperationMode,
    curtail: bool,
) -> (VppCommand, bool) {
    use InverterOperationMode as M;
    match command {
        // Mode 4 cannot cap PV: the inverter's own export limit caps the meter (#649 Q3)
        VppCommand::PvAndBattery { push_w, .. } if !curtail => (VppCommand::Push { push_w }, false),
        VppCommand::PvAndBattery { push_w, .. } => {
            let command = match mode {
                M::ForceCharge if push_w < 0 => VppCommand::Push { push_w },
                M::ForceDischarge => VppCommand::Push { push_w: 0 },
                _ => VppCommand::ChargeOnly,
            };
            (command, true)
        }
        command => (command, false),
    }
}

/// What a read-back did to the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readback {
    /// No new read, or one too soon after the mode change
    Pending,
    Confirmed,
    /// Mismatch, not yet warned
    Mismatch,
    /// `MISMATCH_WARN` mismatches: re-send now
    Resend,
    /// `MISMATCH_FAIL` mismatches: fall back to mode 0 and stop
    Fail,
}

/// Re-send and read-back state of one inverter's VPP link.
#[derive(Debug, Clone, Default)]
pub struct VppLink {
    pub last_sent: Option<VppCommand>,
    last_sent_at: Option<DateTime<Utc>>,
    /// When the sent mode number last changed (the read-back lag runs from here)
    mode_changed_at: Option<DateTime<Utc>>,
    last_read_at: Option<DateTime<Utc>>,
    /// Read of the last counted mismatch (`MISMATCH_INTERVAL_SECS` between counts)
    last_counted_at: Option<DateTime<Utc>>,
    mismatches: u32,
    /// A forced re-send restarts the read-back lag window when it is written
    restart_lag: bool,
    /// Fell back to mode 0 after `MISMATCH_FAIL` mismatches; cleared by a stop path or the
    /// retry every `FALLBACK_RETRY_SECS`
    pub failed: bool,
    /// When the fall-back was noticed (the retry runs from here)
    failed_at: Option<DateTime<Utc>>,
    /// Retries after a fall-back (for the log)
    fallback_retries: u32,
    /// A mode 0 sent by a stop path (kill switch, debug switch) awaits its read-back; checks end
    /// once it is confirmed (or failed), so manual control after a stop is never overwritten
    stop_check: bool,
    /// State read and plan mode the last curtailment command was computed from, and that
    /// command (computed once per read; the next read continues from it)
    curtail_from: Option<(DateTime<Utc>, InverterOperationMode, VppCommand)>,
    curtail_memory: CurtailMemory,
    /// Curtailment mode 8: the PV limit last sent and when it was last raised (the read-back
    /// of PV is settled `READBACK_LAG_SECS` after a raise)
    curtail_sent: Option<(u32, DateTime<Utc>)>,
    /// The last write failed (reported by the writer): re-sent after the minimum gap
    write_failed: bool,
    /// The last command sent is the fall-back's mode 0 (its confirmation is not a working link)
    sent_as_fallback: bool,
    /// No mode 8: this curtailment runs on the export limit (kept until the curtailment ends,
    /// so the limit is not toggled as the surplus moves)
    export_limit_curtail: bool,
    /// The kill switch was on at start (since): handled once a read-back is in, for at most
    /// `STARTUP_READ_WINDOW_SECS` (N6, R5-4)
    kill_start_pending: Option<DateTime<Utc>>,
    /// Force Discharge and Feed-in outside curtailment (R6)
    export: ExportMemory,
    /// The target set type check (R4-3, R5-2) is queued and awaited
    target_check_pending: bool,
    /// A failed (transient) target set type check is retried at (R5-6)
    target_retry_at: Option<DateTime<Utc>>,
    /// The first target set type check of this inverter was queued at, until it is verified:
    /// unverified for `STARTUP_READ_WINDOW_SECS` falls back to the EEPROM path (R7-8)
    target_check_since: Option<DateTime<Utc>>,
    /// The kill switch was turned on and its mode 0 is not sent yet (held by the target set type
    /// check); sent once the check passes, or before the EEPROM fall-back (R7-8)
    kill_exit_pending: bool,
}

impl VppLink {
    /// Whether `target` is written now.
    #[must_use]
    pub fn due(&self, target: &VppCommand, now: DateTime<Utc>) -> bool {
        self.due_with_gap(target, now, MIN_WRITE_GAP_SECS)
    }

    /// [`Self::due`] with another minimum gap (curtailment mode 8: `CURTAIL_WRITE_GAP_SECS`).
    #[must_use]
    pub fn due_with_gap(&self, target: &VppCommand, now: DateTime<Utc>, gap_secs: i64) -> bool {
        let (Some(prev), Some(at)) = (self.last_sent, self.last_sent_at) else {
            return true;
        };
        let since = now - at;
        let gap_passed = since >= Duration::seconds(gap_secs);
        if *target == VppCommand::Exit {
            // A stop is never held by the gap; mode 0 does not lapse, so no keep-alive
            return prev != VppCommand::Exit || (self.write_failed && gap_passed);
        }
        // A command that only lowers export (lower PV limit or push, same mode) is not held by
        // the gap: export must not stay above the cap (R5-3)
        let lowers_export = prev.materially_differs(target) && target.lowers_export_from(&prev);
        if !gap_passed && !lowers_export {
            return false;
        }
        self.write_failed
            || prev.materially_differs(target)
            || since >= Duration::seconds(KEEPALIVE_SECS)
    }

    /// `due_with_gap`, and export over the cap or a grid import ([`ExportMemory::urgent`]):
    /// any change is written at once, not held by the gap nor the material-change band (a
    /// 100 W decrease, R7-1)
    pub fn write_due(&self, target: &VppCommand, now: DateTime<Utc>, gap_secs: i64) -> bool {
        (self.export.urgent() && !self.failed && self.last_sent != Some(*target))
            || self.due_with_gap(target, now, gap_secs)
    }

    pub fn sent(&mut self, command: VppCommand, now: DateTime<Utc>) {
        self.stop_check = false;
        if self.last_sent.map(|c| c.mode()) != Some(command.mode()) {
            self.mode_changed_at = Some(now);
            self.mismatches = 0;
            self.last_counted_at = None;
        } else if self.restart_lag {
            self.mode_changed_at = Some(now);
        }
        self.restart_lag = false;
        self.write_failed = false;
        self.sent_as_fallback = self.failed;
        self.last_sent = Some(command);
        self.last_sent_at = Some(now);
    }

    /// Force the next `due` (re-send after a mismatch).
    fn force_resend(&mut self) {
        self.last_sent_at = None;
        self.restart_lag = true;
    }

    /// Feed one state read: `mode` is the decoded read-back (`None` unknown or unreadable, which
    /// is no evidence either way). A mismatch is counted at most once per
    /// `MISMATCH_INTERVAL_SECS`, after the lag window of the last mode change or forced re-send.
    pub fn observe(&mut self, read_at: DateTime<Utc>, mode: Option<u8>) -> Readback {
        let (Some(sent), Some(changed)) = (self.last_sent, self.mode_changed_at) else {
            return Readback::Pending;
        };
        if self.last_read_at == Some(read_at)
            || read_at < changed + Duration::seconds(READBACK_LAG_SECS)
        {
            return Readback::Pending;
        }
        self.last_read_at = Some(read_at);
        let Some(mode) = mode else {
            return Readback::Pending;
        };
        if mode == sent.mode() {
            self.mismatches = 0;
            self.last_counted_at = None;
            return Readback::Confirmed;
        }
        // After the fall-back to mode 0 nothing more is done (no loop)
        if self.failed
            || self
                .last_counted_at
                .is_some_and(|t| read_at - t < Duration::seconds(MISMATCH_INTERVAL_SECS))
        {
            return Readback::Pending;
        }
        self.last_counted_at = Some(read_at);
        self.mismatches += 1;
        if self.mismatches >= MISMATCH_FAIL {
            self.failed = true;
            return Readback::Fail;
        }
        if self.mismatches == MISMATCH_WARN {
            self.force_resend();
            return Readback::Resend;
        }
        Readback::Mismatch
    }

    /// A stop path (kill switch, debug, mode control off) clears the failure latch.
    fn reset_failure(&mut self) {
        self.failed = false;
        self.failed_at = None;
        self.mismatches = 0;
        self.last_counted_at = None;
    }

    /// After a fall-back: whether the plan is retried now (every `FALLBACK_RETRY_SECS`; the
    /// mismatch count starts over). Returns the retry number.
    fn fallback_retry(&mut self, now: DateTime<Utc>) -> Option<u32> {
        if !self.failed {
            return None;
        }
        let since = *self.failed_at.get_or_insert(now);
        if now - since < Duration::seconds(FALLBACK_RETRY_SECS) {
            return None;
        }
        self.reset_failure();
        self.fallback_retries += 1;
        Some(self.fallback_retries)
    }
}

/// Basic work mode (EEPROM) write tracking of one inverter.
#[derive(Debug, Clone, Default)]
struct BasicWrite {
    target: Option<BasicMode>,
    last_write: Option<DateTime<Utc>>,
    attempts: u32,
    gave_up_logged: bool,
}

/// Export curtailment wanted now (set by `export_curtailment_system` while VPP is active);
/// mode 8 replaces the export limit write, except on inverters whose VPP link fell back to
/// mode 0 (`fallback`, set by [`vpp_direct_system`]): those curtail by the export limit.
#[derive(Resource, Debug, Default, Clone)]
pub struct VppCurtailment {
    pub wanted: bool,
    pub fallback: BTreeSet<String>,
}

/// Inverters on which a non-zero VPP command may be live, for the shutdown handler (it writes
/// mode 0 to those only, so debug mode or an idle link writes nothing on stop), and the stop
/// flag that ends [`vpp_direct_system`]'s writes before that.
#[derive(Resource, Default, Clone)]
pub struct VppLive {
    live: Arc<std::sync::Mutex<BTreeSet<String>>>,
    stopping: Arc<AtomicBool>,
    /// The writer of the VPP system, so the shutdown mode 0 queues behind its writes
    writer: Arc<std::sync::OnceLock<crate::resources::AsyncInverterWriter>>,
}

impl VppLive {
    fn set(&self, inverter_id: &str, live: bool) {
        let mut ids = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if live {
            ids.insert(inverter_id.to_owned());
        } else {
            ids.remove(inverter_id);
        }
    }

    /// Inverters with a live non-zero command
    #[must_use]
    pub fn live_inverters(&self) -> Vec<String> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.live_inverters().is_empty()
    }

    /// The VPP system's writer (registered on its first run)
    #[must_use]
    pub fn writer(&self) -> Option<&crate::resources::AsyncInverterWriter> {
        self.writer.get()
    }

    /// Shutdown: the VPP system writes nothing more
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// Overrides the VPP system's clock (tests only: the gaps, retries and the 15 min fall-back
/// retry run on it instead of `Utc::now()`).
#[derive(Resource, Debug, Clone, Copy)]
pub struct VppClock(pub DateTime<Utc>);

/// Startup window (s) for a complete read of the inverter and Home Assistant states (owner
/// ruling 2026-10-28: reads can stall for up to a minute): #647's state staleness window
/// (`continuous_systems::STATE_STALE_SECS`), the one 120 s read window.
pub const STARTUP_READ_WINDOW_SECS: u64 =
    crate::continuous_systems::STATE_STALE_SECS.unsigned_abs();
/// Retry interval of the direct VPP startup check inside [`STARTUP_READ_WINDOW_SECS`]
pub const STARTUP_RETRY_SECS: u64 = 10;
/// Retry interval of the startup check after the window ran out on the EEPROM path
pub const PATH_RETRY_SECS: u64 = 60;

/// The mode path of this run (owner directions D1, D5, N2). Decided by the startup check, never
/// written to the config: `control.vpp_direct_enabled` keeps the user's value (N8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModePath {
    /// The startup check has not decided yet: no mode writes on either path
    #[default]
    Pending,
    /// Direct VPP; `no_mode8`: inverters that run modes 1–7 only (D5)
    Direct { no_mode8: Vec<String> },
    /// The EEPROM mode writes, and why
    Eeprom(String),
}

impl ModePath {
    #[must_use]
    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    /// Inverters without mode 8 (empty off the direct path)
    #[must_use]
    pub fn no_mode8(&self) -> Vec<String> {
        match self {
            Self::Direct { no_mode8 } => no_mode8.clone(),
            Self::Pending | Self::Eeprom(_) => Vec::new(),
        }
    }

    /// For `/api/config` (`control.vpp_direct_path`) and the log
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Pending => "deciding (direct VPP startup check)".to_owned(),
            Self::Direct { no_mode8 } if no_mode8.is_empty() => "direct VPP".to_owned(),
            Self::Direct { no_mode8 } => {
                format!("direct VPP (modes 1-7, no mode 8: {})", no_mode8.join(", "))
            }
            Self::Eeprom(reason) => format!("EEPROM mode writes ({reason})"),
        }
    }
}

/// Result of an `EnsureVppTargetSetType` check (R4-3, R5-6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetCheck {
    /// Both selects read "Set" (or were set while VPP read Disabled)
    Set,
    /// A select is not "Set" while a VPP command runs: the EEPROM path
    Refused(String),
    /// Read, transport or unlock error: retried every `STARTUP_RETRY_SECS`
    Failed(String),
}

/// Error of a target set type check that found a select not "Set" under a running VPP command
#[derive(Debug, Clone)]
pub struct TargetTypeRefused(pub String);

impl std::fmt::Display for TargetTypeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TargetTypeRefused {}

/// The run's [`ModePath`], shared by the startup check task, the ECS systems and `/api/config`.
/// Without this resource (tests) the path follows `control.vpp_direct_enabled`. It also carries
/// the live debug flag (kept by the VPP system, read by each startup check) and whether the
/// target set type was verified outside debug mode (R5-2): for every inverter by the startup
/// check, or per inverter by its own check (R6, several controlled masters).
#[derive(Resource, Debug, Clone, Default)]
pub struct VppPathState {
    path: Arc<std::sync::RwLock<ModePath>>,
    debug: Arc<std::sync::atomic::AtomicBool>,
    all_verified: Arc<std::sync::atomic::AtomicBool>,
    verified: Arc<std::sync::Mutex<BTreeSet<String>>>,
}

impl VppPathState {
    /// A path state; a path created as direct (no startup check) counts as verified
    #[must_use]
    pub fn new(path: ModePath) -> Self {
        let state = Self::default();
        state.set_target_verified(path.is_direct());
        state.set(path);
        state
    }

    /// Debug mode is on (live)
    #[must_use]
    pub fn debug(&self) -> bool {
        self.debug.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_debug(&self, debug: bool) {
        self.debug.store(debug, std::sync::atomic::Ordering::SeqCst);
    }

    fn verified_ids(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The target set type of `inverter_id` was found or set to "Set" outside debug mode
    #[must_use]
    pub fn target_verified(&self, inverter_id: &str) -> bool {
        self.all_verified.load(std::sync::atomic::Ordering::SeqCst)
            || self.verified_ids().contains(inverter_id)
    }

    /// Every inverter verified (the startup check outside debug), or none
    pub fn set_target_verified(&self, verified: bool) {
        self.verified_ids().clear();
        self.all_verified
            .store(verified, std::sync::atomic::Ordering::SeqCst);
    }

    /// One inverter verified by its own check
    pub fn set_inverter_verified(&self, inverter_id: &str) {
        self.verified_ids().insert(inverter_id.to_owned());
    }

    #[must_use]
    pub fn get(&self) -> ModePath {
        self.path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, path: ModePath) {
        *self
            .path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }
}

static SERVED_PATH: std::sync::OnceLock<VppPathState> = std::sync::OnceLock::new();

/// Register the run's path for `/api/config` (once, at startup)
pub fn register_served_path(state: VppPathState) {
    let _ = SERVED_PATH.set(state);
}

/// The registered path state, for `/api/config`
#[must_use]
pub fn served_path() -> Option<VppPathState> {
    SERVED_PATH.get().cloned()
}

/// Which writer drives the modes now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePath {
    /// Startup check pending: nothing writes a mode or an export limit
    Pending,
    Eeprom,
    Vpp,
}

/// The active writer: the EEPROM path with the option or mode control off, else the decided
/// path (the option itself without a [`VppPathState`]).
#[must_use]
pub fn active_path(
    control: &fluxion_types::config::ControlConfig,
    state: Option<&VppPathState>,
) -> ActivePath {
    if !control.vpp_direct_enabled || !control.mode_control_enabled {
        return ActivePath::Eeprom;
    }
    match state.map(VppPathState::get) {
        None | Some(ModePath::Direct { .. }) => ActivePath::Vpp,
        Some(ModePath::Pending) => ActivePath::Pending,
        Some(ModePath::Eeprom(_)) => ActivePath::Eeprom,
    }
}

/// Whether the VPP path drives the inverter now.
#[must_use]
pub fn vpp_active(
    control: &fluxion_types::config::ControlConfig,
    state: Option<&VppPathState>,
) -> bool {
    active_path(control, state) == ActivePath::Vpp
}

/// Per-inverter state of [`vpp_direct_system`] (one `Local`).
#[derive(Default)]
pub struct VppState {
    links: BTreeMap<String, VppLink>,
    basic: BTreeMap<String, BasicWrite>,
    /// Debug: last command logged per inverter (logged on change only)
    debug_logged: BTreeMap<String, VppCommand>,
    notes: BTreeSet<(String, VppNote)>,
    debug_was_enabled: Option<bool>,
    /// Debug: inverters warned about a non-zero VPP mode left by an earlier run
    leftover_warned: BTreeSet<String>,
    /// `None` before the first run (a kill switch already on at start is not a change, N6)
    kill_was_active: Option<bool>,
    startup_logged: bool,
    /// Mode control off: the one mode 0 write per inverter was sent
    exit_sent_mode_control_off: BTreeSet<String>,
    /// Inverters whose state read is stale (WARN logged on entry)
    stale: BTreeSet<String>,
}

fn send(
    writer: &crate::resources::AsyncInverterWriter,
    live: Option<&VppLive>,
    inverter_id: &str,
    command: VppCommand,
) {
    writer.write_command_async(inverter_id.to_string(), InverterCommand::SetVpp(command));
    if let Some(live) = live {
        live.set(inverter_id, command != VppCommand::Exit);
    }
}

/// Feed the read-back to the link and log what it decided.
fn observe_readback(link: &mut VppLink, raw: Option<&RawInverterState>, id: &str) -> Readback {
    let Some(raw) = raw else {
        return Readback::Pending;
    };
    let readback = link.observe(raw.last_updated, raw.state.vpp_mode);
    match readback {
        Readback::Pending | Readback::Confirmed | Readback::Mismatch => {}
        Readback::Resend => warn!(
            "⚠️ Direct VPP read-back of {} is {:?}, expected mode {:?} - re-sending",
            id,
            raw.state.vpp_mode,
            link.last_sent.map(|c| c.mode())
        ),
        Readback::Fail => error!(
            "❌ Direct VPP read-back of {} is {:?} after {} reads, expected mode {:?}: falling back to mode 0 (basic work mode); the plan is retried every 15 min",
            id,
            raw.state.vpp_mode,
            MISMATCH_FAIL,
            link.last_sent.map(|c| c.mode())
        ),
    }
    readback
}

/// Stop paths (kill switch, debug): check the mode 0 the stop path sent until the read-back
/// confirms it once, then leave the inverter alone (no fight with manual control).
fn observe_stop_readback(link: &mut VppLink, raw: Option<&RawInverterState>, id: &str) {
    let Some(raw) = raw.filter(|_| link.stop_check) else {
        return;
    };
    match link.observe(raw.last_updated, raw.state.vpp_mode) {
        Readback::Pending | Readback::Mismatch => {}
        Readback::Confirmed => link.stop_check = false,
        Readback::Resend => warn!(
            "⚠️ Direct VPP read-back of {} is {:?} after mode 0 - re-sending mode 0",
            id, raw.state.vpp_mode
        ),
        Readback::Fail => {
            link.stop_check = false;
            error!(
                "❌ Direct VPP mode 0 of {} not confirmed after {} reads (reads {:?}); no more checks",
                id, MISMATCH_FAIL, raw.state.vpp_mode
            );
        }
    }
}

/// What the basic work mode check decided.
#[derive(Debug, Clone, PartialEq)]
enum BasicAction {
    None,
    Write(InverterCommand),
    /// `BASIC_MAX_ATTEMPTS` writes did not change the read-back (logged once)
    GiveUp,
}

/// Basic work mode check: a write when the hardware's work mode (`hw_mode`) or the confirmed
/// Back Up minimum differs from `basic`, at most every `BASIC_RETRY_SECS` and
/// `BASIC_MAX_ATTEMPTS` times per target.
fn basic_action(
    entry: &mut BasicWrite,
    basic: BasicMode,
    hw_mode: InverterOperationMode,
    confirmed_min_soc: Option<u8>,
    now: DateTime<Utc>,
) -> BasicAction {
    if entry.target != Some(basic) {
        *entry = BasicWrite {
            target: Some(basic),
            ..Default::default()
        };
    }
    let mode_differs = hw_mode != basic.operation_mode();
    let min_soc_pending = match basic {
        BasicMode::Backup { min_soc_pct } => confirmed_min_soc != Some(min_soc_pct),
        BasicMode::SelfUse => false,
    };
    if !mode_differs && !min_soc_pending {
        entry.attempts = 0;
        entry.gave_up_logged = false;
        return BasicAction::None;
    }
    if entry
        .last_write
        .is_some_and(|t| now - t < Duration::seconds(BASIC_RETRY_SECS))
    {
        return BasicAction::None;
    }
    if entry.attempts >= BASIC_MAX_ATTEMPTS {
        if entry.gave_up_logged {
            return BasicAction::None;
        }
        entry.gave_up_logged = true;
        return BasicAction::GiveUp;
    }
    entry.last_write = Some(now);
    entry.attempts += 1;
    BasicAction::Write(match basic {
        // Hardware already in Back Up: only the minimum SOC is synced (no work mode write)
        BasicMode::Backup { min_soc_pct } if !mode_differs => {
            InverterCommand::SetBackupMinSoc { min_soc_pct }
        }
        _ => basic.command(),
    })
}

/// Inputs from the last state read; stale (> 5 min) readings are unknown.
fn inputs_of(
    battery: Option<&BatteryStatus>,
    raw: Option<&RawInverterState>,
    now: DateTime<Utc>,
) -> VppInputs {
    let fresh = raw.filter(|r| now - r.last_updated <= Duration::seconds(STALE_INPUT_SECS));
    VppInputs {
        soc: battery.map(|b| f32::from(b.soc_percent)),
        pv_w: fresh.map(|r| r.state.pv_power_w),
        // House load from its sensor; else the balance PV − battery charge − export, with the
        // batteries of the parallel units (PV is already the site's)
        load_w: fresh.map(|r| {
            r.state.house_load_w.unwrap_or_else(|| {
                let battery_w =
                    r.state.battery_power_w + r.state.extra_battery_power_w.unwrap_or_default();
                (r.state.pv_power_w - battery_w - r.state.grid_power_w).max(0.0)
            })
        }),
    }
}

/// Turn each controlled inverter's `CurrentMode` into a direct VPP command, re-sent every
/// cycle and checked against the read-back; keep the basic work mode in line with the user's
/// default. Does nothing unless `control.vpp_direct_enabled`.
#[expect(clippy::too_many_arguments)]
pub fn vpp_direct_system(
    writer: Res<crate::resources::AsyncInverterWriter>,
    query: Query<(
        &CurrentMode,
        &Inverter,
        Option<&BatteryStatus>,
        Option<&RawInverterState>,
    )>,
    system_config: Res<crate::resources::SystemConfig>,
    debug: Res<DebugModeConfig>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
    (levers, timezone): (
        Option<Res<crate::resources::UserLeversResource>>,
        Option<Res<crate::resources::TimezoneConfig>>,
    ),
    (mut curtailment, sync): (
        Option<ResMut<VppCurtailment>>,
        Option<Res<crate::continuous_systems::InitialModeSyncTracker>>,
    ),
    (live, clock): (Option<Res<VppLive>>, Option<Res<VppClock>>),
    vpp_path: Option<Res<VppPathState>>,
    mut state: Local<VppState>,
    mut lever_startup: Local<LeversStartup>,
) {
    let control = &system_config.control_config;
    // The decided path (N2): nothing while the startup check is pending or on the EEPROM path
    // The live debug flag for the startup check (R5-2), kept before any early return
    if let Some(state) = vpp_path.as_deref() {
        state.set_debug(debug.enabled);
    }
    let path = vpp_path.as_deref().map(VppPathState::get);
    if !control.vpp_direct_enabled
        || path.as_ref().is_some_and(|p| !p.is_direct())
        || live.as_ref().is_some_and(|l| l.is_stopping())
    {
        return;
    }
    let no_mode8 = match &path {
        Some(ModePath::Direct { no_mode8 }) => no_mode8.clone(),
        _ => control.vpp_no_mode8.clone(),
    };
    let now = clock.map_or_else(Utc::now, |c| c.0);
    let live = live.as_deref();
    if let Some(live) = live {
        let _ = live.writer.get_or_init(|| writer.clone());
    }
    let state = &mut *state;
    let controlled: Vec<_> = query
        .iter()
        .filter(|(_, inverter, _, _)| {
            !system_config.inverters.iter().any(|cfg| {
                cfg.id == inverter.id
                    && matches!(
                        cfg.topology,
                        crate::resources::InverterTopology::Slave { .. }
                    )
            })
        })
        .collect();

    // Mode control off (H37): only the export limit is written, after one mode 0 that ends a
    // command a previous run may have left
    if !control.mode_control_enabled {
        for (_, inverter, _, _) in &controlled {
            if !debug.enabled && state.exit_sent_mode_control_off.insert(inverter.id.clone()) {
                info!(
                    "Direct VPP: mode control disabled, sending mode 0 once to {}",
                    inverter.id
                );
                send(&writer, live, &inverter.id, VppCommand::Exit);
            }
        }
        return;
    }
    if !state.startup_logged {
        info!(
            "Direct VPP mode control active: block modes and curtailment as volatile VPP commands (keep-alive {} s), basic work mode {:?}",
            KEEPALIVE_SECS,
            BasicMode::of(user_control.as_ref().map(|uc| &uc.state))
        );
        state.startup_logged = true;
    }

    // Debug mode switched on: a stop path, mode 0 for a live command; then nothing is written
    let debug_switched_on = state.debug_was_enabled == Some(false) && debug.enabled;
    state.debug_was_enabled = Some(debug.enabled);
    if debug_switched_on {
        for (_, inverter, _, _) in &controlled {
            let link = state.links.entry(inverter.id.clone()).or_default();
            link.reset_failure();
            if link.last_sent.is_some_and(|c| c != VppCommand::Exit) {
                info!(
                    "🔧 Debug mode on: direct VPP mode 0 for {} (no more VPP writes)",
                    inverter.id
                );
                send(&writer, live, &inverter.id, VppCommand::Exit);
                link.sent(VppCommand::Exit, now);
                link.stop_check = true;
            }
        }
    }

    if lever_startup_wait(levers.as_deref(), &mut lever_startup, Instant::now()) {
        return;
    }
    let tz: chrono_tz::Tz = timezone
        .as_ref()
        .and_then(|t| t.tz)
        .unwrap_or(chrono_tz::UTC);
    let levers = levers.filter(|l| l.available).map(|l| l.levers);
    let vpr_active = levers
        .and_then(|l| l.vpr)
        .is_some_and(|w| w.phase(now, &tz) == VprPhase::Active && w.mode != VprMode::None);
    let kill_switch = user_control.as_ref().is_some_and(|uc| !uc.state.enabled)
        || levers.is_some_and(|l| l.automation_disabled);
    let kill_at_start = state.kill_was_active.is_none() && kill_switch;
    let kill_changed = state.kill_was_active.is_some_and(|was| was != kill_switch);
    state.kill_was_active = Some(kill_switch);

    let user_state = user_control.as_ref().map(|uc| &uc.state);
    let basic = BasicMode::of(user_state);
    let limits = VppLimits {
        battery_w: (control.max_battery_charge_rate_kw * 1000.0).round() as i32,
        max_export_w: control
            .export_limit_hard_cap_w
            .map_or(control.maximum_export_power_w, |cap| {
                cap.min(control.maximum_export_power_w)
            }) as i32,
        max_soc: control.max_battery_soc,
        discharge_floor: user_state.map_or(control.min_battery_soc, |uc| {
            uc.planner_min_soc_pct(control.min_battery_soc, control.max_battery_soc)
        }),
    };
    let curtail = curtailment.as_ref().is_some_and(|c| c.wanted);
    // Kill switch without VPR: mode 0, then the configured default work mode, nothing else
    let kill_stop = kill_switch && !vpr_active;

    let mut target_check_failed = false;
    for (current, inverter, battery, raw) in controlled {
        let id = &inverter.id;
        let link = state.links.entry(id.clone()).or_default();
        if target_check_failed {
            continue;
        }
        // Target set type not verified outside debug (the startup check ran in debug mode, or
        // debug changed while it was pending): checked before the first VPP command, never in
        // debug mode (R4-3, R5-2); a transient failure is retried, a refusal runs EEPROM (R5-6)
        // Kill switch already on at start: noted before the check holds this inverter (R6-6)
        if kill_at_start {
            link.kill_start_pending = Some(now);
        }
        // Kill switch turned on: its mode 0 is owed until sent, also while the check below holds
        // this inverter (R7-8)
        if kill_changed {
            link.kill_exit_pending = kill_switch && !debug.enabled;
        }
        if debug.enabled {
            link.target_check_since = None;
        }
        let verified = vpp_path.as_deref().is_none_or(|p| p.target_verified(id));
        // Unverified for the read window: no new check, the EEPROM fall-back below (R7-8)
        let check_expired = link
            .target_check_since
            .is_some_and(|since| now - since >= Duration::seconds(STARTUP_READ_WINDOW_SECS as i64));
        if !verified
            && !debug.enabled
            && !check_expired
            && !link.target_check_pending
            && link.target_retry_at.is_none_or(|at| now >= at)
        {
            info!(
                "Checking the direct VPP target set type of {} before the first VPP command",
                id
            );
            let _ = writer.take_target_check(id);
            writer.write_command_queued(id.clone(), InverterCommand::EnsureVppTargetSetType);
            link.target_check_pending = true;
            link.target_retry_at = None;
            let _ = link.target_check_since.get_or_insert(now);
        }
        if link.target_check_pending {
            match writer.take_target_check(id) {
                None => {}
                Some(TargetCheck::Set) => {
                    link.target_check_pending = false;
                    link.target_check_since = None;
                    if let Some(path) = vpp_path.as_deref() {
                        path.set_inverter_verified(id);
                    }
                }
                Some(TargetCheck::Failed(reason)) => {
                    link.target_check_pending = false;
                    link.target_retry_at = Some(now + Duration::seconds(STARTUP_RETRY_SECS as i64));
                    debug!(
                        "Target set type check of {} failed ({}): retry in {} s",
                        id, reason, STARTUP_RETRY_SECS
                    );
                }
                Some(TargetCheck::Refused(reason)) => {
                    link.target_check_pending = false;
                    warn!(
                        "⚠️ Direct VPP unavailable: target set type of {} ({}), using EEPROM mode writes",
                        id, reason
                    );
                    if let Some(path) = vpp_path.as_deref() {
                        path.set(ModePath::Eeprom(format!(
                            "target set type of {id} not 'Set': {reason}"
                        )));
                    }
                    target_check_failed = true;
                    continue;
                }
            }
        }
        // Not verified (check pending or waiting for a retry): no VPP command yet. Unverified for
        // the read window (an unreadable read-back): the EEPROM path, one way like a refusal; a
        // kill switch mode 0 owed is sent first (R7-8)
        if !debug.enabled && !vpp_path.as_deref().is_none_or(|p| p.target_verified(id)) {
            if check_expired {
                error!(
                    "❌ Direct VPP target set type of {} not verified within {} s (read-back unreadable): using EEPROM mode writes",
                    id, STARTUP_READ_WINDOW_SECS
                );
                let leftover = raw.and_then(|r| r.state.vpp_mode).is_some_and(|m| m != 0);
                if kill_stop
                    && (link.kill_exit_pending || (link.kill_start_pending.is_some() && leftover))
                {
                    info!(
                        "⏸️ FluxION disabled by user - direct VPP mode 0 for {} before the EEPROM path",
                        id
                    );
                    send(&writer, live, id, VppCommand::Exit);
                    link.sent(VppCommand::Exit, now);
                    link.kill_exit_pending = false;
                    link.kill_start_pending = None;
                }
                if let Some(path) = vpp_path.as_deref() {
                    path.set(ModePath::Eeprom(format!(
                        "target set type of {id} not verified within {STARTUP_READ_WINDOW_SECS} s"
                    )));
                }
                target_check_failed = true;
            }
            continue;
        }
        if writer.take_vpp_failure(id) && link.last_sent.is_some() {
            debug!(
                "Direct VPP write to {} failed: re-sending after the minimum gap",
                id
            );
            link.write_failed = true;
        }
        if kill_changed || debug_switched_on {
            link.reset_failure();
            link.export_limit_curtail = false;
            if let Some(c) = curtailment.as_mut() {
                c.fallback.remove(id);
            }
        }
        // Debug writes nothing, so a command an earlier run left (e.g. a crash) stays: say so
        if debug.enabled
            && let Some(mode) = raw.and_then(|r| r.state.vpp_mode)
            && mode != 0
            && state.leftover_warned.insert(id.clone())
        {
            warn!(
                "⚠️ Debug mode: {} reads direct VPP mode {} (left by an earlier run?); debug mode does not clear it - set the power control to Disabled by hand if unwanted",
                id, mode
            );
        }

        // Kill switch without VPR: mode 0, then the configured default work mode (owner ruling
        // R3), written behind the mode 0 in the inverter's queue; nothing else
        if !kill_stop {
            link.kill_start_pending = None;
        }
        if kill_stop
            && !debug.enabled
            && let Some(since) = link.kill_start_pending
        {
            // Kill switch already on at start (owner ruling R4-4, "mode 0 only"): mode 0 once for
            // a live command; the basic work mode (EEPROM) is never written. An unknown
            // read-back (sensor not loaded yet) waits, up to the startup window (R5-4)
            let readback = raw.and_then(|r| r.state.vpp_mode);
            if readback.is_none()
                && now - since >= Duration::seconds(STARTUP_READ_WINDOW_SECS as i64)
            {
                warn!(
                    "⏸️ FluxION disabled by user at start - no VPP read-back of {} in {} s: no mode 0 written",
                    id, STARTUP_READ_WINDOW_SECS
                );
                link.kill_start_pending = None;
            }
            if let Some(mode) = readback {
                link.kill_start_pending = None;
                if mode != 0 {
                    info!(
                        "⏸️ FluxION disabled by user at start - {} reads VPP mode {}: mode 0",
                        id, mode
                    );
                    send(&writer, live, id, VppCommand::Exit);
                    link.sent(VppCommand::Exit, now);
                    link.stop_check = true;
                }
            }
        } else if kill_stop && !debug.enabled {
            // The mode 0 is checked until confirmed: re-sent when the read-back disagrees.
            // Nothing sent yet: mode 0 only when the switch was just turned on
            observe_stop_readback(link, raw, id);
            // Turned on now, or while the target set type check held this inverter (R7-8)
            let turned_on = kill_changed || link.kill_exit_pending;
            link.kill_exit_pending = false;
            let exit_due = match link.last_sent {
                None => turned_on,
                Some(_) => link.due(&VppCommand::Exit, now),
            };
            if exit_due {
                info!("⏸️ FluxION disabled by user - direct VPP mode 0 for {}", id);
                send(&writer, live, id, VppCommand::Exit);
                link.sent(VppCommand::Exit, now);
                link.stop_check = true;
            }
            if turned_on {
                let command = basic.command();
                info!(
                    "⏸️ FluxION disabled by user - basic work mode of {}: {:?} ({:?})",
                    id, basic, command
                );
                writer.write_command_queued(id.clone(), command);
                state.basic.insert(
                    id.clone(),
                    BasicWrite {
                        target: Some(basic),
                        last_write: Some(now),
                        attempts: 1,
                        gave_up_logged: false,
                    },
                );
            }
        }

        // No successful state read for `STATE_STALE_SECS` (#647's stale gate): no VPP command is
        // renewed on stale data, only a mode 0 is written (the executor's stale exit)
        let stale = raw.is_some_and(|r| {
            now - r.last_updated > Duration::seconds(crate::continuous_systems::STATE_STALE_SECS)
        });
        if !stale {
            state.stale.remove(id);
        }

        // Basic work mode: the only EEPROM write, when the read-back differs from the default.
        // Not under the kill switch: its one write above is all, so a manual change made after
        // it is left alone. Not on a stale read (its work mode is frozen)
        if !debug.enabled
            && !kill_stop
            && !stale
            && let Some(raw) = raw
        {
            let entry = state.basic.entry(id.clone()).or_default();
            match basic_action(
                entry,
                basic,
                raw.state.work_mode,
                writer.confirmed_backup_min_soc(id),
                now,
            ) {
                BasicAction::None => {}
                BasicAction::Write(command) => {
                    info!(
                        "📤 Basic work mode of {}: {:?} (hardware reads {:?}) - {:?}",
                        id, basic, raw.state.work_mode, command
                    );
                    writer.write_command_queued(id.clone(), command);
                }
                BasicAction::GiveUp => error!(
                    "❌ Basic work mode of {} still reads {:?} after {} writes (expected {:?}); not writing it again until the default mode changes",
                    id, raw.state.work_mode, BASIC_MAX_ATTEMPTS, basic
                ),
            }
        }

        if kill_stop {
            continue;
        }

        // No block command before the executor has taken a plan for this inverter (no
        // schedule yet, no battery status): CurrentMode is not the plan's mode then. Export
        // curtailment does not wait for a plan: it runs on the basic work mode.
        let mode = if sync
            .as_ref()
            .is_some_and(|s| s.synced_inverters.contains(id))
        {
            current.mode
        } else if curtail {
            basic.operation_mode()
        } else {
            // A command left by an earlier run (crash): mode 0 once, before the plan (N4)
            if !debug.enabled
                && link.last_sent.is_none()
                && let Some(mode) = raw.and_then(|r| r.state.vpp_mode)
                && mode != 0
            {
                info!(
                    "Direct VPP: {} reads mode {} left by an earlier run - mode 0 before the plan",
                    id, mode
                );
                send(&writer, live, id, VppCommand::Exit);
                link.sent(VppCommand::Exit, now);
            }
            continue;
        };

        let inputs = inputs_of(battery, raw, now);
        let read_at = raw.map(|r| r.last_updated);
        // Curtailment steps once per fresh state read (the ECS ticks every 100 ms): between
        // reads the last command stands
        let (command, note) = match (link.curtail_from, read_at) {
            (Some((from, from_mode, last)), Some(read_at))
                if curtail && !link.failed && (from, from_mode) == (read_at, mode) =>
            {
                (last, None)
            }
            _ if curtail => {
                link.export = ExportMemory::default();
                let prev = link.curtail_from.map(|(_, _, c)| c);
                link.curtail_memory.sent_limit_w = link.curtail_sent.map(|(limit, _)| limit);
                link.curtail_memory.settling = match (link.curtail_sent, read_at) {
                    (Some((_, raised_at)), Some(read_at)) => {
                        read_at < raised_at + Duration::seconds(READBACK_LAG_SECS)
                    }
                    _ => true,
                };
                let computed = command_for(
                    mode,
                    basic,
                    Some(&mut link.curtail_memory),
                    &inputs,
                    &limits,
                    prev,
                );
                link.curtail_from = read_at.map(|t| (t, mode, computed.0));
                computed
            }
            _ => {
                link.curtail_from = None;
                link.curtail_memory = CurtailMemory::default();
                link.curtail_sent = None;
                let (command, note) =
                    command_for(mode, basic, None, &inputs, &limits, link.last_sent);
                let kind = match (mode, command) {
                    (InverterOperationMode::ForceDischarge, VppCommand::PvAndBattery { .. }) => {
                        Some(if no_mode8.contains(id) {
                            ExportKind::PushOnly
                        } else {
                            ExportKind::Discharge
                        })
                    }
                    (InverterOperationMode::FeedinPriority, VppCommand::PvAndBattery { .. })
                        if !no_mode8.contains(id) =>
                    {
                        Some(ExportKind::FeedIn)
                    }
                    _ => None,
                };
                match (kind, inputs.pv_w, inputs.load_w, read_at) {
                    (Some(kind), Some(pv), Some(load), Some(read_at)) => {
                        (link.export.step(kind, pv, load, &limits, read_at), note)
                    }
                    _ => {
                        link.export = ExportMemory::default();
                        (command, note)
                    }
                }
            }
        };
        // No mode 8 (D5): modes 1–7, and the export limit for the rest of a curtailment that
        // needs mode 8
        let (command, note) = if no_mode8.contains(id) {
            let (command, export_limit) = without_mode8(command, mode, curtail);
            if curtail && export_limit && !link.export_limit_curtail {
                link.export_limit_curtail = true;
                if let Some(c) = curtailment.as_mut() {
                    c.fallback.insert(id.clone());
                }
                debug!(
                    "Direct VPP: {} has no mode 8, export curtailment by the export limit",
                    id
                );
                (command, Some(VppNote::NoMode8ExportLimit))
            } else {
                (command, note)
            }
        } else {
            (command, note)
        };
        if !curtail && link.export_limit_curtail {
            link.export_limit_curtail = false;
            if !link.failed
                && let Some(c) = curtailment.as_mut()
            {
                c.fallback.remove(id);
            }
        }
        if let Some(note) = note
            && state.notes.insert((id.clone(), note))
        {
            match note {
                VppNote::FeedInBatteryIdle => info!(
                    "Direct VPP: Feed-in Priority runs with push 0 (mode 8, or mode 4 without mode 8) for {}: PV above the export cap is not stored in the battery",
                    id
                ),
                VppNote::BackupAsChargeOnly => info!(
                    "Direct VPP: a Back Up block on the Self Use basic mode runs as mode 7 (no discharge) for {}",
                    id
                ),
                VppNote::CurtailHardStop => warn!(
                    "Direct VPP: export curtailment without PV or house load for {}: PV 0, push 0 (the house imports)",
                    id
                ),
                VppNote::DischargeUnclamped => warn!(
                    "Direct VPP: discharge without PV or house load for {}: push 0 (no export clamp)",
                    id
                ),
                VppNote::NoMode8ExportLimit => info!(
                    "Direct VPP: no mode 8 on this inverter ({}): export curtailment that needs it uses the export limit, the battery runs on modes 4/7",
                    id
                ),
            }
        }

        if debug.enabled {
            if state.debug_logged.get(id) != Some(&command) {
                info!(
                    "🔧 [DEBUG] Would send VPP {} to {} for {:?}",
                    command, id, mode
                );
                state.debug_logged.insert(id.clone(), command);
            }
            // The mode 0 sent when debug was switched on is checked until confirmed
            if link.stop_check {
                observe_stop_readback(link, raw, id);
                if link.stop_check && link.due(&VppCommand::Exit, now) {
                    send(&writer, live, id, VppCommand::Exit);
                    link.sent(VppCommand::Exit, now);
                    link.stop_check = true;
                }
            }
            continue;
        }

        // Read-back of the last command; after a fall-back the export limit curtails, and the
        // plan is retried every 15 min. The export limit stays until the read-back confirms a
        // VPP command (no EEPROM toggling on every retry of a persistent failure)
        if let Some(retry) = link.fallback_retry(now) {
            warn!(
                "🔁 Direct VPP of {}: retrying the plan after the fall-back to mode 0 (retry {})",
                id, retry
            );
        }
        let readback = observe_readback(link, raw, id);
        if readback == Readback::Confirmed
            && !link.failed
            && !link.sent_as_fallback
            && !link.export_limit_curtail
            && let Some(c) = curtailment.as_mut()
            && c.fallback.remove(id)
        {
            info!(
                "✅ Direct VPP of {} confirmed again: export curtailment back on VPP",
                id
            );
        }
        if link.failed {
            let _ = link.failed_at.get_or_insert(now);
        }
        if link.failed
            && let Some(c) = curtailment.as_mut()
            && c.fallback.insert(id.clone())
        {
            warn!(
                "Direct VPP of {} fell back to mode 0: export curtailment uses the export limit",
                id
            );
        }
        let command = if link.failed {
            VppCommand::Exit
        } else {
            command
        };

        // Stale state: no keep-alive or new command (a mode 8 lapses after its duration), only
        // mode 0
        if stale && command != VppCommand::Exit {
            if state.stale.insert(id.clone()) {
                warn!(
                    "Direct VPP of {}: no successful state read for over {} s - no keep-alive of {} until a fresh read",
                    id,
                    crate::continuous_systems::STATE_STALE_SECS,
                    link.last_sent.unwrap_or(command)
                );
            }
            continue;
        }

        let gap = if curtail && matches!(command, VppCommand::PvAndBattery { .. }) {
            CURTAIL_WRITE_GAP_SECS
        } else {
            MIN_WRITE_GAP_SECS
        };
        if link.write_due(&command, now, gap) {
            let changed = link
                .last_sent
                .is_none_or(|c| c.materially_differs(&command));
            if changed {
                info!(
                    "📤 Direct VPP {} for {} ({:?}{}): {}",
                    command,
                    id,
                    mode,
                    if curtail { ", export curtailed" } else { "" },
                    current.reason
                );
            } else {
                debug!("Direct VPP keep-alive {} for {}", command, id);
            }
            send(&writer, live, id, command);
            link.sent(command, now);
            link.export.sent(command, now);
            // Stale exit (#648): after the mode 0, the default basic mode only when the hardware
            // work mode (last read) differs; nothing more until the state is fresh
            if stale
                && changed
                && command == VppCommand::Exit
                && let Some(raw) = raw
                && raw.state.work_mode != basic.operation_mode()
            {
                let basic_command = basic.command();
                info!(
                    "📤 Basic work mode of {} after the stale mode 0: {:?} (hardware reads {:?}) - {:?}",
                    id, basic, raw.state.work_mode, basic_command
                );
                writer.write_command_queued(id.clone(), basic_command);
                state.basic.insert(
                    id.clone(),
                    BasicWrite {
                        target: Some(basic),
                        last_write: Some(now),
                        attempts: 1,
                        gave_up_logged: false,
                    },
                );
            }
            link.curtail_sent = match command {
                VppCommand::PvAndBattery { pv_limit_w, .. } if curtail => match link.curtail_sent {
                    Some((old, raised_at)) if pv_limit_w <= old => Some((pv_limit_w, raised_at)),
                    _ => Some((pv_limit_w, now)),
                },
                _ => None,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InverterOperationMode as M;

    const ALL_MODES: [M; 6] = [
        M::SelfUse,
        M::BackUpMode,
        M::ForceCharge,
        M::ForceDischarge,
        M::NoChargeNoDischarge,
        M::FeedinPriority,
    ];

    fn limits() -> VppLimits {
        VppLimits {
            battery_w: 8000,
            max_export_w: 9000,
            max_soc: 100.0,
            discharge_floor: 20.0,
        }
    }

    fn inputs(soc: f32, pv: f32, load: f32) -> VppInputs {
        VppInputs {
            soc: Some(soc),
            pv_w: Some(pv),
            load_w: Some(load),
        }
    }

    const SELF_USE: BasicMode = BasicMode::SelfUse;
    const BACKUP: BasicMode = BasicMode::Backup { min_soc_pct: 30 };

    /// Every plan mode × basic mode (brief design §2), SOC 50 %, PV 2 kW, load 1 kW
    #[test]
    fn mapping_every_mode_on_every_basic_mode() {
        let i = inputs(50.0, 2000.0, 1000.0);
        let expected = |mode: M, basic: BasicMode| match (mode, basic) {
            (M::SelfUse, BasicMode::SelfUse) => VppCommand::Exit,
            (M::SelfUse, BasicMode::Backup { .. }) => VppCommand::SelfConsume,
            (M::BackUpMode, BasicMode::Backup { .. }) => VppCommand::Exit,
            (M::BackUpMode, BasicMode::SelfUse) => VppCommand::ChargeOnly,
            (M::ForceCharge, _) => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: -8000,
            },
            // PV uncapped, push = 9000 + 1000 − 2000 (R6)
            (M::ForceDischarge, _) => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: 8000,
            },
            (M::NoChargeNoDischarge, _) => VppCommand::ChargeOnly,
            // PV 2 kW below 9000 + 1000: uncapped (R6)
            (M::FeedinPriority, _) => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: 0,
            },
        };
        for basic in [SELF_USE, BACKUP] {
            for mode in ALL_MODES {
                let (command, _) = command_for(mode, basic, None, &i, &limits(), None);
                assert_eq!(command, expected(mode, basic), "{mode:?} on {basic:?}");
            }
        }
    }

    #[test]
    fn notes_for_behaviour_that_differs_from_the_eeprom_mode() {
        let i = inputs(50.0, 2000.0, 1000.0);
        let note = |mode, basic| command_for(mode, basic, None, &i, &limits(), None).1;
        assert_eq!(
            note(M::FeedinPriority, BACKUP),
            Some(VppNote::FeedInBatteryIdle)
        );
        assert_eq!(
            note(M::BackUpMode, SELF_USE),
            Some(VppNote::BackupAsChargeOnly)
        );
        assert_eq!(note(M::ForceCharge, SELF_USE), None);
    }

    /// Discharge clamp: meter export = PV + push − load ≤ the cap
    #[test]
    fn discharge_is_clamped_to_the_export_cap() {
        let l = limits();
        // PV 6 kW, load 0.5 kW: push ≤ 9000 + 500 − 6000 = 3500
        assert_eq!(discharge_push_w(&inputs(80.0, 6000.0, 500.0), &l), 3500);
        // PV alone above cap + load: no push
        assert_eq!(discharge_push_w(&inputs(80.0, 9800.0, 500.0), &l), 0);
        // Night, house 2 kW: limited by the battery
        assert_eq!(discharge_push_w(&inputs(80.0, 0.0, 2000.0), &l), 8000);
        // Rounded down to 100 W
        assert_eq!(discharge_push_w(&inputs(80.0, 6050.0, 500.0), &l), 3400);
        // Unknown PV or load: no discharge
        let unknown = VppInputs {
            soc: Some(80.0),
            pv_w: None,
            load_w: Some(500.0),
        };
        assert_eq!(discharge_push_w(&unknown, &l), 0);
        let (command, note) = command_for(M::ForceDischarge, SELF_USE, None, &unknown, &l, None);
        assert_eq!(
            command,
            VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: 0
            }
        );
        assert_eq!(note, Some(VppNote::DischargeUnclamped));
        // Every clamped push keeps export at or under the cap
        for pv in (0..12000).step_by(700) {
            for load in (0..6000).step_by(450) {
                let i = inputs(80.0, pv as f32, load as f32);
                let push = discharge_push_w(&i, &l);
                assert!(push >= 0 && push <= l.battery_w);
                assert!(
                    push == 0 || pv + push - load <= l.max_export_w,
                    "pv {pv} load {load} push {push}"
                );
            }
        }
    }

    #[test]
    fn soc_guards_stop_grid_charge_and_discharge() {
        let l = limits();
        let full = inputs(100.0, 2000.0, 1000.0);
        assert_eq!(
            command_for(M::ForceCharge, SELF_USE, None, &full, &l, None).0,
            VppCommand::ChargeOnly
        );
        let empty = inputs(20.0, 0.0, 1000.0);
        assert_eq!(
            command_for(M::ForceDischarge, BACKUP, None, &empty, &l, None).0,
            VppCommand::ChargeOnly
        );
    }

    fn curtail(mode: M, i: &VppInputs, prev: Option<VppCommand>) -> (VppCommand, Option<VppNote>) {
        command_for(
            mode,
            SELF_USE,
            Some(&mut CurtailMemory::default()),
            i,
            &limits(),
            prev,
        )
    }

    #[test]
    fn curtailment_is_mode_7_unless_the_battery_cannot_take_the_surplus() {
        // SOC 50, surplus 5 kW within the 8 kW cap: mode 7 (the inverter holds the meter at 0)
        let i = inputs(50.0, 6000.0, 1000.0);
        assert_eq!(curtail(M::SelfUse, &i, None).0, VppCommand::ChargeOnly);
        assert_eq!(
            curtail(M::NoChargeNoDischarge, &i, None).0,
            VppCommand::ChargeOnly
        );
        // Surplus 9 kW above the cap: mode 8, PV for the house + the cap, rounded up
        let over = inputs(50.0, 10_000.0, 1050.0);
        assert_eq!(
            curtail(M::SelfUse, &over, None).0,
            VppCommand::PvAndBattery {
                pv_limit_w: 9100,
                push_w: -8000
            }
        );
        // Battery full: mode 8 with PV for the house only (rounded up: a slight export rather
        // than an import), when there is a surplus
        let full = inputs(100.0, 6000.0, 1234.0);
        assert_eq!(
            curtail(M::SelfUse, &full, None).0,
            VppCommand::PvAndBattery {
                pv_limit_w: 1300,
                push_w: 0
            }
        );
        assert_eq!(
            curtail(M::SelfUse, &inputs(100.0, 1300.0, 1234.0), None).0,
            VppCommand::ChargeOnly
        );
        // Force charge: full charge power, PV for house + charge (grid tops up)
        assert_eq!(
            curtail(M::ForceCharge, &i, None).0,
            VppCommand::PvAndBattery {
                pv_limit_w: 9000,
                push_w: -8000
            }
        );
        // No discharge into a curtailed grid
        assert_eq!(
            curtail(M::ForceDischarge, &i, None).0,
            VppCommand::PvAndBattery {
                pv_limit_w: 1000,
                push_w: 0
            }
        );
        // Unknown load: mode 7 while the battery has room, else a hard stop
        let unknown = |soc| VppInputs {
            soc: Some(soc),
            pv_w: Some(6000.0),
            load_w: None,
        };
        assert_eq!(
            curtail(M::SelfUse, &unknown(50.0), None),
            (VppCommand::ChargeOnly, None)
        );
        assert_eq!(
            curtail(M::SelfUse, &unknown(100.0), None),
            (
                VppCommand::PvAndBattery {
                    pv_limit_w: 0,
                    push_w: 0
                },
                Some(VppNote::CurtailHardStop)
            )
        );
    }

    /// One plant read: PV = sun in mode 7 (battery takes the surplus up to the cap, nothing at
    /// full), PV = min(sun, limit) in mode 8 (battery takes the push exactly). Returns the grid
    /// beyond the house's own deficit (+ export, − import for the battery).
    fn plant(command: VppCommand, sun: i32, load: i32, soc: f32) -> (f32, i32) {
        let cap = 8000;
        let (pv, grid) = match command {
            VppCommand::ChargeOnly => {
                let charge = if soc < 100.0 {
                    (sun - load).clamp(0, cap)
                } else {
                    0
                };
                (sun, sun - load - charge)
            }
            VppCommand::PvAndBattery { pv_limit_w, push_w } => {
                let pv = sun.min(pv_limit_w as i32);
                (pv, pv - load + push_w)
            }
            other => panic!("unexpected {other:?}"),
        };
        (pv as f32, grid - (sun - load).min(0))
    }

    /// Closed loop over sun × load × SOC: settles within a few reads, never flaps; mode 7
    /// while the battery takes the surplus (meter 0), mode 8 only above the cap or at full
    /// battery (meter within one 100 W step, export side), never an import for the battery
    #[test]
    fn curtailment_loop_settles_without_import_or_flapping() {
        for soc in [50.0, 100.0] {
            for sun in (0..14000).step_by(700) {
                for load in [300, 1000, 2550, 4000] {
                    let mut prev = None;
                    let mut memory = CurtailMemory::default();
                    let mut pv = sun as f32;
                    let mut history = Vec::new();
                    for _ in 0..20 {
                        let i = inputs(soc, pv, load as f32);
                        let command =
                            curtail_command(prev, &mut memory, pv, load as f32, i.soc, &limits());
                        let (read_pv, grid) = plant(command, sun, load, soc);
                        pv = read_pv;
                        history.push((command, grid));
                        prev = Some(command);
                    }
                    let (last, last_grid) = history[19];
                    let ctx = format!("soc {soc} sun {sun} load {load}: {history:?}");
                    assert!(history[4..].iter().all(|(c, _)| *c == last), "flaps: {ctx}");
                    let surplus = sun - load;
                    let mode8 = if soc < 100.0 {
                        surplus > 8000
                    } else {
                        surplus > 100
                    };
                    if mode8 {
                        assert!(matches!(last, VppCommand::PvAndBattery { .. }), "{ctx}");
                        assert!((0..POWER_STEP_W).contains(&last_grid), "{ctx}");
                    } else {
                        assert_eq!(last, VppCommand::ChargeOnly, "{ctx}");
                        assert!((0..=POWER_STEP_W).contains(&last_grid), "{ctx}");
                    }
                    assert!(history.iter().all(|(_, g)| *g >= 0), "import: {ctx}");
                }
            }
        }
    }

    /// Mode 7 ↔ 8 hysteresis: noise around the cap does not flap; a real sun drop (2 reads)
    /// or the SOC released below the maximum goes back to mode 7
    #[test]
    fn curtailment_mode_7_8_hysteresis() {
        let l = limits();
        let mut memory = CurtailMemory::default();
        let mut prev = None;
        let mut switches = 0;
        // Load 1 kW, sun around 9 kW ± 400 W: surplus around the 8 kW cap
        for (n, sun) in [9100.0_f32, 8600.0, 9400.0, 8700.0, 9300.0, 8650.0, 9200.0]
            .into_iter()
            .cycle()
            .take(60)
            .enumerate()
        {
            let pv = match prev {
                Some(VppCommand::PvAndBattery { pv_limit_w, .. }) => sun.min(pv_limit_w as f32),
                _ => sun,
            };
            let command = curtail_command(prev, &mut memory, pv, 1000.0, Some(50.0), &l);
            if prev.is_some_and(|p| p.mode() != command.mode()) {
                switches += 1;
            }
            assert!(n == 0 || command.mode() == 8, "read {n}: {command:?}");
            prev = Some(command);
        }
        assert_eq!(switches, 0);
        // One sun-limited read: still mode 8; the second: mode 7
        let m8 = prev;
        let _ = curtail_command(m8, &mut memory, 9000.0, 1000.0, Some(50.0), &l);
        assert_eq!(
            curtail_command(m8, &mut memory, 7000.0, 1000.0, Some(50.0), &l).mode(),
            8
        );
        assert_eq!(
            curtail_command(m8, &mut memory, 7000.0, 1000.0, Some(50.0), &l),
            VppCommand::ChargeOnly
        );

        // Full battery: mode 8 holds while the SOC dithers at the maximum, ends 1 % below
        let mut memory = CurtailMemory::default();
        let full = curtail_command(None, &mut memory, 6000.0, 1000.0, Some(100.0), &l);
        assert_eq!(
            full,
            VppCommand::PvAndBattery {
                pv_limit_w: 1000,
                push_w: 0
            }
        );
        for soc in [99.9, 100.0, 99.5, 99.1] {
            assert_eq!(
                curtail_command(Some(full), &mut memory, 1000.0, 1000.0, Some(soc), &l),
                full,
                "soc {soc}"
            );
        }
        assert_eq!(
            curtail_command(Some(full), &mut memory, 1000.0, 1000.0, Some(98.9), &l),
            VppCommand::ChargeOnly
        );
        // Mode 8 at the cap reaching full SOC: the charge goes to 0
        let at_cap = VppCommand::PvAndBattery {
            pv_limit_w: 9000,
            push_w: -8000,
        };
        assert_eq!(
            curtail_command(Some(at_cap), &mut memory, 9000.0, 1000.0, Some(100.0), &l),
            full
        );
    }

    /// Basic work mode: writes only while the read-back differs, every 5 min, 3 tries, then one
    /// give-up; none in steady state over an hour
    #[test]
    fn basic_mode_writes_are_bounded() {
        let backup = BasicMode::Backup { min_soc_pct: 30 };
        let mut entry = BasicWrite::default();
        let mut writes = 0;
        let mut give_ups = 0;
        for s in (0..3600).step_by(10) {
            match basic_action(&mut entry, backup, M::SelfUse, None, t(s)) {
                BasicAction::Write(c) => {
                    assert_eq!(c, InverterCommand::SetBackupMode { min_soc_pct: 30 });
                    writes += 1;
                }
                BasicAction::GiveUp => give_ups += 1,
                BasicAction::None => {}
            }
        }
        assert_eq!((writes, give_ups), (3, 1));

        // Steady state: hardware in Back Up, minimum confirmed: no write in an hour
        let mut entry = BasicWrite::default();
        for s in (0..3600).step_by(10) {
            assert_eq!(
                basic_action(&mut entry, backup, M::BackUpMode, Some(30), t(s)),
                BasicAction::None
            );
        }
        // Back Up read back but minimum not confirmed: one minimum-only sync, no mode write
        let mut entry = BasicWrite::default();
        assert_eq!(
            basic_action(&mut entry, backup, M::BackUpMode, None, t(0)),
            BasicAction::Write(InverterCommand::SetBackupMinSoc { min_soc_pct: 30 })
        );
        assert_eq!(
            basic_action(&mut entry, backup, M::BackUpMode, Some(30), t(10)),
            BasicAction::None
        );
        // A new target starts over
        let mut entry = BasicWrite::default();
        for s in (0..1000).step_by(10) {
            basic_action(&mut entry, backup, M::SelfUse, None, t(s));
        }
        assert_eq!(
            basic_action(&mut entry, BasicMode::SelfUse, M::BackUpMode, None, t(1000)),
            BasicAction::Write(InverterCommand::SetMode(M::SelfUse))
        );
    }

    #[test]
    fn command_parameters_and_options_match_the_integration() {
        assert_eq!(VppCommand::Exit.parameters(), vec![]);
        assert_eq!(VppCommand::Exit.select_option(), "Disabled");
        assert_eq!(
            VppCommand::Push { push_w: -8000 }.parameters(),
            vec![("push_mode_power", -8000), ("timeout", 0)]
        );
        assert_eq!(
            VppCommand::Push { push_w: 0 }.select_option(),
            "Push Power-Positive/Negative Mode"
        );
        assert_eq!(
            VppCommand::SelfConsume.select_option(),
            "Self-Consume - Charge/Discharge Mode"
        );
        assert_eq!(
            VppCommand::ChargeOnly.select_option(),
            "Self-Consume - Charge Only Mode"
        );
        let m8 = VppCommand::PvAndBattery {
            pv_limit_w: 3000,
            push_w: -3000,
        };
        assert_eq!(m8.select_option(), "Individual Setting - Duration Mode");
        // Duration never 0; NextMotion 160 = exit to the basic work mode
        assert_eq!(
            m8.parameters(),
            vec![
                ("pv_power_limit", 3000),
                ("push_mode_power_8_9", -3000),
                ("duration_8", 180),
                ("timeout_8_9", 160)
            ]
        );
        assert_eq!(
            m8.to_string(),
            "mode 8 {pv_power_limit=3000, push_mode_power_8_9=-3000, duration_8=180, timeout_8_9=160}"
        );
    }

    #[test]
    fn readback_labels_decode_from_the_integration_table() {
        assert_eq!(decode_power_control("Disabled"), Some(0));
        assert_eq!(decode_power_control("Push Power - P/N Mode"), Some(4));
        assert_eq!(decode_power_control("Push Power - Zero Mode"), Some(5));
        assert_eq!(decode_power_control("Self Consume - C/D Mode"), Some(6));
        assert_eq!(
            decode_power_control("Self Consume - Charge Only Mode"),
            Some(7)
        );
        assert_eq!(
            decode_power_control("Individual Setting - Duration Mode"),
            Some(8)
        );
        // Unknown labels, the select's own option text and HA states are not modes
        for label in [
            "Self Use",
            "Push Power-Positive/Negative Mode",
            "unknown",
            "unavailable",
            "",
        ] {
            assert_eq!(decode_power_control(label), None, "{label}");
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn resend_scheduler_keeps_alive_and_holds_the_minimum_gap() {
        let charge = VppCommand::Push { push_w: -8000 };
        let mut link = VppLink::default();
        assert!(link.due(&charge, t(0)), "first command at once");
        link.sent(charge, t(0));
        assert!(
            !link.due(&charge, t(30)),
            "same command before the keep-alive"
        );
        assert!(link.due(&charge, t(60)), "keep-alive");
        // Another mode waits for the 20 s gap
        assert!(!link.due(&VppCommand::ChargeOnly, t(10)));
        assert!(link.due(&VppCommand::ChargeOnly, t(20)));
        // A small push change waits for the keep-alive, a material one does not
        assert!(!link.due(&VppCommand::Push { push_w: -7900 }, t(30)));
        assert!(link.due(&VppCommand::Push { push_w: -7000 }, t(30)));
        // A stop is never held, and mode 0 is not kept alive
        assert!(link.due(&VppCommand::Exit, t(1)));
        link.sent(VppCommand::Exit, t(1));
        assert!(!link.due(&VppCommand::Exit, t(500)));
        // Every keep-alive across 10 minutes stays well inside the ~600 s mode 4 lapse and
        // the 180 s mode 8 duration
        let mut link = VppLink::default();
        let mut sends = Vec::new();
        for s in (0..600).step_by(5) {
            if link.due(&charge, t(s)) {
                link.sent(charge, t(s));
                sends.push(s);
            }
        }
        assert!(sends.windows(2).all(|w| w[1] - w[0] <= KEEPALIVE_SECS));
        const { assert!(KEEPALIVE_SECS < MODE8_DURATION_S as i64) };
    }

    #[test]
    fn readback_mismatch_warns_then_falls_back_without_looping() {
        let charge = VppCommand::Push { push_w: -8000 };
        let mut link = VppLink::default();
        link.sent(charge, t(0));
        // Within the lag: not compared
        assert_eq!(link.observe(t(10), Some(0)), Readback::Pending);
        assert_eq!(link.observe(t(40), Some(4)), Readback::Confirmed);
        // The same read is not counted twice
        assert_eq!(link.observe(t(40), Some(6)), Readback::Pending);
        assert_eq!(link.observe(t(70), Some(6)), Readback::Mismatch);
        // One count per minute; an unknown read is no evidence
        assert_eq!(link.observe(t(100), Some(6)), Readback::Pending);
        assert_eq!(link.observe(t(130), None), Readback::Pending);
        assert_eq!(link.observe(t(135), Some(6)), Readback::Resend);
        assert!(link.due(&charge, t(136)), "forced re-send");
        link.sent(charge, t(136));
        // The re-send restarts the lag window
        assert_eq!(link.observe(t(160), Some(6)), Readback::Pending);
        assert_eq!(link.observe(t(196), Some(6)), Readback::Mismatch);
        assert_eq!(link.observe(t(256), Some(6)), Readback::Mismatch);
        assert_eq!(link.observe(t(316), Some(6)), Readback::Fail);
        assert!(link.failed);
        link.sent(VppCommand::Exit, t(317));
        // After the fall-back: no more counting, no loop
        for s in (350..2000).step_by(30) {
            assert_ne!(link.observe(t(s), Some(6)), Readback::Fail);
        }
        assert!(link.failed);
        link.reset_failure();
        assert!(!link.failed);
    }

    /// At the real state-read cadence (5 s) a lasting mismatch warns after ~1.5 min and falls
    /// back after ~4.5 min, not within seconds; an unreadable sensor never falls back
    #[test]
    fn readback_at_the_5_s_read_cadence() {
        let charge = VppCommand::Push { push_w: -8000 };
        let mut link = VppLink::default();
        link.sent(charge, t(0));
        let mut resend_at = None;
        let mut fail_at = None;
        for s in (5..600).step_by(5) {
            match link.observe(t(s), Some(6)) {
                Readback::Resend => {
                    assert!(resend_at.is_none(), "one re-send");
                    resend_at = Some(s);
                    link.sent(charge, t(s));
                }
                Readback::Fail => {
                    fail_at = Some(s);
                    break;
                }
                _ => {}
            }
        }
        let (resend_at, fail_at) = (resend_at.unwrap(), fail_at.unwrap());
        assert!((60..=120).contains(&resend_at), "re-send at {resend_at}");
        assert!(fail_at >= 240, "fall-back at {fail_at}");

        let mut link = VppLink::default();
        link.sent(charge, t(0));
        for s in (5..3600).step_by(5) {
            assert_eq!(link.observe(t(s), None), Readback::Pending);
        }
        assert!(!link.failed);
    }

    #[test]
    fn a_confirmed_readback_resets_the_mismatch_count() {
        let mut link = VppLink::default();
        link.sent(VppCommand::ChargeOnly, t(0));
        assert_eq!(link.observe(t(40), Some(6)), Readback::Mismatch);
        assert_eq!(link.observe(t(70), Some(7)), Readback::Confirmed);
        assert_eq!(link.observe(t(100), Some(6)), Readback::Mismatch);
    }

    /// After a fall-back the plan is retried every 15 min, mismatch count reset (owner R4)
    #[test]
    fn fallback_is_retried_every_15_minutes() {
        let mut link = VppLink::default();
        assert_eq!(link.fallback_retry(t(0)), None);
        link.failed = true;
        link.mismatches = MISMATCH_FAIL;
        assert_eq!(link.fallback_retry(t(100)), None, "starts the clock");
        assert_eq!(link.fallback_retry(t(100 + 899)), None);
        assert_eq!(link.fallback_retry(t(100 + 900)), Some(1));
        assert!(!link.failed);
        assert_eq!(link.mismatches, 0);
        link.failed = true;
        assert_eq!(link.fallback_retry(t(2000)), None);
        assert_eq!(link.fallback_retry(t(2900)), Some(2));
    }

    /// Curtailment mode 8 may be re-sent after 10 s; other commands keep the 20 s gap (R1)
    #[test]
    fn curtailment_mode_8_has_a_10_s_gap() {
        let mut link = VppLink::default();
        let a = VppCommand::PvAndBattery {
            pv_limit_w: 1000,
            push_w: 0,
        };
        let b = VppCommand::PvAndBattery {
            pv_limit_w: 1500,
            push_w: 0,
        };
        link.sent(a, t(0));
        assert!(!link.due_with_gap(&b, t(9), CURTAIL_WRITE_GAP_SECS));
        assert!(link.due_with_gap(&b, t(10), CURTAIL_WRITE_GAP_SECS));
        assert!(!link.due(&b, t(10)));
        // Unchanged: the 60 s keep-alive only
        assert!(!link.due_with_gap(&a, t(30), CURTAIL_WRITE_GAP_SECS));
        assert!(link.due_with_gap(&a, t(60), CURTAIL_WRITE_GAP_SECS));
    }

    #[test]
    fn only_a_grid_push_arms_the_grid_charge_check() {
        assert!(VppCommand::Push { push_w: -100 }.grid_charges());
        assert!(
            VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: -8000
            }
            .grid_charges()
        );
        assert!(!VppCommand::Push { push_w: 100 }.grid_charges());
        let m8 = VppCommand::PvAndBattery {
            pv_limit_w: 3000,
            push_w: -2000,
        };
        assert!(m8.charges() && !m8.grid_charges());
    }

    /// R6: from one read, Feed-in caps PV at cap + load only when PV reaches it, and Force
    /// Discharge leaves PV uncapped with push = cap + load − PV; unknown PV or load: uncapped
    #[test]
    fn feed_in_and_discharge_start_from_one_read() {
        let cap9 = VppLimits {
            max_export_w: 9000,
            ..limits()
        };
        let at = |pv_limit_w, push_w| VppCommand::PvAndBattery { pv_limit_w, push_w };
        let start = |mode, pv, load| {
            command_for(mode, SELF_USE, None, &inputs(80.0, pv, load), &cap9, None).0
        };
        // Feed-in, PV 15 kW, load 2 kW, cap 9 kW: 9000 + 2000
        assert_eq!(start(M::FeedinPriority, 15_000.0, 2000.0), at(11_000, 0));
        // PV below cap + load: uncapped
        assert_eq!(
            start(M::FeedinPriority, 8000.0, 2000.0),
            at(PV_UNCAPPED_W, 0)
        );
        // Rounded up to 100 W
        assert_eq!(start(M::FeedinPriority, 10_300.0, 1234.0), at(10_300, 0));
        // Force Discharge, PV 8 kW, load 2 kW: push 9000 + 2000 − 8000
        assert_eq!(
            start(M::ForceDischarge, 8000.0, 2000.0),
            at(PV_UNCAPPED_W, 3000)
        );
        // Unknown PV or load: uncapped
        let unknown = VppInputs {
            soc: Some(80.0),
            pv_w: None,
            load_w: Some(2000.0),
        };
        assert_eq!(feed_in_pv_limit_w(&unknown, &cap9), PV_UNCAPPED_W);
    }

    /// R7-5: the push rises only beyond the up band, max(min(200 W, max(1 step, 4 σ̂)), 10 % of
    /// the load): ±200 W read noise on a steady sun moves it at most a few times, a quiet read
    /// rises by one step, and a real rise of the target is followed
    #[test]
    fn the_push_up_band_holds_noise_and_follows_a_rise() {
        let cap9 = VppLimits {
            max_export_w: 9000,
            ..limits()
        };
        for seed in [42u64, 7, 1234, 2026, 99] {
            let mut rng = seed;
            let mut noise = || {
                rng = rng
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((rng >> 33) % 401) as f32 - 200.0
            };
            let mut memory = ExportMemory::default();
            let mut pushes = Vec::new();
            for read in 0..120 {
                let sun = if read < 90 { 7000.0 } else { 6500.0 };
                let command = memory.step(
                    ExportKind::Discharge,
                    sun + noise(),
                    1000.0 + noise(),
                    &cap9,
                    t(read * 5),
                );
                memory.sent(command, t(read * 5));
                let VppCommand::PvAndBattery { push_w, .. } = command else {
                    panic!("{command:?}");
                };
                pushes.push(push_w);
            }
            let rises = pushes[12..90].windows(2).filter(|w| w[1] > w[0]).count();
            assert!(
                rises <= 2,
                "seed {seed}: noise raised the push {rises} times: {pushes:?}"
            );
            // 500 W less sun: the push follows within 6 reads
            assert!(
                pushes[96..].iter().all(|p| *p >= pushes[89] + 300),
                "seed {seed}: {pushes:?}"
            );
        }
        // Quiet: one step up
        let mut memory = ExportMemory::default();
        let mut push = 0;
        for read in 0..60 {
            let sun = if read < 40 { 7000.0 } else { 6900.0 };
            let command = memory.step(
                ExportKind::Discharge,
                sun,
                1000.0,
                &cap9,
                t(1000 + read * 5),
            );
            memory.sent(command, t(1000 + read * 5));
            if let VppCommand::PvAndBattery { push_w, .. } = command {
                if read == 39 {
                    push = push_w;
                }
                if read == 59 {
                    assert_eq!(push_w, push + 100, "quiet: one step up");
                }
            }
        }
    }

    /// R6/R7: the regimes of one quiet read sequence: discharge (PV uncapped) below cap + load,
    /// the curtail regime above it; its limit follows a load drop of 2 steps, a rise beyond the
    /// clip margin, an import at once (urgent); settled sun-limited reads end it
    #[test]
    fn export_regimes_follow_sun_and_load() {
        let cap9 = VppLimits {
            max_export_w: 9000,
            ..limits()
        };
        let mut memory = ExportMemory::default();
        let reads: Vec<(f32, f32)> = [
            vec![(8500.0, 1000.0); 12],
            vec![(8500.0, 4000.0); 3],
            vec![(8500.0, 1000.0); 3],
            vec![(10_600.0, 1000.0)],
            vec![(9500.0, 1000.0); 10],
            vec![
                (9500.0, 1150.0),
                (9500.0, 4000.0),
                (9500.0, 1000.0),
                (9500.0, 9800.0),
            ],
            vec![(6000.0, 1000.0); 14],
        ]
        .concat();
        let mut out = Vec::new();
        for (i, (pv, load)) in reads.iter().enumerate() {
            let at = t(i as i64 * 5);
            let command = memory.step(ExportKind::Discharge, *pv, *load, &cap9, at);
            let urgent = memory.urgent();
            memory.sent(command, at);
            out.push((command, urgent));
        }
        let at = |pv_limit_w, push_w| VppCommand::PvAndBattery { pv_limit_w, push_w };
        let pushes = |range: std::ops::Range<usize>| -> Vec<i32> {
            out[range]
                .iter()
                .map(|(c, _)| match c {
                    VppCommand::PvAndBattery { push_w, .. } => *push_w,
                    _ => -1,
                })
                .collect()
        };
        // Discharge: push 9000 + 1000 − 8500, the first reads 1.5 σ of the prior lower, rising
        // by the up band as the estimate settles
        assert_eq!(out[0], (at(PV_UNCAPPED_W, 1200), false));
        assert_eq!(pushes(6..12), vec![1500; 6]);
        // A load step: the push follows at once; back down: urgent
        assert_eq!(out[12], (at(PV_UNCAPPED_W, 4400), false));
        assert_eq!(out[15], (at(PV_UNCAPPED_W, 1400), true));
        // PV 600 W over cap + load: the curtail regime at once, limit 9000 + 1000
        assert_eq!(out[18], (at(10_000, 0), false));
        // Clipped at 95 % of the limit: not sun-limited, the regime holds
        assert!(out[19..30].iter().all(|o| *o == (at(10_000, 0), false)));
        // Load +3 kW: the limit follows; back to 1 kW: down
        assert_eq!(out[30], (at(13_000, 0), false));
        assert_eq!(out[31], (at(10_000, 0), false));
        // The house imports (load above PV): the limit follows at once, urgent
        assert_eq!(out[32], (at(18_800, 0), true));
        // Sun-limited (PV 6 kW under the 10 kW limit) for 2 settled reads after the read-back
        // lag of the last limit change: discharge, push 9000 + 1000 − 6000
        assert!(out[33..41].iter().all(|o| o.0 == at(10_000, 0)));
        assert_eq!(out[41], (at(PV_UNCAPPED_W, 4000), false));
        assert_eq!(pushes(41..47), vec![4000; 6]);
    }

    /// N8: the decided path, not the config, picks the writer; `Pending` writes nothing
    #[test]
    fn the_decided_path_picks_the_writer() {
        let control = fluxion_types::ControlConfig::default();
        assert_eq!(active_path(&control, None), ActivePath::Vpp);
        let state = VppPathState::default();
        assert_eq!(active_path(&control, Some(&state)), ActivePath::Pending);
        state.set(ModePath::Eeprom("2026.09.2".to_owned()));
        assert_eq!(active_path(&control, Some(&state)), ActivePath::Eeprom);
        assert!(control.vpp_direct_enabled, "the option is never changed");
        state.set(ModePath::Direct { no_mode8: vec![] });
        assert_eq!(active_path(&control, Some(&state)), ActivePath::Vpp);
        let off = fluxion_types::ControlConfig {
            vpp_direct_enabled: false,
            ..Default::default()
        };
        assert_eq!(active_path(&off, Some(&state)), ActivePath::Eeprom);
    }

    /// D5: without mode 8 the force modes are mode 4 with the same push, Hold is mode 7, Self
    /// Use in Back Up mode 6; a curtailment needing mode 8 asks for the export limit
    #[test]
    fn modes_1_to_7_without_mode_8() {
        let i = inputs(50.0, 2000.0, 1000.0);
        for basic in [SELF_USE, BACKUP] {
            for mode in ALL_MODES {
                let (mode8, _) = command_for(mode, basic, None, &i, &limits(), None);
                let expected = match (mode, mode8) {
                    (M::ForceCharge, _) => VppCommand::Push { push_w: -8000 },
                    // The mode 8 push: 9000 + 1000 − 2000
                    (M::ForceDischarge, _) => VppCommand::Push { push_w: 8000 },
                    (M::FeedinPriority, _) => VppCommand::Push { push_w: 0 },
                    (M::NoChargeNoDischarge, c) => {
                        assert_eq!(c, VppCommand::ChargeOnly);
                        c
                    }
                    (_, c) => {
                        assert!(c.mode() != 8, "{mode:?} {c}");
                        c
                    }
                };
                assert_eq!(
                    without_mode8(mode8, mode, false),
                    (expected, false),
                    "{mode:?} on {basic:?}"
                );
            }
        }
        // The discharge push carries over: 9000 + 500 − 6000
        let (mode8, _) = command_for(
            M::ForceDischarge,
            SELF_USE,
            None,
            &inputs(80.0, 6000.0, 500.0),
            &limits(),
            None,
        );
        assert_eq!(
            without_mode8(mode8, M::ForceDischarge, false),
            (VppCommand::Push { push_w: 3500 }, false)
        );

        // Curtailment: mode 7 while the battery absorbs, no export limit
        let curtailed = |mode: M, i: &VppInputs| {
            let mut memory = CurtailMemory::default();
            let (c, _) = command_for(mode, SELF_USE, Some(&mut memory), i, &limits(), None);
            without_mode8(c, mode, true)
        };
        assert_eq!(
            curtailed(M::SelfUse, &inputs(50.0, 2000.0, 1000.0)),
            (VppCommand::ChargeOnly, false)
        );
        // Full battery with a surplus, or a surplus above the charge cap: mode 8 needed
        assert_eq!(
            curtailed(M::SelfUse, &inputs(100.0, 4000.0, 1000.0)),
            (VppCommand::ChargeOnly, true)
        );
        assert_eq!(
            curtailed(M::SelfUse, &inputs(50.0, 12000.0, 1000.0)),
            (VppCommand::ChargeOnly, true)
        );
        assert_eq!(
            curtailed(M::ForceCharge, &inputs(50.0, 2000.0, 1000.0)),
            (VppCommand::Push { push_w: -8000 }, true)
        );
        assert_eq!(
            curtailed(M::ForceDischarge, &inputs(50.0, 2000.0, 1000.0)),
            (VppCommand::Push { push_w: 0 }, true)
        );
        // Hard stop without figures (full battery)
        let unknown = VppInputs {
            soc: Some(100.0),
            ..Default::default()
        };
        assert_eq!(
            curtailed(M::SelfUse, &unknown),
            (VppCommand::ChargeOnly, true)
        );
    }

    /// Parallel site without a house load sensor: the load balance counts the slaves' battery
    /// power too (PV is the site's), so the discharge clamp is site-wide
    #[test]
    fn load_balance_counts_the_parallel_batteries() {
        let now = Utc::now();
        let raw = RawInverterState {
            state: crate::traits::GenericInverterState {
                pv_power_w: 10_000.0,
                battery_power_w: 3000.0,
                extra_battery_power_w: Some(2500.0),
                grid_power_w: 1000.0,
                house_load_w: None,
                ..Default::default()
            },
            last_updated: now,
        };
        assert_eq!(inputs_of(None, Some(&raw), now).load_w, Some(3500.0));
    }

    #[test]
    fn basic_mode_follows_the_user_default() {
        assert_eq!(BasicMode::of(None), BasicMode::SelfUse);
        let mut uc = UserControlState::default();
        assert_eq!(BasicMode::of(Some(&uc)), BasicMode::SelfUse);
        uc.default_mode = DefaultMode::Backup;
        uc.backup_min_soc_pct = 40;
        assert_eq!(
            BasicMode::of(Some(&uc)),
            BasicMode::Backup { min_soc_pct: 40 }
        );
        assert_eq!(
            BasicMode::of(Some(&uc)).command(),
            InverterCommand::SetBackupMode { min_soc_pct: 40 }
        );
        assert_eq!(
            BasicMode::SelfUse.command(),
            InverterCommand::SetMode(M::SelfUse)
        );
    }
    /// A failed write (reported by the writer) is re-sent after the minimum gap, not at once
    /// and not only at the keep-alive; a failed mode 0 too
    #[test]
    fn a_failed_write_is_re_sent_after_the_gap() {
        let charge = VppCommand::Push { push_w: -8000 };
        let mut link = VppLink::default();
        link.sent(charge, t(0));
        link.write_failed = true;
        assert!(!link.due(&charge, t(10)));
        assert!(link.due(&charge, t(MIN_WRITE_GAP_SECS)));
        link.sent(charge, t(MIN_WRITE_GAP_SECS));
        assert!(
            !link.due(&charge, t(MIN_WRITE_GAP_SECS * 2)),
            "cleared by the send"
        );

        link.sent(VppCommand::Exit, t(100));
        assert!(!link.due(&VppCommand::Exit, t(130)));
        link.write_failed = true;
        assert!(!link.due(&VppCommand::Exit, t(105)));
        assert!(link.due(&VppCommand::Exit, t(100 + MIN_WRITE_GAP_SECS)));
    }

    /// Full battery, closed loop with the measured plant: PV clipped 150 W below the limit, two
    /// reads (10 s) before a write shows in PV, writes 10 s apart, a 2 kW load step. Mode 8 holds (no false
    /// flip to mode 7, which would export the whole surplus); a real sun drop below the house
    /// load goes to mode 7 after two settled reads
    #[test]
    fn full_battery_mode_8_holds_through_clip_shortfall_and_a_load_step() {
        let l = limits();
        let mut memory = CurtailMemory::default();
        let mut prev: Option<VppCommand> = None;
        let mut last_send: Option<(VppCommand, i64)> = None;
        let mut curtail_sent: Option<(u32, i64)> = None;
        let mut applied: Option<VppCommand> = None;
        let mut staged: std::collections::VecDeque<Option<VppCommand>> = [None, None].into();
        let mut history = Vec::new();
        for at in (0..600).step_by(5) {
            let load = if at < 200 { 1000 } else { 3000 };
            let sun = if at < 400 { 6000 } else { 1500 };
            let pv = match applied {
                Some(VppCommand::PvAndBattery { pv_limit_w, .. }) if sun > pv_limit_w as i32 => {
                    pv_limit_w as i32 - 150
                }
                _ => sun,
            };
            applied = staged.pop_front().flatten().or(applied);
            memory.sent_limit_w = curtail_sent.map(|(limit, _)| limit);
            memory.settling =
                curtail_sent.is_none_or(|(_, raised)| at < raised + READBACK_LAG_SECS);
            let command =
                curtail_command(prev, &mut memory, pv as f32, load as f32, Some(100.0), &l);
            prev = Some(command);
            let due = last_send
                .is_none_or(|(c, sent_at)| c != command && at - sent_at >= CURTAIL_WRITE_GAP_SECS);
            staged.push_back(due.then_some(command));
            if due {
                last_send = Some((command, at));
                curtail_sent = match command {
                    VppCommand::PvAndBattery { pv_limit_w, .. } => match curtail_sent {
                        Some((old, raised)) if pv_limit_w <= old => Some((pv_limit_w, raised)),
                        _ => Some((pv_limit_w, at)),
                    },
                    _ => None,
                };
            }
            history.push((at, pv, command));
        }
        for (at, pv, command) in &history {
            let ctx = format!("{at} s, PV {pv}: {command:?}");
            if *at < 400 {
                assert_eq!(command.mode(), 8, "false flip at {ctx}");
            }
            if *at >= 420 {
                assert_eq!(*command, VppCommand::ChargeOnly, "{ctx}");
            }
        }
        // Settled on the new load (rounded up)
        assert!(history.iter().any(|(at, _, c)| *at < 400
            && *c
                == VppCommand::PvAndBattery {
                    pv_limit_w: 3000,
                    push_w: 0
                }));
    }
}

/// The executor, export curtailment and VPP systems together, with the option off and on.
#[cfg(test)]
mod ecs_tests {
    use super::*;
    const FORCE_CHARGE: VppCommand = VppCommand::PvAndBattery {
        pv_limit_w: PV_UNCAPPED_W,
        push_w: -8000,
    };
    use crate::components::{OperationSchedule, ScheduledMode, SpotPriceData};
    use crate::continuous_systems::{
        ExportCurtailmentState, InitialModeSyncTracker, export_curtailment_system,
        schedule_execution_system,
    };
    use crate::execution::ExecutionConfig;
    use crate::resources::{
        AsyncInverterWriter, InverterConfig, InverterTopology, PriceSchedule, PricingConfig,
        SystemConfig, SystemSettingsConfig, UserControlResource, UserLeversResource,
    };
    use crate::traits::{GenericInverterState, InverterDataSource};
    use InverterOperationMode as M;
    use async_trait::async_trait;
    use fluxion_types::pricing::TimeBlockPrice;
    use fluxion_types::{UserControlState, UserLevers};
    use std::sync::Mutex;

    type Sent = Arc<Mutex<Vec<InverterCommand>>>;

    /// Records every write and its inverter; `2` makes the target set type check refuse, `3`
    /// fail (transient), as it always does for an inverter "flaky"
    #[derive(Default)]
    struct Recorder(
        Sent,
        Arc<Mutex<Vec<String>>>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    );

    #[async_trait]
    impl InverterDataSource for Recorder {
        async fn read_state(&self, _id: &str) -> anyhow::Result<GenericInverterState> {
            Ok(GenericInverterState::default())
        }
        async fn write_command(&self, id: &str, command: &InverterCommand) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(command.clone());
            self.1.lock().unwrap().push(id.to_owned());
            if *command == InverterCommand::EnsureVppTargetSetType && self.2.load(Ordering::SeqCst)
            {
                return Err(TargetTypeRefused(
                    "select.solax_power_control_mode_target_set_type_direct is 'Update'".to_owned(),
                )
                .into());
            }
            // "flaky": its check always fails (several controlled masters)
            if *command == InverterCommand::EnsureVppTargetSetType
                && (self.3.load(Ordering::SeqCst) || id == "flaky")
            {
                anyhow::bail!("cannot read the target set type: HA timeout");
            }
            Ok(())
        }
        async fn health_check(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn name(&self) -> &str {
            "recorder"
        }
    }

    struct Site {
        world: World,
        schedule: Schedule,
        sent: Sent,
        /// Inverter id of every write
        ids: Arc<Mutex<Vec<String>>>,
        /// The target set type check refuses, or fails (transient)
        fail_target: Arc<AtomicBool>,
        fail_target_transient: Arc<AtomicBool>,
    }

    /// One inverter (hardware: basic Self Use, SOC 50 %, PV 2 kW, house 1 kW), a plan of
    /// `mode` for the current block, sell price `sell`, and `control.vpp_direct_enabled = vpp`.
    fn new_site(mode: M, vpp: bool, sell: f32) -> Site {
        let recorder = Recorder::default();
        let sent = recorder.0.clone();
        let ids = recorder.1.clone();
        let fail_target = recorder.2.clone();
        let fail_target_transient = recorder.3.clone();
        let now = Utc::now();
        let mut world = World::new();
        world.insert_resource(AsyncInverterWriter::new(Arc::new(recorder)));
        world.insert_resource(ExecutionConfig {
            min_mode_change_interval_secs: 300,
        });
        world.insert_resource(DebugModeConfig {
            enabled: false,
            ..Default::default()
        });
        world.insert_resource(SystemConfig {
            inverters: vec![InverterConfig {
                id: "solax".to_owned(),
                inverter_type: fluxion_types::InverterType::Solax,
                entity_prefix: "solax".to_owned(),
                control_entity_prefix: String::new(),
                topology: InverterTopology::Independent,
                pv_extra_prefixes: Vec::new(),
            }],
            pricing_config: PricingConfig {
                spot_price_entity: "sensor.spot_price".to_owned(),
                tomorrow_price_entity: None,
                buy_price_entity: None,
                use_spot_prices_to_buy: true,
                use_spot_prices_to_sell: true,
                fixed_buy_price_czk: PriceSchedule::Flat(4.0),
                fixed_sell_price_czk: PriceSchedule::Flat(2.0),
                spot_buy_fee_czk: 0.5,
                spot_sell_fee_czk: 0.5,
                hdo_sensor_entity: "sensor.cez_hdo_raw_data".to_owned(),
                hdo_low_tariff_czk: 0.5,
                hdo_high_tariff_czk: 1.8,
            },
            control_config: fluxion_types::ControlConfig {
                maximum_export_power_w: 9000,
                max_battery_charge_rate_kw: 8.0,
                force_discharge_enabled: true,
                export_curtailment_enabled: true,
                vpp_direct_enabled: vpp,
                ..Default::default()
            },
            system_config: SystemSettingsConfig {
                update_interval_secs: 60,
                debug_mode: false,
                display_currency: fluxion_types::config::Currency::CZK,
                language: fluxion_i18n::Language::English,
                timezone: None,
                display_timezone: None,
            },
            strategies_config: Default::default(),
            history: Default::default(),
            solar_forecast: Default::default(),
            remote_access: Default::default(),
            notifications: Default::default(),
        });
        world.insert_resource(InitialModeSyncTracker::default());
        world.insert_resource(UserControlResource::new(UserControlState::default()));
        world.insert_resource(UserLeversResource {
            levers: UserLevers::default(),
            available: true,
            sync_expected: false,
        });
        world.insert_resource(ExportCurtailmentState::default());
        world.insert_resource(VppCurtailment::default());
        world.insert_resource(VppLive::default());
        world.spawn((
            Inverter {
                id: "solax".to_owned(),
                inverter_type: fluxion_types::InverterType::Solax,
            },
            CurrentMode::default(),
            BatteryStatus {
                soc_percent: 50,
                ..Default::default()
            },
            RawInverterState {
                state: GenericInverterState {
                    work_mode: M::SelfUse,
                    pv_power_w: 2000.0,
                    house_load_w: Some(1000.0),
                    export_limit_w: Some(9000),
                    export_limit_max_w: Some(15000),
                    ..Default::default()
                },
                last_updated: now,
            },
        ));
        world.spawn(OperationSchedule {
            scheduled_blocks: vec![ScheduledMode {
                block_start: now - Duration::minutes(1),
                duration_minutes: 15,
                target_inverters: None,
                mode,
                reason: "plan".to_owned(),
                decision_uid: None,
                debug_info: None,
            }],
            generated_at: now,
            based_on_price_version: now,
        });
        world.spawn(SpotPriceData {
            time_block_prices: vec![TimeBlockPrice {
                buy_price_czk_per_kwh: None,
                block_start: now - Duration::minutes(1),
                duration_minutes: 15,
                price_czk_per_kwh: 1.0,
                effective_price_czk_per_kwh: 1.5,
                spot_sell_price_czk_per_kwh: Some(sell),
            }],
            block_duration_minutes: 15,
            fetched_at: now,
            ha_last_updated: now,
            fallback_block_starts: Vec::new(),
        });
        let mut schedule = Schedule::default();
        schedule.add_systems(
            (
                schedule_execution_system,
                export_curtailment_system,
                vpp_direct_system,
            )
                .chain(),
        );
        Site {
            world,
            schedule,
            sent,
            ids,
            fail_target,
            fail_target_transient,
        }
    }

    impl Site {
        async fn run(&mut self) -> Vec<InverterCommand> {
            self.schedule.run(&mut self.world);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
        fn user_control(&mut self) -> Mut<'_, UserControlResource> {
            self.world.resource_mut::<UserControlResource>()
        }
        fn set_debug(&mut self, enabled: bool) {
            self.world.resource_mut::<DebugModeConfig>().enabled = enabled;
        }
        fn control(&mut self) -> &mut fluxion_types::ControlConfig {
            &mut self
                .world
                .resource_mut::<SystemConfig>()
                .into_inner()
                .control_config
        }
        /// One state read of the plant: PV, house load and the VPP read-back at `read_at`
        fn set_plant(&mut self, pv_w: f32, load_w: f32, vpp_mode: u8, read_at: DateTime<Utc>) {
            let mut q = self.world.query::<&mut RawInverterState>();
            for mut raw in q.iter_mut(&mut self.world) {
                raw.state.pv_power_w = pv_w;
                raw.state.house_load_w = Some(load_w);
                raw.state.vpp_mode = Some(vpp_mode);
                raw.last_updated = read_at;
            }
        }
        fn set_soc(&mut self, soc: u16) {
            let mut q = self.world.query::<&mut BatteryStatus>();
            for mut battery in q.iter_mut(&mut self.world) {
                battery.soc_percent = soc;
            }
        }
        fn set_sell(&mut self, sell: f32) {
            let mut q = self.world.query::<&mut SpotPriceData>();
            for mut prices in q.iter_mut(&mut self.world) {
                prices.time_block_prices[0].spot_sell_price_czk_per_kwh = Some(sell);
            }
        }
        fn set_hardware(&mut self, work_mode: M, vpp_mode: Option<u8>, read_at: DateTime<Utc>) {
            let mut q = self.world.query::<&mut RawInverterState>();
            for mut raw in q.iter_mut(&mut self.world) {
                raw.state.work_mode = work_mode;
                raw.state.vpp_mode = vpp_mode;
                raw.last_updated = read_at;
            }
        }
    }

    const PLAN_MODES: [M; 5] = [
        M::SelfUse,
        M::ForceCharge,
        M::ForceDischarge,
        M::NoChargeNoDischarge,
        M::FeedinPriority,
    ];

    /// Option off: every plan mode is written exactly as today (one SetMode of that mode on the
    /// first run, nothing VPP), and a curtailed block writes the export limit.
    #[tokio::test]
    async fn option_off_writes_todays_commands_for_every_plan_mode() {
        for mode in PLAN_MODES {
            let mut site = new_site(mode, false, 2.0);
            let expected = if mode == M::SelfUse {
                // Hardware already reads Self Use: nothing to write
                vec![]
            } else {
                vec![InverterCommand::SetMode(mode)]
            };
            assert_eq!(site.run().await, expected, "{mode:?}");
            assert_eq!(site.run().await, vec![], "{mode:?} second run");
        }
        // Back Up default: planner Self Use is Back Up Mode with the minimum SOC
        let mut site = new_site(M::SelfUse, false, 2.0);
        site.user_control().state.default_mode = DefaultMode::Backup;
        site.user_control().state.backup_min_soc_pct = 30;
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetBackupMode { min_soc_pct: 30 }]
        );
        // Curtailment (sell ≤ 0): export limit write as today
        let mut site = new_site(M::SelfUse, false, -1.0);
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetExportLimit(
                fluxion_types::ControlConfig::default().export_limit_min_w
            )]
        );
    }

    /// Option on: no work mode write for any block, one VPP command per plan mode
    #[tokio::test]
    async fn option_on_sends_the_vpp_command_of_every_plan_mode() {
        let expected = |mode: M| match mode {
            M::SelfUse => VppCommand::Exit,
            M::ForceCharge => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: -8000,
            },
            // cap 9000 − (PV − load) 1000, less 1.5 σ of the 200 W noise prior on the first read
            M::ForceDischarge => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: 7700,
            },
            M::NoChargeNoDischarge => VppCommand::ChargeOnly,
            M::FeedinPriority => VppCommand::PvAndBattery {
                pv_limit_w: PV_UNCAPPED_W,
                push_w: 0,
            },
            M::BackUpMode => unreachable!(),
        };
        for mode in PLAN_MODES {
            let mut site = new_site(mode, true, 2.0);
            assert_eq!(
                site.run().await,
                vec![InverterCommand::SetVpp(expected(mode))],
                "{mode:?}"
            );
            // Not re-sent within the keep-alive
            assert_eq!(site.run().await, vec![], "{mode:?} second run");
            assert_eq!(
                site.world.resource::<VppLive>().is_live(),
                mode != M::SelfUse
            );
        }
    }

    /// Back Up default: the basic work mode is written once (EEPROM), Self Use blocks are mode
    /// 0 and an explicit Self Use is mode 6
    #[tokio::test]
    async fn option_on_writes_the_basic_mode_only_when_it_differs() {
        let mut site = new_site(M::SelfUse, true, 2.0);
        site.user_control().state.default_mode = DefaultMode::Backup;
        site.user_control().state.backup_min_soc_pct = 30;
        let sent = site.run().await;
        assert_eq!(
            sent,
            vec![
                InverterCommand::SetBackupMode { min_soc_pct: 30 },
                InverterCommand::SetVpp(VppCommand::Exit)
            ]
        );
        // Hardware now reads Back Up and the minimum is confirmed: no more EEPROM writes
        site.set_hardware(M::BackUpMode, Some(0), Utc::now());
        for _ in 0..3 {
            assert_eq!(site.run().await, vec![]);
        }
        // Hardware already in Back Up at startup: only the minimum SOC sync, no mode write
        let mut site2 = new_site(M::SelfUse, true, 2.0);
        site2.user_control().state.default_mode = DefaultMode::Backup;
        site2.user_control().state.backup_min_soc_pct = 30;
        site2.set_hardware(M::BackUpMode, None, Utc::now());
        assert_eq!(
            site2.run().await,
            vec![
                InverterCommand::SetBackupMinSoc { min_soc_pct: 30 },
                InverterCommand::SetVpp(VppCommand::Exit)
            ]
        );
        // Hardware left in Manual by the EEPROM path: Self Use basic mode written once
        let mut site3 = new_site(M::ForceCharge, true, 2.0);
        site3.set_hardware(M::ForceCharge, None, Utc::now());
        assert_eq!(
            site3.run().await,
            vec![
                InverterCommand::SetMode(M::SelfUse),
                InverterCommand::SetVpp(FORCE_CHARGE)
            ]
        );
    }

    #[tokio::test]
    async fn option_on_curtails_by_vpp_and_never_writes_the_export_limit() {
        let mut site = new_site(M::SelfUse, true, -1.0);
        let sent = site.run().await;
        // PV 2 kW, house 1 kW, SOC 50 %: the battery takes the surplus, mode 7
        assert_eq!(sent, vec![InverterCommand::SetVpp(VppCommand::ChargeOnly)]);
    }

    #[tokio::test]
    async fn kill_switch_debug_and_mode_control_off_write_mode_0() {
        // Kill switch: mode 0 once, then the configured default work mode (owner ruling R3)
        let mut site = new_site(M::ForceCharge, true, 2.0);
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(FORCE_CHARGE)]
        );
        site.user_control().state.enabled = false;
        site.set_hardware(M::ForceCharge, Some(4), Utc::now());
        assert_eq!(
            site.run().await,
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                InverterCommand::SetMode(M::SelfUse)
            ]
        );
        assert_eq!(site.run().await, vec![]);
        assert!(!site.world.resource::<VppLive>().is_live());

        // Automation lever: same stop
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.run().await;
        site.world
            .resource_mut::<UserLeversResource>()
            .levers
            .automation_disabled = true;
        assert_eq!(
            site.run().await,
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                InverterCommand::SetMode(M::SelfUse)
            ]
        );

        // Debug mode switched on: mode 0 once, then only logs
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.run().await;
        site.set_debug(true);
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(VppCommand::Exit)]
        );
        assert_eq!(site.run().await, vec![]);

        // Debug from the start: nothing written at all
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.set_debug(true);
        assert_eq!(site.run().await, vec![]);

        // Mode control off: one mode 0, then only export limit writes (H37)
        let mut site = new_site(M::ForceCharge, true, -1.0);
        site.world
            .resource_mut::<SystemConfig>()
            .control_config
            .mode_control_enabled = false;
        let sent = site.run().await;
        assert!(sent.contains(&InverterCommand::SetVpp(VppCommand::Exit)));
        assert!(
            sent.iter().all(|c| matches!(
                c,
                InverterCommand::SetVpp(VppCommand::Exit) | InverterCommand::SetExportLimit(_)
            )),
            "{sent:?}"
        );
        assert!(
            site.run()
                .await
                .iter()
                .all(|c| matches!(c, InverterCommand::SetExportLimit(_)))
        );
    }

    /// The kill switch's mode 0 is checked: a read-back still in mode 4 gets it re-sent
    #[tokio::test]
    async fn kill_switch_mode_0_is_re_sent_when_the_read_back_disagrees() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.run().await;
        site.user_control().state.enabled = false;
        assert_eq!(
            site.run().await,
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                InverterCommand::SetMode(M::SelfUse)
            ]
        );
        let now = Utc::now();
        site.set_hardware(M::SelfUse, Some(4), now + Duration::seconds(40));
        assert_eq!(site.run().await, vec![]);
        site.set_hardware(M::SelfUse, Some(4), now + Duration::seconds(70));
        assert_eq!(site.run().await, vec![], "one count per minute");
        site.set_hardware(M::SelfUse, Some(4), now + Duration::seconds(100));
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(VppCommand::Exit)]
        );
        // Confirmed: nothing more
        site.set_hardware(M::SelfUse, Some(0), now + Duration::seconds(160));
        assert_eq!(site.run().await, vec![]);
        // The owner takes over by hand (mode 4): FluxION leaves it alone
        for (i, at) in [200, 260, 320, 380, 440, 500].into_iter().enumerate() {
            site.set_hardware(M::SelfUse, Some(4), now + Duration::seconds(at));
            assert_eq!(site.run().await, vec![], "read {i}");
        }
    }

    /// Debug switched on while the link is already in mode 0: nothing is checked or written,
    /// even when someone sets a VPP mode by hand
    #[tokio::test]
    async fn debug_mode_never_overwrites_manual_control() {
        let mut site = new_site(M::SelfUse, true, 2.0);
        site.run().await;
        site.set_debug(true);
        assert_eq!(site.run().await, vec![]);
        let now = Utc::now();
        for at in [40, 100, 160, 220, 280, 340] {
            site.set_hardware(M::SelfUse, Some(4), now + Duration::seconds(at));
            assert_eq!(site.run().await, vec![], "at {at}");
        }
    }

    /// Curtailment does not wait for the first plan; block modes do
    #[tokio::test]
    async fn curtailment_runs_before_the_first_plan() {
        for (sell, expected) in [
            (-1.0, vec![InverterCommand::SetVpp(VppCommand::ChargeOnly)]),
            (2.0, vec![]),
        ] {
            let mut site = new_site(M::ForceCharge, true, sell);
            let plans: Vec<Entity> = site
                .world
                .query_filtered::<Entity, With<OperationSchedule>>()
                .iter(&site.world)
                .collect();
            for plan in plans {
                site.world.despawn(plan);
            }
            assert_eq!(site.run().await, expected, "sell {sell}");
        }
    }

    #[test]
    fn live_commands_are_tracked_per_inverter() {
        let live = VppLive::default();
        live.set("a", true);
        live.set("b", true);
        live.set("a", false);
        assert_eq!(live.live_inverters(), vec!["b".to_owned()]);
        assert!(live.is_live());
        assert!(!live.is_stopping());
        live.stop();
        assert!(live.is_stopping());
    }

    /// Kill switch on an inverter left in Manual (Force Discharge from the EEPROM path): mode 0
    /// alone would hand control back to Manual, so the basic work mode is written too
    #[tokio::test]
    async fn kill_switch_also_corrects_a_manual_basic_work_mode() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.run().await;
        site.set_hardware(M::ForceDischarge, Some(4), Utc::now());
        site.user_control().state.enabled = false;
        let sent = site.run().await;
        assert!(
            sent.contains(&InverterCommand::SetVpp(VppCommand::Exit)),
            "{sent:?}"
        );
        assert!(
            sent.contains(&InverterCommand::SetMode(M::SelfUse)),
            "{sent:?}"
        );
        // Debug mode: neither
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.set_debug(true);
        site.run().await;
        site.set_hardware(M::ForceDischarge, Some(4), Utc::now());
        site.user_control().state.enabled = false;
        assert_eq!(site.run().await, vec![]);
    }

    /// A link that fell back to mode 0 still curtails export at negative prices: by the export
    /// limit, as with the option off
    #[tokio::test]
    async fn after_the_fall_back_curtailment_uses_the_export_limit() {
        let mut site = new_site(M::SelfUse, true, -1.0);
        let first = site.run().await;
        assert!(
            first
                .iter()
                .all(|c| !matches!(c, InverterCommand::SetExportLimit(_))),
            "{first:?}"
        );
        let now = Utc::now();
        let mut sent = Vec::new();
        for at in (40..=400).step_by(30) {
            site.set_hardware(M::SelfUse, Some(0), now + Duration::seconds(at));
            sent.extend(site.run().await);
            if sent.contains(&InverterCommand::SetVpp(VppCommand::Exit)) {
                break;
            }
        }
        assert!(
            sent.contains(&InverterCommand::SetVpp(VppCommand::Exit)),
            "fall-back: {sent:?}"
        );
        let after = site.run().await;
        assert!(
            after
                .iter()
                .any(|c| matches!(c, InverterCommand::SetExportLimit(_))),
            "{after:?}"
        );
        assert!(
            after
                .iter()
                .all(|c| !matches!(c, InverterCommand::SetVpp(v) if *v != VppCommand::Exit)),
            "{after:?}"
        );
    }

    /// Shutdown mode 0 goes right after the write in flight: queued keep-alives are dropped
    #[tokio::test]
    async fn shutdown_mode_0_drops_queued_keep_alives() {
        let recorder = Recorder::default();
        let sent = recorder.0.clone();
        let writer = AsyncInverterWriter::new(Arc::new(recorder));
        let keep_alive = InverterCommand::SetVpp(VppCommand::Push { push_w: -1000 });
        for _ in 0..20 {
            writer.write_command_async("solax".to_owned(), keep_alive.clone());
        }
        let exit = InverterCommand::SetVpp(VppCommand::Exit);
        writer
            .write_vpp_and_wait("solax".to_owned(), exit.clone())
            .await
            .unwrap();
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.last(), Some(&exit));
        assert!(sent.len() < 21, "{} writes", sent.len());
    }

    /// Kill switch with a Back Up default: mode 0, then Back Up with its minimum (R3)
    #[tokio::test]
    async fn kill_switch_writes_the_configured_default_after_mode_0() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        {
            let mut uc = site.user_control();
            uc.state.default_mode = DefaultMode::Backup;
            uc.state.backup_min_soc_pct = 40;
        }
        site.world
            .resource::<AsyncInverterWriter>()
            .set_confirmed_backup_min_soc("solax", 40);
        site.set_hardware(M::BackUpMode, None, Utc::now());
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(FORCE_CHARGE)]
        );
        site.user_control().state.enabled = false;
        assert_eq!(
            site.run().await,
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                InverterCommand::SetBackupMode { min_soc_pct: 40 }
            ]
        );
        assert_eq!(site.run().await, vec![]);
    }
    /// Kill switch: mode 0 and the default once; a manual change made after that is left alone
    /// for an hour (no basic work mode write, no VPP write)
    #[tokio::test]
    async fn kill_switch_leaves_a_later_manual_change_alone() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let t0 = Utc::now();
        site.world.insert_resource(VppClock(t0));
        site.run().await;
        site.user_control().state.enabled = false;
        assert_eq!(
            site.run().await,
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                InverterCommand::SetMode(M::SelfUse)
            ]
        );
        // The real 5 s state read cadence (N9)
        for at in (5..3600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::FeedinPriority, Some(0), now);
            assert_eq!(site.run().await, vec![], "{at} s");
        }
    }

    /// After a fall-back the 15 min retries leave the export limit curtailing until the
    /// read-back confirms a VPP command again (no export limit write per retry)
    #[tokio::test]
    async fn fall_back_retries_keep_the_export_limit_until_confirmed() {
        let mut site = new_site(M::SelfUse, true, -1.0);
        let t0 = Utc::now();
        site.world.insert_resource(VppClock(t0));
        site.run().await;
        let fallback = |site: &Site| {
            site.world
                .resource::<VppCurtailment>()
                .fallback
                .contains("solax")
        };
        let mut limit_writes = 0;
        let mut retries_seen = 0;
        // The read-back never shows mode 7: fall-back, then retries every 15 min for 1 h
        for at in (5..3600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, Some(0), now);
            let sent = site.run().await;
            // A restore of the stored limit (9000 W) between retries would toggle the EEPROM
            limit_writes += sent
                .iter()
                .filter(|c| matches!(c, InverterCommand::SetExportLimit(w) if *w > 0))
                .count();
            if at > 400 {
                retries_seen += sent
                    .iter()
                    .filter(|c| **c == InverterCommand::SetVpp(VppCommand::ChargeOnly))
                    .count();
            }
            if at > 400 {
                assert!(fallback(&site), "export limit dropped at {at} s");
            }
        }
        assert!(retries_seen >= 2, "{retries_seen} retries");
        assert_eq!(limit_writes, 0, "no export limit restore per retry");

        // The read-back confirms mode 7 after a retry: back on VPP
        for at in (3600..5400).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, Some(7), now);
            site.run().await;
            if !fallback(&site) {
                return;
            }
        }
        panic!("never confirmed");
    }

    /// The VPP queue keeps only the latest pending non-zero command; mode 0 and other commands
    /// are never dropped and keep their order
    #[tokio::test]
    async fn vpp_queue_coalesces_keep_alives_but_never_drops_mode_0() {
        let recorder = Recorder::default();
        let sent = recorder.0.clone();
        let writer = AsyncInverterWriter::new(Arc::new(recorder));
        let push = |push_w| InverterCommand::SetVpp(VppCommand::Push { push_w });
        for w in 0..20 {
            writer.write_command_async("solax".to_owned(), push(-w));
        }
        writer.write_command_async(
            "solax".to_owned(),
            InverterCommand::SetVpp(VppCommand::Exit),
        );
        for w in 20..40 {
            writer.write_command_async("solax".to_owned(), push(-w));
        }
        writer.write_command_queued("solax".to_owned(), InverterCommand::SetMode(M::SelfUse));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                InverterCommand::SetVpp(VppCommand::Exit),
                push(-39),
                InverterCommand::SetMode(M::SelfUse)
            ]
        );
    }

    /// D5, an X3-Ultra (no mode 8): the force modes and Feed-in are mode 4, Hold mode 7
    #[tokio::test]
    async fn without_mode_8_the_plan_modes_are_modes_4_and_7() {
        let expected = |mode: M| match mode {
            M::SelfUse => VppCommand::Exit,
            M::ForceCharge => VppCommand::Push { push_w: -8000 },
            // The first read keeps 1.5 σ of the noise prior below cap − (PV − load)
            M::ForceDischarge => VppCommand::Push { push_w: 7700 },
            M::NoChargeNoDischarge => VppCommand::ChargeOnly,
            M::FeedinPriority => VppCommand::Push { push_w: 0 },
            M::BackUpMode => unreachable!(),
        };
        for mode in PLAN_MODES {
            let mut site = new_site(mode, true, 2.0);
            site.control().vpp_no_mode8 = vec!["solax".to_owned()];
            assert_eq!(
                site.run().await,
                vec![InverterCommand::SetVpp(expected(mode))],
                "{mode:?}"
            );
            assert_eq!(site.run().await, vec![], "{mode:?} second run");
        }
    }

    /// D5: without mode 8, curtailment is mode 7 while the battery absorbs the surplus; when
    /// mode 8 would be needed (full battery) the export limit curtails, for the rest of the
    /// curtailment (a confirmed VPP read-back does not drop it), and is restored after it
    #[tokio::test]
    async fn without_mode_8_curtailment_uses_the_export_limit_when_needed() {
        let min_w = fluxion_types::ControlConfig::default().export_limit_min_w;
        let mut site = new_site(M::SelfUse, true, -1.0);
        site.control().vpp_no_mode8 = vec!["solax".to_owned()];
        let start = Utc::now();
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(VppCommand::ChargeOnly)]
        );
        assert!(site.world.resource::<VppCurtailment>().fallback.is_empty());

        // Battery full, surplus 1 kW: mode 8 needed, absent → the export limit
        site.set_soc(100);
        site.set_hardware(M::SelfUse, Some(7), start + Duration::seconds(40));
        let sent = site.run().await;
        assert!(
            sent.iter()
                .all(|c| !matches!(c, InverterCommand::SetVpp(v) if v.mode() == 8)),
            "{sent:?}"
        );
        assert!(
            site.world
                .resource::<VppCurtailment>()
                .fallback
                .contains("solax")
        );
        let sent = site.run().await;
        assert!(
            sent.contains(&InverterCommand::SetExportLimit(min_w)),
            "{sent:?}"
        );
        // The read-back confirms mode 7: the export limit stays
        site.set_hardware(M::SelfUse, Some(7), start + Duration::seconds(100));
        site.run().await;
        assert!(
            site.world
                .resource::<VppCurtailment>()
                .fallback
                .contains("solax")
        );

        // Curtailment over: the limit is restored and VPP has the inverter alone again
        site.set_sell(2.0);
        let mut sent = site.run().await;
        sent.extend(site.run().await);
        assert!(
            sent.contains(&InverterCommand::SetExportLimit(9000)),
            "{sent:?}"
        );
        assert!(site.world.resource::<VppCurtailment>().fallback.is_empty());
    }

    /// D5: a parallel site runs direct VPP: commands go to the master only (never to the
    /// slave), with the site's total battery power, not per unit
    #[tokio::test]
    async fn parallel_site_writes_vpp_to_the_master_only() {
        for mode in [M::ForceCharge, M::ForceDischarge, M::NoChargeNoDischarge] {
            let mut site = new_site(mode, true, 2.0);
            site.control().vpp_no_mode8 = vec!["solax".to_owned()];
            {
                let mut config = site.world.resource_mut::<SystemConfig>();
                config.inverters[0].topology = InverterTopology::Master {
                    slave_ids: vec!["slave".to_owned()],
                };
                let mut slave = config.inverters[0].clone();
                slave.id = "slave".to_owned();
                slave.topology = InverterTopology::Slave {
                    master_id: "solax".to_owned(),
                };
                config.inverters.push(slave);
            }
            site.world.spawn((
                Inverter {
                    id: "slave".to_owned(),
                    inverter_type: fluxion_types::InverterType::Solax,
                },
                CurrentMode::default(),
                BatteryStatus {
                    soc_percent: 50,
                    ..Default::default()
                },
                RawInverterState {
                    state: GenericInverterState {
                        work_mode: M::SelfUse,
                        ..Default::default()
                    },
                    last_updated: Utc::now(),
                },
            ));
            let mut sent = Vec::new();
            for _ in 0..3 {
                sent.extend(site.run().await);
            }
            let expected = match mode {
                M::ForceCharge => VppCommand::Push { push_w: -8000 },
                // The first read keeps 1.5 σ of the noise prior below cap − (PV − load)
                M::ForceDischarge => VppCommand::Push { push_w: 7700 },
                _ => VppCommand::ChargeOnly,
            };
            assert_eq!(sent, vec![InverterCommand::SetVpp(expected)], "{mode:?}");
            assert_eq!(*site.ids.lock().unwrap(), vec!["solax".to_owned()]);
            assert_eq!(
                site.world.resource::<VppLive>().live_inverters(),
                vec!["solax"]
            );
        }
    }

    /// N2: while the startup check is pending nothing writes a mode on either path (2 min at
    /// the 5 s cadence); the decided EEPROM path then writes today's mode, and a later switch
    /// to direct VPP sends the VPP command
    #[tokio::test]
    async fn no_mode_write_until_the_path_is_decided() {
        let mut site = new_site(M::ForceCharge, true, -1.0);
        let path = VppPathState::default();
        site.world.insert_resource(path.clone());
        let t0 = Utc::now();
        for at in (0..=120).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, None, now);
            assert_eq!(site.run().await, vec![], "{at} s");
        }
        path.set(ModePath::Eeprom("no complete read within 120 s".to_owned()));
        let sent = site.run().await;
        assert!(
            sent.contains(&InverterCommand::SetMode(M::ForceCharge)),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .all(|c| !matches!(c, InverterCommand::SetVpp(_))),
            "{sent:?}"
        );
        // Decided outside debug: verified by the startup check
        path.set_target_verified(true);
        path.set(ModePath::Direct { no_mode8: vec![] });
        site.world
            .insert_resource(VppClock(t0 + Duration::seconds(200)));
        let sent = site.run().await;
        assert!(
            sent.iter()
                .any(|c| matches!(c, InverterCommand::SetVpp(v) if v.mode() == 8)),
            "{sent:?}"
        );
    }

    /// N4: a non-zero VPP mode left by an earlier run is ended with one mode 0 before any plan
    /// exists, and nothing more is written for 10 min at the 5 s cadence
    #[tokio::test]
    async fn a_leftover_command_is_cleared_once_before_the_plan() {
        let mut site = new_site(M::SelfUse, true, 2.0);
        let schedules: Vec<Entity> = site
            .world
            .query_filtered::<Entity, With<crate::components::OperationSchedule>>()
            .iter(&site.world)
            .collect();
        for entity in schedules {
            site.world.despawn(entity);
        }
        let t0 = Utc::now();
        site.world.insert_resource(VppClock(t0));
        site.set_hardware(M::SelfUse, Some(4), t0);
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(VppCommand::Exit)]
        );
        for at in (5..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, Some(if at < 20 { 4 } else { 0 }), now);
            assert_eq!(site.run().await, vec![], "{at} s");
        }
    }

    /// N6: the kill switch already on at start writes nothing when the hardware is already in
    /// the default mode with no VPP command; with a live command and another work mode, mode 0
    /// and the default once
    #[tokio::test]
    async fn kill_switch_at_start_writes_only_what_differs() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.user_control().state.enabled = false;
        let t0 = Utc::now();
        for at in (0..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, Some(0), now);
            assert_eq!(site.run().await, vec![], "{at} s");
        }

        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.user_control().state.enabled = false;
        site.world.insert_resource(VppClock(t0));
        site.set_hardware(M::ForceCharge, Some(4), t0);
        // R4-4: mode 0 only, no basic work mode write
        assert_eq!(
            site.run().await,
            vec![InverterCommand::SetVpp(VppCommand::Exit)]
        );
        for at in (5..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::SelfUse, Some(0), now);
            assert_eq!(site.run().await, vec![], "{at} s");
        }
    }

    /// R4-4 (owner ruling "mode 0 only"): kill switch on at start, read-back mode 4 and a
    /// manual work mode other than the default: exactly one mode 0 and no work mode write over
    /// 10 min at the 5 s cadence
    #[tokio::test]
    async fn kill_switch_at_start_writes_mode_0_only() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.user_control().state.enabled = false;
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            // The mode 0 takes effect at the next read; the manual work mode stays
            let vpp = if sent.is_empty() { 4 } else { 0 };
            site.set_hardware(M::BackUpMode, Some(vpp), now);
            sent.extend(site.run().await);
        }
        assert_eq!(sent, vec![InverterCommand::SetVpp(VppCommand::Exit)]);
    }

    /// Inverter model for the closed-loop tests (ported from the round-5 review probe): a VPP
    /// command takes effect `lag` s after it was sent; PV = sun below the PV limit, else 95 % of
    /// the limit (the Gen4 clip shortfall); battery = push (0 when the BMS stops discharge);
    /// meter export = PV + battery − load. Reads may carry ±200 W noise on PV and load.
    struct Plant {
        lag: i64,
        noise: bool,
        seed: u64,
        sent: Vec<(i64, VppCommand)>,
        applied: Option<VppCommand>,
    }

    /// What a closed-loop run measured
    #[derive(Debug, Default)]
    struct Run {
        /// VPP writes, and those that changed the command (not keep-alives)
        writes: usize,
        changed: Vec<(i64, VppCommand)>,
        /// Longest stretch with export above cap + 100 W
        longest_over_cap_s: i64,
        discharged_wh: f32,
        /// First time from 180 s with push 0 and the PV limit at cap + load
        handed_over: Option<i64>,
    }

    impl Plant {
        fn new(lag: i64, noise: bool) -> Self {
            Self {
                lag,
                noise,
                seed: 42,
                sent: Vec::new(),
                applied: None,
            }
        }

        fn noise(&mut self) -> f32 {
            if !self.noise {
                return 0.0;
            }
            self.seed = self
                .seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.seed >> 33) % 401) as f32 - 200.0
        }
    }

    /// Run `site` in a closed loop at the 5 s read cadence for `secs`; `env(t)` gives (sun,
    /// load, battery can discharge)
    async fn closed_loop(
        site: &mut Site,
        plant: &mut Plant,
        secs: i64,
        env: impl Fn(i64) -> (f32, f32, bool),
    ) -> Run {
        let cap = site.control().maximum_export_power_w as f32;
        let mut run = Run::default();
        let mut over = 0;
        let t0 = Utc::now();
        let mut last: Option<VppCommand> = None;
        for at in (0..=secs).step_by(5) {
            let (sun, load, can_discharge) = env(at);
            if let Some((_, command)) = plant.sent.iter().rev().find(|(t, _)| t + plant.lag <= at) {
                plant.applied = Some(*command);
            }
            let (limit, push) = match plant.applied {
                Some(VppCommand::PvAndBattery { pv_limit_w, push_w }) => {
                    (pv_limit_w as f32, push_w as f32)
                }
                Some(VppCommand::Push { push_w }) => (f32::MAX, push_w as f32),
                _ => (f32::MAX, 0.0),
            };
            let pv = if sun >= limit { limit * 0.95 } else { sun };
            let battery = if push > 0.0 && !can_discharge {
                0.0
            } else {
                push
            };
            let export = pv + battery - load;
            if plant.applied.is_some() {
                over = if export > cap + 100.0 { over + 5 } else { 0 };
                run.longest_over_cap_s = run.longest_over_cap_s.max(over);
                run.discharged_wh += battery.max(0.0) * 5.0 / 3600.0;
                if at >= 180 && push == 0.0 && limit >= cap + load - 100.0 && limit < f32::MAX {
                    run.handed_over.get_or_insert(at);
                }
            }
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            let (pv_noise, load_noise) = (plant.noise(), plant.noise());
            let mode = plant.applied.map_or(0, |c| c.mode());
            site.set_plant(
                (pv + pv_noise).max(0.0),
                (load + load_noise).max(0.0),
                mode,
                now,
            );
            for command in site.run().await {
                if let InverterCommand::SetVpp(v) = command {
                    run.writes += 1;
                    if last != Some(v) {
                        run.changed.push((at, v));
                    }
                    last = Some(v);
                    plant.sent.push((at, v));
                }
            }
        }
        run
    }

    /// R5-1: steady sun of 8.5 kW, load 1 kW, cap 9 kW: the battery covers the 1.5 kW PV
    /// cannot (≥ 90 % of the ideal energy over 15 min), at 15 s and 30 s plant lag with the
    /// clip shortfall; export never above the cap for longer than the lag
    #[tokio::test]
    async fn force_discharge_sells_up_to_the_cap_under_a_steady_sun() {
        for lag in [15, 30] {
            let mut site = new_site(M::ForceDischarge, true, 2.0);
            let mut plant = Plant::new(lag, false);
            let run = closed_loop(&mut site, &mut plant, 900, |_| (8500.0, 1000.0, true)).await;
            // From the first command applied (one lag)
            let ideal = 1500.0 * (900 - lag) as f32 / 3600.0;
            assert!(
                run.discharged_wh >= 0.9 * ideal,
                "lag {lag}: {:.0} of {ideal:.0} Wh: {:?}",
                run.discharged_wh,
                run.changed
            );
            assert!(run.longest_over_cap_s <= lag, "lag {lag}: {run:?}");
        }
    }

    /// R5-1: Force Discharge starting under a cloud (PV 2 kW) while the sun rises to 15 kW
    /// (60–180 s): push 0 and PV at cap + load within 120 s of the full sun, and it stays
    /// there; export never above the cap for longer than the lag
    #[tokio::test]
    async fn force_discharge_hands_over_to_rising_sun() {
        for lag in [15, 30] {
            let mut site = new_site(M::ForceDischarge, true, 2.0);
            let mut plant = Plant::new(lag, false);
            let run = closed_loop(&mut site, &mut plant, 900, |at| {
                let sun = match at {
                    0..60 => 2000.0,
                    60..180 => 2000.0 + (at - 60) as f32 / 120.0 * 13_000.0,
                    _ => 15_000.0,
                };
                (sun, 1000.0, true)
            })
            .await;
            let at = run.handed_over.expect("hand-over");
            assert!(
                at <= 300,
                "lag {lag}: handed over at {at} s: {:?}",
                run.changed
            );
            let after: Vec<_> = run.changed.iter().filter(|(t, _)| *t > at).collect();
            assert!(
                after.is_empty(),
                "lag {lag}: changed after the hand-over: {after:?}"
            );
            assert!(run.longest_over_cap_s <= lag, "lag {lag}: {run:?}");
        }
    }

    /// R5-5: ±200 W read noise on PV and load, steady sun (5 and 8.5 kW): fewer than 8
    /// changed writes in 15 min (keep-alives apart), export never above the cap for longer
    /// than the lag
    #[tokio::test]
    async fn force_discharge_ignores_read_noise() {
        for sun in [5000.0, 8500.0] {
            for lag in [15, 30] {
                let mut site = new_site(M::ForceDischarge, true, 2.0);
                let mut plant = Plant::new(lag, true);
                let run = closed_loop(&mut site, &mut plant, 900, |_| (sun, 1000.0, true)).await;
                assert!(
                    run.changed.len() < 8,
                    "sun {sun} lag {lag}: {} changed: {:?}",
                    run.changed.len(),
                    run.changed
                );
                assert!(
                    run.longest_over_cap_s <= lag,
                    "sun {sun} lag {lag}: {run:?}"
                );
            }
        }
    }

    /// R4-2: Feed-in at 15 kW sun while the house load drops from 2 to 1 kW: the lower PV limit
    /// is written at once, so export is back at the cap one plant lag later and stays there
    #[tokio::test]
    async fn feed_in_follows_a_load_drop_at_once() {
        for lag in [15, 30] {
            let mut site = new_site(M::FeedinPriority, true, 2.0);
            let mut plant = Plant::new(lag, false);
            let run = closed_loop(&mut site, &mut plant, 600, |at| {
                (15_000.0, if at < 120 { 2000.0 } else { 1000.0 }, true)
            })
            .await;
            assert!(run.longest_over_cap_s <= lag, "lag {lag}: {run:?}");
        }
    }

    /// R5-3: a load drop 5 s after a keep-alive (Feed-in and Force Discharge at full sun, and
    /// Force Discharge under a 5 kW sun): the export-lowering command is not held by the 20 s
    /// write gap, so export is above the cap for no longer than the lag
    #[tokio::test]
    async fn a_load_drop_after_a_keep_alive_is_written_at_once() {
        for (mode, sun) in [
            (M::FeedinPriority, 15_000.0),
            (M::ForceDischarge, 15_000.0),
            (M::ForceDischarge, 5000.0),
        ] {
            for lag in [15, 30] {
                // A first run finds the keep-alive time, the second drops the load 5 s after it
                let mut site = new_site(mode, true, 2.0);
                let mut plant = Plant::new(lag, false);
                let steady = closed_loop(&mut site, &mut plant, 400, |_| (sun, 2000.0, true)).await;
                let keep_alive = plant
                    .sent
                    .iter()
                    .map(|(t, _)| *t)
                    .filter(|t| *t >= 300)
                    .min()
                    .unwrap_or_else(|| panic!("{mode:?} {sun}: no keep-alive: {steady:?}"));
                let drop_at = keep_alive + 5;
                let mut site = new_site(mode, true, 2.0);
                let mut plant = Plant::new(lag, false);
                let run = closed_loop(&mut site, &mut plant, 600, |at| {
                    (sun, if at < drop_at { 2000.0 } else { 1000.0 }, true)
                })
                .await;
                assert!(
                    run.longest_over_cap_s <= lag,
                    "{mode:?} sun {sun} lag {lag}, drop at {drop_at} s: {run:?}"
                );
            }
        }
    }

    /// R4-3: debug mode switched off: the target set type check runs first, and the first VPP
    /// command only after it passed; a failed check switches the run to the EEPROM path with
    /// no VPP command
    #[tokio::test]
    async fn debug_off_checks_the_target_set_type_before_the_first_vpp_command() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        // Decided in debug mode: the target set type is not verified
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path);
        site.set_debug(true);
        for _ in 0..3 {
            assert_eq!(site.run().await, vec![]);
        }
        site.set_debug(false);
        let mut sent = site.run().await;
        sent.extend(site.run().await);
        assert_eq!(
            sent,
            vec![
                InverterCommand::EnsureVppTargetSetType,
                InverterCommand::SetVpp(FORCE_CHARGE)
            ]
        );

        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path.clone());
        site.fail_target.store(true, Ordering::SeqCst);
        site.set_debug(true);
        site.run().await;
        site.set_debug(false);
        let mut sent = site.run().await;
        for _ in 0..3 {
            sent.extend(site.run().await);
        }
        assert_eq!(sent[0], InverterCommand::EnsureVppTargetSetType);
        assert!(
            sent.iter()
                .all(|c| !matches!(c, InverterCommand::SetVpp(_))),
            "{sent:?}"
        );
        assert!(
            matches!(path.get(), ModePath::Eeprom(r) if r.contains("target set type")),
            "{:?}",
            path.get()
        );
        // The EEPROM path then writes the mode
        assert!(
            sent.contains(&InverterCommand::SetMode(M::ForceCharge)),
            "{sent:?}"
        );
    }

    /// R5-2: debug switched off while the startup check is still pending (decided in debug):
    /// the target set type check is the first command once the path is direct
    #[tokio::test]
    async fn debug_off_while_the_path_is_pending_checks_the_target_set_type() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Pending);
        site.world.insert_resource(path.clone());
        site.set_debug(true);
        for _ in 0..3 {
            assert_eq!(site.run().await, vec![]);
        }
        assert!(
            path.debug(),
            "the live debug flag reaches the startup check"
        );
        site.set_debug(false);
        site.run().await;
        assert!(!path.debug());
        // Decided by a check that ran in debug mode: not verified
        path.set(ModePath::Direct { no_mode8: vec![] });
        let mut sent = site.run().await;
        sent.extend(site.run().await);
        assert_eq!(
            sent,
            vec![
                InverterCommand::EnsureVppTargetSetType,
                InverterCommand::SetVpp(FORCE_CHARGE)
            ]
        );
    }

    /// R5-6: a transient failure of the target set type check (HA, transport, unlock) is
    /// retried 10 s after it is seen, with no VPP command meanwhile; the path stays direct
    #[tokio::test]
    async fn a_transient_target_check_failure_is_retried() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path.clone());
        site.fail_target_transient.store(true, Ordering::SeqCst);
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=25).step_by(5) {
            site.world
                .insert_resource(VppClock(t0 + Duration::seconds(at)));
            sent.extend(site.run().await.into_iter().map(|c| (at, c)));
        }
        assert_eq!(
            sent,
            // The result is read on the next 5 s tick; the retry follows 10 s after it
            vec![
                (0, InverterCommand::EnsureVppTargetSetType),
                (15, InverterCommand::EnsureVppTargetSetType)
            ]
        );
        assert_eq!(path.get(), ModePath::Direct { no_mode8: vec![] });
        // HA answers again: verified, the first VPP command follows
        site.fail_target_transient.store(false, Ordering::SeqCst);
        let mut sent = Vec::new();
        for at in (30..=40).step_by(5) {
            site.world
                .insert_resource(VppClock(t0 + Duration::seconds(at)));
            sent.extend(site.run().await);
        }
        assert_eq!(
            sent,
            vec![
                InverterCommand::EnsureVppTargetSetType,
                InverterCommand::SetVpp(FORCE_CHARGE)
            ]
        );
        assert!(path.target_verified("solax"));
    }

    /// R5-4: kill switch on at start while the first read-backs are unknown (sensor not loaded):
    /// the mode 0 waits for a known read-back and is then written exactly once
    #[tokio::test]
    async fn kill_switch_at_start_waits_for_a_known_read_back() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        site.user_control().state.enabled = false;
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            let vpp = if at < 10 {
                None
            } else if sent.is_empty() {
                Some(4)
            } else {
                Some(0)
            };
            site.set_hardware(M::SelfUse, vpp, now);
            sent.extend(site.run().await);
        }
        assert_eq!(sent, vec![InverterCommand::SetVpp(VppCommand::Exit)]);
    }

    /// Kill switch on at start on the EEPROM path (owner ruling R4-4, extended): hands off, no
    /// mode write at all over 10 min, with a manual Back Up on the hardware
    #[tokio::test]
    async fn kill_switch_at_start_on_the_eeprom_path_writes_nothing() {
        let mut site = new_site(M::ForceCharge, false, 2.0);
        site.user_control().state.enabled = false;
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_hardware(M::BackUpMode, None, now);
            sent.extend(site.run().await);
        }
        assert_eq!(sent, vec![]);
    }

    /// Run `site` with both clocks (executor and VPP system) at `now`
    async fn run_at(site: &mut Site, now: DateTime<Utc>) -> Vec<InverterCommand> {
        site.world
            .insert_resource(crate::resources::ClockResource(Arc::new(move || now)));
        site.world.insert_resource(VppClock(now));
        site.run().await
    }

    /// #648 drift reset: a VPP target write counts as a mode write for the drift watch, so a
    /// mismatched read-back from before it is not counted with one after it when the path
    /// switches back to the EEPROM writes
    #[tokio::test]
    async fn read_backs_from_before_a_vpp_write_are_not_drift() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set(ModePath::Eeprom("startup window".to_owned()));
        site.world.insert_resource(path.clone());
        let t0 = Utc::now();
        let at = |secs| t0 + Duration::seconds(secs);
        // EEPROM path: ForceCharge written, confirmed, then one mismatched read at 40 s
        site.set_hardware(M::SelfUse, None, at(0));
        assert_eq!(
            run_at(&mut site, at(0)).await,
            vec![InverterCommand::SetMode(M::ForceCharge)]
        );
        site.set_hardware(M::ForceCharge, None, at(10));
        assert_eq!(run_at(&mut site, at(10)).await, vec![]);
        site.set_hardware(M::SelfUse, None, at(40));
        assert_eq!(run_at(&mut site, at(40)).await, vec![]);
        // Direct VPP (one way from EEPROM): the target is written as a VPP command at 50 s
        path.set(ModePath::Direct { no_mode8: vec![] });
        let sent = run_at(&mut site, at(50)).await;
        assert!(
            sent.contains(&InverterCommand::SetVpp(FORCE_CHARGE)),
            "{sent:?}"
        );
        // Back on the EEPROM path (e.g. a refusal): the hardware matches, no write
        path.set(ModePath::Eeprom("target set type".to_owned()));
        site.set_hardware(M::ForceCharge, None, at(60));
        assert_eq!(run_at(&mut site, at(60)).await, vec![]);
        // A mismatch at 80 s is the first since the VPP write: no re-apply yet
        site.set_hardware(M::SelfUse, None, at(80));
        assert_eq!(run_at(&mut site, at(80)).await, vec![], "read 40 s counted");
        // A second one 30 s later is drift
        site.set_hardware(M::SelfUse, None, at(115));
        assert_eq!(
            run_at(&mut site, at(115)).await,
            vec![InverterCommand::SetMode(M::ForceCharge)]
        );
    }

    /// R7-8: a target set type check that keeps failing (read-back unreadable) falls back to the
    /// EEPROM path after the 120 s read window, one way: no more checks, EEPROM writes follow
    #[tokio::test]
    async fn an_unverified_target_set_type_falls_back_to_eeprom_after_120_s() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path.clone());
        site.fail_target_transient.store(true, Ordering::SeqCst);
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for secs in (0..=300).step_by(5) {
            let now = t0 + Duration::seconds(secs);
            site.set_hardware(M::SelfUse, None, now);
            if secs == 150 {
                // HA answers again: the fall-back stays (one way)
                site.fail_target_transient.store(false, Ordering::SeqCst);
            }
            sent.extend(run_at(&mut site, now).await.into_iter().map(|c| (secs, c)));
        }
        assert!(
            matches!(path.get(), ModePath::Eeprom(reason) if reason.contains("120 s")),
            "{:?}",
            path.get()
        );
        let checks: Vec<i64> = sent
            .iter()
            .filter(|(_, c)| *c == InverterCommand::EnsureVppTargetSetType)
            .map(|(at, _)| *at)
            .collect();
        assert!(checks.len() >= 8, "{checks:?}");
        assert!(checks.iter().all(|at| *at < 120), "{checks:?}");
        assert!(
            !sent
                .iter()
                .any(|(_, c)| matches!(c, InverterCommand::SetVpp(_))),
            "{sent:?}"
        );
        // The EEPROM path writes the plan
        assert!(
            sent.iter()
                .any(|(at, c)| *at >= 120 && *c == InverterCommand::SetMode(M::ForceCharge)),
            "{sent:?}"
        );
    }

    /// R7-8: the kill switch turned on while the target set type check holds the inverter: its
    /// mode 0 is not skipped. Sent before the EEPROM fall-back when the check never passes, and
    /// with the default work mode once the check passes
    #[tokio::test]
    async fn kill_switch_mode_0_is_not_skipped_by_a_pending_target_check() {
        for passes_at in [None, Some(60)] {
            let mut site = new_site(M::ForceCharge, true, 2.0);
            let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
            path.set_target_verified(false);
            site.world.insert_resource(path.clone());
            site.fail_target_transient.store(true, Ordering::SeqCst);
            let t0 = Utc::now();
            let mut sent = Vec::new();
            for secs in (0..=300).step_by(5) {
                let now = t0 + Duration::seconds(secs);
                // A leftover command reads mode 4 until the mode 0
                let vpp = if sent
                    .iter()
                    .any(|(_, c)| *c == InverterCommand::SetVpp(VppCommand::Exit))
                {
                    0
                } else {
                    4
                };
                site.set_hardware(M::SelfUse, Some(vpp), now);
                if secs == 30 {
                    site.user_control().state.enabled = false;
                }
                if passes_at == Some(secs) {
                    site.fail_target_transient.store(false, Ordering::SeqCst);
                }
                sent.extend(run_at(&mut site, now).await.into_iter().map(|c| (secs, c)));
            }
            let exits: Vec<i64> = sent
                .iter()
                .filter(|(_, c)| *c == InverterCommand::SetVpp(VppCommand::Exit))
                .map(|(at, _)| *at)
                .collect();
            assert_eq!(exits.len(), 1, "{passes_at:?}: {sent:?}");
            let others: Vec<&InverterCommand> = sent
                .iter()
                .map(|(_, c)| c)
                .filter(|c| **c != InverterCommand::EnsureVppTargetSetType)
                .collect();
            match passes_at {
                // Never verified: mode 0 at the fall-back; the hardware already reads Self Use,
                // so the EEPROM path writes nothing more
                None => {
                    assert!(exits[0] >= 120 && exits[0] < 130, "{sent:?}");
                    assert_eq!(
                        others,
                        vec![&InverterCommand::SetVpp(VppCommand::Exit)],
                        "{sent:?}"
                    );
                }
                // Verified after the switch: mode 0, then the default (owner ruling R3)
                Some(_) => {
                    assert!(exits[0] >= 60 && exits[0] < 120, "{sent:?}");
                    assert_eq!(
                        others,
                        vec![
                            &InverterCommand::SetVpp(VppCommand::Exit),
                            &InverterCommand::SetMode(M::SelfUse)
                        ],
                        "{sent:?}"
                    );
                }
            }
        }
    }

    // ===== R6: two export regimes, probed with the reviewer's harness =====

    /// How the plant clips PV at a mode 8 limit
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Clip {
        /// Sun at or above the limit gives 95 % of it, else the sun
        Step,
        /// PV never above 95 % of the limit (a continuous shortfall)
        Min,
    }

    /// What a probe run measured
    #[derive(Debug, Default)]
    struct Probe {
        changed: usize,
        /// Longest stretch with export above cap + 100 W
        over_s: i64,
        discharged_wh: f32,
        ideal_wh: f32,
        /// From full sun: push 0 and PV free up to cap + load
        handover: Option<i64>,
        /// Longest stretch with the PV limit below the house load
        limit_below_load_s: i64,
        /// Longest stretch of grid import (export below −100 W)
        import_s: i64,
        commands: Vec<(i64, VppCommand)>,
    }

    impl Probe {
        fn changed_per_15_min(&self, secs: i64) -> f32 {
            self.changed as f32 * 900.0 / secs as f32
        }
        fn discharge_pct(&self) -> Option<f32> {
            (self.ideal_wh > 1.0).then(|| 100.0 * self.discharged_wh / self.ideal_wh)
        }
    }

    /// One probe scenario of the reviewer's matrix
    struct Scenario {
        mode: M,
        cap_w: u32,
        lag: i64,
        noise: bool,
        clip: Clip,
        no_mode8: bool,
        secs: i64,
        full_sun_at: Option<i64>,
        soc: fn(i64) -> u16,
        /// (sun, load, the battery can discharge)
        env: Box<dyn Fn(i64) -> (f32, f32, bool)>,
    }

    impl Scenario {
        fn new(mode: M, cap_w: u32, lag: i64, noise: bool, secs: i64) -> Self {
            Self {
                mode,
                cap_w,
                lag,
                noise,
                clip: Clip::Step,
                no_mode8: false,
                secs,
                full_sun_at: None,
                soc: |_| 50,
                env: Box::new(|_| (8500.0, 1000.0, true)),
            }
        }
        fn env(mut self, env: impl Fn(i64) -> (f32, f32, bool) + 'static) -> Self {
            self.env = Box::new(env);
            self
        }
    }

    /// The reviewer's closed loop (R6): 5 s reads, commands applied `lag` s after they are
    /// sent, PV clipped at a mode 8 limit, optional ±200 W read noise on PV and load
    async fn probe(s: &Scenario) -> Probe {
        let mut site = new_site(s.mode, true, 2.0);
        site.control().maximum_export_power_w = s.cap_w;
        if s.no_mode8 {
            site.control().vpp_no_mode8 = vec!["solax".to_owned()];
        }
        let cap = s.cap_w as f32;
        let mut plant = Plant::new(s.lag, s.noise);
        let mut r = Probe::default();
        let (mut over, mut below, mut import) = (0, 0, 0);
        let t0 = Utc::now();
        let mut last: Option<VppCommand> = None;
        for at in (0..=s.secs).step_by(5) {
            let (sun, load, can) = (s.env)(at);
            if let Some((_, c)) = plant.sent.iter().rev().find(|(t, _)| t + s.lag <= at) {
                plant.applied = Some(*c);
            }
            let (limit, push) = match plant.applied {
                Some(VppCommand::PvAndBattery { pv_limit_w, push_w }) => {
                    (pv_limit_w as f32, push_w as f32)
                }
                Some(VppCommand::Push { push_w }) => (f32::MAX, push_w as f32),
                _ => (f32::MAX, 0.0),
            };
            let pv = match s.clip {
                Clip::Step if sun >= limit => limit * 0.95,
                Clip::Step => sun,
                Clip::Min => sun.min(limit * 0.95),
            };
            let battery = if push > 0.0 && !can { 0.0 } else { push };
            let export = pv + battery - load;
            if plant.applied.is_some() {
                over = if export > cap + 100.0 { over + 5 } else { 0 };
                r.over_s = r.over_s.max(over);
                r.discharged_wh += battery.max(0.0) * 5.0 / 3600.0;
                if s.mode == M::ForceDischarge && can {
                    r.ideal_wh += (cap + load - sun).clamp(0.0, 8000.0) * 5.0 / 3600.0;
                }
                below = if limit < load { below + 5 } else { 0 };
                r.limit_below_load_s = r.limit_below_load_s.max(below);
                import = if export < -100.0 { import + 5 } else { 0 };
                r.import_s = r.import_s.max(import);
                if let Some(full) = s.full_sun_at
                    && r.handover.is_none()
                    && at >= full
                    && push == 0.0
                    && limit >= cap + load - 100.0
                    && limit < f32::MAX
                {
                    r.handover = Some(at - full);
                }
            }
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            site.set_soc((s.soc)(at));
            let (pv_noise, load_noise) = (plant.noise(), plant.noise());
            let mode = plant.applied.map_or(0, |c| c.mode());
            site.set_plant(
                (pv + pv_noise).max(0.0),
                (load + load_noise).max(0.0),
                mode,
                now,
            );
            for c in site.run().await {
                if let InverterCommand::SetVpp(v) = c {
                    if last != Some(v) {
                        r.changed += 1;
                        r.commands.push((at, v));
                    }
                    last = Some(v);
                    plant.sent.push((at, v));
                }
            }
        }
        r
    }

    /// Sun 2 kW, rising to 15 kW from 60 to 180 s, falling to 1 kW from 900 to 960 s
    fn sunrise(at: i64) -> f32 {
        match at {
            0..60 => 2000.0,
            60..180 => 2000.0 + (at - 60) as f32 / 120.0 * 13_000.0,
            180..900 => 15_000.0,
            900..960 => 15_000.0 - (at - 900) as f32 / 60.0 * 14_000.0,
            _ => 1000.0,
        }
    }

    /// The reviewer's R6 matrix for one cap, lag, noise and clip: (name, scenario, the
    /// discharge criterion applies)
    fn r6_matrix(cap_w: u32, lag: i64, noise: bool, clip: Clip) -> Vec<(String, Scenario, bool)> {
        let at = |mode, secs| {
            let mut s = Scenario::new(mode, cap_w, lag, noise, secs);
            s.clip = clip;
            s
        };
        let mut out = Vec::new();
        let mut rising = at(M::ForceDischarge, 1500).env(|t| (sunrise(t), 1000.0, true));
        rising.full_sun_at = Some(180);
        out.push(("FD sun 2-15-1".to_owned(), rising, false));
        for sun in [3000.0f32, 5000.0, 8500.0, 15_000.0] {
            out.push((
                format!("FD sun {sun} load 1-4-1"),
                at(M::ForceDischarge, 1200).env(move |t| {
                    (
                        sun,
                        if (300..600).contains(&t) {
                            4000.0
                        } else {
                            1000.0
                        },
                        true,
                    )
                }),
                false,
            ));
            out.push((
                format!("FD sun {sun} load 4-1-4"),
                at(M::ForceDischarge, 1200).env(move |t| {
                    (
                        sun,
                        if (300..600).contains(&t) {
                            1000.0
                        } else {
                            4000.0
                        },
                        true,
                    )
                }),
                false,
            ));
        }
        for sun in [3000.0f32, 5000.0, 7000.0, 8500.0, 9500.0, 10_000.0] {
            out.push((
                format!("FD steady sun {sun}"),
                at(M::ForceDischarge, 900).env(move |_| (sun, 1000.0, true)),
                sun <= 8500.0,
            ));
        }
        let mut floor = at(M::ForceDischarge, 900).env(|t| (4000.0, 3000.0, t < 300));
        floor.soc = |t| if t < 300 { 15 } else { 10 };
        out.push(("FD SOC floor at 300 s".to_owned(), floor, false));
        let mut bms = at(M::ForceDischarge, 900).env(|t| (4000.0, 3000.0, t < 300));
        bms.soc = |_| 25;
        out.push(("FD BMS stop at 300 s".to_owned(), bms, false));
        let mut fi = at(M::FeedinPriority, 1200).env(|t| {
            let load = match t {
                0..300 => 2000.0,
                300..600 => 1000.0,
                600..900 => 1900.0,
                _ => 4000.0,
            };
            (15_000.0, load, true)
        });
        fi.soc = |_| 100;
        out.push(("FI full, load 2-1-1.9-4".to_owned(), fi, false));
        let mut fi_sun = at(M::FeedinPriority, 1500).env(|t| (sunrise(t), 1000.0, true));
        fi_sun.soc = |_| 100;
        fi_sun.full_sun_at = Some(180);
        out.push(("FI full, sun 2-15-1".to_owned(), fi_sun, false));
        out
    }

    /// The R6 pass criteria for one run; empty when it passes
    fn r6_failures(name: &str, s: &Scenario, r: &Probe, discharge: bool) -> Vec<String> {
        let mut out = Vec::new();
        if r.over_s > s.lag {
            out.push(format!("export over the cap {} s", r.over_s));
        }
        if s.noise && r.changed_per_15_min(s.secs) >= 8.0 {
            out.push(format!(
                "{:.1} changed writes / 15 min",
                r.changed_per_15_min(s.secs)
            ));
        }
        if discharge && let Some(pct) = r.discharge_pct().filter(|p| *p < 90.0) {
            out.push(format!("discharge {pct:.0} % of ideal"));
        }
        if s.mode == M::ForceDischarge
            && name.contains("2-15-1")
            && r.handover.is_none_or(|h| h > 120)
        {
            out.push(format!("hand-over {:?}", r.handover));
        }
        if r.limit_below_load_s > s.lag {
            out.push(format!(
                "PV limit below the load {} s",
                r.limit_below_load_s
            ));
        }
        if s.mode == M::ForceDischarge && r.import_s > s.lag {
            out.push(format!("grid import {} s", r.import_s));
        }
        out
    }

    /// Run a slice of the R6 matrix and assert every criterion
    async fn assert_r6(cap_w: u32, lag: i64, noise: bool, clip: Clip) {
        let mut failed = Vec::new();
        for (name, s, discharge) in r6_matrix(cap_w, lag, noise, clip) {
            let r = probe(&s).await;
            let f = r6_failures(&name, &s, &r, discharge);
            if !f.is_empty() {
                failed.push(format!("{name}: {f:?} {:?}", r.commands));
            }
        }
        assert!(
            failed.is_empty(),
            "cap {cap_w} lag {lag} noise {noise} {clip:?}:\n{}",
            failed.join("\n")
        );
    }

    /// R6: the reviewer's probe matrix at a 9 kW cap, both lags, with and without noise
    #[tokio::test]
    async fn r6_matrix_cap_9_kw() {
        for lag in [15, 30] {
            for noise in [false, true] {
                assert_r6(9000, lag, noise, Clip::Step).await;
            }
        }
    }

    /// R6: the reviewer's probe matrix at a 3 kW cap, both lags, with and without noise
    #[tokio::test]
    async fn r6_matrix_cap_3_kw() {
        for lag in [15, 30] {
            for noise in [false, true] {
                assert_r6(3000, lag, noise, Clip::Step).await;
            }
        }
    }

    /// R6: the matrix with a continuous clip shortfall (PV never above 95 % of the limit)
    #[tokio::test]
    async fn r6_matrix_continuous_clip_shortfall() {
        for cap in [9000, 3000] {
            assert_r6(cap, 30, true, Clip::Min).await;
        }
    }

    /// The criteria table of the whole matrix (for the review report)
    #[tokio::test]
    #[ignore = "prints the R6 probe table, ~5 min"]
    async fn r6_probe_table() {
        println!(
            "scenario|clip|cap|lag|noise|over_s|disch%|handover_s|chg/15|limit<load_s|import_s|result"
        );
        for clip in [Clip::Step, Clip::Min] {
            for cap in [9000, 3000] {
                for lag in [15, 30] {
                    for noise in [false, true] {
                        for (name, s, discharge) in r6_matrix(cap, lag, noise, clip) {
                            let r = probe(&s).await;
                            let f = r6_failures(&name, &s, &r, discharge);
                            println!(
                                "R6|{name}|{clip:?}|{cap}|{lag}|{noise}|{}|{}|{}|{:.1}|{}|{}|{}",
                                r.over_s,
                                r.discharge_pct()
                                    .map_or("-".to_owned(), |p| format!("{p:.0}")),
                                r.handover.map_or("-".to_owned(), |h| h.to_string()),
                                r.changed_per_15_min(s.secs),
                                r.limit_below_load_s,
                                r.import_s,
                                if f.is_empty() {
                                    "PASS".to_owned()
                                } else {
                                    f.join("; ")
                                }
                            );
                        }
                    }
                }
            }
        }
    }

    /// R6-3: mode 4 (no mode 8, Jakub) keeps the band and the smoothing: read noise writes
    /// fewer than 8 changes per 15 min, and a load step still moves the push
    #[tokio::test]
    async fn mode_4_discharge_ignores_read_noise() {
        for sun in [5000.0f32, 8500.0] {
            for lag in [15, 30] {
                let mut s = Scenario::new(M::ForceDischarge, 9000, lag, true, 900).env(move |t| {
                    (
                        sun,
                        if (300..600).contains(&t) {
                            4000.0
                        } else {
                            1000.0
                        },
                        true,
                    )
                });
                s.no_mode8 = true;
                let r = probe(&s).await;
                assert!(
                    r.changed_per_15_min(900) < 8.0 && r.over_s <= lag,
                    "sun {sun} lag {lag}: {r:?}"
                );
                assert!(
                    r.commands
                        .iter()
                        .all(|(_, c)| matches!(c, VppCommand::Push { .. })),
                    "{r:?}"
                );
                let at = |secs| {
                    r.commands
                        .iter()
                        .rev()
                        .find(|(t, _)| *t <= secs)
                        .map(|(_, c)| *c)
                };
                assert!(
                    matches!(at(450), Some(VppCommand::Push { push_w }) if push_w >= 4000),
                    "sun {sun} lag {lag}: {r:?}"
                );
            }
        }
    }

    /// R6-3 / R7-6: the mode 4 push never exceeds the battery (cap + load − PV is 8.5 kW, the
    /// battery 8 kW): read noise writes nothing. Where the clamp sits relative to the bands has
    /// no observable effect (every band compares the push with cap − (PV − load)); the clamp
    /// itself does
    #[tokio::test]
    async fn mode_4_push_is_clamped_to_the_battery() {
        let mut s =
            Scenario::new(M::ForceDischarge, 9000, 15, true, 900).env(|_| (2000.0, 1500.0, true));
        s.no_mode8 = true;
        let r = probe(&s).await;
        assert_eq!(r.commands, vec![(0, VppCommand::Push { push_w: 8000 })]);
    }

    /// R6-4: a mixed change (one field up, the other down) is export-lowering when PV limit
    /// + push falls, so it skips the 20 s gap
    #[test]
    fn a_mixed_change_lowering_limit_plus_push_lowers_export() {
        let at = |pv_limit_w, push_w| VppCommand::PvAndBattery { pv_limit_w, push_w };
        // Into the curtail regime: limit down, push down
        assert!(at(10_000, 0).lowers_export_from(&at(PV_UNCAPPED_W, 1500)));
        // Limit up 500, push down 1000
        assert!(at(10_500, 0).lowers_export_from(&at(10_000, 1000)));
        // Limit down 500, push up 1000: export rises
        assert!(!at(9500, 1000).lowers_export_from(&at(10_000, 0)));
        // Out of the curtail regime: uncapped
        assert!(!at(PV_UNCAPPED_W, 2000).lowers_export_from(&at(10_000, 0)));
        assert!(
            VppCommand::Push { push_w: 100 }.lowers_export_from(&VppCommand::Push { push_w: 900 })
        );
    }

    /// R6-1: Force Discharge at a 9 kW cap, sun 8.5 kW, load 1 kW, then 4 kW from 300 to
    /// 600 s: the battery covers the load rise (the reviewer's test)
    #[tokio::test]
    async fn r6_force_discharge_follows_a_load_rise() {
        for lag in [15, 30] {
            let mut site = new_site(M::ForceDischarge, true, 2.0);
            let mut plant = Plant::new(lag, false);
            let run = closed_loop(&mut site, &mut plant, 600, |at| {
                (8500.0, if at >= 300 { 4000.0 } else { 1000.0 }, true)
            })
            .await;
            let push_at_450 = plant
                .sent
                .iter()
                .rev()
                .find(|(t, _)| *t <= 450)
                .map(|(_, c)| *c);
            assert!(
                matches!(push_at_450, Some(VppCommand::PvAndBattery { push_w, .. }) if push_w >= 4000),
                "lag {lag}: push at 450 s {push_at_450:?}: {:?}",
                run.changed
            );
        }
    }

    /// R6-1b: Force Discharge handed over (push 0, sun 15 kW, load 1 kW), then the load rises
    /// to 4 kW: the PV limit rises to cap + load (the reviewer's test)
    #[tokio::test]
    async fn r6_handed_over_discharge_follows_a_load_rise() {
        let mut site = new_site(M::ForceDischarge, true, 2.0);
        let mut plant = Plant::new(15, false);
        let run = closed_loop(&mut site, &mut plant, 600, |at| {
            (15_000.0, if at >= 300 { 4000.0 } else { 1000.0 }, true)
        })
        .await;
        let last = plant.sent.last().map(|(_, c)| *c);
        assert!(
            matches!(last, Some(VppCommand::PvAndBattery { pv_limit_w, .. }) if pv_limit_w >= 12_900),
            "last {last:?}: {:?}",
            run.changed
        );
    }

    /// R6-5: ±200 W noise, steady sun 8.5 kW, load 1 kW, 9 kW cap: ≥ 90 % of the ideal
    /// discharge (the reviewer's test)
    #[tokio::test]
    async fn r6_noise_keeps_the_discharge() {
        for lag in [15, 30] {
            let mut site = new_site(M::ForceDischarge, true, 2.0);
            let mut plant = Plant::new(lag, true);
            let run = closed_loop(&mut site, &mut plant, 900, |_| (8500.0, 1000.0, true)).await;
            let ideal = 1500.0 * (900 - lag) as f32 / 3600.0;
            assert!(
                run.discharged_wh >= 0.9 * ideal,
                "lag {lag}: {:.0}/{ideal:.0}",
                run.discharged_wh
            );
        }
    }

    /// R6-6: kill switch on at start, a leftover mode 4, and a target set type to verify
    /// first: mode 0 once, after the check (the reviewer's test)
    #[tokio::test]
    async fn r6_kill_at_start_with_a_pending_target_check() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path);
        site.user_control().state.enabled = false;
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=600).step_by(5) {
            let now = t0 + Duration::seconds(at);
            site.world.insert_resource(VppClock(now));
            let vpp = if sent.contains(&InverterCommand::SetVpp(VppCommand::Exit)) {
                0
            } else {
                4
            };
            site.set_hardware(M::SelfUse, Some(vpp), now);
            sent.extend(site.run().await);
        }
        assert_eq!(
            sent,
            vec![
                InverterCommand::EnsureVppTargetSetType,
                InverterCommand::SetVpp(VppCommand::Exit)
            ]
        );
    }

    /// Several controlled masters: the target set type is verified per inverter, so one
    /// inverter's check does not release another whose check keeps failing
    #[tokio::test]
    async fn the_target_set_type_is_verified_per_inverter() {
        let mut site = new_site(M::ForceCharge, true, 2.0);
        {
            let mut config = site.world.resource_mut::<SystemConfig>();
            let mut flaky = config.inverters[0].clone();
            flaky.id = "flaky".to_owned();
            config.inverters.push(flaky);
        }
        site.world.spawn((
            Inverter {
                id: "flaky".to_owned(),
                inverter_type: fluxion_types::InverterType::Solax,
            },
            CurrentMode::default(),
            BatteryStatus {
                soc_percent: 50,
                ..Default::default()
            },
            RawInverterState {
                state: GenericInverterState {
                    work_mode: M::SelfUse,
                    pv_power_w: 2000.0,
                    house_load_w: Some(1000.0),
                    ..Default::default()
                },
                last_updated: Utc::now(),
            },
        ));
        let path = VppPathState::new(ModePath::Direct { no_mode8: vec![] });
        path.set_target_verified(false);
        site.world.insert_resource(path.clone());
        let t0 = Utc::now();
        let mut sent = Vec::new();
        for at in (0..=60).step_by(5) {
            site.world
                .insert_resource(VppClock(t0 + Duration::seconds(at)));
            let commands = site.run().await;
            let ids = std::mem::take(&mut *site.ids.lock().unwrap());
            sent.extend(ids.into_iter().zip(commands));
        }
        assert!(path.target_verified("solax") && !path.target_verified("flaky"));
        let vpp_to = |id: &str| {
            sent.iter()
                .filter(|(to, c)| to == id && matches!(c, InverterCommand::SetVpp(_)))
                .count()
        };
        assert!(vpp_to("solax") > 0, "{sent:?}");
        assert_eq!(vpp_to("flaky"), 0, "{sent:?}");
        let checks = sent
            .iter()
            .filter(|(to, c)| to == "flaky" && *c == InverterCommand::EnsureVppTargetSetType)
            .count();
        assert!(checks >= 4, "retried every 10 s: {sent:?}");
    }

    // ----- R7 closed loop: the reviewer's plant (probe7.rs) as tests, 6 seeds -----

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum RClip {
        /// PV at or above the limit clips to 95 % of it
        Step,
        /// PV = min(sun, 95 % of the limit)
        Min,
    }

    #[derive(Debug, Default)]
    struct R7 {
        changed: usize,
        over_s: i64,
        over_peak: f32,
        disch_wh: f32,
        ideal_wh: f32,
        handover: Option<i64>,
        below_s: i64,
        import_s: i64,
        /// (second, applied PV limit, applied push, true load)
        trace: Vec<(i64, f32, f32, f32)>,
        cmds: Vec<(i64, VppCommand)>,
        end_exit_delay: Vec<i64>,
        /// Mode 8 still applied beyond the lag and one read after an export block ends
        mode8_after_end_s: i64,
    }

    struct R7Cfg {
        mode: M,
        cap: u32,
        lag: i64,
        noise: f32,
        seed: u64,
        clip: RClip,
        no_mode8: bool,
        secs: i64,
        full_sun_at: Option<i64>,
        /// Through the ECS (`vpp_direct_system`); otherwise [`ExportMemory`] and
        /// [`VppLink::write_due`] alone, which is what the system runs for Force Discharge at
        /// SOC 50 % (fast enough for the seed matrix)
        ecs: bool,
        soc: Box<dyn Fn(i64) -> u16>,
        env: Box<dyn Fn(i64) -> (f32, f32, bool)>,
        plan: Option<Box<dyn Fn(i64) -> M>>,
    }

    impl R7Cfg {
        #[allow(clippy::too_many_arguments)]
        fn new(mode: M, cap: u32, lag: i64, noise: f32, seed: u64, clip: RClip, secs: i64) -> Self {
            Self {
                mode,
                cap,
                lag,
                noise,
                seed,
                clip,
                no_mode8: false,
                secs,
                full_sun_at: None,
                ecs: mode != M::ForceDischarge,
                soc: Box::new(|_| 50),
                env: Box::new(|_| (8500.0, 1000.0, true)),
                plan: None,
            }
        }

        fn env(mut self, f: impl Fn(i64) -> (f32, f32, bool) + 'static) -> Self {
            self.env = Box::new(f);
            self
        }
    }

    /// Read noise: no noise once, then ±200 W uniform on PV and load over 5 seeds (R7-2)
    const R7_SEEDS: [(f32, u64); 6] = [
        (0.0, 42),
        (200.0, 42),
        (200.0, 7),
        (200.0, 1234),
        (200.0, 2026),
        (200.0, 99),
    ];

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }

    /// The plant: commands apply `lag` s after they are sent; PV under a limit clips to 95 %
    /// of it; the meter reads PV and load with uniform noise every 5 s
    async fn r7_run(c: &R7Cfg) -> R7 {
        let ecs = c.ecs || c.plan.is_some();
        let kind = if c.no_mode8 {
            ExportKind::PushOnly
        } else {
            ExportKind::Discharge
        };
        let limits = VppLimits {
            battery_w: 8000,
            max_export_w: c.cap as i32,
            max_soc: 100.0,
            discharge_floor: 20.0,
        };
        let mut link = VppLink::default();
        let mut site = new_site(c.mode, true, 2.0);
        site.control().maximum_export_power_w = c.cap;
        if c.no_mode8 {
            site.control().vpp_no_mode8 = vec!["solax".to_owned()];
        }
        if c.plan.is_some() {
            site.schedule = Schedule::default();
            site.schedule
                .add_systems((export_curtailment_system, vpp_direct_system).chain());
            site.world
                .resource_mut::<InitialModeSyncTracker>()
                .synced_inverters
                .insert("solax".to_owned());
        }
        let cap = c.cap as f32;
        let mut seed = c.seed;
        let mut sent: Vec<(i64, VppCommand)> = Vec::new();
        let mut applied: Option<VppCommand> = None;
        let mut r = R7::default();
        let (mut over, mut below, mut import, mut m8end) = (0, 0, 0, 0);
        let t0 = Utc::now();
        let mut last: Option<VppCommand> = None;
        let mut plan_was = c.mode;
        let mut end_at: Option<i64> = None;
        let exporting = |m: M| matches!(m, M::ForceDischarge | M::FeedinPriority);
        for at in (0..=c.secs).step_by(5) {
            let plan = c.plan.as_ref().map_or(c.mode, |p| p(at));
            if exporting(plan_was) && !exporting(plan) {
                end_at = Some(at);
            }
            if exporting(plan) {
                end_at = None;
            }
            plan_was = plan;
            let (sun, load, can) = (c.env)(at);
            if let Some((_, cmd)) = sent.iter().rev().find(|(t, _)| t + c.lag <= at) {
                applied = Some(*cmd);
            }
            let (limit, push) = match applied {
                Some(VppCommand::PvAndBattery { pv_limit_w, push_w }) => {
                    (pv_limit_w as f32, push_w as f32)
                }
                Some(VppCommand::Push { push_w }) => (f32::MAX, push_w as f32),
                _ => (f32::MAX, 0.0),
            };
            let pv = match c.clip {
                RClip::Step if sun >= limit => limit * 0.95,
                RClip::Step => sun,
                RClip::Min => sun.min(limit * 0.95),
            };
            let battery = if push > 0.0 && !can { 0.0 } else { push };
            let export = pv + battery - load;
            r.trace.push((at, limit, push, load));
            m8end = match (end_at, applied) {
                // Export mode 8 (Force Charge is mode 8 with a charge, push < 0)
                (Some(e), Some(VppCommand::PvAndBattery { push_w, .. }))
                    if push_w >= 0 && at > e + c.lag + 5 =>
                {
                    m8end + 5
                }
                _ => 0,
            };
            r.mode8_after_end_s = r.mode8_after_end_s.max(m8end);
            // Outside export blocks the plant does not model the inverter's own modes
            if applied.is_some() && exporting(plan) {
                // No mode 8: the inverter's export limit caps PV; count only what the battery adds
                let over_w = if c.no_mode8 {
                    export - cap.max(pv - load)
                } else {
                    export - cap
                };
                over = if over_w > 100.0 { over + 5 } else { 0 };
                r.over_s = r.over_s.max(over);
                r.over_peak = r.over_peak.max(over_w);
                r.disch_wh += battery.max(0.0) * 5.0 / 3600.0;
                if plan == M::ForceDischarge && can {
                    r.ideal_wh += (cap + load - sun).clamp(0.0, 8000.0) * 5.0 / 3600.0;
                }
                below = if limit < load { below + 5 } else { 0 };
                r.below_s = r.below_s.max(below);
                import = if plan == M::ForceDischarge && export < -100.0 {
                    import + 5
                } else {
                    0
                };
                r.import_s = r.import_s.max(import);
                if let Some(full) = c.full_sun_at
                    && r.handover.is_none()
                    && at >= full
                    && push == 0.0
                    && limit >= cap + load - 100.0
                    && limit < f32::MAX
                {
                    r.handover = Some(at - full);
                }
            }
            let now = t0 + Duration::seconds(at);
            let (n1, n2) = if c.noise > 0.0 {
                let span = (2.0 * c.noise) as u64 + 1;
                (
                    (lcg(&mut seed) % span) as f32 - c.noise,
                    (lcg(&mut seed) % span) as f32 - c.noise,
                )
            } else {
                (0.0, 0.0)
            };
            let (pv_read, load_read) = ((pv + n1).max(0.0), (load + n2).max(0.0));
            let written: Vec<VppCommand> = if ecs {
                r7_ecs_step(&mut site, c, at, plan, applied, pv_read, load_read, now).await
            } else {
                let command = link.export.step(kind, pv_read, load_read, &limits, now);
                if link.write_due(&command, now, MIN_WRITE_GAP_SECS) {
                    link.sent(command, now);
                    link.export.sent(command, now);
                    vec![command]
                } else {
                    Vec::new()
                }
            };
            for v in written {
                if last != Some(v) {
                    r.changed += 1;
                    r.cmds.push((at, v));
                }
                if let Some(e) = end_at
                    && v == VppCommand::Exit
                    && last != Some(VppCommand::Exit)
                {
                    r.end_exit_delay.push(at - e);
                }
                last = Some(v);
                sent.push((at, v));
            }
        }
        r
    }

    /// One read through the ECS: the commands written
    #[allow(clippy::too_many_arguments)]
    async fn r7_ecs_step(
        site: &mut Site,
        c: &R7Cfg,
        at: i64,
        plan: M,
        applied: Option<VppCommand>,
        pv_read: f32,
        load_read: f32,
        now: DateTime<Utc>,
    ) -> Vec<VppCommand> {
        site.world.insert_resource(VppClock(now));
        site.set_soc((c.soc)(at));
        if c.plan.is_some() {
            let mut q = site.world.query::<&mut CurrentMode>();
            for mut cm in q.iter_mut(&mut site.world) {
                cm.mode = plan;
            }
        }
        site.set_plant(pv_read, load_read, applied.map_or(0, |x| x.mode()), now);
        site.run()
            .await
            .into_iter()
            .filter_map(|cmd| match cmd {
                InverterCommand::SetVpp(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    /// What a scenario is held to
    #[derive(Clone, Copy, Default)]
    struct R7Criteria {
        /// ≥ 90 % of the ideal discharge
        discharge: bool,
        /// Push 0 and PV limit at cap + load within 120 s of full sun
        handover: bool,
        /// Cloud edges or a PV ramp: each needs a write, so the < 8 writes / 15 min rule is off
        writes_exempt: bool,
    }

    /// Failed criteria of one run. Export > cap + 100 W only within the lag without noise; with
    /// noise within the lag and one read (a noisy read is not told from a step at once), and
    /// at cloud edges `R7_NOISY_EDGE_OVER_S` beyond the lag (known limit); fewer than 8 changed
    /// writes per 15 min with noise; the limit never under the load and no grid import beyond
    /// the lag; mode 8 gone within the lag and one read after a block ends
    fn r7_failures(tag: &str, c: &R7Cfg, r: &R7, k: R7Criteria) -> Vec<String> {
        let per15 = r.changed as f32 * 900.0 / c.secs as f32;
        let pct = (r.ideal_wh > 1.0).then(|| 100.0 * r.disch_wh / r.ideal_wh);
        let over_allowed = match (c.noise > 0.0, k.writes_exempt) {
            (false, _) => c.lag,
            (true, false) => c.lag + 5,
            (true, true) => c.lag + R7_NOISY_EDGE_OVER_S,
        };
        let mut f = Vec::new();
        if r.over_s > over_allowed {
            f.push(format!("over {} s, peak {:.0} W", r.over_s, r.over_peak));
        }
        if k.discharge && pct.is_some_and(|p| p < 90.0) {
            f.push(format!("discharge {:.0} %", pct.unwrap_or(0.0)));
        }
        if k.handover && r.handover.is_none_or(|h| h > 120) {
            f.push(format!("handover {:?}", r.handover));
        }
        if c.noise > 0.0 && !k.writes_exempt && per15 >= 8.0 {
            f.push(format!("{per15:.1} writes / 15 min"));
        }
        if r.below_s > c.lag {
            f.push(format!("limit under the load {} s", r.below_s));
        }
        if r.import_s > c.lag {
            f.push(format!("import {} s", r.import_s));
        }
        if r.mode8_after_end_s > 0 {
            f.push(format!(
                "mode 8 {} s after the block end",
                r.mode8_after_end_s
            ));
        }
        f.into_iter()
            .map(|f| {
                format!(
                    "{tag} {:?} cap {} lag {} noise {} seed {}: {f}",
                    c.clip, c.cap, c.lag, c.noise, c.seed
                )
            })
            .collect()
    }

    fn sunrise7(at: i64) -> f32 {
        match at {
            0..60 => 2000.0,
            60..180 => 2000.0 + (at - 60) as f32 / 120.0 * 13_000.0,
            180..900 => 15_000.0,
            900..960 => 15_000.0 - (at - 900) as f32 / 60.0 * 14_000.0,
            _ => 1000.0,
        }
    }

    /// Alternating levels with segment lengths 30..=90 s from a fixed seed
    fn clouds(seed: u64, secs: i64, high: f32, low: f32) -> Vec<(i64, f32)> {
        let mut s = seed;
        let mut out = Vec::new();
        let mut t = 0;
        let mut hi = true;
        while t <= secs {
            out.push((t, if hi { high } else { low }));
            t += 30 + (lcg(&mut s) % 61) as i64;
            hi = !hi;
        }
        out
    }

    fn level_at(segs: &[(i64, f32)], at: i64) -> f32 {
        segs.iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .map_or(segs[0].1, |(_, v)| *v)
    }

    fn r7_assert(failures: &[String]) {
        assert!(
            failures.is_empty(),
            "{} failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// R7-1/R7-2/R7-3/R7-4: Force Discharge and Feed-in on mode 8, one cap, lag and clip model
    async fn r7_slice(cap: u32, lag: i64, clip: RClip) -> Vec<String> {
        let mk = |mode, secs, noise, seed| R7Cfg::new(mode, cap, lag, noise, seed, clip, secs);
        let capf = cap as f32;
        let plain = R7Criteria::default();
        let mut fails = Vec::new();
        for (noise, seed) in R7_SEEDS {
            for sun in [3000.0f32, 5000.0, 7000.0, 8500.0, 9500.0] {
                let c = mk(M::ForceDischarge, 900, noise, seed).env(move |_| (sun, 1000.0, true));
                let k = R7Criteria {
                    discharge: sun <= 8500.0,
                    ..plain
                };
                fails.extend(r7_failures(
                    &format!("FD steady {sun}"),
                    &c,
                    &r7_run(&c).await,
                    k,
                ));
            }
            let mut c =
                mk(M::ForceDischarge, 1500, noise, seed).env(|t| (sunrise7(t), 1000.0, true));
            c.full_sun_at = Some(180);
            let k = R7Criteria {
                handover: true,
                ..plain
            };
            fails.extend(r7_failures("FD sun 2-15-1", &c, &r7_run(&c).await, k));
            for sun in [3000.0f32, 5000.0, 8500.0, 15_000.0] {
                let c = mk(M::ForceDischarge, 1200, noise, seed).env(move |t| {
                    (
                        sun,
                        if (300..600).contains(&t) {
                            4000.0
                        } else {
                            1000.0
                        },
                        true,
                    )
                });
                fails.extend(r7_failures(
                    &format!("FD sun {sun} load 1-4-1"),
                    &c,
                    &r7_run(&c).await,
                    plain,
                ));
                let c = mk(M::ForceDischarge, 1200, noise, seed).env(move |t| {
                    (
                        sun,
                        if (300..600).contains(&t) {
                            1000.0
                        } else {
                            4000.0
                        },
                        true,
                    )
                });
                fails.extend(r7_failures(
                    &format!("FD sun {sun} load 4-1-4"),
                    &c,
                    &r7_run(&c).await,
                    plain,
                ));
            }
            for sun in [5000.0f32, 15_000.0] {
                let c = mk(M::ForceDischarge, 900, noise, seed)
                    .env(move |t| (sun, if t < 300 { 4000.0 } else { 1000.0 }, true));
                fails.extend(r7_failures(
                    &format!("FD sun {sun} load drop 4-1"),
                    &c,
                    &r7_run(&c).await,
                    plain,
                ));
            }
            let mut c = mk(M::ForceDischarge, 900, noise, seed).env(|t| (4000.0, 3000.0, t < 300));
            c.ecs = true;
            c.soc = Box::new(|t| if t < 300 { 15 } else { 10 });
            fails.extend(r7_failures(
                "FD SOC floor at 300",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            let mut c = mk(M::ForceDischarge, 900, noise, seed).env(|t| (4000.0, 3000.0, t < 300));
            c.ecs = true;
            c.soc = Box::new(|_| 25);
            fails.extend(r7_failures(
                "FD BMS stop at 300",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            let mut c = mk(M::FeedinPriority, 1200, noise, seed).env(|t| {
                let load = match t {
                    0..300 => 2000.0,
                    300..600 => 1000.0,
                    600..900 => 1900.0,
                    _ => 4000.0,
                };
                (15_000.0, load, true)
            });
            c.ecs = true;
            c.soc = Box::new(|_| 100);
            fails.extend(r7_failures(
                "FI full load 2-1-1.9-4",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            let mut c =
                mk(M::FeedinPriority, 1500, noise, seed).env(|t| (sunrise7(t), 1000.0, true));
            c.ecs = true;
            c.soc = Box::new(|_| 100);
            fails.extend(r7_failures(
                "FI full sun 2-15-1",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            // Clouds every 30-90 s: discharge <-> curtail
            let clouds_k = R7Criteria {
                writes_exempt: true,
                ..plain
            };
            let (high, low) = if cap >= 9000 {
                (15_000.0, 3000.0)
            } else {
                (8000.0, 1500.0)
            };
            let segs = clouds(seed ^ 0x5eed, 1800, high, low);
            let c = mk(M::ForceDischarge, 1800, noise, seed)
                .env(move |t| (level_at(&segs, t), 1000.0, true));
            fails.extend(r7_failures(
                "FD clouds hi/lo",
                &c,
                &r7_run(&c).await,
                clouds_k,
            ));
            // Clouds at the regime boundary: cap + load ± 600 W
            let segs = clouds(seed ^ 0xb0d, 1800, capf + 1600.0, capf + 400.0);
            let c = mk(M::ForceDischarge, 1800, noise, seed)
                .env(move |t| (level_at(&segs, t), 1000.0, true));
            fails.extend(r7_failures(
                "FD clouds boundary ±600",
                &c,
                &r7_run(&c).await,
                clouds_k,
            ));
            // Steady at the boundary: no oscillation (R7-4)
            let c =
                mk(M::ForceDischarge, 900, noise, seed).env(move |_| (capf + 1000.0, 1000.0, true));
            fails.extend(r7_failures(
                "FD steady at cap+load",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            let c =
                mk(M::ForceDischarge, 900, noise, seed).env(move |_| (capf + 1300.0, 1000.0, true));
            fails.extend(r7_failures(
                "FD steady cap+load+300",
                &c,
                &r7_run(&c).await,
                plain,
            ));
            // R7-4: after a curtail the sun settles below cap + load; the push must rise after
            // the lag. At 9 kW the 5 % clip is 450 W, so 500 W below reads as clipped full sun:
            // the exit margin is max(300 W, 5 % of the limit + 1 step), and 9 kW is tested 900 W
            // below
            for below in if cap < 9000 {
                vec![500.0f32, 700.0]
            } else {
                vec![900.0]
            } {
                let c = mk(M::ForceDischarge, 900, noise, seed).env(move |t| {
                    (
                        if t < 300 {
                            15_000.0
                        } else {
                            capf + 1000.0 - below
                        },
                        1000.0,
                        true,
                    )
                });
                let r = r7_run(&c).await;
                let tag = format!("FD curtail then sun cap+load-{below}");
                fails.extend(r7_failures(&tag, &c, &r, plain));
                let pushes_at = r
                    .trace
                    .iter()
                    .find(|(at, _, push, _)| *at >= 300 && *push > 0.0);
                if pushes_at.is_none_or(|(at, ..)| *at > 300 + lag + 90) {
                    fails.push(format!(
                        "{tag} {clip:?} cap {cap} lag {lag} noise {noise} seed {seed}: push > 0 at {:?}",
                        pushes_at.map(|(at, ..)| at)
                    ));
                }
            }
            // R7-3: curtailing under a 15 kW sun while the load ramps 1 → 4 kW (200..600 s): the
            // limit reaches cap + load within 60 s after the lag
            let c = mk(M::ForceDischarge, 900, noise, seed).env(move |t| {
                (
                    15_000.0,
                    1000.0 + ((t - 200).clamp(0, 400) as f32) * 7.5,
                    true,
                )
            });
            let r = r7_run(&c).await;
            fails.extend(r7_failures("FD curtail load ramp 1-4", &c, &r, clouds_k));
            let short: Vec<i64> = r
                .trace
                .iter()
                .filter(|(at, limit, _, load)| {
                    *at >= 600 + lag + 60 && *limit < capf + load - 100.0
                })
                .map(|(at, ..)| *at)
                .collect();
            if !short.is_empty() {
                fails.push(format!(
                    "FD curtail load ramp 1-4 {clip:?} cap {cap} lag {lag} noise {noise} seed {seed}: limit short of cap + load at {short:?}"
                ));
            }
            // R7-1: a +130 W step over the cap is cut within the lag
            let c = mk(M::ForceDischarge, 900, noise, seed).env(move |t| {
                (
                    if t < 300 { capf - 600.0 } else { capf - 470.0 },
                    1000.0,
                    true,
                )
            });
            let r = r7_run(&c).await;
            if noise == 0.0 {
                let k = R7Criteria {
                    discharge: true,
                    ..plain
                };
                fails.extend(r7_failures("FD sun step +130 W", &c, &r, k));
            }
        }
        fails
    }

    /// R7-1: closed-loop PV ramps at 2, 4, 8 and 15 W/s, up and down over cap ± 3 kW: export
    /// over the cap + 100 W only within the lag without noise. With ±200 W read noise a ramp
    /// is not told from noise at once: measured bounds, the PR's known limit
    async fn r7_ramps(cap: u32, lag: i64, clip: RClip, no_mode8: bool) -> Vec<String> {
        let capf = cap as f32;
        let mut fails = Vec::new();
        for (noise, seed) in R7_SEEDS {
            for rate in [2.0f32, 4.0, 8.0, 15.0] {
                let (lo, hi) = ((capf - 3000.0).max(0.0), capf + 3000.0);
                let secs = ((hi - lo) / rate) as i64 + 300;
                let mk = |env: Box<dyn Fn(i64) -> (f32, f32, bool)>| {
                    let mut c = R7Cfg::new(M::ForceDischarge, cap, lag, noise, seed, clip, secs);
                    c.env = env;
                    c.no_mode8 = no_mode8;
                    c
                };
                let up = mk(Box::new(move |t| {
                    ((lo + rate * t as f32).min(hi), 1000.0, true)
                }));
                let down = mk(Box::new(move |t| {
                    ((hi - rate * t as f32).max(lo), 1000.0, true)
                }));
                for (tag, c) in [("up", up), ("down", down)] {
                    if no_mode8 && tag == "down" {
                        continue;
                    }
                    let r = r7_run(&c).await;
                    let tag = format!(
                        "{} ramp {tag} {rate} W/s",
                        if no_mode8 { "M4" } else { "FD" }
                    );
                    let k = R7Criteria {
                        writes_exempt: true,
                        ..R7Criteria::default()
                    };
                    if noise == 0.0 {
                        fails.extend(r7_failures(&tag, &c, &r, k));
                    } else if r.over_s > lag + R7_NOISY_RAMP_OVER_S
                        || r.over_peak > R7_NOISY_RAMP_PEAK_W
                    {
                        fails.push(format!(
                            "{tag} {clip:?} cap {cap} lag {lag} noise {noise} seed {seed}: over {} s, peak {:.0} W",
                            r.over_s, r.over_peak
                        ));
                    }
                }
            }
        }
        fails
    }

    /// Known limits with ±200 W read noise, measured over the seeds: export over cap + 100 W
    /// beyond the lag at cloud edges (a push off by up to ~250 W is within the noise band of a
    /// 3-read mean, then an edge), and on a PV ramp (a slope of 2–15 W/s per 5 s read is not
    /// significant against the noise for 6–24 reads) with its peak over the cap
    const R7_NOISY_EDGE_OVER_S: i64 = 15;
    const R7_NOISY_RAMP_OVER_S: i64 = 165;
    const R7_NOISY_RAMP_PEAK_W: f32 = 925.0;

    #[tokio::test]
    async fn r7_closed_loop_9_kw_lag_15_step() {
        r7_assert(&r7_slice(9000, 15, RClip::Step).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_9_kw_lag_30_step() {
        r7_assert(&r7_slice(9000, 30, RClip::Step).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_3_kw_lag_15_step() {
        r7_assert(&r7_slice(3000, 15, RClip::Step).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_3_kw_lag_30_step() {
        r7_assert(&r7_slice(3000, 30, RClip::Step).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_9_kw_lag_15_min() {
        r7_assert(&r7_slice(9000, 15, RClip::Min).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_9_kw_lag_30_min() {
        r7_assert(&r7_slice(9000, 30, RClip::Min).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_3_kw_lag_15_min() {
        r7_assert(&r7_slice(3000, 15, RClip::Min).await);
    }
    #[tokio::test]
    async fn r7_closed_loop_3_kw_lag_30_min() {
        r7_assert(&r7_slice(3000, 30, RClip::Min).await);
    }

    #[tokio::test]
    async fn r7_pv_ramps() {
        let mut fails = Vec::new();
        for cap in [9000u32, 3000] {
            for lag in [15i64, 30] {
                for clip in [RClip::Step, RClip::Min] {
                    fails.extend(r7_ramps(cap, lag, clip, false).await);
                }
            }
        }
        r7_assert(&fails);
    }

    /// Mode 4 (no mode 8, the Ultra pair): 27.5 kW and 9 kW caps, ramps included
    async fn r7_mode_4(cap: u32, suns: [f32; 3]) {
        let plain = R7Criteria::default();
        let mut fails = Vec::new();
        {
            for lag in [15i64, 30] {
                for (noise, seed) in R7_SEEDS {
                    let mk = |secs| {
                        let mut c =
                            R7Cfg::new(M::ForceDischarge, cap, lag, noise, seed, RClip::Step, secs);
                        c.no_mode8 = true;
                        c
                    };
                    for sun in suns {
                        let c = mk(1200).env(move |t| {
                            (
                                sun,
                                if (300..600).contains(&t) {
                                    4000.0
                                } else {
                                    1000.0
                                },
                                true,
                            )
                        });
                        fails.extend(r7_failures(
                            &format!("M4 sun {sun} load 1-4-1"),
                            &c,
                            &r7_run(&c).await,
                            plain,
                        ));
                        let c = mk(900).env(move |_| (sun, 1000.0, true));
                        let k = R7Criteria {
                            discharge: true,
                            ..plain
                        };
                        fails.extend(r7_failures(
                            &format!("M4 steady {sun}"),
                            &c,
                            &r7_run(&c).await,
                            k,
                        ));
                    }
                    let segs = clouds(seed ^ 0x5eed, 1800, cap as f32 + 5000.0, 3000.0);
                    let c = mk(1800).env(move |t| (level_at(&segs, t), 1000.0, true));
                    let k = R7Criteria {
                        writes_exempt: true,
                        ..plain
                    };
                    fails.extend(r7_failures("M4 clouds", &c, &r7_run(&c).await, k));
                    let mut c = mk(900).env(|t| (4000.0, 3000.0, t < 300));
                    c.ecs = true;
                    c.soc = Box::new(|t| if t < 300 { 15 } else { 10 });
                    fails.extend(r7_failures(
                        "M4 SOC floor at 300",
                        &c,
                        &r7_run(&c).await,
                        plain,
                    ));
                    let mut c =
                        R7Cfg::new(M::FeedinPriority, cap, lag, noise, seed, RClip::Step, 900)
                            .env(|_| (15_000.0, 2000.0, true));
                    c.no_mode8 = true;
                    c.ecs = true;
                    c.soc = Box::new(|_| 100);
                    fails.extend(r7_failures("M4 FI full", &c, &r7_run(&c).await, plain));
                }
                fails.extend(r7_ramps(cap, lag, RClip::Step, true).await);
            }
        }
        r7_assert(&fails);
    }

    #[tokio::test]
    async fn r7_mode_4_closed_loop_27_5_kw() {
        r7_mode_4(27_500, [5000.0, 15_000.0, 30_000.0]).await;
    }

    #[tokio::test]
    async fn r7_mode_4_closed_loop_9_kw() {
        r7_mode_4(9000, [3000.0, 5000.0, 8500.0]).await;
    }

    /// Block ends: FD curtailing → Self Use → FD discharging → Feed-in curtailing → FD → Self
    /// Use, and FD curtailing → Force Charge → FD at low sun
    async fn r7_block_ends(cap: u32, lag: i64) {
        let mut fails = Vec::new();
        {
            {
                for (noise, seed) in R7_SEEDS {
                    let mut c =
                        R7Cfg::new(M::ForceDischarge, cap, lag, noise, seed, RClip::Step, 1400)
                            .env(|t| {
                                let sun = match t {
                                    0..450 => 15_000.0,
                                    450..700 => 5000.0,
                                    _ => 15_000.0,
                                };
                                (sun, 1000.0, true)
                            });
                    c.plan = Some(Box::new(|t| match t {
                        0..300 => M::ForceDischarge,
                        300..450 => M::SelfUse,
                        450..700 => M::ForceDischarge,
                        700..900 => M::FeedinPriority,
                        900..1100 => M::ForceDischarge,
                        _ => M::SelfUse,
                    }));
                    let r = r7_run(&c).await;
                    fails.extend(r7_failures(
                        "BLOCK FD-SU-FD-FI-FD-SU",
                        &c,
                        &r,
                        R7Criteria::default(),
                    ));
                    if r.end_exit_delay.iter().any(|d| *d > 5) {
                        fails.push(format!("BLOCK FD-SU-FD-FI-FD-SU cap {cap} lag {lag} seed {seed}: exit delays {:?}", r.end_exit_delay));
                    }
                    let mut c =
                        R7Cfg::new(M::ForceDischarge, cap, lag, noise, seed, RClip::Step, 900)
                            .env(|t| (if t < 600 { 15_000.0 } else { 3000.0 }, 1000.0, true));
                    c.plan = Some(Box::new(|t| match t {
                        0..300 => M::ForceDischarge,
                        300..600 => M::ForceCharge,
                        _ => M::ForceDischarge,
                    }));
                    let r = r7_run(&c).await;
                    fails.extend(r7_failures("BLOCK FD-FC-FD", &c, &r, R7Criteria::default()));
                }
            }
        }
        r7_assert(&fails);
    }

    #[tokio::test]
    async fn r7_block_ends_9_kw_lag_15() {
        r7_block_ends(9000, 15).await;
    }

    #[tokio::test]
    async fn r7_block_ends_9_kw_lag_30() {
        r7_block_ends(9000, 30).await;
    }

    #[tokio::test]
    async fn r7_block_ends_3_kw_lag_15() {
        r7_block_ends(3000, 15).await;
    }

    #[tokio::test]
    async fn r7_block_ends_3_kw_lag_30() {
        r7_block_ends(3000, 30).await;
    }
}
