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

mod backtest;
mod config_api;
mod plugin_api;
mod routes;
mod validation;

pub use backtest::BacktestState;
pub use config_api::ConfigApiState;
pub use plugin_api::PluginApiState;
use routes::{DashboardTemplate, LiveDataTemplate};

use askama::Template;
use axum::{
    Json, Router,
    extract::State,
    response::{
        Html, IntoResponse,
        sse::{Event, Sse},
    },
    routing::get,
};
use fluxion_core::{ConfigUpdateSender, WebQueryResponse, WebQuerySender};
use fluxion_i18n::I18n;
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::{StreamExt, wrappers::IntervalStream};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, trace};

/// Application state for web handlers
#[derive(Clone, Debug)]
pub struct AppState {
    pub query_sender: WebQuerySender,
    pub i18n: Arc<I18n>,
}

/// Extract ingress path from request headers
/// Returns the ingress path prefix (e.g., "/hassio/ingress/641a79a3_fluxion")
/// or empty string if not running under ingress
fn extract_ingress_path(headers: &axum::http::HeaderMap) -> String {
    let path = headers
        .get("X-Ingress-Path")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_default();

    if path.is_empty() {
        trace!("No X-Ingress-Path header found, running in standalone mode");
    } else {
        info!("Running under HA Ingress with path: {}", path);
    }

    path
}

