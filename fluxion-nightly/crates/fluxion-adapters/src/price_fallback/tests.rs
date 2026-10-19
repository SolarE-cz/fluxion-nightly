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

//! Tests of the price feed health check and OTE fallback (brief H35).
//!
//! Fixtures in `tests/fixtures/price_fallback/` (public market data, no site data):
//! - `ote-dam-*.xml`: OTE `GetDamPricePeriodE` (PT15M) responses as recorded on 2026-10-19
//!   for 2026-10-12/13, 2026-10-13/14 and the DST days 2025-10-25/26 (100 slots on the 26th)
//!   and 2026-03-28/29 (92 slots on the 29th).
//! - `cnb-daily-*.json`: CNB `/cnbapi/exrates/daily?date=…` responses for those query dates.
//! - `integration-spot-attrs-2026-10-13.json`: 88 values of
//!   `sensor.current_spot_electricity_price_15min` on 2026-10-13 (00:00–21:45 Prague) as one site
//!   recorded them (the state of each slot equals its attribute), in the attribute format
//!   (local ISO time → CZK/kWh). Only the price values and their times are kept.

use super::ote::{
    Decimal, czk_per_kwh, day_slots, parse_cnb_eur_rate, parse_dam_response, period_start,
};
use super::*;
use chrono::TimeZone;
use mockito::{Matcher, Server};

const ENTITY: &str = "sensor.current_spot_electricity_price_15min";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/price_fallback/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn prague(text: &str) -> DateTime<Utc> {
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
    Prague
        .from_local_datetime(&naive)
        .earliest()
        .unwrap()
        .with_timezone(&Utc)
}

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

/// The recorded integration values of 2026-10-13
fn recorded_attrs() -> BTreeMap<DateTime<Utc>, f64> {
    let json: serde_json::Value =
        serde_json::from_str(&fixture("integration-spot-attrs-2026-10-13.json")).unwrap();
    json.as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                DateTime::parse_from_rfc3339(k).unwrap().with_timezone(&Utc),
                v.as_f64().unwrap(),
            )
        })
        .collect()
}

fn rate(file: &str) -> EurCzkRate {
    parse_cnb_eur_rate(&fixture(file)).unwrap()
}

fn market(ote_file: &str, rate_files: &[&str], fetched_for: &str) -> MarketData {
    MarketData {
        fetched_for: day(fetched_for),
        fetched_at: prague(&format!("{fetched_for} 18:00")),
        prices: parse_dam_response(&fixture(ote_file)).unwrap(),
        // First file: the fixing before the fetch day; second: the fetch day's own
        rates: rate_files
            .iter()
            .zip([RateSource::BeforeToday, RateSource::Today])
            .map(|(f, source)| (source, rate(f)))
            .collect(),
    }
}

/// What the integration shows for `day`, converted from the OTE fixture with `rate`
fn integration_values(
    market: &MarketData,
    rate: EurCzkRate,
    day: NaiveDate,
) -> BTreeMap<DateTime<Utc>, f64> {
    day_slots(day)
        .into_iter()
        .filter_map(|s| Some((s, czk_per_kwh(*market.prices.get(&s)?, rate.rate))))
        .collect()
}

fn spot_data(values: &BTreeMap<DateTime<Utc>, f64>) -> SpotPriceData {
    #[expect(clippy::cast_possible_truncation)]
    let blocks = values
        .iter()
        .map(|(start, price)| TimeBlockPrice {
            block_start: *start,
            duration_minutes: 15,
            price_czk_per_kwh: *price as f32,
            effective_price_czk_per_kwh: *price as f32,
            spot_sell_price_czk_per_kwh: None,
            buy_price_czk_per_kwh: None,
        })
        .collect();
    SpotPriceData {
        time_block_prices: blocks,
        ..SpotPriceData::default()
    }
}

/// The entity on 2026-10-13: the 88 recorded slots plus 22:00–23:45 converted like the
/// integration, nothing of tomorrow
fn entity_2026_10_13() -> BTreeMap<DateTime<Utc>, f64> {
    let m = market("ote-dam-2026-10-13_14.xml", &[], "2026-10-13");
    let mut values = integration_values(&m, rate("cnb-daily-2026-10-12.json"), day("2026-10-13"));
    values.extend(recorded_attrs());
    values
}

/// Config with no alert delay (each test applies prices at chosen times)
fn config() -> PriceFallbackConfig {
    let mut cfg = PriceFallbackConfig::new(ENTITY, parse_check_time("13:45").unwrap(), true, true);
    cfg.alert_delay = Duration::zero();
    cfg
}

fn fallback_with(config: PriceFallbackConfig) -> PriceFallback {
    PriceFallback::new(config, Arc::new(RwLock::new(PriceFeedStatus::default()))).unwrap()
}

fn inject(fallback: &PriceFallback, market: MarketData) {
    fallback.state.lock().market = Some(market);
}

fn market_2026_10_13() -> MarketData {
    market(
        "ote-dam-2026-10-13_14.xml",
        &["cnb-daily-2026-10-12.json", "cnb-daily-2026-10-13.json"],
        "2026-10-13",
    )
}

// ---------------------------------------------------------------------------------------------
// Conversion and parsing
// ---------------------------------------------------------------------------------------------

#[test]
fn conversion_matches_recorded_integration_attributes() {
    let prices = parse_dam_response(&fixture("ote-dam-2026-10-12_13.xml")).unwrap();
    let eur = rate("cnb-daily-2026-10-12.json");
    assert_eq!(eur.valid_for, day("2026-10-12"));
    assert_eq!(eur.rate, Decimal::parse("24.39").unwrap());

    let recorded = recorded_attrs();
    assert_eq!(recorded.len(), 88);
    for (start, value) in &recorded {
        let converted = czk_per_kwh(prices[start], eur.rate);
        assert!(
            (converted - value).abs() < 1e-9,
            "{start}: converted {converted}, integration {value}"
        );
    }

    // The fixing of the day before (valid for 2026-10-09) does not reproduce the attributes
    let older = rate("cnb-daily-2026-10-11.json");
    let exact = recorded
        .iter()
        .filter(|(s, v)| (czk_per_kwh(prices[*s], older.rate) - **v).abs() < 1e-9)
        .count();
    assert!(exact < recorded.len() / 2);
}

