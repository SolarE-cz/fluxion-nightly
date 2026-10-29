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

//! Price feed health check and OTE fallback (brief H35).
//!
//! FluxION reads spot prices from one HA entity (the `cz_energy_spot_prices` integration). When
//! that entity lacks tomorrow's slots after the check time (default 13:45 Europe/Prague), or
//! lacks slots of the rest of today, this module:
//! - logs a WARN once per hour, keeps a dashboard banner and a status for the runtime JSON, and
//!   keeps one HA persistent notification (`fluxion_prices_missing`) up while it lasts;
//! - fetches the OTE day-ahead prices and the CNB EUR/CZK rate in a background task (30 s
//!   timeout, retried every 10 minutes while needed, never when the entity has the prices);
//! - converts them like the integration, checks them against the entity's slots of today, and
//!   fills only the slots the entity lacks. Entity values are never replaced, so once the entity
//!   has the prices again it wins.
//! - with a separate buy entity, fills slots only when buy = a × spot + b fits today's buy prices
//!   (max residual 0.01 CZK/kWh); otherwise it fills nothing (a shorter horizon beats wrong prices).
//! - keeps the last successful validation (day, CNB rate source, buy fit) in `/data`. On the
//!   second day of an outage the entity has no prices of today to check against; a validation at
//!   most 48 hours old is then used instead.

pub mod ote;

use crate::ha::HomeAssistantClient;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Europe::Prague;
use fluxion_core::price_feed::{BuyPriceFit, FallbackState, PriceFeedStatus};
use fluxion_core::{SpotPriceData, TimeBlockPrice};
use ote::{CNB_RATES_URL, Decimal, EurCzkRate, OTE_PUBLIC_URL, OteClient};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// `notification_id` of the HA persistent notification
pub const NOTIFICATION_ID: &str = "fluxion_prices_missing";
/// Default time of day (Europe/Prague) after which tomorrow's prices are expected
pub const DEFAULT_CHECK_TIME: &str = "13:45";
/// A fallback slot matches the entity within this relative difference ...
pub const VALIDATION_REL_TOLERANCE: f64 = 0.005;
/// ... or within this absolute difference (CZK/kWh)
pub const VALIDATION_ABS_TOLERANCE: f64 = 0.01;
/// Share of today's overlapping slots that must match
pub const VALIDATION_MIN_MATCH: f64 = 0.9;
/// Fewest overlapping slots a validation (or buy fit) rests on
pub const MIN_OVERLAP_SLOTS: usize = 8;
/// Largest residual of the buy = a × spot + b fit that still fills buy prices (CZK/kWh)
pub const BUY_FIT_MAX_RESIDUAL: f64 = 0.01;

/// Where the add-on keeps the last successful validation
pub const DEFAULT_VALIDATION_PATH: &str = "/data/price_fallback_validation.json";

const SLOT: Duration = Duration::minutes(15);

/// Settings of the health check and the fallback
#[derive(Debug, Clone)]
pub struct PriceFallbackConfig {
    /// Spot price entity (for messages)
    pub spot_entity: String,
    /// Time of day (Europe/Prague) after which tomorrow's prices are expected
    pub check_time: NaiveTime,
    /// `pricing.ote_fallback_enabled`
    pub fallback_enabled: bool,
    /// `pricing.price_alert_notification`
    pub notification_enabled: bool,
    pub ote_url: String,
    pub cnb_url: String,
    /// Timeout of one fetch (OTE prices and CNB rate)
    pub fetch_timeout: std::time::Duration,
    /// Time between fetch attempts while the fallback is needed
    pub retry_interval: Duration,
    /// Time between two WARNs about the same outage
    pub warn_interval: Duration,
    /// Time between two attempts at the HA notification after a failed call
    pub notification_retry: Duration,
    /// How long prices must be missing before the alert (WARN, banner, notification) and the
    /// fetch start; a failed read or an HA restart shorter than this raises nothing
    pub alert_delay: Duration,
    /// Fixed clock for tests and replays; `None` = the system clock
    pub fixed_now: Option<DateTime<Utc>>,
    /// File keeping the last successful validation across restarts; `None` = memory only
    pub validation_path: Option<PathBuf>,
    /// Age up to which a stored validation stands in when today cannot be checked
    pub validation_max_age: Duration,
}

impl PriceFallbackConfig {
    #[must_use]
    pub fn new(
        spot_entity: impl Into<String>,
        check_time: NaiveTime,
        fallback_enabled: bool,
        notification_enabled: bool,
    ) -> Self {
        Self {
            spot_entity: spot_entity.into(),
            check_time,
            fallback_enabled,
            notification_enabled,
            ote_url: OTE_PUBLIC_URL.to_owned(),
            cnb_url: CNB_RATES_URL.to_owned(),
            fetch_timeout: std::time::Duration::from_secs(30),
            retry_interval: Duration::minutes(10),
            warn_interval: Duration::hours(1),
            notification_retry: Duration::minutes(1),
            alert_delay: Duration::minutes(2),
            fixed_now: None,
            validation_path: None,
            validation_max_age: Duration::hours(48),
        }
    }
}

