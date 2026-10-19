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

//! Cost of charge of the energy in the battery (brief H33).
//!
//! Every kWh that charges the battery is priced at what it cost:
//! - from solar: the effective sell price (the sale given up), or 0 when it could not have been
//!   sold (export cap reached, export disabled or curtailed, sell price ≤ 0);
//! - from the grid: the effective buy price.
//!
//! The cost per kWh later delivered from the battery is `price / round_trip + wear`, with
//! `round_trip = battery_efficiency²` (`control.battery_efficiency` is one-way: 0.95 → 0.9025).
//! It is a weighted average over the usable energy (energy above the current minimum SOC).
//! Discharging lowers the usable energy but not the average. At the minimum the cost resets.
//! When solar and grid charge in the same interval, the site meter decides: the grid part is
//! min(battery charge, site grid import), the rest is solar. On parallel units each unit reports
//! only its own share of PV, load and battery, so a per-unit house load cannot split the charge.
//!
//! Without a saved cost (first start) the cost stays unknown until the battery first reaches its
//! minimum; the planner then uses its `cheap_ref` rules.
//!
//! Energy is integrated per poll interval with the trapezoid rule; `usable_kwh` is re-anchored
//! to the measured SOC on every update, so integration drift does not accumulate.

use std::path::PathBuf;

use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Where the cost of charge is persisted (survives restarts).
pub const BATTERY_COST_BASIS_PATH: &str = "/data/battery_cost_basis.json";

/// SOC (percentage points) above the minimum at which the battery counts as empty: the cost
/// resets there. Inverters report whole-percent SOC and rarely settle exactly on the floor (the
/// owner's battery at a 25 % Back Up floor read 25 % twice in 72 h, mostly 26-28 %), so one
/// whole percent above the floor counts as the minimum.
const RESET_MARGIN_PCT: f32 = 1.0;

/// Longest poll interval (s) that is integrated. A longer gap is handled like a restart: a SOC
/// rise across it is priced at the current effective buy price.
const MAX_INTEGRATION_GAP_SECS: f32 = 600.0;

/// The cost of charge is written to disk at most this often (s), and on every reset.
const PERSIST_INTERVAL_SECS: i64 = 300;

/// Battery figures the cost of charge needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBasisBattery {
    pub capacity_kwh: f32,
    /// Current minimum SOC (%): `control.min_battery_soc`, or the Back Up floor in Back Up
    pub min_soc_pct: f32,
    pub max_soc_pct: f32,
    /// One-way efficiency (`control.battery_efficiency`); the round trip is its square
    pub efficiency: f32,
    pub wear_czk_per_kwh: f32,
}

impl CostBasisBattery {
    #[must_use]
    pub fn round_trip(&self) -> f32 {
        (self.efficiency * self.efficiency).max(1e-6)
    }

    /// Cost per kWh later delivered from the battery of energy charged at `price_czk`.
    #[must_use]
    pub fn delivered_cost(&self, price_czk: f32) -> f32 {
        price_czk / self.round_trip() + self.wear_czk_per_kwh
    }

    /// Usable energy (kWh above the minimum) at `soc_pct`.
    #[must_use]
    pub fn usable_kwh(&self, soc_pct: f32) -> f32 {
        let span = (self.max_soc_pct - self.min_soc_pct).max(0.0);
        ((soc_pct - self.min_soc_pct).clamp(0.0, span)) / 100.0 * self.capacity_kwh
    }

    fn at_minimum(&self, soc_pct: f32) -> bool {
        soc_pct <= self.min_soc_pct + RESET_MARGIN_PCT
    }
}

/// Energy flows of one interval (kWh) and the prices in force (CZK/kWh).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBasisStep {
    /// Measured SOC at the end of the interval (%)
    pub soc_pct: f32,
    /// Energy into the battery (AC side, before losses), summed over all units
    pub battery_charge_kwh: f32,
    /// Grid import at the site meter
    pub grid_import_kwh: f32,
    /// How much more could have been exported in the interval (export limit minus export).
    /// `None` = no limit known. 0 when export is disabled or curtailed.
    pub export_headroom_kwh: Option<f32>,
    /// Effective sell price
    pub sell_czk: f32,
    /// Effective buy price
    pub buy_czk: f32,
    pub at: DateTime<Utc>,
}

/// Cost of charge of the energy in the battery.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BatteryCostBasis {
    /// Cost per kWh delivered from the battery (CZK/kWh), losses and wear included.
    /// `None` = unknown (the battery reached the minimum and nothing has charged it since).
    pub cost_czk_per_kwh: Option<f32>,
    /// Usable energy (kWh above the current minimum), anchored to the measured SOC
    pub usable_kwh: f32,
    /// Part of `usable_kwh` that came from solar
    pub solar_kwh: f32,
    /// Part of `usable_kwh` that came from the grid
    pub grid_kwh: f32,
    pub updated_at: Option<DateTime<Utc>>,
    /// Energy of unknown cost is in the battery (first start without a saved cost): the cost
    /// stays unknown until the battery reaches the minimum.
    #[serde(default)]
    pub unknown_until_reset: bool,
}