#[test]
fn rounding_is_half_to_even_like_python_decimal() {
    let one = Decimal::parse("1").unwrap();
    // 0.05 EUR/MWh × 1 / 1000 = 0.00005 → 0.0000; 0.15 → 0.00015 → 0.0002
    assert_eq!(czk_per_kwh(Decimal::parse("0.05").unwrap(), one), 0.0);
    assert_eq!(czk_per_kwh(Decimal::parse("0.15").unwrap(), one), 0.0002);
    assert_eq!(czk_per_kwh(Decimal::parse("0.25").unwrap(), one), 0.0002);
    assert_eq!(czk_per_kwh(Decimal::parse("-0.15").unwrap(), one), -0.0002);
    assert_eq!(czk_per_kwh(Decimal::parse("-0.05").unwrap(), one), 0.0);
    // 181.20 × 24.34 / 1000 = 4.410408 → 4.4104
    let rate = Decimal::parse("24.34").unwrap();
    assert_eq!(czk_per_kwh(Decimal::parse("181.20").unwrap(), rate), 4.4104);
    assert!(Decimal::parse("1e3").is_err());
    assert!(Decimal::parse("").is_err());
}

#[test]
fn dam_parser_reads_92_and_100_periods_on_dst_days() {
    let autumn = parse_dam_response(&fixture("ote-dam-2025-10-25_26.xml")).unwrap();
    let slots = day_slots(day("2025-10-26"));
    assert_eq!(slots.len(), 100);
    assert!(slots.iter().all(|s| autumn.contains_key(s)));
    assert_eq!(autumn.len(), 196);
    // Period 100 starts at 23:45 local (22:45 UTC, CET)
    assert_eq!(
        period_start(day("2025-10-26"), 100).unwrap(),
        Utc.with_ymd_and_hms(2025, 10, 26, 22, 45, 0).unwrap()
    );

    let spring = parse_dam_response(&fixture("ote-dam-2026-03-28_29.xml")).unwrap();
    let slots = day_slots(day("2026-03-29"));
    assert_eq!(slots.len(), 92);
    assert!(slots.iter().all(|s| spring.contains_key(s)));
    assert_eq!(spring.len(), 96 + 92);
    assert_eq!(day_slots(day("2026-10-13")).len(), 96);
}

#[test]
fn soap_fault_and_garbage_are_errors() {
    let fault = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><soap:Fault><faultcode>soap:Client</faultcode><faultstring>Message part bogus was not recognized.</faultstring></soap:Fault></soap:Body></soap:Envelope>"#;
    let err = parse_dam_response(fault).unwrap_err().to_string();
    assert!(err.contains("Message part bogus"), "{err}");
    assert!(parse_dam_response("<html>Application is not available</p>").is_err());
    assert!(parse_cnb_eur_rate("{\"rates\": []}").is_err());
    assert!(parse_cnb_eur_rate("not json").is_err());
}

#[test]
fn dam_query_asks_for_15_minute_periods() {
    let q = ote::dam_query(day("2026-10-19"), day("2026-10-20"));
    assert!(q.contains("<pub:GetDamPricePeriodE>"));
    assert!(q.contains("<pub:StartDate>2026-10-19</pub:StartDate>"));
    assert!(q.contains("<pub:EndDate>2026-10-20</pub:EndDate>"));
    assert!(q.contains("<pub:PeriodResolution>PT15M</pub:PeriodResolution>"));
}

// ---------------------------------------------------------------------------------------------
// Validation and filling
// ---------------------------------------------------------------------------------------------

#[test]
fn only_missing_slots_are_filled_and_entity_values_kept() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    // Entity value of one slot differs from OTE by a little; it must stay as it is
    let mut entity = entity_2026_10_13();
    let kept_slot = prague("2026-10-13 20:00");
    *entity.get_mut(&kept_slot).unwrap() += 0.003;
    let mut data = spot_data(&entity);

    fb.apply(&mut data, None, prague("2026-10-13 18:00"));

    let tomorrow = day_slots(day("2026-10-14"));
    assert_eq!(data.fallback_block_starts, tomorrow);
    assert_eq!(data.time_block_prices.len(), 96 + 96);
    let block = |s: DateTime<Utc>| {
        data.time_block_prices
            .iter()
            .find(|b| b.block_start == s)
            .unwrap()
            .clone()
    };
    #[expect(clippy::cast_possible_truncation)]
    let kept = entity[&kept_slot] as f32;
    assert_eq!(block(kept_slot).price_czk_per_kwh, kept);
    // Tomorrow 00:00 = OTE price × 24.39 / 1000 (the fixing the entity uses)
    let m = market_2026_10_13();
    let expected = czk_per_kwh(m.prices[&tomorrow[0]], Decimal::parse("24.39").unwrap());
    #[expect(clippy::cast_possible_truncation)]
    let expected = expected as f32;
    assert_eq!(block(tomorrow[0]).price_czk_per_kwh, expected);
    assert!(
        data.time_block_prices
            .windows(2)
            .all(|w| w[0].block_start < w[1].block_start)
    );

    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Active);
    assert_eq!(status.fallback_slots, 96);
    assert!(status.tomorrow_missing);
    assert!(!status.today_missing);
    assert_eq!(status.eur_czk_rate, Some(24.39));
    assert_eq!(
        status.banner.as_deref(),
        Some(
            "Tomorrow's prices missing from sensor.current_spot_electricity_price_15min since 13:45 — using OTE fallback"
        )
    );
    assert!(!fb.wants_fetch());
}

#[test]
fn validation_picks_the_fixing_the_integration_used() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&entity_2026_10_13());
    fb.apply(&mut data, None, prague("2026-10-13 18:00"));
    let status = fb.status();
    assert_eq!(status.eur_czk_rate, Some(24.39));
    assert_eq!(status.validation_match, Some(1.0));
}