/// Parse "HH:MM" (the `pricing.tomorrow_prices_check_time` option)
#[must_use]
pub fn parse_check_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

/// Which CNB fixing converts the prices, relative to the day they are shown on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    /// The latest fixing before today (what the integration fetches at midnight)
    BeforeToday,
    /// Today's fixing (the integration has it after an HA restart past 14:30)
    Today,
}

/// The last successful validation of the OTE conversion against the price entity
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredValidation {
    /// When it last passed
    pub validated_at: DateTime<Utc>,
    /// Prague day it was checked on
    pub day: NaiveDate,
    pub rate_source: RateSource,
    pub rate_valid_for: NaiveDate,
    /// EUR/CZK of that day (for the record)
    pub eur_czk: f64,
    /// Share of today's slots that matched
    pub match_share: f64,
    /// buy = a × spot + b, when the site has a buy entity and the fit was exact
    pub buy_fit: Option<BuyPriceFit>,
}

impl StoredValidation {
    fn same_outcome(&self, other: &Self) -> bool {
        let fit_key =
            |f: &Option<BuyPriceFit>| f.map(|f| ((f.a * 1e4).round(), (f.b * 1e4).round()));
        self.day == other.day
            && self.rate_source == other.rate_source
            && self.rate_valid_for == other.rate_valid_for
            && fit_key(&self.buy_fit) == fit_key(&other.buy_fit)
    }
}

/// OTE prices of one fetch
#[derive(Debug, Clone)]
struct MarketData {
    /// Prague day the fetch asked for (that day and the next), not the day it completed
    fetched_for: NaiveDate,
    /// When the fetch completed
    fetched_at: DateTime<Utc>,
    /// EUR/MWh per slot start
    prices: BTreeMap<DateTime<Utc>, Decimal>,
    /// Candidate EUR/CZK fixings relative to `fetched_for`, preferred first
    rates: Vec<(RateSource, EurCzkRate)>,
}

impl MarketData {
    /// The prices and fixings that apply on `today`. A fetch of the day before still holds
    /// today's prices (kept after midnight while OTE cannot be reached again); its rates are
    /// relabelled: the fixing it fetched for its own day is "before today" now, when it is
    /// known to be final (dated that day, or fetched after the 14:30 fixing). Today's own
    /// fixing is unknown until the next fetch.
    fn for_day(&self, today: NaiveDate) -> Option<Self> {
        if today == self.fetched_for {
            return Some(self.clone());
        }
        if self.fetched_for.succ_opt() != Some(today) {
            return None;
        }
        let start = ote::period_start(today, 1)?;
        let prices: BTreeMap<_, _> = self.prices.range(start..).map(|(k, v)| (*k, *v)).collect();
        if prices.is_empty() {
            return None;
        }
        let fixing_published = self
            .fetched_for
            .and_hms_opt(15, 0, 0)
            .and_then(|t| t.and_local_timezone(Prague).earliest())
            .is_some_and(|t| self.fetched_at >= t);
        let rates = self
            .rates
            .iter()
            .filter(|(source, rate)| {
                *source == RateSource::Today
                    && (rate.valid_for == self.fetched_for || fixing_published)
            })
            .map(|(_, rate)| (RateSource::BeforeToday, *rate))
            .collect();
        Some(Self {
            fetched_for: today,
            fetched_at: self.fetched_at,
            prices,
            rates,
        })
    }
}

/// Outcome of the last health evaluation
#[derive(Debug, Clone, Default)]
struct Health {
    evaluated: bool,
    today_missing: bool,
    tomorrow_missing: bool,
    sensor_tomorrow_slots: usize,
    expected_tomorrow_slots: usize,
    fallback: FallbackState,
    fallback_reason: Option<String>,
    fallback_slots: usize,
    /// Every missing slot is filled from OTE
    covered: bool,
    validation_match: Option<f64>,
    rate: Option<f64>,
    /// Day of the stored validation used instead of today's check
    validated_on: Option<NaiveDate>,
}

#[derive(Debug, Default)]
struct State {
    market: Option<MarketData>,
    want_fetch: bool,
    last_attempt: Option<DateTime<Utc>>,
    last_error: Option<String>,
    health: Health,
    missing_since: Option<DateTime<Utc>>,
    /// Prices missing for at least `alert_delay`
    alerting: bool,
    last_warn: Option<DateTime<Utc>>,
    /// Last WARN about a failed fetch (at most one per `warn_interval`)
    last_fetch_warn: Option<DateTime<Utc>>,
    warn_count: u32,
    /// Last reason a fetched fallback was rejected (WARN once per reason)
    rejected_reason: Option<String>,
    /// What HA shows: `None` until known (start), then the last state set successfully
    notification_shown: Option<bool>,
    last_notification_attempt: Option<DateTime<Utc>>,
    last_notification_warn: Option<DateTime<Utc>>,
    notification_warn_count: u32,
    /// Buy fit in use for the filled slots
    buy_fit: Option<BuyPriceFit>,
    /// Last successful validation (also in `validation_path`)
    stored: Option<StoredValidation>,
}

