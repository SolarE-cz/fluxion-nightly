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

//! OTE day-ahead market prices and the CNB EUR/CZK rate (brief H35).
//!
//! Same public sources as the `cz_energy_spot_prices` HA integration, re-implemented here:
//! - OTE `PublicDataService` SOAP call `GetDamPricePeriodE` with `PT15M` resolution. Each item
//!   has the market day (`Date`), the 1-based 15-minute `PeriodIndex` (92, 96 or 100 per day)
//!   and the `Price` in EUR/MWh. Slot start = Prague midnight of the day, in UTC, plus
//!   `(PeriodIndex - 1) × 15 min`.
//! - CNB daily rates (`/cnbapi/exrates/daily?date=YYYY-MM-DD`); a date without a fixing
//!   returns the latest earlier fixing.
//!
//! CZK/kWh = EUR/MWh × (EUR/CZK) / 1000, rounded half-to-even to 4 decimals, computed in exact
//! decimal arithmetic like the integration's sensor attributes.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Europe::Prague;
use quick_xml::{Reader, events::Event};
use std::collections::BTreeMap;

/// OTE public data service (SOAP)
pub const OTE_PUBLIC_URL: &str = "https://www.ote-cr.cz/services/PublicDataService";
/// CNB daily exchange rates (JSON)
pub const CNB_RATES_URL: &str = "https://api.cnb.cz/cnbapi/exrates/daily";

/// An exact decimal: `mantissa × 10^-scale`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub mantissa: i128,
    pub scale: u32,
}

impl Decimal {
    /// Parse a plain decimal number ("181.20", "-5.3", "24.34"); no exponent
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
        if int_part.is_empty() && frac_part.is_empty() {
            bail!("empty number");
        }
        if !int_part
            .chars()
            .chain(frac_part.chars())
            .all(|c| c.is_ascii_digit())
        {
            bail!("not a decimal number: {text}");
        }
        // Two 18-digit mantissas multiply within i128 (the conversion never overflows)
        if int_part.len() + frac_part.len() > 18 {
            bail!("number too long: {text}");
        }
        let mantissa: i128 = format!("{int_part}{frac_part}")
            .parse()
            .with_context(|| format!("not a decimal number: {text}"))?;
        Ok(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale: u32::try_from(frac_part.len()).unwrap_or(u32::MAX),
        })
    }

    #[must_use]
    pub fn to_f64(self) -> f64 {
        #[expect(clippy::cast_precision_loss, reason = "display/compare value only")]
        let m = self.mantissa as f64;
        m / 10f64.powi(i32::try_from(self.scale).unwrap_or(i32::MAX))
    }
}

/// Divide by 10^`digits`, rounding half to even (Python `Decimal` rounding)
fn div_pow10_half_even(value: i128, digits: u32) -> i128 {
    if digits == 0 {
        return value;
    }
    let divisor = 10i128.pow(digits);
    let quotient = value.div_euclid(divisor);
    let remainder = value.rem_euclid(divisor);
    let twice = remainder * 2;
    if twice > divisor || (twice == divisor && quotient.rem_euclid(2) == 1) {
        quotient + 1
    } else {
        quotient
    }
}

/// EUR/MWh × EUR/CZK rate → CZK/kWh rounded half-to-even to 4 decimals, as the integration does
#[must_use]
pub fn czk_per_kwh(price_eur_mwh: Decimal, eur_czk: Decimal) -> f64 {
    // price × rate / 1000: scale grows by 3
    let product = price_eur_mwh.mantissa * eur_czk.mantissa;
    let scale = price_eur_mwh.scale + eur_czk.scale + 3;
    let ten_thousandths = if scale >= 4 {
        div_pow10_half_even(product, scale - 4)
    } else {
        product * 10i128.pow(4 - scale)
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "4-decimal price fits f64 exactly enough"
    )]
    let value = ten_thousandths as f64;
    value / 10_000.0
}