#[test]
fn validation_fails_when_ote_differs_from_the_entity() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let entity: BTreeMap<_, _> = entity_2026_10_13()
        .into_iter()
        .map(|(s, v)| (s, v * 1.02 + 0.05))
        .collect();
    let mut data = spot_data(&entity);

    fb.apply(&mut data, None, prague("2026-10-13 18:00"));

    assert!(data.fallback_block_starts.is_empty());
    assert_eq!(data.time_block_prices.len(), 96);
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    let banner = status.banner.unwrap();
    assert!(
        banner.starts_with(
            "Tomorrow's prices missing from sensor.current_spot_electricity_price_15min since 13:45 — no fallback available (OTE matches the entity on"
        ),
        "{banner}"
    );
}

#[test]
fn validation_passes_at_90_percent_and_fails_below() {
    let shifted = |n: usize| {
        let mut entity = entity_2026_10_13();
        for v in entity.values_mut().take(n) {
            *v += 0.5;
        }
        entity
    };
    // 9 of 96 off: 90.6 % match → used
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&shifted(9));
    fb.apply(&mut data, None, prague("2026-10-13 18:00"));
    assert_eq!(fb.status().fallback, FallbackState::Active);
    assert_eq!(data.fallback_block_starts.len(), 96);

    // 10 of 96 off: 89.6 % → not used
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&shifted(10));
    fb.apply(&mut data, None, prague("2026-10-13 18:00"));
    assert_eq!(fb.status().fallback, FallbackState::Rejected);
    assert!(data.fallback_block_starts.is_empty());
}

#[test]
fn tolerance_is_half_a_percent_or_one_haler() {
    assert!(values_match(4.0, 4.0099));
    assert!(values_match(4.0, 4.019)); // 0.475 %
    assert!(!values_match(4.0, 4.03)); // 0.75 %
    assert!(values_match(0.10, 0.109)); // 0.009 CZK
    assert!(!values_match(0.10, 0.12));
}

#[test]
fn no_alert_and_no_fill_before_the_check_time() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&entity_2026_10_13());

    fb.apply(&mut data, None, prague("2026-10-13 13:40"));

    assert!(data.fallback_block_starts.is_empty());
    let status = fb.status();
    assert!(!status.tomorrow_missing);
    assert_eq!(status.banner, None);
    assert_eq!(status.fallback, FallbackState::Idle);
    assert!(!fb.wants_fetch());
    assert_eq!(fb.warn_count(), 0);
}

#[test]
fn entity_wins_once_it_has_tomorrow() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&entity_2026_10_13());
    fb.apply(&mut data, None, prague("2026-10-13 18:00"));
    assert_eq!(data.fallback_block_starts.len(), 96);

    // Later the entity has tomorrow (values differ a little from OTE): they are used as they are
    let m = market_2026_10_13();
    let mut entity = entity_2026_10_13();
    let tomorrow = integration_values(&m, rate("cnb-daily-2026-10-12.json"), day("2026-10-14"));
    entity.extend(tomorrow.iter().map(|(s, v)| (*s, v + 0.001)));
    let mut data = spot_data(&entity);
    fb.apply(&mut data, None, prague("2026-10-13 19:00"));

    assert!(data.fallback_block_starts.is_empty());
    let first = prague("2026-10-14 00:00");
    let block = data
        .time_block_prices
        .iter()
        .find(|b| b.block_start == first)
        .unwrap();
    #[expect(clippy::cast_possible_truncation)]
    let expected = (tomorrow[&first] + 0.001) as f32;
    assert_eq!(block.price_czk_per_kwh, expected);
    let status = fb.status();
    assert_eq!(status.banner, None);
    assert_eq!(status.missing_since, None);
    assert!(!fb.wants_fetch());
}

#[test]
fn dst_days_fill_100_and_92_slots() {
    for (ote_file, cnb_file, today, tomorrow, expected) in [
        (
            "ote-dam-2025-10-25_26.xml",
            "cnb-daily-2025-10-24.json",
            "2025-10-25",
            "2025-10-26",
            100,
        ),
        (
            "ote-dam-2026-03-28_29.xml",
            "cnb-daily-2026-03-27.json",
            "2026-03-28",
            "2026-03-29",
            92,
        ),
    ] {
        let m = market(ote_file, &[cnb_file], today);
        let entity = integration_values(&m, rate(cnb_file), day(today));
        assert_eq!(entity.len(), 96);
        let fb = fallback_with(config());
        inject(&fb, m);
        let mut data = spot_data(&entity);

        fb.apply(&mut data, None, prague(&format!("{today} 15:00")));

        assert_eq!(data.fallback_block_starts, day_slots(day(tomorrow)));
        assert_eq!(data.fallback_block_starts.len(), expected);
        let status = fb.status();
        assert_eq!(status.expected_tomorrow_slots, expected);
        assert_eq!(status.fallback, FallbackState::Active);
    }
}

#[test]
fn missing_rest_of_today_is_reported_and_filled() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    // Only the 88 recorded slots (to 21:45): 22:00–23:45 missing, before the check time
    let mut data = spot_data(&recorded_attrs());

    fb.apply(&mut data, None, prague("2026-10-13 10:00"));

    let status = fb.status();
    assert!(status.today_missing);
    assert!(!status.tomorrow_missing);
    assert_eq!(status.fallback, FallbackState::Active);
    assert_eq!(data.fallback_block_starts.len(), 8);
    assert_eq!(
        status.banner.as_deref(),
        Some(
            "Today's prices missing from sensor.current_spot_electricity_price_15min — using OTE fallback"
        )
    );
}

#[test]
fn no_overlap_with_today_means_no_fallback() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = SpotPriceData::default();

    fb.apply(&mut data, None, prague("2026-10-13 18:00"));

    assert!(data.time_block_prices.is_empty());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    assert!(
        status
            .banner
            .unwrap()
            .contains("no fallback available (only 0 of today's slots to check OTE against")
    );
}

#[test]
fn market_two_days_old_is_dropped() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = SpotPriceData::default();

    fb.apply(&mut data, None, prague("2026-10-15 01:00"));

    assert!(data.fallback_block_starts.is_empty());
    assert!(fb.state.lock().market.is_none());
    assert_eq!(fb.status().fallback, FallbackState::Pending);
    assert!(fb.wants_fetch());
}