/// Start the web server with message passing to ECS
///
/// # Arguments
/// * `query_sender` - Channel sender to query ECS World
/// * `i18n` - Internationalization support
/// * `port` - Port to listen on (8099 for HA Ingress)
/// * `config_json` - Current configuration as JSON
/// * `config_update_sender` - Optional channel for config updates
/// * `backtest_db_path` - Optional path to backtest database
/// * `plugin_api_state` - Optional plugin API state for plugin management
///
/// # HA Ingress Support
/// When running as HA addon, routes are accessible via:
/// - `http://homeassistant.local:8123/api/hassio_ingress/{addon_slug}/`
/// - Panel button in HA sidebar
///
/// The server automatically works for both:
/// - Standalone: `http://localhost:8099/`
/// - HA Ingress: `http://ha:8123/api/hassio_ingress/fluxion/`
///
/// # Errors
/// Returns error if server fails to bind or serve
pub async fn start_web_server(
    query_sender: WebQuerySender,
    i18n: Arc<I18n>,
    port: u16,
    config_json: serde_json::Value,
    config_update_sender: Option<ConfigUpdateSender>,
    backtest_db_path: Option<std::path::PathBuf>,
    plugin_api_state: Option<PluginApiState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let app_state = AppState {
        query_sender,
        i18n: i18n.clone(),
    };
    let config_state =
        config_api::ConfigApiState::new(config_json, "/data/config.json", config_update_sender);

    let mut app = Router::new()
        .route("/", get(index_handler))
        .route("/stream", get(stream_handler))
        .route("/chart-data", get(chart_data_handler))
        .route("/export", get(export_handler))
        .route("/health", get(health_handler))
        // Config API routes
        .route(
            "/api/config",
            get(config_api::get_config_handler).with_state(config_state.clone()),
        )
        .route(
            "/api/config/validate",
            axum::routing::post(config_api::validate_config_handler)
                .with_state(config_state.clone()),
        )
        .route(
            "/api/config/update",
            axum::routing::post(config_api::update_config_handler).with_state(config_state.clone()),
        )
        .route(
            "/api/config/reset",
            axum::routing::post(config_api::reset_section_handler).with_state(config_state.clone()),
        )
        .route(
            "/api/config/export",
            get(config_api::export_config_handler).with_state(config_state),
        );

    // Add backtest routes if database path is provided
    if let Some(db_path) = backtest_db_path {
        info!("📊 Backtest feature enabled with database: {:?}", db_path);
        let backtest_state = backtest::BacktestState::new(db_path, i18n);

        app = app
            .route(
                "/backtest",
                get(backtest::backtest_page_handler).with_state(backtest_state.clone()),
            )
            .route(
                "/api/backtest/days",
                get(backtest::available_days_handler).with_state(backtest_state.clone()),
            )
            .route(
                "/api/backtest/day/{date}",
                get(backtest::day_data_handler).with_state(backtest_state.clone()),
            )
            .route(
                "/api/backtest/simulate",
                axum::routing::post(backtest::simulate_handler).with_state(backtest_state.clone()),
            )
            .route(
                "/api/backtest/compare",
                axum::routing::post(backtest::compare_handler).with_state(backtest_state),
            );
    }

    // Add plugin management API routes if state is provided
    if let Some(plugin_state) = plugin_api_state {
        info!("🔌 Plugin API enabled");
        app = app
            .route(
                "/api/plugins",
                get(plugin_api::list_plugins_handler).with_state(plugin_state.clone()),
            )
            .route(
                "/api/plugins/register",
                axum::routing::post(plugin_api::register_plugin_handler)
                    .with_state(plugin_state.clone()),
            )
            .route(
                "/api/plugins/{name}",
                axum::routing::delete(plugin_api::unregister_plugin_handler)
                    .with_state(plugin_state.clone()),
            )
            .route(
                "/api/plugins/{name}/priority",
                axum::routing::put(plugin_api::update_priority_handler)
                    .with_state(plugin_state.clone()),
            )
            .route(
                "/api/plugins/{name}/enabled",
                axum::routing::put(plugin_api::update_enabled_handler)
                    .with_state(plugin_state),
            );
    }

    let app = app
        .layer(CorsLayer::permissive()) // Allow HA Ingress
        .with_state(app_state);

    let addr = format!("0.0.0.0:{port}");
    info!("🌐 Starting web server on {addr}");
    info!("📱 Standalone: http://localhost:{}/", port);
    info!("🏠 HA Ingress: http://homeassistant:8123/api/hassio_ingress/fluxion/");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Main dashboard page handler
async fn index_handler(
    State(app_state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    debug!("Dashboard page requested");
    let ingress_path = extract_ingress_path(&headers);

    match app_state.query_sender.query_dashboard().await {
        Ok(response) => {
            let template = DashboardTemplate::from_query_response(
                response,
                app_state.i18n.clone(),
                ingress_path,
            );
            // Askama 0.14: use .render() and convert to axum Html response
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Template render error: {}", e);
                    Html(format!(
                        "<html><body><h1>Error</h1><p>Failed to render template: {e}</p></body></html>"
                    ))
                    .into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to query dashboard data: {}", e);
            Html(format!(
                "<html><body><h1>Error</h1><p>Failed to load dashboard: {e}</p></body></html>"
            ))
            .into_response()
        }
    }
}

/// SSE stream handler for live updates
/// Sends only live data HTML updates every second (no chart)
async fn stream_handler(
    State(app_state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    trace!("SSE stream connected");

    let interval = tokio::time::interval(Duration::from_secs(1));
    let stream = IntervalStream::new(interval).map(move |_| {
        let app_state = app_state.clone();
        async move {
            match app_state.query_sender.query_dashboard().await {
                Ok(response) => {
                    // SSE doesn't have access to headers, use empty ingress path
                    // (live data template doesn't use URLs anyway)
                    let dashboard = DashboardTemplate::from_query_response(
                        response,
                        app_state.i18n.clone(),
                        String::new(),
                    );

                    // Create live data template from dashboard (without chart)
                    let live_template = LiveDataTemplate {
                        debug_mode: dashboard.debug_mode,
                        inverters: dashboard.inverters,
                        schedule: dashboard.schedule,
                        prices: dashboard.prices,
                        health: dashboard.health,
                        i18n: dashboard.i18n,
                        last_update_formatted: dashboard.last_update_formatted,
                        next_change_formatted: dashboard.next_change_formatted,
                    };

                    let html = live_template.render().unwrap_or_else(|e| {
                        format!("<div class='error'>Template error: {e}</div>")
                    });
                    Ok::<_, Infallible>(Event::default().event("update").data(html))
                }
                Err(e) => {
                    let error_html = format!("<div class='error'>Query error: {e}</div>");
                    Ok::<_, Infallible>(Event::default().event("update").data(error_html))
                }
            }
        }
    });

    Sse::new(stream.then(|f| f))
}

/// Chart data JSON response
#[derive(Serialize)]
struct ChartDataJson {
    labels: Vec<String>,
    prices: Vec<f32>,
    modes: Vec<String>,
    strategies: Vec<Option<String>>,
    profits: Vec<Option<f32>>,
    current_time_label: Option<String>,
    current_battery_soc: Option<f32>,
}

/// Chart data endpoint - returns JSON for chart updates (once per minute)
async fn chart_data_handler(State(app_state): State<AppState>) -> impl IntoResponse {
    match app_state.query_sender.query_dashboard().await {
        Ok(response) => {
            let template = DashboardTemplate::from_query_response(
                response.clone(),
                app_state.i18n.clone(),
                String::new(), // JSON endpoint doesn't need ingress path
            );

            // Extract battery SOC from first inverter
            let current_battery_soc = response.inverters.first().map(|inv| inv.battery_soc);

            // Extract chart data from template
            if let Some(prices) = template.prices {
                let chart_json = ChartDataJson {
                    labels: prices.chart_data.labels,
                    prices: prices.chart_data.prices,
                    modes: prices.chart_data.modes,
                    strategies: prices.chart_data.strategies,
                    profits: prices.chart_data.profits,
                    current_time_label: prices.chart_data.current_time_label,
                    current_battery_soc,
                };
                Json(chart_json).into_response()
            } else {
                (axum::http::StatusCode::NO_CONTENT, "").into_response()
            }
        }
        Err(_) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "").into_response(),
    }
}

/// Export data endpoint - returns compact JSON for analysis
/// Optimized format with abbreviated field names, Unix timestamps, and encoded decision reasons
async fn export_handler(State(app_state): State<AppState>) -> impl IntoResponse {
    match app_state.query_sender.query_dashboard().await {
        Ok(response) => {
            // Format timestamp for filename
            let filename = format!(
                "fluxion_export_{}.json",
                response.timestamp.format("%Y%m%d_%H%M%S")
            );

            // Create compact JSON structure with space optimizations
            let export_data = create_compact_export(&response);

            let json_string = match serde_json::to_string_pretty(&export_data) {
                Ok(json) => json,
                Err(e) => {
                    error!("Failed to serialize compact export data: {}", e);
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "").into_response();
                }
            };

            // Set headers for file download
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            headers.insert(
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\"")
                    .parse()
                    .unwrap(),
            );

            (headers, json_string).into_response()
        }
        Err(e) => {
            error!("Failed to query dashboard data for export: {}", e);
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "").into_response()
        }
    }
}

