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

use askama::Template;
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use chrono_tz::Tz;
use fluxion_core::web_bridge::TimezoneSource;
use fluxion_core::{InverterData, ScheduleData, SystemHealthData, WebQueryResponse};
use fluxion_i18n::I18n;
use fluxion_types::UserControlState;
use std::sync::Arc;

/// Price data for Chart.js rendering
#[derive(Debug, Clone, serde::Serialize)]
pub struct PriceDataWithChart {
    pub current_price: f32,
    pub min_price: f32,
    pub max_price: f32,
    pub avg_price: f32,
    pub chart_data: ChartData,
    // Today's price statistics
    pub today_min_price: f32,
    pub today_max_price: f32,
    pub today_avg_price: f32,
    pub today_median_price: f32,
    // Tomorrow's price statistics (None if not yet available)
    pub tomorrow_min_price: Option<f32>,
    pub tomorrow_max_price: Option<f32>,
    pub tomorrow_avg_price: Option<f32>,
    pub tomorrow_median_price: Option<f32>,
}

/// Battery SOC history point for Chart.js
#[derive(Debug, Clone, serde::Serialize)]
pub struct SocHistoryPoint {
    pub label: String,
    pub soc: f32,
}

/// Battery SOC prediction point for Chart.js
#[derive(Debug, Clone, serde::Serialize)]
pub struct SocPredictionPoint {
    pub label: String,
    pub soc: f32,
}

/// PV generation history point for Chart.js
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvHistoryPoint {
    pub label: String,
    pub power_w: f32,
}

/// Chart data for Chart.js
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChartData {
    pub labels: Vec<String>,
    /// Block start instants (RFC 3339, UTC), aligned with `labels`
    pub block_starts_utc: Vec<String>,
    pub prices: Vec<f32>,
    pub modes: Vec<String>,
    pub target_socs: Vec<Option<f32>>,
    pub strategies: Vec<Option<String>>,
    pub profits: Vec<Option<f32>>,
    pub current_time_label: Option<String>,
    pub battery_soc_history: Vec<SocHistoryPoint>,
    pub battery_soc_prediction: Vec<SocPredictionPoint>,
    pub current_battery_soc: Option<f32>,
    pub pv_generation_history: Vec<PvHistoryPoint>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub debug_info: Vec<Option<fluxion_core::strategy::BlockDebugInfo>>,
    pub is_historical: Vec<bool>, // True for past blocks (shows regenerated schedule, not actual history)
    pub reasons: Vec<Option<String>>,
    pub decision_uids: Vec<Option<String>>,

    // Price breakdown for stacked bar chart
    /// Base spot prices (same as prices, for clarity in stacked display)
    pub spot_prices: Vec<f32>,
    /// HDO grid fee for each block (varies by tariff period)
    pub grid_fees: Vec<f32>,
    /// Tariff type for each block: "low" or "high" (for coloring)
    pub tariff_types: Vec<String>,
    /// Spot buy fee for each block
    pub buy_fees: Vec<f32>,
    /// Total effective price: spot + grid_fee + buy_fees
    pub effective_prices: Vec<f32>,
    /// Hourly consumption profile (24 values, kWh per hour, index = UTC hour of day)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hourly_consumption_profile: Option<Vec<f32>>,
    /// True when an HDO schedule with low-tariff periods is known, so `tariff_types` is real
    /// (otherwise every block defaults to "high")
    pub tariff_known: bool,
    /// Actual PV power (kW), mean of the PV history samples inside each block; `None` where the
    /// block has no samples. Aligned with `labels`.
    pub pv_actual_kw: Vec<Option<f32>>,
    /// Forecast PV power (kW) for blocks that have not ended yet; `None` elsewhere.
    /// Aligned with `labels`. Corrected by the PV forecast correction when one is in force.
    pub solar_forecast_kw: Vec<Option<f32>>,
    /// The forecast before the PV forecast correction (brief H34); all `None` when no
    /// correction is in force. Aligned with `labels`.
    pub solar_forecast_raw_kw: Vec<Option<f32>>,
    /// The correction in force: factor and the days it rests on
    pub pv_forecast_correction: Option<fluxion_core::pv_correction::CorrectionResult>,
    /// Price source per block when not the HA price entity (`"ote_fallback"`, brief H35).
    /// Aligned with `labels`.
    pub price_sources: Vec<Option<String>>,
}