/// Review #2: after midnight the fetch of yesterday still holds today's prices; it is kept
/// (filtered by slot time) and its fixing of yesterday is today's "fixing before today"
#[test]
fn fetch_of_yesterday_still_fills_today_after_midnight() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let entity = entity_2026_10_13();
    fb.apply(&mut spot_data(&entity), None, prague("2026-10-13 18:00"));
    assert_eq!(fb.status().fallback, FallbackState::Active);

    // 00:30: the entity is unavailable and OTE cannot be reached again
    let now = prague("2026-10-14 00:30");
    let mut data = SpotPriceData::default();
    fb.apply(&mut data, None, now);

    assert_eq!(data.fallback_block_starts.len(), 94);
    assert_eq!(data.fallback_block_starts[0], now);
    let m = market_2026_10_13();
    #[expect(clippy::cast_possible_truncation)]
    let expected = czk_per_kwh(m.prices[&now], Decimal::parse("24.375").unwrap()) as f32;
    assert_eq!(data.time_block_prices[0].price_czk_per_kwh, expected);
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Active);
    assert_eq!(status.eur_czk_rate, Some(24.375));
    assert!(
        status
            .banner
            .unwrap()
            .ends_with("using OTE fallback, validated 2026-10-13")
    );
}

/// A fixing fetched before 14:30 may not be the day's final one: not used after midnight
#[test]
fn unpublished_fixing_is_not_carried_past_midnight() {
    let fb = fallback_with(config());
    let mut m = market(
        "ote-dam-2026-10-13_14.xml",
        // Asked for 2026-10-13 at 10:00, CNB answered with the fixing of 2026-10-12
        &["cnb-daily-2026-10-12.json", "cnb-daily-2026-10-12.json"],
        "2026-10-13",
    );
    m.fetched_at = prague("2026-10-13 10:00");
    inject(&fb, m);
    fb.apply(
        &mut spot_data(&entity_2026_10_13()),
        None,
        prague("2026-10-13 18:00"),
    );

    let mut data = SpotPriceData::default();
    fb.apply(&mut data, None, prague("2026-10-14 00:30"));

    assert!(data.fallback_block_starts.is_empty());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    assert!(
        status
            .fallback_reason
            .unwrap()
            .contains("the CNB fixing of the last validation is unavailable")
    );
    assert!(fb.wants_fetch());
}

/// A fetch started before midnight is labelled with the day it asked for
#[tokio::test]
async fn fetch_is_labelled_by_the_day_it_asked_for() {
    let mut server = Server::new_async().await;
    let _mocks = ote_cnb_mocks(&mut server, 1).await;
    let mut cfg = mocked_config(&server.url());
    // Completes after midnight
    cfg.fixed_now = Some(prague("2026-10-14 00:00"));
    let fb = fallback_with(cfg);
    let entity = entity_2026_10_13();
    let started = prague("2026-10-13 23:59");
    fb.apply(&mut spot_data(&entity), None, started);
    fb.maybe_fetch(started).await;

    assert_eq!(
        fb.state.lock().market.as_ref().unwrap().fetched_for,
        day("2026-10-13")
    );
}

// ---------------------------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------------------------

#[test]
fn warn_is_logged_once_per_hour() {
    let fb = fallback_with(config());
    let entity = entity_2026_10_13();
    for (time, count) in [
        ("2026-10-13 13:50", 1),
        ("2026-10-13 13:51", 1),
        ("2026-10-13 14:49", 1),
        ("2026-10-13 14:50", 2),
        ("2026-10-13 15:10", 2),
        ("2026-10-13 15:50", 3),
    ] {
        fb.apply(&mut spot_data(&entity), None, prague(time));
        assert_eq!(fb.warn_count(), count, "at {time}");
    }
    assert_eq!(fb.status().missing_since, Some(prague("2026-10-13 13:50")));
    assert!(
        fb.status()
            .banner
            .unwrap()
            .ends_with("no fallback available (waiting for OTE prices)")
    );
}

#[tokio::test]
async fn notification_is_created_once_and_dismissed_when_prices_are_back() {
    let mut ha = Server::new_async().await;
    let create = ha
        .mock("POST", "/api/services/persistent_notification/create")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "notification_id": "fluxion_prices_missing"
        })))
        .with_status(200)
        .with_body("[]")
        .expect(1)
        .create_async()
        .await;
    let dismiss = ha
        .mock("POST", "/api/services/persistent_notification/dismiss")
        .match_body(Matcher::Json(serde_json::json!({
            "notification_id": "fluxion_prices_missing"
        })))
        .with_status(200)
        .with_body("[]")
        .expect(1)
        .create_async()
        .await;
    let client = HomeAssistantClient::new(ha.url(), "t").unwrap();

    let mut cfg = config();
    cfg.fallback_enabled = false;
    let fb = fallback_with(cfg);
    let entity = entity_2026_10_13();

    // Missing for most of an hour: one create
    for time in [
        "2026-10-13 14:00",
        "2026-10-13 14:05",
        "2026-10-13 14:30",
        "2026-10-13 14:59",
    ] {
        let now = prague(time);
        fb.apply(&mut spot_data(&entity), None, now);
        fb.run_once(Some(&client), now).await;
    }
    // Prices complete: one dismiss
    let m = market_2026_10_13();
    let mut full = entity.clone();
    full.extend(integration_values(
        &m,
        rate("cnb-daily-2026-10-12.json"),
        day("2026-10-14"),
    ));
    for time in ["2026-10-13 15:10", "2026-10-13 15:20"] {
        let now = prague(time);
        fb.apply(&mut spot_data(&full), None, now);
        fb.run_once(Some(&client), now).await;
    }

    create.assert_async().await;
    dismiss.assert_async().await;
}

async fn notification_mocks(
    ha: &mut Server,
    creates: usize,
    dismisses: usize,
) -> Vec<mockito::Mock> {
    vec![
        ha.mock("POST", "/api/services/persistent_notification/create")
            .with_status(200)
            .with_body("[]")
            .expect(creates)
            .create_async()
            .await,
        ha.mock("POST", "/api/services/persistent_notification/dismiss")
            .with_status(200)
            .with_body("[]")
            .expect(dismisses)
            .create_async()
            .await,
    ]
}