impl CostBasisStep {
    /// Every price, energy and the SOC is a finite number.
    fn is_finite(&self) -> bool {
        [
            self.soc_pct,
            self.battery_charge_kwh,
            self.grid_import_kwh,
            self.sell_czk,
            self.buy_czk,
            self.export_headroom_kwh.unwrap_or(0.0),
        ]
        .iter()
        .all(|v| v.is_finite())
    }
}

impl BatteryCostBasis {
    /// Share of the usable energy from solar (0..1), `None` when the battery is empty.
    #[must_use]
    pub fn solar_share(&self) -> Option<f32> {
        let total = self.solar_kwh + self.grid_kwh;
        (total > 1e-6).then(|| self.solar_kwh / total)
    }

    fn reset(&mut self, at: DateTime<Utc>) {
        *self = Self {
            updated_at: Some(at),
            ..Self::default()
        };
    }

    /// Add `stored_kwh` (energy now in the battery) at `cost` per delivered kWh to the weighted
    /// average, on top of `base_kwh` already there at the current cost.
    fn add(&mut self, base_kwh: f32, stored_kwh: f32, cost: f32) {
        if !(stored_kwh.is_finite() && cost.is_finite() && base_kwh.is_finite()) {
            debug!(
                "Battery cost of charge: non-finite input ignored (base {base_kwh}, stored {stored_kwh}, cost {cost})"
            );
            return;
        }
        if stored_kwh <= 0.0 {
            return;
        }
        self.cost_czk_per_kwh = Some(match self.cost_czk_per_kwh {
            Some(old) if base_kwh > 0.0 => {
                (old * base_kwh + cost * stored_kwh) / (base_kwh + stored_kwh)
            }
            _ => cost,
        });
    }

    /// Re-anchor the usable energy (and the solar/grid split) to the measured value.
    fn anchor(&mut self, usable_kwh: f32) {
        let split = self.solar_kwh + self.grid_kwh;
        if split > 1e-6 {
            let scale = usable_kwh / split;
            self.solar_kwh *= scale;
            self.grid_kwh *= scale;
        } else {
            self.solar_kwh = 0.0;
            self.grid_kwh = usable_kwh;
        }
        self.usable_kwh = usable_kwh;
    }

    /// Apply one interval. Returns true when the cost reset at the minimum.
    pub fn update(&mut self, step: &CostBasisStep, battery: &CostBasisBattery) -> bool {
        if !step.is_finite() {
            debug!(
                "Battery cost of charge: interval with a non-finite price, power or SOC ignored: {step:?}"
            );
            return false;
        }
        if battery.at_minimum(step.soc_pct) {
            let was_set = self.cost_czk_per_kwh.is_some() || self.usable_kwh > 0.0;
            self.reset(step.at);
            return was_set;
        }
        if self.unknown_until_reset {
            self.usable_kwh = battery.usable_kwh(step.soc_pct);
            self.updated_at = Some(step.at);
            return false;
        }
        let charge = step.battery_charge_kwh.max(0.0);
        if charge > 0.0 {
            // Site meter: what the grid supplied is the grid part, the rest came from solar.
            let grid = step.grid_import_kwh.clamp(0.0, charge);
            let solar = charge - grid;
            // Solar that could not have been sold is worth 0.
            let sellable = if step.sell_czk > 0.0 {
                step.export_headroom_kwh
                    .map_or(solar, |headroom| solar.min(headroom.max(0.0)))
            } else {
                0.0
            };
            let solar_value_czk = sellable * step.sell_czk;
            let eta = battery.efficiency.max(1e-6);
            let (solar_stored, grid_stored) = (solar * eta, grid * eta);
            let base = self.usable_kwh;
            if solar_stored > 0.0 {
                // Average price of this interval's solar: sellable part at sell, rest at 0.
                let price = solar_value_czk / solar;
                self.add(base, solar_stored, battery.delivered_cost(price));
            }
            if grid_stored > 0.0 {
                self.add(
                    base + solar_stored,
                    grid_stored,
                    battery.delivered_cost(step.buy_czk),
                );
            }
            self.solar_kwh += solar_stored;
            self.grid_kwh += grid_stored;
        }
        self.anchor(battery.usable_kwh(step.soc_pct));
        self.updated_at = Some(step.at);
        false
    }