/// Health check endpoint
async fn health_handler(State(app_state): State<AppState>) -> impl IntoResponse {
    match app_state.query_sender.query_health().await {
        Ok(health) => {
            let healthy = health.inverter_source && health.price_source;
            if healthy {
                (axum::http::StatusCode::OK, "OK")
            } else {
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, "DEGRADED")
            }
        }
        Err(_) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "ERROR"),
    }
}

/// Create compact export data with space optimizations
#[expect(clippy::too_many_lines)]
fn create_compact_export(response: &WebQueryResponse) -> serde_json::Value {
    serde_json::json!({
        // Metadata with abbreviated keys
        "meta": {
            "ts": response.timestamp.timestamp(),
            "tz": response.timezone,
            "dbg": response.debug_mode,
            "ver": "2.0",
            "desc": "FluxION compact export"
        },

        // System health (abbreviated)
        "health": {
            "inv": response.health.inverter_source,
            "price": response.health.price_source,
            "upd": response.health.last_update.timestamp(),
            "errs": response.health.errors
        },

        // Compact inverter data
        "inv": response.inverters.iter().map(|inv| {
            serde_json::json!({
                "id": inv.id,
                "topo": inv.topology,
                "mode": inv.mode,
                "reason": inv.mode_reason,
                "online": inv.online,
                "err": inv.error_code,

                // Battery (rounded values)
                "soc": round_2_decimals(inv.battery_soc),
                "bat_pwr": round_nearest(inv.battery_power_w),
                "bat_v": round_1_decimal(inv.battery_voltage_v),
                "bat_a": round_1_decimal(inv.battery_current_a),
                "bat_temp": round_nearest(inv.battery_temperature_c),

                // Grid (rounded values)
                "grid_pwr": round_nearest(inv.grid_power_w),
                "grid_v": round_1_decimal(inv.grid_voltage_v),
                "grid_hz": round_2_decimals(inv.grid_frequency_hz),

                // Solar (rounded values)
                "pv_pwr": round_nearest(inv.pv_power_w),
                "pv1_pwr": round_nearest(inv.pv1_power_w),
                "pv2_pwr": round_nearest(inv.pv2_power_w),
                "daily_kwh": round_1_decimal(inv.daily_energy_kwh),
                "total_kwh": round_nearest(inv.total_energy_kwh),

                // Optional fields (only if present)
                "house_w": inv.house_load_w.map(round_nearest),
                "grid_in": inv.grid_import_w.map(round_nearest),
                "grid_out": inv.grid_export_w.map(round_nearest),
                "bat_cap": inv.battery_capacity_kwh.map(round_1_decimal),
            })
        }).collect::<Vec<_>>(),

        // Operation schedule (keep existing field names - already compact enough)
        "sched": response.schedule.as_ref().map(|sched| {
            serde_json::json!({
                "mode": sched.current_mode,
                "reason": sched.current_reason,
                "strategy": sched.current_strategy,
                "profit": sched.expected_profit.map(round_2_decimals),
                "next": sched.next_change.map(|dt| dt.timestamp()),
                "blocks": sched.blocks_today,
                "soc_min": round_1_decimal(sched.target_soc_min),
                "soc_max": round_1_decimal(sched.target_soc_max),
                "total_profit": sched.total_expected_profit.map(round_2_decimals),
            })
        }),

        // Compact price data
        "prices": response.prices.as_ref().map(|prices| {
            serde_json::json!({
                "cur": round_2_decimals(prices.current_price),
                "min": round_2_decimals(prices.min_price),
                "max": round_2_decimals(prices.max_price),
                "avg": round_2_decimals(prices.avg_price),

                // Compact price blocks
                "blocks": prices.blocks.iter().map(|block| {
                    serde_json::json!({
                        "ts": block.timestamp.timestamp(),
                        "p": round_2_decimals(block.price),
                        "op": abbreviate_operation(&block.block_type),
                        "soc": block.target_soc.map(round_1_decimal),
                        "st": block.strategy.as_ref().map(|s| abbreviate_strategy(s)),
                        "pr": block.expected_profit.map(round_2_decimals),
                        "r": block.reason.as_ref().map(|r| abbreviate_reason(r)),
                        "uid": block.decision_uid.as_ref(),
                        "h": block.is_historical
                    })
                }).collect::<Vec<_>>(),

                // Today stats
                "today_min": round_2_decimals(prices.today_min_price),
                "today_max": round_2_decimals(prices.today_max_price),
                "today_avg": round_2_decimals(prices.today_avg_price),
                "today_med": round_2_decimals(prices.today_median_price),

                // Tomorrow stats (if available)
                "tom_min": prices.tomorrow_min_price.map(round_2_decimals),
                "tom_max": prices.tomorrow_max_price.map(round_2_decimals),
                "tom_avg": prices.tomorrow_avg_price.map(round_2_decimals),
                "tom_med": prices.tomorrow_median_price.map(round_2_decimals),
            })
        }),

        // Compact battery history
        "bat_hist": response.battery_soc_history.as_ref().map(|hist| {
            hist.iter().map(|p| serde_json::json!({
                "ts": p.timestamp.timestamp(),
                "soc": round_1_decimal(p.soc)
            })).collect::<Vec<_>>()
        }),

        // Compact battery predictions
        "bat_pred": response.battery_soc_prediction.as_ref().map(|pred| {
            pred.iter().map(|p| serde_json::json!({
                "ts": p.timestamp.timestamp(),
                "soc": round_1_decimal(p.soc)
            })).collect::<Vec<_>>()
        }),

        // Compact PV history
        "pv_hist": response.pv_generation_history.as_ref().map(|hist| {
            hist.iter().map(|p| serde_json::json!({
                "ts": p.timestamp.timestamp(),
                "pwr": round_nearest(p.power_w)
            })).collect::<Vec<_>>()
        }),

        // Consumption stats
        "consumption": response.consumption_stats.as_ref().map(|stats| {
            serde_json::json!({
                "ema_kwh": stats.ema_kwh.map(round_2_decimals),
                "ema_days": stats.ema_days,
                "today_kwh": stats.today_import_kwh.map(round_2_decimals),
                "yesterday_kwh": stats.yesterday_import_kwh.map(round_2_decimals),
            })
        })
    })
}

// Helper functions for rounding
fn round_nearest(value: f32) -> f32 {
    value.round()
}

fn round_1_decimal(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

fn round_2_decimals(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

// Helper functions for abbreviation
fn abbreviate_operation(op: &str) -> &str {
    match op {
        "charge" => "c",
        "discharge" => "d",
        "self-use" => "s",
        _ => "u", // unknown
    }
}

fn abbreviate_strategy(strategy: &str) -> String {
    match strategy {
        "Winter-Adaptive" => "WA".to_owned(),
        "Self-Use" => "SU".to_owned(),
        "Time-Aware Charge" => "TAC".to_owned(),
        "Winter-Peak-Discharge" => "WPD".to_owned(),
        "Price-Arbitrage" => "PA".to_owned(),
        other => other.chars().filter(|c| c.is_uppercase()).take(3).collect(),
    }
}

fn abbreviate_reason(reason: &str) -> String {
    // For now, just truncate long reasons to save space
    // Later we could implement the full DecisionReason enum parsing
    if reason.len() > 50 {
        format!("{}...", reason.get(..50).unwrap_or_default())
    } else {
        reason.to_owned()
    }
}