/// HA keeps notifications in memory: an HA restart during the outage loses it, so it is sent
/// again with each hourly WARN (same id, it replaces itself)
#[tokio::test]
async fn notification_is_resent_with_the_hourly_warn() {
    let mut ha = Server::new_async().await;
    let mocks = notification_mocks(&mut ha, 3, 0).await;
    let client = HomeAssistantClient::new(ha.url(), "t").unwrap();
    let fb = fallback_with(config());
    let entity = entity_2026_10_13();
    for time in [
        "2026-10-13 14:00",
        "2026-10-13 14:30",
        "2026-10-13 15:00",
        "2026-10-13 15:30",
        "2026-10-13 16:00",
    ] {
        let now = prague(time);
        fb.apply(&mut spot_data(&entity), None, now);
        fb.run_once(Some(&client), now).await;
    }
    assert_eq!(fb.warn_count(), 3);
    for m in mocks {
        m.assert_async().await;
    }
}

/// After a restart with complete prices, one dismiss clears a notification left by the previous
/// run; nothing more is sent
#[tokio::test]
async fn startup_with_complete_prices_dismisses_once() {
    let mut ha = Server::new_async().await;
    let mocks = notification_mocks(&mut ha, 0, 1).await;
    let client = HomeAssistantClient::new(ha.url(), "t").unwrap();
    let fb = fallback_with(config());
    // Before any price read nothing is sent
    fb.run_once(Some(&client), prague("2026-10-13 10:00")).await;
    for time in ["2026-10-13 10:00", "2026-10-13 10:10", "2026-10-13 11:00"] {
        let now = prague(time);
        fb.apply(&mut spot_data(&entity_2026_10_13()), None, now);
        fb.run_once(Some(&client), now).await;
    }
    for m in mocks {
        m.assert_async().await;
    }
}

/// A failed read or an HA restart shorter than the alert delay raises nothing
#[tokio::test]
async fn short_outage_raises_no_alert() {
    let mut ha = Server::new_async().await;
    // One create (the real outage) and the one start-up dismiss, nothing for the short outage
    let mocks = notification_mocks(&mut ha, 1, 1).await;
    let client = HomeAssistantClient::new(ha.url(), "t").unwrap();
    let mut cfg = PriceFallbackConfig::new(ENTITY, parse_check_time("13:45").unwrap(), true, true);
    assert_eq!(cfg.alert_delay, Duration::minutes(2));
    cfg.ote_url = "http://127.0.0.1:9/ote".to_owned();
    cfg.cnb_url = "http://127.0.0.1:9/cnb".to_owned();
    let fb = fallback_with(cfg);
    let entity = entity_2026_10_13();
    let m = market_2026_10_13();
    let mut full = entity.clone();
    full.extend(integration_values(
        &m,
        rate("cnb-daily-2026-10-12.json"),
        day("2026-10-14"),
    ));

    // Complete, then the entity unreadable for 90 s, then complete again
    let t0 = prague("2026-10-13 10:00");
    fb.apply(&mut spot_data(&full), None, t0);
    for secs in [15, 60, 105] {
        let now = t0 + Duration::seconds(secs);
        fb.apply(&mut SpotPriceData::default(), None, now);
        assert!(!fb.wants_fetch());
        assert_eq!(fb.status().banner, None);
        fb.run_once(Some(&client), now).await;
    }
    fb.apply(&mut spot_data(&full), None, t0 + Duration::seconds(120));
    assert_eq!(fb.warn_count(), 0);
    assert!(fb.status().missing_since.is_none());

    // Missing for 2 minutes: alert
    let t1 = prague("2026-10-13 14:00");
    fb.apply(&mut spot_data(&entity), None, t1);
    assert_eq!(fb.status().banner, None);
    fb.apply(&mut spot_data(&entity), None, t1 + Duration::minutes(2));
    assert!(fb.status().banner.is_some());
    assert!(fb.wants_fetch());
    assert_eq!(fb.warn_count(), 1);
    fb.run_once(Some(&client), t1 + Duration::minutes(2)).await;
    for m in mocks {
        m.assert_async().await;
    }
}

#[tokio::test]
async fn options_off_mean_no_fetch_and_no_notification() {
    let mut server = Server::new_async().await;
    let any = server
        .mock("POST", Matcher::Any)
        .with_status(200)
        .expect(0)
        .create_async()
        .await;
    let any_get = server
        .mock("GET", Matcher::Any)
        .with_status(200)
        .expect(0)
        .create_async()
        .await;
    let client = HomeAssistantClient::new(server.url(), "t").unwrap();
    let mut cfg =
        PriceFallbackConfig::new(ENTITY, parse_check_time("13:45").unwrap(), false, false);
    cfg.alert_delay = Duration::zero();
    cfg.ote_url = format!("{}/ote", server.url());
    cfg.cnb_url = format!("{}/cnb", server.url());
    let fb = fallback_with(cfg);

    let mut data = spot_data(&entity_2026_10_13());
    fb.apply(&mut data, None, prague("2026-10-13 18:00"));
    fb.run_once(Some(&client), prague("2026-10-13 18:00")).await;

    assert!(data.fallback_block_starts.is_empty());
    assert!(!fb.wants_fetch());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Disabled);
    assert_eq!(
        status.banner.as_deref(),
        Some(
            "Tomorrow's prices missing from sensor.current_spot_electricity_price_15min since 13:45 — no fallback available (OTE fallback is off)"
        )
    );
    assert_eq!(fb.warn_count(), 1);
    any.assert_async().await;
    any_get.assert_async().await;
}

// ---------------------------------------------------------------------------------------------
// Fetching (mock HTTP)
// ---------------------------------------------------------------------------------------------

