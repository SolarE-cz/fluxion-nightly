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

//! User levers read from Home Assistant helpers (the "FVE-Automatizace" card).
//!
//! Contract: `docs/legacy-levers/CONTRACT.md`. The HA sync fills [`UserLevers`]; execution
//! applies VPR and the kill switch; the Winter Reserve planner applies the price and
//! discharge rules.

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// VPR ("vlastní pracovní režim") mode, one per option of the HA select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VprMode {
    /// "Žádný": no override.
    #[default]
    None,
    /// "Běžný režim"
    Normal,
    /// "Vybíjení baterie"
    DischargeBattery,
    /// "Nabíjení baterie"
    ChargeBattery,
    /// "Zákaz vybíjení baterie"
    NoDischarge,
    /// "Prodej do sítě"
    SellToGrid,
    /// "Nabíjení ze sítě"
    ChargeFromGrid,
}

impl VprMode {
    /// HA select options, in display order. Index 0 is "no override".
    pub const OPTIONS: [&'static str; 7] = [
        "Žádný",
        "Běžný režim",
        "Vybíjení baterie",
        "Nabíjení baterie",
        "Zákaz vybíjení baterie",
        "Prodej do sítě",
        "Nabíjení ze sítě",
    ];

    /// Parse an HA select option. Unknown text is `None` (no override).
    pub fn from_option(option: &str) -> Self {
        match option.trim() {
            "Běžný režim" => Self::Normal,
            "Vybíjení baterie" => Self::DischargeBattery,
            "Nabíjení baterie" => Self::ChargeBattery,
            "Zákaz vybíjení baterie" => Self::NoDischarge,
            "Prodej do sítě" => Self::SellToGrid,
            "Nabíjení ze sítě" => Self::ChargeFromGrid,
            _ => Self::None,
        }
    }

    /// The HA select option for this mode.
    pub fn option(self) -> &'static str {
        match self {
            Self::None => Self::OPTIONS[0],
            Self::Normal => Self::OPTIONS[1],
            Self::DischargeBattery => Self::OPTIONS[2],
            Self::ChargeBattery => Self::OPTIONS[3],
            Self::NoDischarge => Self::OPTIONS[4],
            Self::SellToGrid => Self::OPTIONS[5],
            Self::ChargeFromGrid => Self::OPTIONS[6],
        }
    }
}

/// One-time VPR window in the site's local time (HA timezone).
/// `until <= from` means the window ends the next day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VprWindow {
    pub mode: VprMode,
    pub from_local: NaiveTime,
    pub until_local: NaiveTime,
    /// Latest `last_changed` of the mode, start and stop helpers: when the user set this window.
    pub set_at: DateTime<Utc>,
}

/// Where `now` falls relative to a one-time VPR window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VprPhase {
    Pending,
    Active,
    /// The window has passed: the select must be reset to "Žádný".
    Expired,
}

impl VprWindow {
    /// UTC bounds of the one occurrence this window refers to.
    ///
    /// Anchored on the local date of `set_at`; `until <= from` ends the next day. If that
    /// occurrence already ended when it was set, the user meant the next one (one day later).
    pub fn bounds<Tz: TimeZone>(&self, tz: &Tz) -> (DateTime<Utc>, DateTime<Utc>) {
        let local = |date: chrono::NaiveDate, time: NaiveTime| {
            tz.from_local_datetime(&date.and_time(time))
                .earliest()
                // A time skipped by a DST jump: take the same wall-clock time an hour later.
                .or_else(|| {
                    tz.from_local_datetime(&(date.and_time(time) + Duration::hours(1)))
                        .earliest()
                })
                .map_or(self.set_at, |t| t.with_timezone(&Utc))
        };
        let set_date = self.set_at.with_timezone(tz).date_naive();
        let end_date = if self.until_local <= self.from_local {
            set_date.succ_opt().unwrap_or(set_date)
        } else {
            set_date
        };
        let (start, end) = (
            local(set_date, self.from_local),
            local(end_date, self.until_local),
        );
        if end <= self.set_at {
            (start + Duration::days(1), end + Duration::days(1))
        } else {
            (start, end)
        }
    }

    /// Phase at `now`. A `None` mode is always `Expired` (nothing to apply).
    pub fn phase<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> VprPhase {
        if self.mode == VprMode::None {
            return VprPhase::Expired;
        }
        let (start, end) = self.bounds(tz);
        if now < start {
            VprPhase::Pending
        } else if now < end {
            VprPhase::Active
        } else {
            VprPhase::Expired
        }
    }
}

/// Discharge (export) limit for one local-time window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DischargeWindowLimit {
    /// Export allowed in at most this many hours (×4 blocks) of the window per local day. 0 = never.
    pub hours: u8,
    /// Export never takes the battery below this SOC (%).
    pub min_soc_pct: f32,
}