/// Live data template (for SSE updates only)
#[derive(Template)]
#[template(path = "live_data.html", escape = "none")]
pub struct LiveDataTemplate {
    #[expect(
        dead_code,
        reason = "Header with debug badge moved outside SSE update area"
    )]
    pub debug_mode: bool,
    pub inverters: Vec<InverterData>,
    pub schedule: Option<ScheduleData>,
    pub prices: Option<PriceDataWithChart>,
    pub health: SystemHealthData,
    pub i18n: Arc<I18n>,
    pub last_update_formatted: String,
    pub next_change_formatted: Option<String>,
    /// Aggregated consumption statistics (EMA, imports)
    pub consumption_stats: Option<fluxion_core::web_bridge::ConsumptionStats>,
    /// Solar forecast data
    pub solar_forecast: Option<fluxion_core::web_bridge::SolarForecastInfo>,
    /// Cost of charge of the battery energy (brief H33)
    pub battery_cost_basis: Option<fluxion_core::web_bridge::BatteryCostBasisInfo>,
}

impl LiveDataTemplate {
    pub fn t(&self, key: &str) -> String {
        self.i18n.get(key).unwrap_or_else(|_| key.to_owned())
    }
}

/// Resolve the display timezone of a response.
/// Returns `None` (UTC) when no usable timezone is known.
pub fn display_timezone(response: &WebQueryResponse) -> (Option<Tz>, TimezoneSource) {
    match response
        .timezone
        .as_deref()
        .and_then(|name| name.parse::<Tz>().ok())
    {
        Some(tz) => (Some(tz), response.timezone_source),
        None => (None, TimezoneSource::Unknown),
    }
}

/// Format a timestamp in the display timezone, or in UTC when unknown.
/// Each timestamp is converted individually, so DST offsets are per instant.
/// Chart and Upcoming Schedule label of a block's effective mode. Blocks that run as Back Up
/// Mode in the Back Up default mode carry the minimum SOC, e.g. "Back Up Mode (25%)"; the page
/// colours every label starting with "Back Up Mode" with the Back Up legend colour.
pub fn display_mode_label(block: &fluxion_core::web_bridge::PriceBlockData) -> String {
    match block.block_type.as_str() {
        "charge" => "Force Charge".to_owned(),
        "discharge" => "Force Discharge".to_owned(),
        "backup" => match block.backup_min_soc_pct {
            Some(min_soc) => format!("Back Up Mode ({min_soc}%)"),
            None => "Back Up Mode".to_owned(),
        },
        "no-charge-discharge" => "Hold".to_owned(),
        "feedin-priority" => "Feed-in Priority".to_owned(),
        _ => "Self-Use".to_owned(),
    }
}

fn format_in_display_tz(ts: DateTime<Utc>, tz: Option<Tz>, fmt: &str) -> String {
    match tz {
        Some(tz) => ts.with_timezone(&tz).format(fmt).to_string(),
        None => ts.format(fmt).to_string(),
    }
}

/// Dashboard template
#[derive(Template)]
#[template(path = "index.html", escape = "none")]
pub struct DashboardTemplate {
    pub debug_mode: bool,
    /// False when FluxION only curtails export (brief H37); the header shows "mode control off"
    pub mode_control_enabled: bool,
    pub inverters: Vec<InverterData>,
    pub schedule: Option<ScheduleData>,
    pub prices: Option<PriceDataWithChart>,
    pub chart_data_json: String,
    pub health: SystemHealthData,
    pub i18n: Arc<I18n>,
    #[expect(dead_code, reason = "May be used in future template updates")]
    pub timezone: Option<String>,
    /// True when no display timezone is known and times are shown in UTC
    pub timezone_unknown: bool,
    /// Home Assistant timezone name, when HA provides one (first option of the selector)
    pub ha_timezone: Option<String>,
    /// Zone abbreviation for the chart header ("UTC" when unknown)
    pub display_tz_label: String,
    pub last_update_formatted: String,
    pub next_change_formatted: Option<String>,
    /// Ingress path prefix for HA Ingress support (e.g., "/hassio/ingress/641a79a3_fluxion")
    /// Empty string when running standalone
    pub ingress_path: String,
    /// Aggregated consumption statistics (EMA, imports)
    pub consumption_stats: Option<fluxion_core::web_bridge::ConsumptionStats>,
    /// Solar forecast data
    pub solar_forecast: Option<fluxion_core::web_bridge::SolarForecastInfo>,
    /// User control state for dashboard panel
    pub user_control: Option<UserControlState>,
    /// Cost of charge of the battery energy (brief H33)
    pub battery_cost_basis: Option<fluxion_core::web_bridge::BatteryCostBasisInfo>,
    /// Missing-price banner (brief H35); `None` when the price feed is complete
    pub price_feed_banner: Option<String>,
}

impl DashboardTemplate {
    /// Helper method for translations in templates
    pub fn t(&self, key: &str) -> String {
        self.i18n.get(key).unwrap_or_else(|_| key.to_owned())
    }