/// Start (UTC) of the 15-minute period `index` (1-based) of the Prague market day `day`
#[must_use]
pub fn period_start(day: NaiveDate, index: u32) -> Option<DateTime<Utc>> {
    let midnight = Prague
        .from_local_datetime(&day.and_time(NaiveTime::MIN))
        .earliest()?
        .with_timezone(&Utc);
    Some(midnight + Duration::minutes(i64::from(index.checked_sub(1)?) * 15))
}

/// UTC starts of the 15-minute slots of the Prague day `day` (92, 96 or 100)
#[must_use]
pub fn day_slots(day: NaiveDate) -> Vec<DateTime<Utc>> {
    let (Some(start), Some(end)) = (
        period_start(day, 1),
        day.succ_opt().and_then(|d| period_start(d, 1)),
    ) else {
        return Vec::new();
    };
    let mut slots = Vec::with_capacity(100);
    let mut t = start;
    while t < end {
        slots.push(t);
        t += Duration::minutes(15);
    }
    slots
}

/// SOAP request for the 15-minute day-ahead prices from `start` to `end` (market days)
#[must_use]
pub fn dam_query(start: NaiveDate, end: NaiveDate) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:pub="http://www.ote-cr.cz/schema/service/public">
  <soapenv:Header/>
  <soapenv:Body>
    <pub:GetDamPricePeriodE>
      <pub:StartDate>{start}</pub:StartDate>
      <pub:EndDate>{end}</pub:EndDate>
      <pub:PeriodResolution>PT15M</pub:PeriodResolution>
    </pub:GetDamPricePeriodE>
  </soapenv:Body>
</soapenv:Envelope>
"#,
        start = start.format("%Y-%m-%d"),
        end = end.format("%Y-%m-%d"),
    )
}

#[derive(Default)]
struct ItemFields {
    date: Option<String>,
    index: Option<String>,
    price: Option<String>,
}