/// Lever values. `None` / defaults mean "no user rule"; planner behaviour is then unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct UserLevers {
    /// VPR: highest priority, applied even while `automation_disabled`.
    pub vpr: Option<VprWindow>,
    /// "Zakázat Automatizaci": planner and price rules stop; FluxION sets Self-Use once.
    pub automation_disabled: bool,
    /// Morning window 04:00–14:00 local ("Počet ranních hodin", "Max ranní vybití baterie").
    pub morning_discharge: Option<DischargeWindowLimit>,
    /// Evening window 12:00–24:00 local ("Počet večerních hodin", "Max večerní vybití baterie").
    pub evening_discharge: Option<DischargeWindowLimit>,
    /// "Vynutit nabíjení baterie při ceně": effective buy ≤ this → Force Charge.
    pub force_charge_at_or_below_czk: Option<f32>,
    /// "Zákaz vybíjení při ceně": effective buy ≤ this → battery must not discharge.
    pub no_discharge_at_or_below_czk: Option<f32>,
}

impl UserLevers {
    /// Price levers at or below this value are treated as "off" (HA input_number minimum).
    pub const PRICE_OFF_CZK: f32 = -10.0;

    /// Normalise a price lever: values at the "off" sentinel or non-finite become `None`.
    pub fn price_lever(value: f32) -> Option<f32> {
        (value.is_finite() && value > Self::PRICE_OFF_CZK).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpr_options_round_trip() {
        for option in VprMode::OPTIONS {
            assert_eq!(VprMode::from_option(option).option(), option);
        }
        assert_eq!(VprMode::from_option("nonsense"), VprMode::None);
    }

    fn window(mode: VprMode, from: &str, until: &str, set_at: &str) -> VprWindow {
        VprWindow {
            mode,
            from_local: NaiveTime::parse_from_str(from, "%H:%M").unwrap(),
            until_local: NaiveTime::parse_from_str(until, "%H:%M").unwrap(),
            set_at: set_at.parse().unwrap(),
        }
    }

    #[test]
    fn vpr_phase_same_day() {
        let tz = chrono_tz::Europe::Prague;
        // Set at 14:59 CEST for 15:00-17:30 CEST.
        let w = window(
            VprMode::ChargeBattery,
            "15:00",
            "17:30",
            "2026-10-14T12:59:00Z",
        );
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(w.phase(at("2026-10-14T12:59:30Z"), &tz), VprPhase::Pending);
        assert_eq!(w.phase(at("2026-10-14T13:00:00Z"), &tz), VprPhase::Active);
        assert_eq!(w.phase(at("2026-10-14T15:29:59Z"), &tz), VprPhase::Active);
        assert_eq!(w.phase(at("2026-10-14T15:30:00Z"), &tz), VprPhase::Expired);
    }

    #[test]
    fn vpr_phase_set_inside_window_is_active() {
        let tz = chrono_tz::Europe::Prague;
        let w = window(
            VprMode::NoDischarge,
            "15:00",
            "17:30",
            "2026-10-14T14:00:00Z",
        );
        let now = "2026-10-14T14:00:01Z".parse().unwrap();
        assert_eq!(w.phase(now, &tz), VprPhase::Active);
    }

    #[test]
    fn vpr_phase_crosses_midnight_and_past_window_means_next_occurrence() {
        let tz = chrono_tz::Europe::Prague;
        // Set at 15:00 CEST for 22:00-02:00: active from 22:00 CEST tonight until 02:00 tomorrow.
        let w = window(
            VprMode::ChargeFromGrid,
            "22:00",
            "02:00",
            "2026-10-14T13:00:00Z",
        );
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(w.phase(at("2026-10-14T19:59:00Z"), &tz), VprPhase::Pending);
        assert_eq!(w.phase(at("2026-10-14T23:00:00Z"), &tz), VprPhase::Active);
        assert_eq!(w.phase(at("2026-10-15T00:00:00Z"), &tz), VprPhase::Expired);
        // Set at 23:00 CEST for 01:00-03:00: today's 01:00-03:00 is past, so it means tonight.
        let w = window(
            VprMode::ChargeFromGrid,
            "01:00",
            "03:00",
            "2026-10-14T21:00:00Z",
        );
        assert_eq!(w.phase(at("2026-10-14T23:30:00Z"), &tz), VprPhase::Active);
        assert_eq!(w.phase(at("2026-10-15T01:00:00Z"), &tz), VprPhase::Expired);
    }

    #[test]
    fn vpr_phase_none_mode_is_expired() {
        let tz = chrono_tz::Europe::Prague;
        let w = window(VprMode::None, "00:00", "23:59", "2026-10-14T10:00:00Z");
        assert_eq!(
            w.phase("2026-10-14T11:00:00Z".parse().unwrap(), &tz),
            VprPhase::Expired
        );
    }

    #[test]
    fn price_lever_off_sentinel() {
        assert_eq!(UserLevers::price_lever(-10.0), None);
        assert_eq!(UserLevers::price_lever(f32::NAN), None);
        assert_eq!(UserLevers::price_lever(-1.0), Some(-1.0));
    }
}