    /// Create template from ECS query response
    #[expect(
        clippy::too_many_lines,
        reason = "Template construction requires processing multiple data sources"
    )]
    pub fn from_query_response(
        response: WebQueryResponse,
        i18n: Arc<I18n>,
        ingress_path: String,
        user_control: Option<UserControlState>,
    ) -> Self {
        let (display_tz, timezone_source) = display_timezone(&response);
        let timezone_unknown = timezone_source == TimezoneSource::Unknown;
        let ha_timezone = if timezone_source == TimezoneSource::HomeAssistant {
            response.timezone.clone()
        } else {
            None
        };
        let display_tz_label = display_tz.map_or_else(
            || "UTC".to_owned(),
            |tz| Utc::now().with_timezone(&tz).format("%Z").to_string(),
        );

        // Format last update time in the display timezone
        let last_update_formatted =
            format_in_display_tz(response.health.last_update, display_tz, "%Y-%m-%d %H:%M:%S");

        // Format next change time if available
        let next_change_formatted = if let Some(ref schedule) = response.schedule {
            schedule
                .next_change
                .map(|next| format_in_display_tz(next, display_tz, "%H:%M:%S"))
        } else {
            None
        };

        // Extract hourly consumption profile before response.prices moves it
        let hourly_consumption_profile = response
            .consumption_stats
            .as_ref()
            .and_then(|cs| cs.hourly_consumption_profile.clone());

        // Prepare chart data for Chart.js from price data if available
        let prices = response.prices.map(|price_data| {
            let mut labels = Vec::new();
            let mut block_starts_utc = Vec::new();
            let mut prices = Vec::new();
            let mut modes = Vec::new();
            let mut target_socs = Vec::new();
            let mut strategies = Vec::new();
            let mut profits = Vec::new();
            let mut debug_info_vec = Vec::new();
            let mut is_historical_vec = Vec::new();
            let mut reasons = Vec::new();
            let mut decision_uids = Vec::new();

            // Price breakdown for stacked bars
            let mut spot_prices = Vec::new();
            let mut grid_fees = Vec::new();
            let mut tariff_types = Vec::new();
            let mut buy_fees = Vec::new();
            let mut effective_prices = Vec::new();
            let mut solar_forecast_kw = Vec::new();
            let mut solar_forecast_raw_kw = Vec::new();
            let mut price_sources = Vec::new();

            // Get HDO and pricing fee info from response
            let hdo_schedule = response.hdo_schedule.as_ref();
            let tariff_known = hdo_schedule.is_some_and(|hdo| !hdo.low_tariff_periods.is_empty());
            let pricing_fees = response.pricing_fees.as_ref();

            // Get current time in the appropriate timezone for the "now" marker
            // Round to nearest 15-minute block to match chart data
            let now = chrono::Utc::now();
            let minutes = now.minute();
            #[expect(
                clippy::integer_division,
                reason = "Intentional integer division to round to 15-minute blocks"
            )]
            let rounded_minutes = (minutes / 15) * 15; // Round down to 15-min block
            let rounded_time = now
                .with_minute(rounded_minutes)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(now);

            let current_time_label = Some(format_in_display_tz(
                rounded_time,
                display_tz,
                "%m-%d %H:%M",
            ));

            for block in &price_data.blocks {
                // Format time label in the correct timezone
                let label = format_in_display_tz(block.timestamp, display_tz, "%m-%d %H:%M");

                labels.push(label);
                block_starts_utc.push(block.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true));
                prices.push(block.price);
                target_socs.push(block.target_soc);
                strategies.push(block.strategy.clone());
                profits.push(block.expected_profit);
                debug_info_vec.push(block.debug_info.clone());
                is_historical_vec.push(block.is_historical); // Track if block is past (regenerated schedule)
                reasons.push(block.reason.clone());
                decision_uids.push(block.decision_uid.clone());
                solar_forecast_kw.push(block.solar_forecast_kw);
                solar_forecast_raw_kw.push(block.solar_forecast_raw_kw);
                price_sources.push(block.price_source.clone());

                modes.push(display_mode_label(block));

                // Calculate price breakdown for stacked bars
                let spot_price = block.price;
                spot_prices.push(spot_price);

                let block_time_str = format_in_display_tz(block.timestamp, display_tz, "%H:%M");
                let (grid_fee, tariff_type) = if let Some(hdo) = hdo_schedule {
                    // Log HDO info for first block only
                    if labels.len() == 1 {
                        tracing::debug!(
                            "📊 HDO schedule has {} low tariff periods: {:?}",
                            hdo.low_tariff_periods.len(),
                            hdo.low_tariff_periods
                        );
                    }
                    let is_low =
                        block_in_low_tariff(block.timestamp, display_tz, &hdo.low_tariff_periods);
                    // Log tariff decision for a few sample blocks
                    if labels.len() <= 5 || labels.len() % 20 == 0 {
                        tracing::debug!(
                            "📊 Block {} (time {}): is_low={}, periods_count={}",
                            labels.len(),
                            block_time_str,
                            is_low,
                            hdo.low_tariff_periods.len()
                        );
                    }
                    if is_low {
                        (hdo.low_tariff_czk, "low".to_owned())
                    } else {
                        (hdo.high_tariff_czk, "high".to_owned())
                    }
                } else {
                    // Default to high tariff if no HDO data
                    (0.0, "high".to_owned())
                };
                grid_fees.push(grid_fee);
                tariff_types.push(tariff_type);

                // Calculate buy fees (constant for all blocks)
                let buy_fee = if let Some(fees) = pricing_fees {
                    fees.buy_fee_czk
                } else {
                    0.0
                };
                buy_fees.push(buy_fee);

                // The planner's buy price when known (covers fixed buy pricing); else spot + fees
                let effective_price = block
                    .effective_buy_price
                    .unwrap_or(spot_price + grid_fee + buy_fee);
                effective_prices.push(effective_price);
            }

            // Convert battery SOC history to chart format
            let battery_soc_history = response.battery_soc_history.as_ref().map_or_else(
                || {
                    tracing::warn!("📊 [WEB ROUTES] No battery history from ECS, using empty vec");
                    Vec::new()
                },
                |history| {
                    tracing::debug!(
                        "📊 [WEB ROUTES] Converting {} history points from ECS to chart format",
                        history.len()
                    );
                    if !history.is_empty() {
                        tracing::debug!(
                            "📊 [WEB ROUTES] History range: {:.1}% ({}) -> {:.1}% ({})",
                            history.first().map_or(0.0, |p| p.soc),
                            history
                                .first()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default(),
                            history.last().map_or(0.0, |p| p.soc),
                            history
                                .last()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default()
                        );
                    }

                    history
                        .iter()
                        .map(|point| {
                            let label =
                                format_in_display_tz(point.timestamp, display_tz, "%m-%d %H:%M");
                            SocHistoryPoint {
                                label,
                                soc: point.soc,
                            }
                        })
                        .collect()
                },
            );

            // Convert battery SOC prediction to chart format
            let battery_soc_prediction = response.battery_soc_prediction.as_ref().map_or_else(
                || {
                    tracing::debug!(
                        "📈 [WEB ROUTES] No battery prediction from ECS, using empty vec"
                    );
                    Vec::new()
                },
                |prediction| {
                    tracing::debug!(
                        "📈 [WEB ROUTES] Converting {} prediction points from ECS to chart format",
                        prediction.len()
                    );
                    if !prediction.is_empty() {
                        tracing::debug!(
                            "📈 [WEB ROUTES] Prediction range: {:.1}% ({}) -> {:.1}% ({})",
                            prediction.first().map_or(0.0, |p| p.soc),
                            prediction
                                .first()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default(),
                            prediction.last().map_or(0.0, |p| p.soc),
                            prediction
                                .last()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default()
                        );
                    }

                    prediction
                        .iter()
                        .map(|point| {
                            let label =
                                format_in_display_tz(point.timestamp, display_tz, "%m-%d %H:%M");
                            SocPredictionPoint {
                                label,
                                soc: point.soc,
                            }
                        })
                        .collect()
                },
            );

            let current_battery_soc = response.inverters.first().map(|inv| inv.battery_soc);

            // Bucket PV history into the chart blocks by UTC instant
            let block_starts: Vec<DateTime<Utc>> =
                price_data.blocks.iter().map(|b| b.timestamp).collect();
            let pv_actual_kw = bucket_pv_history_kw(
                &block_starts,
                response.pv_generation_history.as_deref().unwrap_or(&[]),
            );

            // Convert PV generation history to chart format
            let pv_generation_history = response.pv_generation_history.as_ref().map_or_else(
                || {
                    tracing::debug!("☀️ [WEB ROUTES] No PV history from ECS, using empty vec");
                    Vec::new()
                },
                |pv_history| {
                    tracing::debug!(
                        "☀️ [WEB ROUTES] Converting {} PV history points from ECS to chart format",
                        pv_history.len()
                    );
                    if !pv_history.is_empty() {
                        tracing::debug!(
                            "☀️ [WEB ROUTES] PV range: {:.0}W ({}) -> {:.0}W ({})",
                            pv_history.first().map_or(0.0, |p| p.power_w),
                            pv_history
                                .first()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default(),
                            pv_history.last().map_or(0.0, |p| p.power_w),
                            pv_history
                                .last()
                                .map(|p| p.timestamp.format("%H:%M").to_string())
                                .unwrap_or_default()
                        );
                    }

                    pv_history
                        .iter()
                        .map(|point| {
                            let label =
                                format_in_display_tz(point.timestamp, display_tz, "%m-%d %H:%M");
                            PvHistoryPoint {
                                label,
                                power_w: point.power_w,
                            }
                        })
                        .collect()
                },
            );

            PriceDataWithChart {
                current_price: price_data.current_price,
                min_price: price_data.min_price,
                max_price: price_data.max_price,
                avg_price: price_data.avg_price,
                today_min_price: price_data.today_min_price,
                today_max_price: price_data.today_max_price,
                today_avg_price: price_data.today_avg_price,
                today_median_price: price_data.today_median_price,
                tomorrow_min_price: price_data.tomorrow_min_price,
                tomorrow_max_price: price_data.tomorrow_max_price,
                tomorrow_avg_price: price_data.tomorrow_avg_price,
                tomorrow_median_price: price_data.tomorrow_median_price,
                chart_data: ChartData {
                    labels,
                    block_starts_utc,
                    prices,
                    modes,
                    target_socs,
                    strategies,
                    profits,
                    current_time_label,
                    battery_soc_history,
                    battery_soc_prediction,
                    current_battery_soc,
                    pv_generation_history,
                    debug_info: debug_info_vec,
                    is_historical: is_historical_vec,
                    reasons,
                    decision_uids,
                    // Price breakdown for stacked bars
                    spot_prices,
                    grid_fees,
                    tariff_types,
                    buy_fees,
                    effective_prices,
                    hourly_consumption_profile: hourly_consumption_profile.clone(),
                    tariff_known,
                    pv_actual_kw,
                    solar_forecast_kw,
                    solar_forecast_raw_kw,
                    pv_forecast_correction: response
                        .solar_forecast
                        .as_ref()
                        .and_then(|sf| sf.correction),
                    price_sources,
                },
            }
        });

        // Serialize chart data to JSON for JavaScript
        let chart_data_json = prices
            .as_ref()
            .and_then(|p| serde_json::to_string(&p.chart_data).ok())
            .unwrap_or_else(|| "{\"labels\":[],\"prices\":[],\"modes\":[],\"target_socs\":[],\"strategies\":[],\"profits\":[]}".to_owned());

        Self {
            debug_mode: response.debug_mode,
            mode_control_enabled: response.mode_control_enabled,
            inverters: response.inverters,
            schedule: response.schedule,
            prices,
            chart_data_json,
            health: response.health,
            i18n,
            timezone: response.timezone,
            timezone_unknown,
            ha_timezone,
            display_tz_label,
            last_update_formatted,
            next_change_formatted,
            ingress_path,
            consumption_stats: response.consumption_stats,
            solar_forecast: response.solar_forecast,
            user_control,
            battery_cost_basis: response.battery_cost_basis,
            price_feed_banner: response.price_feed.and_then(|p| p.banner),
        }
    }
}