    /// Resume from a stored state (restart or a long data gap) at the measured SOC. With a saved
    /// cost, a SOC rise while nothing was observed is priced at the current effective buy price
    /// and the energy that was there keeps its cost. Without one, energy above the minimum has an
    /// unknown cost until the battery reaches the minimum. Returns the rise (kWh) that was priced.
    pub fn resume(
        &mut self,
        soc_pct: f32,
        buy_czk: f32,
        battery: &CostBasisBattery,
        at: DateTime<Utc>,
    ) -> f32 {
        if !(soc_pct.is_finite() && buy_czk.is_finite()) {
            debug!(
                "Battery cost of charge: resume with a non-finite SOC {soc_pct} or buy price {buy_czk} ignored"
            );
            return 0.0;
        }
        if battery.at_minimum(soc_pct) {
            self.reset(at);
            return 0.0;
        }
        let usable = battery.usable_kwh(soc_pct);
        if self.cost_czk_per_kwh.is_none() {
            if usable > 1e-3 {
                self.unknown_until_reset = true;
                self.solar_kwh = 0.0;
                self.grid_kwh = 0.0;
            }
            self.usable_kwh = usable;
            self.updated_at = Some(at);
            return 0.0;
        }
        let known = self.usable_kwh.min(usable);
        let gap = usable - known;
        if gap > 1e-3 {
            self.anchor(known);
            self.add(known, gap, battery.delivered_cost(buy_czk));
            self.grid_kwh += gap;
        }
        self.anchor(usable);
        self.updated_at = Some(at);
        gap.max(0.0)
    }
}

/// One poll of the measured inverter state (sums over the inverters).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerSample {
    pub at: DateTime<Utc>,
    pub soc_pct: f32,
    /// Battery power (W), positive = charge, summed over all units
    pub battery_w: f32,
    /// Grid import at the site meter (W), ≥ 0
    pub import_w: f32,
    /// Export to the grid at the site meter (W), ≥ 0
    pub export_w: f32,
    /// Export limit in force (W); `None` = unknown
    pub export_limit_w: Option<f32>,
}

/// Prices in force at a sample (CZK/kWh).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBasisPrices {
    pub buy_czk: f32,
    pub sell_czk: f32,
}

/// Interval energies between two samples, trapezoid rule.
#[must_use]
pub fn integrate(
    prev: &PowerSample,
    now: &PowerSample,
    prices: CostBasisPrices,
) -> Option<CostBasisStep> {
    let dt_h = (now.at - prev.at).num_milliseconds() as f32 / 3_600_000.0;
    if dt_h <= 0.0 {
        return None;
    }
    let trap = |a: f32, b: f32| (a.max(0.0) + b.max(0.0)) / 2.0 * dt_h / 1000.0;
    let headroom = match (prev.export_limit_w, now.export_limit_w) {
        (Some(a), Some(b)) => Some(trap(a - prev.export_w, b - now.export_w)),
        (_, Some(b)) => Some(trap(b - now.export_w, b - now.export_w)),
        _ => None,
    };
    Some(CostBasisStep {
        soc_pct: now.soc_pct,
        battery_charge_kwh: trap(prev.battery_w, now.battery_w),
        grid_import_kwh: trap(prev.import_w, now.import_w),
        export_headroom_kwh: headroom,
        sell_czk: prices.sell_czk,
        buy_czk: prices.buy_czk,
        at: now.at,
    })
}

/// ECS resource: the cost of charge, the last sample and the persistence state.
#[derive(Resource, Debug, Default)]
pub struct BatteryCostTracker {
    pub basis: BatteryCostBasis,
    last_sample: Option<PowerSample>,
    /// True until the first sample after start: the stored state is resumed then
    pending_resume: bool,
    persist_path: Option<PathBuf>,
    last_persisted: Option<DateTime<Utc>>,
}