async fn ote_cnb_mocks(server: &mut Server, expect: usize) -> Vec<mockito::Mock> {
    vec![
        server
            .mock("POST", "/ote")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("<pub:StartDate>2026-10-13</pub:StartDate>".into()),
                Matcher::Regex("<pub:EndDate>2026-10-14</pub:EndDate>".into()),
            ]))
            .with_status(200)
            .with_body(fixture("ote-dam-2026-10-13_14.xml"))
            .expect(expect)
            .create_async()
            .await,
        server
            .mock("GET", "/cnb")
            .match_query(Matcher::UrlEncoded("date".into(), "2026-10-12".into()))
            .with_status(200)
            .with_body(fixture("cnb-daily-2026-10-12.json"))
            .expect(expect)
            .create_async()
            .await,
        server
            .mock("GET", "/cnb")
            .match_query(Matcher::UrlEncoded("date".into(), "2026-10-13".into()))
            .with_status(200)
            .with_body(fixture("cnb-daily-2026-10-13.json"))
            .expect(expect)
            .create_async()
            .await,
    ]
}

fn mocked_config(url: &str) -> PriceFallbackConfig {
    let mut cfg = config();
    cfg.ote_url = format!("{url}/ote");
    cfg.cnb_url = format!("{url}/cnb");
    cfg
}

#[tokio::test]
async fn fetch_fills_tomorrow_and_is_not_repeated_within_ten_minutes() {
    let mut server = Server::new_async().await;
    let mocks = ote_cnb_mocks(&mut server, 1).await;
    let fb = fallback_with(mocked_config(&server.url()));
    let entity = entity_2026_10_13();

    let now = prague("2026-10-13 18:00");
    fb.apply(&mut spot_data(&entity), None, now);
    assert!(fb.wants_fetch());
    assert_eq!(fb.status().fallback, FallbackState::Pending);
    fb.maybe_fetch(now).await;
    fb.maybe_fetch(now + Duration::minutes(5)).await;

    let mut data = spot_data(&entity);
    fb.apply(&mut data, None, now + Duration::minutes(5));
    assert_eq!(data.fallback_block_starts.len(), 96);
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Active);
    // Two candidate fixings (2026-10-12 and 2026-10-13); the one the entity uses is chosen
    assert_eq!(status.eur_czk_rate, Some(24.39));
    assert!(!fb.wants_fetch());
    for m in mocks {
        m.assert_async().await;
    }
}

#[tokio::test]
async fn no_fetch_when_the_entity_has_tomorrow() {
    let mut server = Server::new_async().await;
    let mocks = ote_cnb_mocks(&mut server, 0).await;
    let fb = fallback_with(mocked_config(&server.url()));
    let m = market_2026_10_13();
    let mut entity = entity_2026_10_13();
    entity.extend(integration_values(
        &m,
        rate("cnb-daily-2026-10-12.json"),
        day("2026-10-14"),
    ));

    let now = prague("2026-10-13 18:00");
    fb.apply(&mut spot_data(&entity), None, now);
    fb.maybe_fetch(now).await;

    assert!(!fb.wants_fetch());
    assert_eq!(fb.status().fallback, FallbackState::Idle);
    for m in mocks {
        m.assert_async().await;
    }
}

#[tokio::test]
async fn network_failure_shows_banner_and_retries_after_ten_minutes() {
    // Nothing listens on this port: the request fails at once
    let mut cfg = config();
    cfg.ote_url = "http://127.0.0.1:9/ote".to_owned();
    cfg.cnb_url = "http://127.0.0.1:9/cnb".to_owned();
    let fb = fallback_with(cfg);
    let entity = entity_2026_10_13();
    let now = prague("2026-10-13 18:00");

    fb.apply(&mut spot_data(&entity), None, now);
    fb.maybe_fetch(now).await;
    let mut data = spot_data(&entity);
    fb.apply(&mut data, None, now);

    assert!(data.fallback_block_starts.is_empty());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Pending);
    assert!(status.last_fetch_error.is_some());
    let banner = status.banner.unwrap();
    assert!(
        banner.contains("— no fallback available (OTE fetch failed:"),
        "{banner}"
    );
    assert!(fb.wants_fetch());
    let first = status.last_fetch_attempt;
    fb.maybe_fetch(now + Duration::minutes(9)).await;
    assert_eq!(fb.status().last_fetch_attempt, first);
    fb.maybe_fetch(now + Duration::minutes(10)).await;
    assert_eq!(
        fb.status().last_fetch_attempt,
        Some(now + Duration::minutes(10))
    );
}