/// Default block length when the next block start does not give one
const DEFAULT_BLOCK_MINUTES: i64 = 15;

/// Mean PV power (kW) of the history samples inside each block, matched by UTC instant.
/// Block `i` covers `[starts[i], starts[i + 1])`; the last block (or one followed by a gap
/// longer than an hour) covers 15 minutes. `None` where a block has no samples.
pub fn bucket_pv_history_kw(
    block_starts: &[DateTime<Utc>],
    history: &[fluxion_core::web_bridge::PvGenerationHistoryPoint],
) -> Vec<Option<f32>> {
    let default_len = chrono::Duration::minutes(DEFAULT_BLOCK_MINUTES);
    let mut sums = vec![(0.0_f32, 0.0_f32); block_starts.len()];
    for point in history {
        // Last block starting at or before the sample
        let idx = block_starts.partition_point(|start| *start <= point.timestamp);
        let Some(i) = idx.checked_sub(1) else {
            continue;
        };
        let end = block_starts
            .get(i + 1)
            .copied()
            .filter(|next| *next - block_starts[i] <= chrono::Duration::hours(1))
            .unwrap_or(block_starts[i] + default_len);
        if point.timestamp < end {
            sums[i].0 += point.power_w;
            sums[i].1 += 1.0;
        }
    }
    sums.into_iter()
        .map(|(sum, count)| (count > 0.0).then(|| sum / count / 1000.0))
        .collect()
}