impl BatteryCostTracker {
    /// Load the persisted cost of charge (if any); it is resumed at the first sample.
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let basis = std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| serde_json::from_str::<BatteryCostBasis>(&json).ok());
        if basis.is_some() {
            info!("Battery cost of charge: loaded {}", path.display());
        }
        Self {
            basis: basis.unwrap_or_default(),
            last_sample: None,
            pending_resume: true,
            persist_path: Some(path),
            last_persisted: None,
        }
    }

    /// In memory only (tests).
    #[must_use]
    pub fn in_memory(basis: BatteryCostBasis) -> Self {
        Self {
            basis,
            pending_resume: true,
            ..Self::default()
        }
    }

    /// Cost per delivered kWh for the planner (`None` = unknown).
    #[must_use]
    pub fn cost_czk_per_kwh(&self) -> Option<f32> {
        self.basis.cost_czk_per_kwh
    }

    /// Feed one poll. The first sample (and the first after a long gap) resumes the stored state.
    pub fn observe(
        &mut self,
        sample: PowerSample,
        prices: CostBasisPrices,
        battery: &CostBasisBattery,
    ) {
        let step = self
            .last_sample
            .filter(|prev| {
                let gap = (sample.at - prev.at).num_milliseconds() as f32 / 1000.0;
                gap <= MAX_INTEGRATION_GAP_SECS
            })
            .and_then(|prev| integrate(&prev, &sample, prices));
        if self.last_sample.is_some_and(|prev| prev.at >= sample.at) {
            return; // same poll again
        }
        let mut force_persist = false;
        match step {
            Some(step) if !self.pending_resume => {
                if self.basis.update(&step, battery) {
                    info!(
                        "Battery cost of charge: reset at the minimum SOC {:.0}%",
                        sample.soc_pct
                    );
                    force_persist = true;
                }
            }
            _ => {
                let old_usable = self.basis.usable_kwh;
                let gap = self
                    .basis
                    .resume(sample.soc_pct, prices.buy_czk, battery, sample.at);
                if gap > 0.0 {
                    info!(
                        "Battery cost of charge: SOC rose while not observed: {:.2} kWh priced at the current buy price {:.2} CZK/kWh, the old {:.2} kWh keep their cost",
                        gap,
                        prices.buy_czk,
                        old_usable.min(self.basis.usable_kwh),
                    );
                } else if self.basis.unknown_until_reset {
                    info!(
                        "Battery cost of charge: no saved cost, {:.2} kWh of unknown cost; unknown until the battery reaches the minimum SOC",
                        self.basis.usable_kwh
                    );
                }
                self.pending_resume = false;
                force_persist = true;
            }
        }
        self.last_sample = Some(sample);
        self.persist(sample.at, force_persist);
    }

    fn persist(&mut self, now: DateTime<Utc>, force: bool) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let due = force
            || self
                .last_persisted
                .is_none_or(|t| (now - t).num_seconds() >= PERSIST_INTERVAL_SECS);
        if !due {
            return;
        }
        self.last_persisted = Some(now);
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_string(&self.basis)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                std::fs::write(&tmp, json)?;
                std::fs::rename(&tmp, path)
            });
        if let Err(e) = written {
            warn!(
                "Battery cost of charge: cannot write {}: {e}",
                path.display()
            );
        }
    }
}

/// Minimum SOC the cost of charge counts from: the Back Up floor the planner uses in Back Up
/// default mode, else `control.min_battery_soc`.
#[must_use]
pub fn cost_basis_battery(
    control: &fluxion_types::config::ControlConfig,
    user_control: Option<&fluxion_types::UserControlState>,
) -> CostBasisBattery {
    let min_soc_pct = crate::scheduling::backup_planner_floor(
        user_control,
        control.min_battery_soc,
        control.max_battery_soc,
    )
    .unwrap_or(control.min_battery_soc);
    CostBasisBattery {
        capacity_kwh: control.battery_capacity_kwh,
        min_soc_pct,
        max_soc_pct: control.max_battery_soc,
        efficiency: control.battery_efficiency,
        wear_czk_per_kwh: control.battery_wear_cost_czk_per_kwh,
    }
}

/// One site sample from the polled inverters (`(is_slave, state)`): SOC is the mean over the
/// inverters (as the scheduler uses it). Battery power is summed over every inverter and every
/// read-only parallel unit (`extra_battery_power_w`). Grid import, export and the live export
/// limit come from the inverters that are not slaves (the site meter); the export limit is
/// capped at `max_export_w`.
#[must_use]
pub fn site_sample(
    units: &[(bool, &crate::traits::GenericInverterState)],
    at: DateTime<Utc>,
    max_export_w: f32,
) -> PowerSample {
    let n = units.len().max(1) as f32;
    let soc_pct = units.iter().map(|(_, s)| s.battery_soc).sum::<f32>() / n;
    let battery_w: f32 = units
        .iter()
        .map(|(_, s)| s.battery_power_w + s.extra_battery_power_w.unwrap_or(0.0))
        .sum();
    let meter: Vec<_> = units
        .iter()
        .filter(|(slave, _)| !slave)
        .map(|(_, s)| *s)
        .collect();
    let import_w: f32 = meter
        .iter()
        .map(|s| s.grid_import_w.unwrap_or((-s.grid_power_w).max(0.0)))
        .sum();
    let export_w: f32 = meter
        .iter()
        .map(|s| s.grid_export_w.unwrap_or(s.grid_power_w.max(0.0)))
        .sum();
    let live_limit_w: Option<f32> = meter
        .iter()
        .map(|s| s.export_limit_w.map(|w| w as f32))
        .sum::<Option<f32>>();
    PowerSample {
        at,
        soc_pct,
        battery_w,
        import_w,
        export_w,
        export_limit_w: Some(live_limit_w.map_or(max_export_w, |w| w.min(max_export_w))),
    }
}