/// Price feed health check and OTE fallback; shared by the price source and its background task
#[derive(Debug)]
pub struct PriceFallback {
    config: PriceFallbackConfig,
    client: OteClient,
    state: Mutex<State>,
    status: Arc<RwLock<PriceFeedStatus>>,
}

impl PriceFallback {
    pub fn new(
        config: PriceFallbackConfig,
        status: Arc<RwLock<PriceFeedStatus>>,
    ) -> anyhow::Result<Self> {
        let client = OteClient::new(config.ote_url.clone(), config.cnb_url.clone())?;
        let stored = config.validation_path.as_deref().and_then(load_validation);
        if let Some(v) = &stored {
            info!(
                "💰 Last OTE fallback validation: {} (EUR/CZK {} of {}, buy fit {})",
                v.day,
                v.eur_czk,
                v.rate_valid_for,
                v.buy_fit.map_or_else(
                    || "none".to_owned(),
                    |f| format!("{:.4} × spot + {:.4}", f.a, f.b)
                )
            );
        }
        let fallback = Self {
            config,
            client,
            state: Mutex::new(State {
                stored,
                ..State::default()
            }),
            status,
        };
        fallback.publish(&fallback.state.lock());
        Ok(fallback)
    }

    #[must_use]
    pub fn config(&self) -> &PriceFallbackConfig {
        &self.config
    }