#[tokio::test]
async fn soap_fault_and_slow_server_are_reported() {
    let mut server = Server::new_async().await;
    let _fault = server
        .mock("POST", "/ote")
        .with_status(500)
        .with_body(r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><soap:Fault><faultcode>soap:Server</faultcode><faultstring>Application is not available</faultstring></soap:Fault></soap:Body></soap:Envelope>"#)
        .create_async()
        .await;
    let _cnb = server
        .mock("GET", "/cnb")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(fixture("cnb-daily-2026-10-12.json"))
        .create_async()
        .await;
    let fb = fallback_with(mocked_config(&server.url()));
    let entity = entity_2026_10_13();
    let now = prague("2026-10-13 18:00");
    fb.apply(&mut spot_data(&entity), None, now);
    fb.maybe_fetch(now).await;
    fb.apply(&mut spot_data(&entity), None, now);
    let banner = fb.status().banner.unwrap();
    assert!(banner.contains("Application is not available"), "{banner}");

    // A server slower than the fetch timeout
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _hold = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let mut cfg = config();
    cfg.ote_url = format!("http://{addr}/ote");
    cfg.cnb_url = format!("http://{addr}/cnb");
    cfg.fetch_timeout = std::time::Duration::from_millis(200);
    let fb = fallback_with(cfg);
    fb.apply(&mut spot_data(&entity), None, now);
    fb.maybe_fetch(now).await;
    fb.apply(&mut spot_data(&entity), None, now);
    assert!(
        fb.status()
            .banner
            .unwrap()
            .contains("OTE fetch timed out after 0 s")
    );
}

// ---------------------------------------------------------------------------------------------
// Buy price fit
// ---------------------------------------------------------------------------------------------

#[test]
fn buy_fit_is_used_only_when_near_exact() {
    let entity = entity_2026_10_13();
    // The integration's buy entity: round(spot × 1.21, 4)
    let exact: Vec<(f64, f64)> = entity
        .values()
        .map(|s| (*s, (s * 1.21 * 10_000.0).round() / 10_000.0))
        .collect();
    let fit = fit_buy_price(&exact).unwrap();
    assert!(fit.used);
    assert!((fit.a - 1.21).abs() < 1e-3, "{fit:?}");
    assert!(fit.b.abs() < 1e-3, "{fit:?}");
    assert!(fit.max_residual < 0.001);

    // spot × 1.21 + fee is affine: still exact
    let affine: Vec<(f64, f64)> = entity.values().map(|s| (*s, s * 1.21 + 0.45)).collect();
    let fit = fit_buy_price(&affine).unwrap();
    assert!(fit.used && (fit.b - 0.45).abs() < 1e-6);

    // A tariff step the fit cannot follow
    let stepped: Vec<(f64, f64)> = entity
        .iter()
        .map(|(t, s)| {
            let fee = if t.with_timezone(&Prague).time() < NaiveTime::from_hms_opt(6, 0, 0).unwrap()
            {
                0.3
            } else {
                1.2
            };
            (*s, s * 1.21 + fee)
        })
        .collect();
    let fit = fit_buy_price(&stepped).unwrap();
    assert!(!fit.used);
    assert!(fit.max_residual > BUY_FIT_MAX_RESIDUAL);

    // Too few slots, or a single spot value, do not count
    assert!(!fit_buy_price(&exact[..5]).unwrap().used);
    assert!(!fit_buy_price(&[(2.0, 2.42); 10]).unwrap().used);
    assert!(fit_buy_price(&[]).is_none());
}

/// buy = round(spot × 1.21, 4) over the entity's slots (a buy entity of spot × 1.21)
fn buy_121(entity: &BTreeMap<DateTime<Utc>, f64>) -> HashMap<DateTime<Utc>, f32> {
    #[expect(clippy::cast_possible_truncation)]
    let map = entity
        .iter()
        .map(|(s, v)| (*s, ((v * 1.21 * 10_000.0).round() / 10_000.0) as f32))
        .collect();
    map
}

#[test]
fn buy_fit_failure_fills_nothing() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let entity = entity_2026_10_13();
    // A buy price with a tariff step: not a × spot + b
    #[expect(clippy::cast_possible_truncation)]
    let buy: HashMap<DateTime<Utc>, f32> = entity
        .iter()
        .map(|(t, s)| {
            let early = t.with_timezone(&Prague).time() < NaiveTime::from_hms_opt(6, 0, 0).unwrap();
            (*t, (s * 1.21 + if early { 0.3 } else { 1.2 }) as f32)
        })
        .collect();
    let mut data = spot_data(&entity);

    fb.apply(&mut data, Some(&buy), prague("2026-10-13 18:00"));

    assert!(data.fallback_block_starts.is_empty());
    assert_eq!(data.time_block_prices.len(), 96);
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    let banner = status.banner.unwrap();
    assert!(
        banner.contains(
            "— no fallback available (buy price fit failed: buy entity is not a × spot + b (max residual"
        ),
        "{banner}"
    );
    assert_eq!(fb.warn_count(), 1);
    // Nothing kept for a later day from a failed check
    assert_eq!(fb.stored_validation(), None);
}

#[test]
fn buy_entity_without_todays_buy_prices_fills_nothing() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let mut data = spot_data(&entity_2026_10_13());
    fb.apply(&mut data, Some(&HashMap::new()), prague("2026-10-13 18:00"));
    assert!(data.fallback_block_starts.is_empty());
    let banner = fb.status().banner.unwrap();
    assert!(
        banner.contains("buy price fit failed: only 0 of today's slots have a buy price"),
        "{banner}"
    );
}

#[test]
fn exact_buy_fit_fills_buy_of_fallback_slots() {
    let fb = fallback_with(config());
    inject(&fb, market_2026_10_13());
    let entity = entity_2026_10_13();
    let mut data = spot_data(&entity);
    fb.apply(
        &mut data,
        Some(&buy_121(&entity)),
        prague("2026-10-13 18:00"),
    );
    assert_eq!(data.fallback_block_starts.len(), 96);
    for start in &data.fallback_block_starts {
        let b = data
            .time_block_prices
            .iter()
            .find(|b| b.block_start == *start)
            .unwrap();
        assert!((b.buy_price_czk_per_kwh.unwrap() - b.price_czk_per_kwh * 1.21).abs() < 1e-3);
    }
    let status = fb.status();
    assert!(status.buy_fit.unwrap().used);
    assert_eq!(status.fallback, FallbackState::Active);
}

// ---------------------------------------------------------------------------------------------
// Second day of an outage: the stored validation
// ---------------------------------------------------------------------------------------------

fn stored_config(dir: &tempfile::TempDir) -> PriceFallbackConfig {
    let mut cfg = config();
    cfg.validation_path = Some(dir.path().join("price_fallback_validation.json"));
    cfg
}

/// Day 1 (2026-10-13 18:00): the entity has today, OTE validates, buy fits 1.21 × spot
fn validate_day_one(fb: &PriceFallback) {
    inject(fb, market_2026_10_13());
    let entity = entity_2026_10_13();
    let mut data = spot_data(&entity);
    fb.apply(
        &mut data,
        Some(&buy_121(&entity)),
        prague("2026-10-13 18:00"),
    );
    assert_eq!(fb.status().fallback, FallbackState::Active);
}

/// OTE fetched on `fetched_for`: prices of 2026-10-13/14, fixing before 2026-10-14 = 24.375
fn day_two_market(fetched_for: &str) -> MarketData {
    market(
        "ote-dam-2026-10-13_14.xml",
        &["cnb-daily-2026-10-13.json"],
        fetched_for,
    )
}