/// Parse a `GetDamPricePeriodE` response into EUR/MWh per slot start (UTC). A SOAP fault is an
/// error; items without a price are skipped (not published yet).
pub fn parse_dam_response(xml: &str) -> Result<BTreeMap<DateTime<Utc>, Decimal>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut prices = BTreeMap::new();
    let mut item: Option<ItemFields> = None;
    let mut current: Option<String> = None;
    let mut in_fault = false;
    let mut fault_text = String::new();

    loop {
        match reader
            .read_event()
            .context("OTE response is not valid XML")?
        {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "Fault" => in_fault = true,
                    "Item" => item = Some(ItemFields::default()),
                    _ => {}
                }
                current = Some(name);
            }
            Event::Text(t) => {
                let text = t.decode().context("OTE response text is not UTF-8")?;
                if in_fault && current.as_deref() == Some("faultstring") {
                    fault_text.push_str(&text);
                }
                if let (Some(fields), Some(name)) = (item.as_mut(), current.as_deref()) {
                    match name {
                        "Date" => fields.date = Some(text.into_owned()),
                        "PeriodIndex" => fields.index = Some(text.into_owned()),
                        "Price" => fields.price = Some(text.into_owned()),
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"Item"
                    && let Some(fields) = item.take()
                {
                    add_item(&mut prices, fields)?;
                }
                current = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if in_fault {
        let reason = if fault_text.is_empty() {
            "unknown fault".to_owned()
        } else {
            fault_text
        };
        bail!("OTE returned a SOAP fault: {reason}");
    }
    Ok(prices)
}

fn add_item(prices: &mut BTreeMap<DateTime<Utc>, Decimal>, fields: ItemFields) -> Result<()> {
    let date = fields.date.context("OTE item without Date")?;
    let day = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .with_context(|| format!("OTE item with invalid Date {date}"))?;
    let index_text = fields
        .index
        .with_context(|| format!("OTE item of {day} without PeriodIndex"))?;
    let index: u32 = index_text
        .trim()
        .parse()
        .with_context(|| format!("OTE item of {day} with invalid PeriodIndex {index_text}"))?;
    if !(1..=100).contains(&index) {
        bail!("OTE item of {day} with PeriodIndex {index} out of range");
    }
    let Some(price) = fields.price.filter(|p| !p.trim().is_empty()) else {
        return Ok(());
    };
    let price =
        Decimal::parse(&price).with_context(|| format!("OTE item of {day}/{index}: bad Price"))?;
    let start = period_start(day, index).with_context(|| format!("no Prague midnight on {day}"))?;
    prices.insert(start, price);
    Ok(())
}

/// EUR/CZK fixing from a CNB daily rates response
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EurCzkRate {
    /// Day the fixing is valid for
    pub valid_for: NaiveDate,
    pub rate: Decimal,
}

/// Parse a CNB `/cnbapi/exrates/daily` response and return the EUR rate
pub fn parse_cnb_eur_rate(json: &str) -> Result<EurCzkRate> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("CNB response is not JSON")?;
    let rates = value
        .get("rates")
        .and_then(|r| r.as_array())
        .context("CNB response without rates")?;
    let eur = rates
        .iter()
        .find(|r| r.get("currencyCode").and_then(|c| c.as_str()) == Some("EUR"))
        .context("CNB response without an EUR rate")?;
    let amount = eur
        .get("amount")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(1);
    if amount != 1 {
        bail!("CNB EUR rate is per {amount} EUR, expected 1");
    }
    // serde_json prints a parsed float in its shortest round-trip form ("24.34")
    let rate_text = eur
        .get("rate")
        .and_then(|r| r.as_number())
        .context("CNB EUR rate missing")?
        .to_string();
    let rate = Decimal::parse(&rate_text).with_context(|| format!("CNB EUR rate {rate_text}"))?;
    if rate.mantissa <= 0 {
        bail!("CNB EUR rate {rate_text} is not positive");
    }
    let valid_for_text = eur
        .get("validFor")
        .and_then(|v| v.as_str())
        .context("CNB EUR rate without validFor")?;
    let valid_for = NaiveDate::parse_from_str(valid_for_text, "%Y-%m-%d")
        .with_context(|| format!("CNB validFor {valid_for_text}"))?;
    Ok(EurCzkRate { valid_for, rate })
}

/// HTTP client for the OTE prices and the CNB rate
#[derive(Debug, Clone)]
pub struct OteClient {
    http: reqwest::Client,
    ote_url: String,
    cnb_url: String,
}

impl OteClient {
    pub fn new(ote_url: impl Into<String>, cnb_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build the OTE HTTP client")?;
        Ok(Self {
            http,
            ote_url: ote_url.into(),
            cnb_url: cnb_url.into(),
        })
    }

    /// EUR/MWh per slot of the market days `start..=end`
    pub async fn fetch_dam_prices(
        &self,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<BTreeMap<DateTime<Utc>, Decimal>> {
        let response = self
            .http
            .post(&self.ote_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .body(dam_query(start, end))
            .send()
            .await
            .context("OTE request failed")?;
        let status = response.status();
        let body = response.text().await.context("OTE response unreadable")?;
        if !status.is_success() {
            // A SOAP fault comes with HTTP 500; report its text when there is one
            if let Err(e) = parse_dam_response(&body) {
                bail!("OTE returned HTTP {status}: {e:#}");
            }
            bail!("OTE returned HTTP {status}");
        }
        parse_dam_response(&body)
    }

    /// EUR/CZK fixing for `day` (the latest fixing on or before it)
    pub async fn fetch_eur_rate(&self, day: NaiveDate) -> Result<EurCzkRate> {
        let response = self
            .http
            .get(&self.cnb_url)
            .query(&[("date", day.format("%Y-%m-%d").to_string())])
            .send()
            .await
            .context("CNB request failed")?;
        let status = response.status();
        let body = response.text().await.context("CNB response unreadable")?;
        if !status.is_success() {
            bail!("CNB returned HTTP {status}");
        }
        parse_cnb_eur_rate(&body)
    }
}