    /// The time the price source checks prices at (the fixed clock in tests)
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        self.config.fixed_now.unwrap_or_else(Utc::now)
    }

    /// Current status (as on the dashboard)
    #[must_use]
    pub fn status(&self) -> PriceFeedStatus {
        self.status.read().clone()
    }

    /// WARNs logged about missing prices so far
    #[must_use]
    pub fn warn_count(&self) -> u32 {
        self.state.lock().warn_count
    }

    /// WARNs logged for a failed HA notification update
    #[must_use]
    pub fn notification_warn_count(&self) -> u32 {
        self.state.lock().notification_warn_count
    }

    /// The background task should fetch OTE prices (fallback needed and not complete)
    #[must_use]
    pub fn wants_fetch(&self) -> bool {
        self.state.lock().want_fetch
    }

    /// The last successful validation (as stored)
    #[must_use]
    pub fn stored_validation(&self) -> Option<StoredValidation> {
        self.state.lock().stored.clone()
    }

    /// Check `data` (read from the price entity) at `now`: update the health status, and fill the
    /// slots the entity lacks from validated OTE prices. Never changes a slot the entity has.
    /// `buy`: the buy entity's prices when the site has one; filled slots then get
    /// buy = a × spot + b, and nothing is filled when that cannot be derived.
    pub fn apply(
        &self,
        data: &mut SpotPriceData,
        buy: Option<&HashMap<DateTime<Utc>, f32>>,
        now: DateTime<Utc>,
    ) {
        let prague_now = now.with_timezone(&Prague);
        let today = prague_now.date_naive();
        let Some(tomorrow) = today.succ_opt() else {
            return;
        };
        let today_slots = ote::day_slots(today);
        let tomorrow_slots = ote::day_slots(tomorrow);

        // Slots the entity covers (a block may span more than one 15-minute slot)
        let mut covered: BTreeSet<DateTime<Utc>> = BTreeSet::new();
        let mut sensor_values: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
        let fifteen_minute = data
            .time_block_prices
            .iter()
            .all(|b| b.duration_minutes == 15 || b.duration_minutes == 0);
        for block in &data.time_block_prices {
            let minutes = if block.duration_minutes == 0 {
                15
            } else {
                block.duration_minutes
            };
            let end = block.block_start + Duration::minutes(i64::from(minutes));
            let mut t = block.block_start;
            while t < end {
                covered.insert(t);
                t += SLOT;
            }
            sensor_values.insert(block.block_start, f64::from(block.price_czk_per_kwh));
        }

        let after_check = prague_now.time() >= self.config.check_time;
        let remaining_today: Vec<DateTime<Utc>> = today_slots
            .iter()
            .copied()
            .filter(|s| *s + SLOT > now)
            .collect();
        let today_missing = remaining_today.iter().any(|s| !covered.contains(s));
        let sensor_tomorrow_slots = tomorrow_slots
            .iter()
            .filter(|s| covered.contains(s))
            .count();
        let tomorrow_missing = after_check && sensor_tomorrow_slots < tomorrow_slots.len();
        let missing = today_missing || tomorrow_missing;

        // Slots that should be there and are not
        let mut needed: Vec<DateTime<Utc>> = remaining_today
            .iter()
            .copied()
            .filter(|s| !covered.contains(s))
            .collect();
        if after_check {
            needed.extend(
                tomorrow_slots
                    .iter()
                    .copied()
                    .filter(|s| !covered.contains(s)),
            );
        }

        let mut state = self.state.lock();
        // Prices of the fetch that apply today (a fetch of yesterday still holds today's)
        let market = state.market.as_ref().and_then(|m| m.for_day(today));
        if market.is_none() {
            state.market = None;
        }

        if missing {
            let since = *state.missing_since.get_or_insert(now);
            state.alerting = now - since >= self.config.alert_delay;
        } else {
            if state.missing_since.take().is_some() && state.alerting {
                info!(
                    "✅ Price entity {} has the prices again; the entity's prices are used",
                    self.config.spot_entity
                );
            }
            state.alerting = false;
            state.last_warn = None;
            state.last_fetch_warn = None;
        }

        let mut health = Health {
            evaluated: true,
            today_missing,
            tomorrow_missing,
            sensor_tomorrow_slots,
            expected_tomorrow_slots: tomorrow_slots.len(),
            fallback: FallbackState::Idle,
            ..Health::default()
        };
        let mut buy_fit = None;

        if !self.config.fallback_enabled {
            health.fallback = FallbackState::Disabled;
            health.fallback_reason = Some("OTE fallback is off".to_owned());
        } else if missing && !fifteen_minute {
            health.fallback = FallbackState::Rejected;
            health.fallback_reason = Some("price entity is not in 15-minute slots".to_owned());
        } else if state.alerting {
            let check = market.as_ref().map(|market| {
                let entity = EntityPrices {
                    spot: &sensor_values,
                    buy,
                    today_slots: &today_slots,
                };
                (
                    market.clone(),
                    self.conversion(market, &entity, today, now, &state),
                )
            });
            match check {
                None => {
                    health.fallback = FallbackState::Pending;
                    health.fallback_reason = Some(
                        state
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "waiting for OTE prices".to_owned()),
                    );
                }
                Some((_, Err(reason))) => {
                    health.fallback = FallbackState::Rejected;
                    health.fallback_reason = Some(reason);
                }
                Some((market, Ok(conversion))) => {
                    if let Some(validation) = conversion.store.clone() {
                        self.store_validation(&mut state, validation, now);
                    }
                    health.validation_match = conversion.match_share;
                    health.rate = Some(conversion.rate.rate.to_f64());
                    health.validated_on = conversion.validated_on;
                    buy_fit = conversion.buy_fit;
                    let filled = fill_missing(data, &market, &conversion, &needed);
                    health.fallback_slots = filled;
                    health.covered = filled == needed.len();
                    if health.covered {
                        health.fallback = FallbackState::Active;
                    } else {
                        health.fallback = FallbackState::Pending;
                        health.fallback_reason = Some(format!(
                            "OTE has {filled} of the {} missing slots",
                            needed.len()
                        ));
                    }
                }
            }
        }
        state.buy_fit = buy_fit;

        // A rejected fallback is reported once per reason
        if health.fallback == FallbackState::Rejected {
            if state.rejected_reason != health.fallback_reason {
                warn!(
                    "⚠️ OTE price fallback not used: {}",
                    health
                        .fallback_reason
                        .as_deref()
                        .unwrap_or("unknown reason")
                );
                state.rejected_reason.clone_from(&health.fallback_reason);
            }
        } else {
            state.rejected_reason = None;
        }

        state.want_fetch =
            self.config.fallback_enabled && fifteen_minute && state.alerting && !health.covered;
        state.health = health;

        if state.alerting
            && state
                .last_warn
                .is_none_or(|w| now - w >= self.config.warn_interval)
        {
            // Re-send the HA notification with the hourly WARN: HA keeps notifications in
            // memory only, so an HA restart during the outage would lose it
            if state.notification_shown == Some(true) {
                state.notification_shown = None;
            }
            let banner = banner(&self.config, &state).unwrap_or_default();
            warn!(
                "⚠️ {banner} (entity has {} of {} slots of tomorrow)",
                state.health.sensor_tomorrow_slots, state.health.expected_tomorrow_slots
            );
            state.last_warn = Some(now);
            state.warn_count += 1;
        }

        self.publish(&state);
    }

    /// How to convert the OTE prices: checked against the entity's prices of today, or, when
    /// the entity has none, the stored validation if it is at most `validation_max_age` old.
    fn conversion(
        &self,
        market: &MarketData,
        entity: &EntityPrices<'_>,
        today: NaiveDate,
        now: DateTime<Utc>,
        state: &State,
    ) -> Result<Conversion, String> {
        // A validation of day D covers D and D+1 only, however often it was refreshed on D
        let fresh_stored = state.stored.as_ref().filter(|v| {
            now - v.validated_at <= self.config.validation_max_age
                && v.day.succ_opt().is_some_and(|next| today <= next)
        });
        match validate(market, entity.spot, entity.today_slots) {
            Ok(validation) => {
                // With a buy entity: today's fit, else a fresh stored one, else nothing
                let buy_fit = match entity.buy {
                    None => None,
                    Some(buy) => {
                        let pairs = buy_pairs(entity.spot, buy, entity.today_slots);
                        if pairs.len() >= MIN_OVERLAP_SLOTS {
                            let fit = fit_buy_price(&pairs)
                                .ok_or_else(|| "no buy prices to fit".to_owned())?;
                            if !fit.used {
                                return Err(format!(
                                    "buy price fit failed: buy entity is not a × spot + b (max residual {:.4} CZK/kWh over {} slots)",
                                    fit.max_residual, fit.slots
                                ));
                            }
                            Some(fit)
                        } else if let Some(fit) =
                            fresh_stored.and_then(|v| v.buy_fit).filter(|f| f.used)
                        {
                            Some(fit)
                        } else {
                            return Err(format!(
                                "buy price fit failed: only {} of today's slots have a buy price, need {MIN_OVERLAP_SLOTS}",
                                pairs.len()
                            ));
                        }
                    }
                };
                Ok(Conversion {
                    rate: validation.rate,
                    buy_fit,
                    match_share: Some(validation.share()),
                    validated_on: None,
                    store: Some(StoredValidation {
                        validated_at: now,
                        day: today,
                        rate_source: validation.source,
                        rate_valid_for: validation.rate.valid_for,
                        eur_czk: validation.rate.rate.to_f64(),
                        match_share: validation.share(),
                        buy_fit,
                    }),
                })
            }
            Err(Rejection::Mismatch(reason)) => Err(reason),
            Err(Rejection::NoOverlap(reason)) => {
                let Some(stored) = state.stored.as_ref() else {
                    return Err(format!("{reason}, and no earlier validation"));
                };
                if fresh_stored.is_none() {
                    return Err(format!(
                        "{reason}; last validation {} is too old (used on that day and the next only, within {} h)",
                        stored.day,
                        self.config.validation_max_age.num_hours()
                    ));
                }
                let rate = market
                    .rates
                    .iter()
                    .find(|(source, _)| *source == stored.rate_source)
                    .map(|(_, rate)| *rate)
                    .ok_or_else(|| {
                        format!("{reason}; the CNB fixing of the last validation is unavailable")
                    })?;
                let buy_fit = match entity.buy {
                    None => None,
                    Some(_) => Some(stored.buy_fit.filter(|f| f.used).ok_or_else(|| {
                        format!("{reason}; the last validation has no buy price fit")
                    })?),
                };
                Ok(Conversion {
                    rate,
                    buy_fit,
                    match_share: None,
                    validated_on: Some(stored.day),
                    store: None,
                })
            }
        }
    }

    /// Keep a successful validation; written to `validation_path` when its outcome changes or
    /// the stored one is more than an hour old (at most about once an hour)
    fn store_validation(
        &self,
        state: &mut State,
        validation: StoredValidation,
        now: DateTime<Utc>,
    ) {
        let due = state.stored.as_ref().is_none_or(|old| {
            !old.same_outcome(&validation) || now - old.validated_at >= Duration::hours(1)
        });
        if !due {
            return;
        }
        if let Some(path) = &self.config.validation_path
            && let Err(e) = save_validation(path, &validation)
        {
            warn!(
                "⚠️ OTE fallback validation not saved to {}: {e:#}",
                path.display()
            );
        }
        state.stored = Some(validation);
    }

    /// One round of the background task: fetch when needed, then sync the HA notification
    pub async fn run_once(&self, ha: Option<&HomeAssistantClient>, now: DateTime<Utc>) {
        self.maybe_fetch(now).await;
        if let Some(ha) = ha {
            self.sync_notification(ha, now).await;
        }
    }

    /// Run the background task (every 15 s) until the runtime stops
    pub fn spawn(
        self: Arc<Self>,
        ha: Option<Arc<HomeAssistantClient>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.run_once(ha.as_deref(), self.now()).await;
            }
        })
    }

    /// Fetch OTE prices and the CNB rate when the fallback is needed and the last attempt is
    /// older than the retry interval. Errors are recorded, never raised.
    pub async fn maybe_fetch(&self, now: DateTime<Utc>) {
        let today = now.with_timezone(&Prague).date_naive();
        {
            let mut state = self.state.lock();
            if !self.config.fallback_enabled || !state.want_fetch {
                return;
            }
            if state
                .last_attempt
                .is_some_and(|t| now - t < self.config.retry_interval)
            {
                return;
            }
            state.last_attempt = Some(now);
        }

        info!("💰 Fetching OTE day-ahead prices for the price fallback");
        let result =
            tokio::time::timeout(self.config.fetch_timeout, self.fetch_market(today)).await;
        let mut state = self.state.lock();
        match result {
            Ok(Ok(market)) => {
                let tomorrow_slots = today
                    .succ_opt()
                    .map(ote::day_slots)
                    .unwrap_or_default()
                    .iter()
                    .filter(|s| market.prices.contains_key(s))
                    .count();
                info!(
                    "✅ OTE prices fetched: {} slots ({} of tomorrow), EUR/CZK {}",
                    market.prices.len(),
                    tomorrow_slots,
                    market
                        .rates
                        .iter()
                        .map(|(_, r)| format!("{} ({})", r.rate.to_f64(), r.valid_for))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                state.market = Some(market);
                state.last_error = None;
            }
            Ok(Err(e)) => {
                state.last_error = Some(format!("OTE fetch failed: {e:#}"));
            }
            Err(_) => {
                state.last_error = Some(format!(
                    "OTE fetch timed out after {} s",
                    self.config.fetch_timeout.as_secs()
                ));
            }
        }
        // A failing fetch is retried every 10 minutes; WARN at most once per warn interval
        if let Some(error) = state.last_error.clone() {
            if state
                .last_fetch_warn
                .is_none_or(|w| now - w >= self.config.warn_interval)
            {
                warn!(
                    "⚠️ OTE price fallback: {error}; retrying every {} min",
                    self.config.retry_interval.num_minutes()
                );
                state.last_fetch_warn = Some(now);
            } else {
                debug!("OTE price fallback: {error}");
            }
        }
        self.publish(&state);
    }

    async fn fetch_market(&self, today: NaiveDate) -> anyhow::Result<MarketData> {
        let tomorrow = today.succ_opt().unwrap_or(today);
        let yesterday = today.pred_opt().unwrap_or(today);
        // The integration converts with the fixing it fetched at midnight: the latest one before
        // today. After an HA restart past 14:30 it has today's. Both are candidates.
        let (prices, before, current) = tokio::join!(
            self.client.fetch_dam_prices(today, tomorrow),
            self.client.fetch_eur_rate(yesterday),
            self.client.fetch_eur_rate(today),
        );
        let prices = prices?;
        let mut rates: Vec<(RateSource, EurCzkRate)> = Vec::new();
        let mut rate_error = None;
        for (source, rate) in [
            (RateSource::BeforeToday, before),
            (RateSource::Today, current),
        ] {
            match rate {
                Ok(rate) => rates.push((source, rate)),
                Err(e) => rate_error = Some(e),
            }
        }
        if rates.is_empty() {
            return Err(rate_error
                .unwrap_or_else(|| anyhow::anyhow!("no CNB rate"))
                .context("CNB EUR/CZK rate unavailable"));
        }
        Ok(MarketData {
            fetched_for: today,
            fetched_at: self.now(),
            prices,
            rates,
        })
    }

    /// Create the HA notification while prices are missing, dismiss it when they are back
    pub async fn sync_notification(&self, ha: &HomeAssistantClient, now: DateTime<Utc>) {
        if !self.config.notification_enabled {
            return;
        }
        let (wanted, message) = {
            let mut state = self.state.lock();
            if !state.health.evaluated {
                return;
            }
            let wanted = state.alerting;
            if state.notification_shown == Some(wanted) {
                return;
            }
            if state
                .last_notification_attempt
                .is_some_and(|t| now - t < self.config.notification_retry)
            {
                return;
            }
            state.last_notification_attempt = Some(now);
            (wanted, notification_message(&self.config, &state))
        };

        let result = if wanted {
            ha.call_service(
                "persistent_notification.create",
                serde_json::json!({
                    "notification_id": NOTIFICATION_ID,
                    "title": "FluxION: electricity prices missing",
                    "message": message,
                }),
            )
            .await
        } else {
            ha.call_service(
                "persistent_notification.dismiss",
                serde_json::json!({ "notification_id": NOTIFICATION_ID }),
            )
            .await
        };

        let mut state = self.state.lock();
        match result {
            Ok(()) => {
                state.notification_shown = Some(wanted);
                state.last_notification_warn = None;
            }
            // Retried every minute; WARN at most once per warn interval, like the alarm
            Err(e)
                if state
                    .last_notification_warn
                    .is_none_or(|w| now - w >= self.config.warn_interval) =>
            {
                warn!("⚠️ Price alert notification not updated in HA: {e}");
                state.last_notification_warn = Some(now);
                state.notification_warn_count += 1;
            }
            Err(e) => debug!("Price alert notification not updated in HA: {e}"),
        }
    }

    fn publish(&self, state: &State) {
        let health = &state.health;
        let status = PriceFeedStatus {
            entity: self.config.spot_entity.clone(),
            check_time: self.config.check_time.format("%H:%M").to_string(),
            tomorrow_missing: health.tomorrow_missing,
            today_missing: health.today_missing,
            sensor_tomorrow_slots: health.sensor_tomorrow_slots,
            expected_tomorrow_slots: health.expected_tomorrow_slots,
            missing_since: state.missing_since,
            fallback: if self.config.fallback_enabled {
                health.fallback
            } else {
                FallbackState::Disabled
            },
            fallback_reason: health.fallback_reason.clone(),
            fallback_slots: health.fallback_slots,
            validation_match: health.validation_match,
            eur_czk_rate: health.rate,
            last_fetch_attempt: state.last_attempt,
            last_fetch_error: state.last_error.clone(),
            buy_fit: state.buy_fit,
            banner: banner(&self.config, state),
        };
        *self.status.write() = status;
    }
}