/// Check if a time (HH:MM format) falls within any of the low tariff periods
/// HDO windows are local wall-clock times, so the block is matched in the display timezone.
fn block_in_low_tariff(ts: DateTime<Utc>, tz: Option<Tz>, periods: &[(String, String)]) -> bool {
    is_time_in_low_tariff(&format_in_display_tz(ts, tz, "%H:%M"), periods)
}

fn is_time_in_low_tariff(time_str: &str, periods: &[(String, String)]) -> bool {
    // Parse the block time
    let time_parts: Vec<&str> = time_str.split(':').collect();
    if time_parts.len() != 2 {
        tracing::warn!("HDO: Invalid time format (no colon): '{}'", time_str);
        return false;
    }
    let Ok(time_hour) = time_parts[0].parse::<u32>() else {
        tracing::warn!("HDO: Invalid hour in time: '{}'", time_str);
        return false;
    };
    let Ok(time_minute) = time_parts[1].parse::<u32>() else {
        tracing::warn!("HDO: Invalid minute in time: '{}'", time_str);
        return false;
    };
    let time_minutes = time_hour * 60 + time_minute;

    for (start, end) in periods {
        // Parse start time
        let start_parts: Vec<&str> = start.split(':').collect();
        if start_parts.len() != 2 {
            tracing::warn!("HDO: Invalid start time format: '{}'", start);
            continue;
        }
        let Ok(start_hour) = start_parts[0].parse::<u32>() else {
            tracing::warn!("HDO: Invalid start hour: '{}'", start);
            continue;
        };
        let Ok(start_minute) = start_parts[1].parse::<u32>() else {
            tracing::warn!("HDO: Invalid start minute: '{}'", start);
            continue;
        };
        let start_minutes = start_hour * 60 + start_minute;

        // Parse end time (handle "24:00" as 1440)
        let end_parts: Vec<&str> = end.split(':').collect();
        if end_parts.len() != 2 {
            tracing::warn!("HDO: Invalid end time format: '{}'", end);
            continue;
        }
        let Ok(end_hour) = end_parts[0].parse::<u32>() else {
            tracing::warn!("HDO: Invalid end hour: '{}'", end);
            continue;
        };
        let Ok(end_minute) = end_parts[1].parse::<u32>() else {
            tracing::warn!("HDO: Invalid end minute: '{}'", end);
            continue;
        };
        let end_minutes = if end_hour == 24 {
            24 * 60 // 1440 minutes
        } else {
            end_hour * 60 + end_minute
        };

        // Check if time falls within this period
        if start_minutes <= end_minutes {
            // Normal range (e.g., 06:00-12:00)
            if time_minutes >= start_minutes && time_minutes < end_minutes {
                return true;
            }
        } else {
            // Overnight range (e.g., 22:00-06:00)
            if time_minutes >= start_minutes || time_minutes < end_minutes {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_time_in_low_tariff_cez_schedule() {
        // CEZ HDO schedule: low tariff periods
        let periods = vec![
            ("00:00".to_owned(), "06:00".to_owned()),
            ("07:00".to_owned(), "09:00".to_owned()),
            ("10:00".to_owned(), "13:00".to_owned()),
            ("14:00".to_owned(), "16:00".to_owned()),
            ("17:00".to_owned(), "24:00".to_owned()),
        ];

        // Times that should be in LOW tariff
        assert!(
            is_time_in_low_tariff("00:00", &periods),
            "00:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("03:30", &periods),
            "03:30 should be low"
        );
        assert!(
            is_time_in_low_tariff("05:59", &periods),
            "05:59 should be low"
        );
        assert!(
            is_time_in_low_tariff("07:00", &periods),
            "07:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("08:30", &periods),
            "08:30 should be low"
        );
        assert!(
            is_time_in_low_tariff("10:00", &periods),
            "10:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("12:45", &periods),
            "12:45 should be low"
        );
        assert!(
            is_time_in_low_tariff("14:00", &periods),
            "14:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("15:30", &periods),
            "15:30 should be low"
        );
        assert!(
            is_time_in_low_tariff("17:00", &periods),
            "17:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("20:00", &periods),
            "20:00 should be low"
        );
        assert!(
            is_time_in_low_tariff("23:59", &periods),
            "23:59 should be low"
        );

        // Times that should be in HIGH tariff
        assert!(
            !is_time_in_low_tariff("06:00", &periods),
            "06:00 should be high"
        );
        assert!(
            !is_time_in_low_tariff("06:30", &periods),
            "06:30 should be high"
        );
        assert!(
            !is_time_in_low_tariff("09:00", &periods),
            "09:00 should be high"
        );
        assert!(
            !is_time_in_low_tariff("09:45", &periods),
            "09:45 should be high"
        );
        assert!(
            !is_time_in_low_tariff("13:00", &periods),
            "13:00 should be high"
        );
        assert!(
            !is_time_in_low_tariff("13:30", &periods),
            "13:30 should be high"
        );
        assert!(
            !is_time_in_low_tariff("16:00", &periods),
            "16:00 should be high"
        );
        assert!(
            !is_time_in_low_tariff("16:45", &periods),
            "16:45 should be high"
        );
    }

    #[test]
    fn test_block_in_low_tariff_uses_local_time() {
        use chrono::TimeZone;

        let periods = vec![
            ("14:00".to_owned(), "16:00".to_owned()),
            ("17:00".to_owned(), "24:00".to_owned()),
        ];
        let prague: Tz = "Europe/Prague".parse().unwrap();
        // 13:00Z is 15:00 CEST (low); 14:00Z is 16:00 CEST (high).
        let at = |h| Utc.with_ymd_and_hms(2026, 10, 14, h, 0, 0).unwrap();
        assert!(block_in_low_tariff(at(13), Some(prague), &periods));
        assert!(!block_in_low_tariff(at(14), Some(prague), &periods));
        assert!(block_in_low_tariff(at(16), Some(prague), &periods));
    }

    fn pv(ts: &str, power_w: f32) -> fluxion_core::web_bridge::PvGenerationHistoryPoint {
        fluxion_core::web_bridge::PvGenerationHistoryPoint {
            timestamp: ts.parse().unwrap(),
            power_w,
        }
    }

    #[test]
    fn pv_history_is_bucketed_into_blocks_by_utc_instant() {
        let starts: Vec<DateTime<Utc>> = [
            "2026-10-16T10:00:00Z",
            "2026-10-16T10:15:00Z",
            "2026-10-16T10:30:00Z",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        // Samples at arbitrary minutes, as the PV history records them
        let history = [
            pv("2026-10-16T09:59:00Z", 9000.0), // before the first block: dropped
            pv("2026-10-16T10:02:17Z", 1000.0),
            pv("2026-10-16T10:11:40Z", 3000.0),
            pv("2026-10-16T10:37:05Z", 1500.0),
            pv("2026-10-16T10:45:00Z", 9000.0), // after the last block's 15 min: dropped
        ];

        let kw = bucket_pv_history_kw(&starts, &history);

        assert_eq!(kw, vec![Some(2.0), None, Some(1.5)]);
    }

    #[test]
    fn pv_history_bucketing_handles_empty_inputs() {
        assert!(bucket_pv_history_kw(&[], &[pv("2026-10-16T10:00:00Z", 1.0)]).is_empty());
        let starts: Vec<DateTime<Utc>> = vec!["2026-10-16T10:00:00Z".parse().unwrap()];
        assert_eq!(bucket_pv_history_kw(&starts, &[]), vec![None]);
    }

    #[test]
    fn test_is_time_in_low_tariff_empty_periods() {
        let periods: Vec<(String, String)> = vec![];
        assert!(!is_time_in_low_tariff("12:00", &periods));
    }

    /// Brief H37: the header says "mode control off" when FluxION only curtails export
    #[test]
    fn dashboard_header_marks_mode_control_off() {
        for (enabled, shown) in [(false, true), (true, false)] {
            let mut response =
                response_with_timezone(Some("Europe/Prague"), TimezoneSource::Configured);
            response.mode_control_enabled = enabled;
            let i18n = Arc::new(I18n::new(fluxion_i18n::Language::English).unwrap());
            let html = DashboardTemplate::from_query_response(response, i18n, String::new(), None)
                .render()
                .unwrap();
            assert_eq!(
                html.contains("mode control off"),
                shown,
                "enabled={enabled}"
            );
        }
    }

    #[test]
    fn dashboard_shows_the_battery_cost_of_charge_once() {
        let inverter = |id: &str| -> InverterData {
            serde_json::from_value(serde_json::json!({
                "id": id, "topology": "independent", "mode": "SelfUse", "mode_reason": "",
                "mode_synced": true, "battery_soc": 50.0, "battery_power_w": 0.0,
                "battery_voltage_v": 0.0, "battery_current_a": 0.0, "battery_temperature_c": 0.0,
                "grid_power_w": 0.0, "grid_voltage_v": 0.0, "grid_frequency_hz": 50.0,
                "pv_power_w": 0.0, "pv1_power_w": 0.0, "pv2_power_w": 0.0,
                "daily_energy_kwh": 0.0, "total_energy_kwh": 0.0, "online": true,
                "run_mode": "Normal", "error_code": 0, "inverter_temperature_c": 0.0
            }))
            .unwrap()
        };
        let mut response =
            response_with_timezone(Some("Europe/Prague"), TimezoneSource::Configured);
        response.inverters = vec![inverter("master"), inverter("slave")];
        response.battery_cost_basis = Some(fluxion_core::web_bridge::BatteryCostBasisInfo {
            usable_kwh: 4.0,
            cost_czk_per_kwh: Some(1.854),
            solar_pct: Some(60.0),
            grid_pct: Some(40.0),
            updated_at: None,
        });
        let i18n = Arc::new(I18n::new(fluxion_i18n::Language::English).unwrap());
        let html = DashboardTemplate::from_query_response(response, i18n, String::new(), None)
            .render()
            .unwrap();
        let line = "4.0 kWh usable at 1.85 CZK/kWh (solar 60 % / grid 40 %)";
        assert_eq!(html.matches(line).count(), 1, "{line} once in the page");
    }

    fn response_with_timezone(timezone: Option<&str>, source: TimezoneSource) -> WebQueryResponse {
        WebQueryResponse {
            timestamp: Utc::now(),
            debug_mode: false,
            mode_control_enabled: true,
            inverters: vec![],
            schedule: None,
            prices: None,
            health: SystemHealthData {
                inverter_source: true,
                price_source: true,
                last_update: Utc::now(),
                errors: vec![],
            },
            timezone: timezone.map(str::to_owned),
            timezone_source: source,
            battery_soc_history: None,
            battery_soc_prediction: None,
            pv_generation_history: None,
            consumption_stats: None,
            hdo_schedule: None,
            pricing_fees: None,
            solar_forecast: None,
            battery_cost_basis: None,
            price_feed: None,
        }
    }

    #[test]
    fn display_timezone_reports_source() {
        let (tz, source) = display_timezone(&response_with_timezone(
            Some("Europe/Prague"),
            TimezoneSource::HomeAssistant,
        ));
        assert_eq!(tz, Some(chrono_tz::Europe::Prague));
        assert_eq!(source, TimezoneSource::HomeAssistant);

        let (tz, source) = display_timezone(&response_with_timezone(
            Some("Europe/Prague"),
            TimezoneSource::Configured,
        ));
        assert_eq!(tz, Some(chrono_tz::Europe::Prague));
        assert_eq!(source, TimezoneSource::Configured);

        let (tz, source) = display_timezone(&response_with_timezone(None, TimezoneSource::Unknown));
        assert_eq!(tz, None);
        assert_eq!(source, TimezoneSource::Unknown);

        // An unparseable name is treated as unknown (UTC)
        let (tz, source) = display_timezone(&response_with_timezone(
            Some("Mars/Olympus"),
            TimezoneSource::Configured,
        ));
        assert_eq!(tz, None);
        assert_eq!(source, TimezoneSource::Unknown);
    }

    #[test]
    fn chart_labels_use_per_block_offset_across_dst_change() {
        // Europe/Prague leaves CEST for CET on 2026-10-25 at 03:00 local (01:00Z)
        let block_starts = [
            "2026-10-24T22:00:00Z",
            "2026-10-24T23:00:00Z",
            "2026-10-25T00:00:00Z",
            "2026-10-25T01:00:00Z",
            "2026-10-25T02:00:00Z",
            "2026-10-25T03:00:00Z",
        ];
        let labels: Vec<String> = block_starts
            .iter()
            .map(|s| {
                let ts: DateTime<Utc> = s.parse().unwrap();
                format_in_display_tz(ts, Some(chrono_tz::Europe::Prague), "%H:%M")
            })
            .collect();
        assert_eq!(
            labels,
            ["00:00", "01:00", "02:00", "02:00", "03:00", "04:00"]
        );
    }

    #[test]
    fn display_mode_label_names_hold_and_feedin_blocks() {
        let mut block = fluxion_core::web_bridge::PriceBlockData {
            timestamp: "2026-10-17T10:00:00Z".parse().unwrap(),
            price: 2.0,
            effective_buy_price: None,
            block_type: "no-charge-discharge".to_owned(),
            target_soc: None,
            strategy: None,
            expected_profit: None,
            reason: None,
            decision_uid: None,
            debug_info: None,
            is_historical: false,
            solar_forecast_kw: None,
            solar_forecast_raw_kw: None,
            backup_min_soc_pct: None,
            price_source: None,
        };
        assert_eq!(display_mode_label(&block), "Hold");

        block.block_type = "feedin-priority".to_owned();
        assert_eq!(display_mode_label(&block), "Feed-in Priority");
    }
}