#[test]
fn second_day_uses_a_fresh_stored_validation() {
    let dir = tempfile::tempdir().unwrap();
    let fb = fallback_with(stored_config(&dir));
    validate_day_one(&fb);
    let stored = fb.stored_validation().unwrap();
    assert_eq!(stored.day, day("2026-10-13"));
    assert_eq!(stored.rate_source, RateSource::BeforeToday);
    assert_eq!(stored.rate_valid_for, day("2026-10-12"));
    assert!(stored.buy_fit.unwrap().used);
    assert!(dir.path().join("price_fallback_validation.json").exists());

    // Day 2, 10:00: the entity is unavailable (no prices of today at all)
    inject(&fb, day_two_market("2026-10-14"));
    let mut data = SpotPriceData::default();
    let now = prague("2026-10-14 10:00");
    fb.apply(&mut data, Some(&HashMap::new()), now);

    // 10:00–23:45 filled with the stored rate source (the fixing before 2026-10-14) and buy fit
    assert_eq!(data.fallback_block_starts.len(), 56);
    assert_eq!(data.fallback_block_starts[0], now);
    let m = day_two_market("2026-10-14");
    let b = &data.time_block_prices[0];
    #[expect(clippy::cast_possible_truncation)]
    let expected = czk_per_kwh(m.prices[&now], Decimal::parse("24.375").unwrap()) as f32;
    assert_eq!(b.price_czk_per_kwh, expected);
    assert!((b.buy_price_czk_per_kwh.unwrap() - expected * 1.21).abs() < 1e-3);
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Active);
    assert_eq!(
        status.banner.as_deref(),
        Some(
            "Today's prices missing from sensor.current_spot_electricity_price_15min — using OTE fallback, validated 2026-10-13"
        )
    );
}

#[test]
fn stale_stored_validation_fills_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fb = fallback_with(stored_config(&dir));
    validate_day_one(&fb);

    // 48 h and a minute after the validation
    inject(&fb, day_two_market("2026-10-15"));
    let mut data = SpotPriceData::default();
    fb.apply(&mut data, None, prague("2026-10-15 18:01"));

    assert!(data.fallback_block_starts.is_empty());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    let banner = status.banner.unwrap();
    assert!(
        banner.contains(
            "— no fallback available (only 0 of today's slots to check OTE against, need 8; last validation 2026-10-13 is too old (used on that day and the next only, within 48 h))"
        ),
        "{banner}"
    );
    assert!(fb.warn_count() >= 1);
}

/// Review #4: a validation of day D covers D and D+1 only, even when refreshed late on D
#[test]
fn stored_validation_covers_its_day_and_the_next_only() {
    let dir = tempfile::tempdir().unwrap();
    let fb = fallback_with(stored_config(&dir));
    validate_day_one(&fb);
    // Refreshed at 23:30 (hourly while the entity has today)
    inject(&fb, market_2026_10_13());
    let entity = entity_2026_10_13();
    fb.apply(
        &mut spot_data(&entity),
        Some(&buy_121(&entity)),
        prague("2026-10-13 23:30"),
    );
    assert_eq!(
        fb.stored_validation().unwrap().validated_at,
        prague("2026-10-13 23:30")
    );

    // D+1 23:50: still used (OTE has the last slot of the day)
    inject(&fb, day_two_market("2026-10-14"));
    let mut data = SpotPriceData::default();
    fb.apply(&mut data, Some(&HashMap::new()), prague("2026-10-14 23:50"));
    assert_eq!(data.fallback_block_starts.len(), 1);

    // D+2 00:15: within 48 h of the refresh, but no longer used
    inject(&fb, day_two_market("2026-10-15"));
    let mut data = SpotPriceData::default();
    fb.apply(&mut data, Some(&HashMap::new()), prague("2026-10-15 00:15"));
    assert!(data.fallback_block_starts.is_empty());
    let status = fb.status();
    assert_eq!(status.fallback, FallbackState::Rejected);
    assert!(
        status
            .fallback_reason
            .unwrap()
            .contains("last validation 2026-10-13 is too old")
    );
}

#[test]
fn restart_reloads_the_stored_validation() {
    let dir = tempfile::tempdir().unwrap();
    let first = fallback_with(stored_config(&dir));
    validate_day_one(&first);
    let stored = first.stored_validation().unwrap();
    drop(first);

    let restarted = fallback_with(stored_config(&dir));
    assert_eq!(restarted.stored_validation(), Some(stored));
    inject(&restarted, day_two_market("2026-10-14"));
    let mut data = SpotPriceData::default();
    restarted.apply(&mut data, Some(&HashMap::new()), prague("2026-10-14 10:00"));
    assert_eq!(data.fallback_block_starts.len(), 56);
    assert!(
        restarted
            .status()
            .banner
            .unwrap()
            .ends_with("using OTE fallback, validated 2026-10-13")
    );

    // A corrupt file is ignored, not fatal
    std::fs::write(
        dir.path().join("price_fallback_validation.json"),
        "{not json",
    )
    .unwrap();
    assert_eq!(fallback_with(stored_config(&dir)).stored_validation(), None);
}

#[test]
fn a_mismatch_today_is_not_overridden_by_the_stored_validation() {
    let dir = tempfile::tempdir().unwrap();
    let fb = fallback_with(stored_config(&dir));
    validate_day_one(&fb);
    let entity: BTreeMap<_, _> = entity_2026_10_13()
        .into_iter()
        .map(|(s, v)| (s, v * 1.02 + 0.05))
        .collect();
    let mut data = spot_data(&entity);
    fb.apply(&mut data, None, prague("2026-10-13 19:00"));
    assert!(data.fallback_block_starts.is_empty());
    assert_eq!(fb.status().fallback, FallbackState::Rejected);
}

/// Review #5: a failing notification update is retried every minute but WARNs once per hour
#[tokio::test]
async fn notification_failure_warns_once_per_hour() {
    let mut ha = Server::new_async().await;
    let _create = ha
        .mock("POST", "/api/services/persistent_notification/create")
        .with_status(500)
        .create_async()
        .await;
    let client = HomeAssistantClient::new(ha.url(), "t").unwrap();
    let mut cfg = config();
    cfg.notification_enabled = true;
    let fb = fallback_with(cfg);
    let entity = entity_2026_10_13();
    let t0 = prague("2026-10-13 18:00");
    for minute in 0..59 {
        let now = t0 + Duration::minutes(minute);
        fb.apply(&mut spot_data(&entity), None, now);
        fb.sync_notification(&client, now).await;
    }
    assert_eq!(fb.notification_warn_count(), 1);
    let now = t0 + Duration::minutes(60);
    fb.apply(&mut spot_data(&entity), None, now);
    fb.sync_notification(&client, now).await;
    assert_eq!(fb.notification_warn_count(), 2);
}