/// Dashboard banner; `None` when the entity has the prices
fn banner(config: &PriceFallbackConfig, state: &State) -> Option<String> {
    if !state.alerting {
        return None;
    }
    let health = &state.health;
    let head = match (health.today_missing, health.tomorrow_missing) {
        (false, false) => return None,
        (false, true) => format!(
            "Tomorrow's prices missing from {} since {}",
            config.spot_entity,
            config.check_time.format("%H:%M")
        ),
        (true, false) => format!("Today's prices missing from {}", config.spot_entity),
        (true, true) => format!(
            "Today's and tomorrow's prices missing from {}",
            config.spot_entity
        ),
    };
    let text = if health.fallback == FallbackState::Active {
        match health.validated_on {
            Some(day) => format!("{head} — using OTE fallback, validated {day}"),
            None => format!("{head} — using OTE fallback"),
        }
    } else {
        match &health.fallback_reason {
            Some(reason) => format!("{head} — no fallback available ({reason})"),
            None => format!("{head} — no fallback available"),
        }
    };
    Some(text)
}

fn notification_message(config: &PriceFallbackConfig, state: &State) -> String {
    let what = if state.health.tomorrow_missing {
        format!(
            "Tomorrow's electricity prices are missing from {} (checked after {} Europe/Prague).",
            config.spot_entity,
            config.check_time.format("%H:%M")
        )
    } else {
        format!(
            "Electricity prices for the rest of today are missing from {}.",
            config.spot_entity
        )
    };
    let fallback = if config.fallback_enabled {
        "FluxION tries the OTE fallback; the FluxION dashboard shows whether it is in use."
    } else {
        "The OTE fallback is off, so FluxION plans only as far as the prices reach."
    };
    format!(
        "{what} {fallback} Reloading the Czech Energy Spot Prices integration usually brings the prices back. This notification is dismissed when the prices are complete again."
    )
}