/// Feed the polled inverter state into the cost of charge. SOC is the mean over the inverters
/// (as the scheduler uses it). Battery power is summed over every inverter and every read-only
/// parallel unit (`pv_extra_prefixes`) that reports one. Grid import, export and the export
/// limit come from the inverters that are not slaves (the site meter). No HA writes.
pub fn battery_cost_basis_system(
    mut tracker: ResMut<BatteryCostTracker>,
    inverters: Query<(
        &fluxion_types::inverter::Inverter,
        &crate::components::RawInverterState,
    )>,
    prices: Query<&fluxion_types::pricing::SpotPriceData>,
    system_config: Res<crate::resources::SystemConfig>,
    user_control: Option<Res<crate::resources::UserControlResource>>,
) {
    let states: Vec<_> = inverters.iter().collect();
    let Some(at) = states.iter().map(|(_, raw)| raw.last_updated).max() else {
        return;
    };
    if tracker.last_sample.is_some_and(|s| s.at >= at) {
        return;
    }
    let control = &system_config.control_config;
    let Some(block) = prices.iter().next().and_then(|p| {
        p.time_block_prices.iter().find(|b| {
            at >= b.block_start
                && at < b.block_start + chrono::Duration::minutes(i64::from(b.duration_minutes))
        })
    }) else {
        return;
    };
    let block_prices = CostBasisPrices {
        buy_czk: block.effective_price_czk_per_kwh,
        sell_czk: block
            .spot_sell_price_czk_per_kwh
            .unwrap_or(control.grid_export_fee_czk_per_kwh),
    };
    let is_slave = |id: &str| {
        system_config.inverters.iter().any(|i| {
            i.id == id
                && matches!(
                    i.topology,
                    fluxion_types::config::InverterTopology::Slave { .. }
                )
        })
    };
    let units: Vec<(bool, &crate::traits::GenericInverterState)> = states
        .iter()
        .map(|(inv, raw)| (is_slave(&inv.id), &raw.state))
        .collect();
    let sample = site_sample(&units, at, control.maximum_export_power_w as f32);
    let battery = cost_basis_battery(control, user_control.as_deref().map(|u| &u.state));
    tracker.observe(sample, block_prices, &battery);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn battery() -> CostBasisBattery {
        CostBasisBattery {
            capacity_kwh: 10.0,
            min_soc_pct: 10.0,
            max_soc_pct: 100.0,
            efficiency: 0.95,
            wear_czk_per_kwh: 0.1,
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 10, 0, 0).unwrap()
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// One interval charging `charge` kWh while the site meter imports `import`, ending at `soc`.
    fn step(soc: f32, charge: f32, import: f32, sell: f32, buy: f32) -> CostBasisStep {
        CostBasisStep {
            soc_pct: soc,
            battery_charge_kwh: charge,
            grid_import_kwh: import,
            export_headroom_kwh: None,
            sell_czk: sell,
            buy_czk: buy,
            at: t0(),
        }
    }

    /// A battery at the minimum, so the next charge sets the cost on its own.
    fn empty() -> BatteryCostBasis {
        BatteryCostBasis::default()
    }

    fn sample(at: DateTime<Utc>, soc: f32, battery_w: f32, import_w: f32) -> PowerSample {
        PowerSample {
            at,
            soc_pct: soc,
            battery_w,
            import_w,
            export_w: 0.0,
            export_limit_w: None,
        }
    }

    const PRICES: CostBasisPrices = CostBasisPrices {
        buy_czk: 4.0,
        sell_czk: 2.0,
    };

    #[test]
    fn round_trip_is_the_square_of_the_one_way_efficiency() {
        assert!(approx(battery().round_trip(), 0.9025));
    }

    #[test]
    fn solar_only_charge_costs_the_sell_price_plus_losses_and_wear() {
        let mut b = empty();
        // 2 kWh in at η 0.95 = 1.9 kWh stored: 10 % → 29 %, nothing imported
        b.update(&step(29.0, 2.0, 0.0, 3.0, 4.0), &battery());
        let want = 3.0 / 0.9025 + 0.1;
        assert!(approx(b.cost_czk_per_kwh.unwrap(), want), "{b:?}");
        assert!(approx(b.solar_share().unwrap(), 1.0));
    }

    #[test]
    fn grid_only_charge_costs_the_buy_price_plus_losses_and_wear() {
        let mut b = empty();
        // The site imports more than the charge: all of it is grid
        b.update(&step(29.0, 2.0, 2.5, 3.0, 4.0), &battery());
        let want = 4.0 / 0.9025 + 0.1;
        assert!(approx(b.cost_czk_per_kwh.unwrap(), want), "{b:?}");
        assert!(approx(b.solar_share().unwrap(), 0.0));
    }

    #[test]
    fn mixed_interval_is_split_by_the_site_meter() {
        let mut b = empty();
        // 2.0 kWh charge, 1.0 kWh imported: grid 1.0, solar 1.0
        b.update(&step(29.0, 2.0, 1.0, 3.0, 4.0), &battery());
        let want = (battery().delivered_cost(3.0) + battery().delivered_cost(4.0)) / 2.0;
        assert!(approx(b.cost_czk_per_kwh.unwrap(), want), "{b:?}");
        assert!(approx(b.solar_share().unwrap(), 0.5));
        assert!(approx(b.usable_kwh, 1.9));
    }

    #[test]
    fn two_charges_give_the_energy_weighted_average() {
        let mut b = empty();
        let bat = battery();
        // 1 kWh in at buy 2.0 (0.95 stored), then 3 kWh in at buy 4.0 (2.85 stored)
        b.update(&step(19.5, 1.0, 1.0, 1.0, 2.0), &bat);
        b.update(&step(48.0, 3.0, 3.0, 1.0, 4.0), &bat);
        let want = (bat.delivered_cost(2.0) * 0.95 + bat.delivered_cost(4.0) * 2.85) / 3.8;
        assert!(approx(b.cost_czk_per_kwh.unwrap(), want), "{b:?}");
        assert!(approx(b.usable_kwh, 3.8));
    }

    #[test]
    fn discharge_keeps_the_cost_per_kwh_and_lowers_the_usable_energy() {
        let mut b = empty();
        let bat = battery();
        b.update(&step(29.0, 2.0, 1.0, 3.0, 4.0), &bat);
        let cost = b.cost_czk_per_kwh.unwrap();
        let share = b.solar_share().unwrap();
        b.update(&step(20.0, 0.0, 0.0, 3.0, 4.0), &bat);
        assert!(approx(b.cost_czk_per_kwh.unwrap(), cost));
        assert!(approx(b.solar_share().unwrap(), share));
        assert!(approx(b.usable_kwh, 1.0));
    }

    #[test]
    fn reaching_the_minimum_resets_and_the_next_charge_sets_the_cost() {
        let mut b = empty();
        let bat = battery();
        b.update(&step(29.0, 2.0, 2.0, 3.0, 4.0), &bat);
        assert!(b.update(&step(10.0, 0.0, 1.0, 3.0, 4.0), &bat));
        assert_eq!(b.cost_czk_per_kwh, None);
        assert!(approx(b.usable_kwh, 0.0));
        b.update(&step(29.0, 2.0, 0.0, 1.5, 4.0), &bat);
        assert!(approx(b.cost_czk_per_kwh.unwrap(), bat.delivered_cost(1.5)));
    }

    #[test]
    fn solar_that_could_not_be_sold_is_worth_only_losses_and_wear() {
        let bat = battery();
        // Export capped: no headroom left
        let mut capped = empty();
        let mut s = step(29.0, 2.0, 0.0, 3.0, 4.0);
        s.export_headroom_kwh = Some(0.0);
        capped.update(&s, &bat);
        assert!(approx(capped.cost_czk_per_kwh.unwrap(), 0.1), "{capped:?}");
        // Negative or zero sell price
        let mut negative = empty();
        negative.update(&step(29.0, 2.0, 0.0, -0.4, 4.0), &bat);
        assert!(
            approx(negative.cost_czk_per_kwh.unwrap(), 0.1),
            "{negative:?}"
        );
        // Half the solar charge could have been sold
        let mut half = empty();
        let mut s = step(29.0, 2.0, 0.0, 3.0, 4.0);
        s.export_headroom_kwh = Some(1.0);
        half.update(&s, &bat);
        assert!(approx(
            half.cost_czk_per_kwh.unwrap(),
            bat.delivered_cost(1.5)
        ));
    }

    #[test]
    fn one_whole_percent_above_the_floor_resets() {
        let backup = CostBasisBattery {
            capacity_kwh: 23.0,
            min_soc_pct: 25.0,
            ..battery()
        };
        let mut b = empty();
        b.update(&step(60.0, 2.0, 2.0, 1.0, 2.0), &backup);
        assert!(b.cost_czk_per_kwh.is_some());
        // 27 % is not the minimum, 26 % is
        assert!(!b.update(&step(27.0, 0.0, 0.0, 1.0, 2.0), &backup));
        assert!(b.cost_czk_per_kwh.is_some());
        assert!(b.update(&step(26.0, 0.0, 0.0, 1.0, 2.0), &backup));
        assert_eq!(b.cost_czk_per_kwh, None);
        // And a first start at 26 % is a known (empty) start, not an unknown one
        let mut first = BatteryCostBasis::default();
        first.resume(26.0, 4.0, &backup, t0());
        assert!(!first.unknown_until_reset);
    }

    #[test]
    fn non_finite_inputs_are_ignored_and_never_stored() {
        let bat = battery();
        let mut b = empty();
        b.update(&step(29.0, 2.0, 1.0, 3.0, 4.0), &bat);
        let before = b.clone();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for s in [
                step(35.0, 1.0, 0.0, bad, 4.0),
                step(35.0, 1.0, 1.0, 3.0, bad),
                step(35.0, bad, 0.0, 3.0, 4.0),
                step(35.0, 1.0, bad, 3.0, 4.0),
                step(bad, 1.0, 1.0, 3.0, 4.0),
                CostBasisStep {
                    export_headroom_kwh: Some(bad),
                    ..step(35.0, 1.0, 0.0, 3.0, 4.0)
                },
            ] {
                assert!(!b.update(&s, &bat));
                assert_eq!(b, before, "{s:?}");
            }
            assert!(approx(b.resume(60.0, bad, &bat, t0()), 0.0));
            assert_eq!(b, before);
            b.add(1.0, 1.0, bad);
            b.add(1.0, bad, 3.0);
            assert_eq!(b, before);
        }
        // The tracker skips a NaN sample and keeps a finite cost
        let mut tracker = BatteryCostTracker::in_memory(BatteryCostBasis::default());
        tracker.observe(sample(t0(), 10.0, 0.0, 0.0), PRICES, &bat);
        tracker.observe(
            sample(t0() + Duration::minutes(5), 15.0, f32::NAN, 0.0),
            PRICES,
            &bat,
        );
        tracker.observe(
            sample(t0() + Duration::minutes(10), 20.0, 4000.0, 4000.0),
            PRICES,
            &bat,
        );
        assert!(tracker.cost_czk_per_kwh().is_none_or(f32::is_finite));
        assert!(tracker.basis.usable_kwh.is_finite());
        let json = serde_json::to_string(&tracker.basis).unwrap();
        assert!(!json.contains("NaN"), "{json}");
    }

    #[test]
    fn back_up_floor_is_the_minimum() {
        let backup = CostBasisBattery {
            min_soc_pct: 40.0,
            ..battery()
        };
        let mut b = empty();
        // 45 % is 0.5 kWh above the Back Up floor
        b.update(&step(45.0, 1.0, 1.0, 1.0, 2.0), &backup);
        assert!(approx(b.usable_kwh, 0.5));
        assert!(b.cost_czk_per_kwh.is_some());
        // Self-Use minimum 10 % would still see energy at 40 %; the Back Up floor resets.
        assert!(b.update(&step(40.0, 0.0, 0.5, 1.0, 2.0), &backup));
        assert_eq!(b.cost_czk_per_kwh, None);
    }

    #[test]
    fn usable_energy_follows_the_measured_soc_not_the_integration() {
        let mut b = empty();
        // Integration says 0.95 kWh stored, the SOC says 1.5 kWh: the SOC wins.
        b.update(&step(25.0, 1.0, 1.0, 1.0, 2.0), &battery());
        assert!(approx(b.usable_kwh, 1.5));
        assert!(approx(b.grid_kwh + b.solar_kwh, 1.5));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("battery_cost_basis.json");
        let bat = battery();
        let mut tracker = BatteryCostTracker::load(&path);
        // Empty battery at start: the next charge sets a known cost
        tracker.observe(sample(t0(), 10.0, 0.0, 0.0), PRICES, &bat);
        tracker.observe(
            sample(t0() + Duration::minutes(10), 19.5, 4000.0, 4000.0),
            PRICES,
            &bat,
        );
        assert!(tracker.cost_czk_per_kwh().is_some());
        // Persisted at the first sample, then within PERSIST_INTERVAL_SECS: force one more save
        tracker.persist(t0() + Duration::hours(1), true);
        let reloaded = BatteryCostTracker::load(&path);
        assert_eq!(reloaded.basis, tracker.basis);
    }

    #[test]
    fn first_start_without_a_saved_cost_is_unknown_until_the_minimum() {
        let bat = battery();
        let mut tracker = BatteryCostTracker::in_memory(BatteryCostBasis::default());
        tracker.observe(sample(t0(), 50.0, 0.0, 0.0), PRICES, &bat);
        assert_eq!(tracker.cost_czk_per_kwh(), None);
        assert!(tracker.basis.unknown_until_reset);
        assert!(approx(tracker.basis.usable_kwh, 4.0));
        // Charging does not make the mix known
        tracker.observe(
            sample(t0() + Duration::minutes(10), 59.5, 4000.0, 4000.0),
            PRICES,
            &bat,
        );
        assert_eq!(tracker.cost_czk_per_kwh(), None);
        // Reaching the minimum resets; the next charge sets the cost
        tracker.observe(
            sample(t0() + Duration::hours(5), 10.0, 0.0, 0.0),
            PRICES,
            &bat,
        );
        assert!(!tracker.basis.unknown_until_reset);
        tracker.observe(
            sample(t0() + Duration::minutes(310), 19.5, 4000.0, 0.0),
            PRICES,
            &bat,
        );
        assert!(approx(
            tracker.cost_czk_per_kwh().unwrap(),
            bat.delivered_cost(2.0)
        ));
    }

    #[test]
    fn soc_rise_while_down_keeps_old_cost_and_prices_the_gap_at_buy() {
        let bat = battery();
        // Stored: 2 kWh usable (30 %) at 1.5 CZK/kWh
        let saved = BatteryCostBasis {
            cost_czk_per_kwh: Some(1.5),
            usable_kwh: 2.0,
            solar_kwh: 2.0,
            grid_kwh: 0.0,
            updated_at: Some(t0()),
            unknown_until_reset: false,
        };
        let mut b = saved.clone();
        // Restarted at 50 %: 4 kWh usable, 2 kWh unobserved, buy now 3.0
        let gap = b.resume(50.0, 3.0, &bat, t0() + Duration::hours(2));
        assert!(approx(gap, 2.0));
        let want = (1.5 * 2.0 + bat.delivered_cost(3.0) * 2.0) / 4.0;
        assert!(approx(b.cost_czk_per_kwh.unwrap(), want), "{b:?}");
        assert!(approx(b.solar_share().unwrap(), 0.5));
        // Restarted lower than stored: discharged while down, cost unchanged
        let mut lower = saved;
        assert!(approx(lower.resume(20.0, 3.0, &bat, t0()), 0.0));
        assert!(approx(lower.cost_czk_per_kwh.unwrap(), 1.5));
        assert!(approx(lower.usable_kwh, 1.0));
    }

    #[test]
    fn tracker_integrates_with_the_trapezoid_rule() {
        let bat = battery();
        let mut tracker = BatteryCostTracker::in_memory(BatteryCostBasis::default());
        tracker.observe(sample(t0(), 10.0, 0.0, 0.0), PRICES, &bat);
        assert_eq!(tracker.cost_czk_per_kwh(), None);
        // 0 W → 4000 W over 10 min: 1/3 kWh in by the trapezoid rule, all imported
        tracker.observe(
            sample(t0() + Duration::minutes(10), 19.5, 4000.0, 4000.0),
            PRICES,
            &bat,
        );
        assert!(approx(
            tracker.cost_czk_per_kwh().unwrap(),
            bat.delivered_cost(4.0)
        ));
        assert!(approx(tracker.basis.grid_kwh, 0.95));
        let step = integrate(
            &sample(t0(), 10.0, 0.0, 0.0),
            &sample(t0() + Duration::minutes(10), 19.5, 4000.0, 4000.0),
            PRICES,
        )
        .unwrap();
        assert!(approx(step.battery_charge_kwh, 1.0 / 3.0));
        assert!(approx(step.grid_import_kwh, 1.0 / 3.0));
    }

    fn unit(
        soc: f32,
        battery_w: f32,
        extra_battery_w: Option<f32>,
        grid_w: f32,
        house_w: f32,
    ) -> crate::traits::GenericInverterState {
        crate::traits::GenericInverterState {
            inverter_id: "solax".into(),
            battery_soc: soc,
            battery_power_w: battery_w,
            extra_battery_power_w: extra_battery_w,
            grid_power_w: grid_w,
            house_load_w: Some(house_w),
            ..Default::default()
        }
    }

    #[test]
    fn two_unit_site_sums_battery_power_and_attributes_by_the_site_meter() {
        // Master `solax` with a parallel unit `solax_extra` in pv_extra_prefixes. Each reports
        // only its own share: master house 447 W, PV 1335 W, battery 839 W; extra house 1207 W,
        // PV 2256 W, battery 703 W. Site meter: no import, no export.
        let master = unit(60.0, 839.0, Some(703.0), 0.0, 447.0);
        let site = site_sample(&[(false, &master)], t0(), 10_000.0);
        assert!(approx(site.battery_w, 1542.0));
        assert!(approx(site.import_w, 0.0));
        // All of the 1542 W charge is solar: the site imported nothing
        let bat = battery();
        let mut tracker = BatteryCostTracker::in_memory(BatteryCostBasis::default());
        tracker.observe(sample(t0(), 10.0, 0.0, 0.0), PRICES, &bat);
        let later = PowerSample {
            at: t0() + Duration::minutes(10),
            soc_pct: 12.2,
            ..site
        };
        let first = PowerSample {
            at: t0() + Duration::seconds(1),
            soc_pct: 10.6,
            ..site
        };
        tracker.observe(first, PRICES, &bat);
        tracker.observe(later, PRICES, &bat);
        assert!(approx(tracker.basis.solar_share().unwrap(), 1.0));
        assert!(approx(
            tracker.cost_czk_per_kwh().unwrap(),
            bat.delivered_cost(2.0)
        ));
        // A 1000 W site import while charging 1542 W: 1000 W grid, 542 W solar
        let importing = unit(60.0, 839.0, Some(703.0), -1000.0, 447.0);
        let s = site_sample(&[(false, &importing)], t0(), 10_000.0);
        assert!(approx(s.import_w, 1000.0));
        let step = integrate(
            &PowerSample { at: t0(), ..s },
            &PowerSample {
                at: t0() + Duration::hours(1),
                ..s
            },
            PRICES,
        )
        .unwrap();
        assert!(approx(step.battery_charge_kwh, 1.542));
        assert!(approx(step.grid_import_kwh, 1.0));
        // A slave inverter's meter is not the site meter, its battery still counts
        let slave = unit(40.0, 500.0, None, -800.0, 300.0);
        let both = site_sample(&[(false, &master), (true, &slave)], t0(), 10_000.0);
        assert!(approx(both.battery_w, 2042.0));
        assert!(approx(both.import_w, 0.0));
        assert!(approx(both.soc_pct, 50.0));
    }
}
