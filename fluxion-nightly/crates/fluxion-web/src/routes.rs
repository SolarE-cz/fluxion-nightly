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
use chrono::Timelike;
use chrono_tz::Tz;
use fluxion_core::{InverterData, ScheduleData, SystemHealthData, WebQueryResponse};
use fluxion_i18n::I18n;
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
}

impl LiveDataTemplate {
    pub fn t(&self, key: &str) -> String {
        self.i18n.get(key).unwrap_or_else(|_| key.to_owned())
    }
}

/// Dashboard template
#[derive(Template)]
#[template(path = "index.html", escape = "none")]
pub struct DashboardTemplate {
    pub debug_mode: bool,
    pub inverters: Vec<InverterData>,
    pub schedule: Option<ScheduleData>,
    pub prices: Option<PriceDataWithChart>,
    pub chart_data_json: String,
    pub health: SystemHealthData,
    pub i18n: Arc<I18n>,
    #[expect(dead_code, reason = "May be used in future template updates")]
    pub timezone: Option<String>,
    pub last_update_formatted: String,
    pub next_change_formatted: Option<String>,
    /// Ingress path prefix for HA Ingress support (e.g., "/hassio/ingress/641a79a3_fluxion")
    /// Empty string when running standalone
    pub ingress_path: String,
    /// Aggregated consumption statistics (EMA, imports)
    pub consumption_stats: Option<fluxion_core::web_bridge::ConsumptionStats>,
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
    ) -> Self {
        let timezone = response.timezone.clone();

        // Format last update time in the correct timezone
        let last_update_formatted = if let Some(tz_name) = &timezone {
            if let Ok(tz) = tz_name.parse::<Tz>() {
                response
                    .health
                    .last_update
                    .with_timezone(&tz)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            } else {
                response
                    .health
                    .last_update
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            }
        } else {
            response
                .health
                .last_update
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };

        // Format next change time if available
        let next_change_formatted = if let Some(ref schedule) = response.schedule {
            schedule.next_change.map(|next| {
                if let Some(tz_name) = &timezone
                    && let Ok(tz) = tz_name.parse::<Tz>()
                {
                    return next.with_timezone(&tz).format("%H:%M:%S").to_string();
                }
                next.format("%H:%M:%S").to_string()
            })
        } else {
            None
        };

        // Prepare chart data for Chart.js from price data if available
        let prices = response.prices.map(|price_data| {
            let mut labels = Vec::new();
            let mut prices = Vec::new();
            let mut modes = Vec::new();
            let mut target_socs = Vec::new();
            let mut strategies = Vec::new();
            let mut profits = Vec::new();
            let mut debug_info_vec = Vec::new();
            let mut is_historical_vec = Vec::new();

            // Price breakdown for stacked bars
            let mut spot_prices = Vec::new();
            let mut grid_fees = Vec::new();
            let mut tariff_types = Vec::new();
            let mut buy_fees = Vec::new();
            let mut effective_prices = Vec::new();

            // Get HDO and pricing fee info from response
            let hdo_schedule = response.hdo_schedule.as_ref();
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

            let current_time_label = if let Some(tz_name) = &timezone {
                if let Ok(tz) = tz_name.parse::<Tz>() {
                    Some(
                        rounded_time
                            .with_timezone(&tz)
                            .format("%m-%d %H:%M")
                            .to_string(),
                    )
                } else {
                    Some(rounded_time.format("%m-%d %H:%M").to_string())
                }
            } else {
                Some(rounded_time.format("%m-%d %H:%M").to_string())
            };

            for block in &price_data.blocks {
                // Format time label in the correct timezone
                let label = if let Some(tz_name) = &timezone {
                    if let Ok(tz) = tz_name.parse::<Tz>() {
                        block
                            .timestamp
                            .with_timezone(&tz)
                            .format("%m-%d %H:%M")
                            .to_string()
                    } else {
                        block.timestamp.format("%m-%d %H:%M").to_string()
                    }
                } else {
                    block.timestamp.format("%m-%d %H:%M").to_string()
                };

                labels.push(label);
                prices.push(block.price);
                target_socs.push(block.target_soc);
                strategies.push(block.strategy.clone());
                profits.push(block.expected_profit);
                debug_info_vec.push(block.debug_info.clone());
                is_historical_vec.push(block.is_historical); // Track if block is past (regenerated schedule)

                // Map mode for display
                let mode = match block.block_type.as_str() {
                    "charge" => "Force Charge",
                    "discharge" => "Force Discharge",
                    "backup" => "Back Up Mode",
                    _ => "Self-Use",
                };
                modes.push(mode.to_owned());

                // Calculate price breakdown for stacked bars
                let spot_price = block.price;
                spot_prices.push(spot_price);

                // Determine if block is in low tariff period based on HDO schedule
                let block_time_str = block.timestamp.format("%H:%M").to_string();
                let (grid_fee, tariff_type) = if let Some(hdo) = hdo_schedule {
                    // Log HDO info for first block only
                    if labels.len() == 1 {
                        tracing::debug!(
                            "📊 HDO schedule has {} low tariff periods: {:?}",
                            hdo.low_tariff_periods.len(),
                            hdo.low_tariff_periods
                        );
                    }
                    let is_low = is_time_in_low_tariff(&block_time_str, &hdo.low_tariff_periods);
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

                // Calculate effective price
                let effective_price = spot_price + grid_fee + buy_fee;
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
                            let label = if let Some(tz_name) = &timezone {
                                if let Ok(tz) = tz_name.parse::<Tz>() {
                                    point
                                        .timestamp
                                        .with_timezone(&tz)
                                        .format("%m-%d %H:%M")
                                        .to_string()
                                } else {
                                    point.timestamp.format("%m-%d %H:%M").to_string()
                                }
                            } else {
                                point.timestamp.format("%m-%d %H:%M").to_string()
                            };
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
                            let label = if let Some(tz_name) = &timezone {
                                if let Ok(tz) = tz_name.parse::<Tz>() {
                                    point
                                        .timestamp
                                        .with_timezone(&tz)
                                        .format("%m-%d %H:%M")
                                        .to_string()
                                } else {
                                    point.timestamp.format("%m-%d %H:%M").to_string()
                                }
                            } else {
                                point.timestamp.format("%m-%d %H:%M").to_string()
                            };
                            SocPredictionPoint {
                                label,
                                soc: point.soc,
                            }
                        })
                        .collect()
                },
            );

            let current_battery_soc = response.inverters.first().map(|inv| inv.battery_soc);

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
                            let label = if let Some(tz_name) = &timezone {
                                if let Ok(tz) = tz_name.parse::<Tz>() {
                                    point
                                        .timestamp
                                        .with_timezone(&tz)
                                        .format("%m-%d %H:%M")
                                        .to_string()
                                } else {
                                    point.timestamp.format("%m-%d %H:%M").to_string()
                                }
                            } else {
                                point.timestamp.format("%m-%d %H:%M").to_string()
                            };
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
                    // Price breakdown for stacked bars
                    spot_prices,
                    grid_fees,
                    tariff_types,
                    buy_fees,
                    effective_prices,
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
            inverters: response.inverters,
            schedule: response.schedule,
            prices,
            chart_data_json,
            health: response.health,
            i18n,
            timezone: response.timezone,
            last_update_formatted,
            next_change_formatted,
            ingress_path,
            consumption_stats: response.consumption_stats,
        }
    }
}

/// Check if a time (HH:MM format) falls within any of the low tariff periods
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
    fn test_is_time_in_low_tariff_empty_periods() {
        let periods: Vec<(String, String)> = vec![];
        assert!(!is_time_in_low_tariff("12:00", &periods));
    }
}