#[derive(Debug, Clone, Copy)]
struct Validation {
    source: RateSource,
    rate: EurCzkRate,
    matched: usize,
    overlap: usize,
}

impl Validation {
    fn share(&self) -> f64 {
        #[expect(clippy::cast_precision_loss, reason = "slot counts are small")]
        let share = self.matched as f64 / self.overlap.max(1) as f64;
        share
    }
}

/// Why OTE was not checked or did not pass
enum Rejection {
    /// Too few of today's slots in the entity to check against
    NoOverlap(String),
    /// Checked and different from the entity
    Mismatch(String),
}

/// The entity's prices for one check
struct EntityPrices<'a> {
    spot: &'a BTreeMap<DateTime<Utc>, f64>,
    buy: Option<&'a HashMap<DateTime<Utc>, f32>>,
    today_slots: &'a [DateTime<Utc>],
}

/// How fallback slots are converted
#[derive(Debug, Clone)]
struct Conversion {
    rate: EurCzkRate,
    /// buy = a × spot + b for filled slots (site with a buy entity)
    buy_fit: Option<BuyPriceFit>,
    match_share: Option<f64>,
    /// Day of the stored validation used instead of today's check
    validated_on: Option<NaiveDate>,
    /// Today's successful check, to keep
    store: Option<StoredValidation>,
}

/// True when the fallback value matches the entity value within the tolerances
#[must_use]
pub fn values_match(fallback: f64, sensor: f64) -> bool {
    let diff = (fallback - sensor).abs();
    diff <= VALIDATION_ABS_TOLERANCE + 1e-9 || diff <= VALIDATION_REL_TOLERANCE * sensor.abs()
}

/// Check the OTE prices against the entity over today's slots both have, for each candidate
/// rate; the best rate is used when it matches on at least 90 % of them.
fn validate(
    market: &MarketData,
    sensor: &BTreeMap<DateTime<Utc>, f64>,
    today_slots: &[DateTime<Utc>],
) -> Result<Validation, Rejection> {
    let overlap: Vec<(Decimal, f64)> = today_slots
        .iter()
        .filter_map(|s| Some((*market.prices.get(s)?, *sensor.get(s)?)))
        .collect();
    if overlap.len() < MIN_OVERLAP_SLOTS {
        return Err(Rejection::NoOverlap(format!(
            "only {} of today's slots to check OTE against, need {MIN_OVERLAP_SLOTS}",
            overlap.len()
        )));
    }
    // The rate with the most matching slots; on a tie, the one reproducing the entity's
    // 4-decimal values exactly on the most slots, then the preferred (first) one
    let mut best: Option<(Validation, usize)> = None;
    for (source, rate) in &market.rates {
        let converted: Vec<(f64, f64)> = overlap
            .iter()
            .map(|(eur, czk)| (ote::czk_per_kwh(*eur, rate.rate), *czk))
            .collect();
        let matched = converted
            .iter()
            .filter(|(f, s)| values_match(*f, *s))
            .count();
        let exact = converted
            .iter()
            .filter(|(f, s)| (f - s).abs() < 0.000_05)
            .count();
        if best.is_none_or(|(b, e)| (matched, exact) > (b.matched, e)) {
            best = Some((
                Validation {
                    source: *source,
                    rate: *rate,
                    matched,
                    overlap: overlap.len(),
                },
                exact,
            ));
        }
    }
    let (best, _) = best.ok_or_else(|| Rejection::Mismatch("no EUR/CZK rate".to_owned()))?;
    if best.share() >= VALIDATION_MIN_MATCH {
        Ok(best)
    } else {
        Err(Rejection::Mismatch(format!(
            "OTE matches the entity on {} of {} of today's slots, need 90 %",
            best.matched, best.overlap
        )))
    }
}

/// `(spot, buy)` over today's entity slots that have a buy price
fn buy_pairs(
    spot: &BTreeMap<DateTime<Utc>, f64>,
    buy: &HashMap<DateTime<Utc>, f32>,
    today_slots: &[DateTime<Utc>],
) -> Vec<(f64, f64)> {
    today_slots
        .iter()
        .filter_map(|s| Some((*spot.get(s)?, f64::from(*buy.get(s)?))))
        .collect()
}

/// Add the `needed` slots OTE has to `data`, with the buy price from the fit when there is one;
/// returns how many were added
fn fill_missing(
    data: &mut SpotPriceData,
    market: &MarketData,
    conversion: &Conversion,
    needed: &[DateTime<Utc>],
) -> usize {
    let mut filled = 0;
    for start in needed {
        let Some(eur) = market.prices.get(start) else {
            continue;
        };
        let price = ote::czk_per_kwh(*eur, conversion.rate.rate);
        #[expect(clippy::cast_possible_truncation, reason = "prices are stored as f32")]
        let buy = conversion.buy_fit.map(|f| (f.a * price + f.b) as f32);
        #[expect(clippy::cast_possible_truncation, reason = "prices are stored as f32")]
        let price = price as f32;
        data.time_block_prices.push(TimeBlockPrice {
            block_start: *start,
            duration_minutes: 15,
            price_czk_per_kwh: price,
            effective_price_czk_per_kwh: price,
            spot_sell_price_czk_per_kwh: None,
            buy_price_czk_per_kwh: buy,
        });
        data.fallback_block_starts.push(*start);
        filled += 1;
    }
    if filled > 0 {
        data.time_block_prices.sort_by_key(|b| b.block_start);
        data.fallback_block_starts.sort();
        data.block_duration_minutes = 15;
    }
    filled
}

fn load_validation(path: &std::path::Path) -> Option<StoredValidation> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                "⚠️ OTE fallback validation {} unreadable: {e}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("⚠️ OTE fallback validation {} invalid: {e}", path.display());
            None
        }
    }
}

fn save_validation(path: &std::path::Path, validation: &StoredValidation) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(validation)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Least-squares fit buy = a × spot + b over `(spot, buy)` pairs. `used` when it rests on enough
/// slots and its largest residual is at most `BUY_FIT_MAX_RESIDUAL`. `None` without pairs.
#[must_use]
pub fn fit_buy_price(pairs: &[(f64, f64)]) -> Option<BuyPriceFit> {
    if pairs.is_empty() {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "slot counts are small")]
    let n = pairs.len() as f64;
    let sx: f64 = pairs.iter().map(|p| p.0).sum();
    let sy: f64 = pairs.iter().map(|p| p.1).sum();
    let sxx: f64 = pairs.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = pairs.iter().map(|p| p.0 * p.1).sum();
    let denom = n * sxx - sx * sx;
    let (a, b, determined) = if denom.abs() > 1e-12 {
        let a = (n * sxy - sx * sy) / denom;
        (a, (sy - a * sx) / n, true)
    } else {
        // One spot value only: a and b are not determined
        (0.0, sy / n, false)
    };
    let max_residual = pairs
        .iter()
        .map(|(x, y)| (y - (a * x + b)).abs())
        .fold(0.0, f64::max);
    Some(BuyPriceFit {
        a,
        b,
        max_residual,
        slots: pairs.len(),
        used: determined
            && pairs.len() >= MIN_OVERLAP_SLOTS
            && max_residual <= BUY_FIT_MAX_RESIDUAL + 1e-9,
    })
}

#[cfg(test)]
mod tests;
