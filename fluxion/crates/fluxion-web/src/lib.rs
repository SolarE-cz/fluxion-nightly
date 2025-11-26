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

mod chart;
mod config_api;
mod config_ui;
mod routes;

pub use chart::generate_price_chart_svg;
pub use config_api::ConfigApiState;
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
use fluxion_core::{ConfigUpdateSender, WebQuerySender};
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
) -> Result<(), Box<dyn std::error::Error>> {
    let app_state = AppState { query_sender, i18n };
    let config_state =
        config_api::ConfigApiState::new(config_json, "/data/config.json", config_update_sender);

    let config_ui_handler = config_ui_handler(app_state.i18n.clone());

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/config", get(config_ui_handler))
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
            axum::routing::post(config_api::validate_config_handler),
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
        )
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
#[serde(rename_all = "camelCase")]
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

/// Export data endpoint - returns comprehensive JSON for analysis
/// Format optimized for Claude Sonnet 4.5 with clear hierarchy and descriptive fields
#[expect(
    clippy::too_many_lines,
    reason = "Comprehensive export structure requires building detailed JSON"
)]
async fn export_handler(State(app_state): State<AppState>) -> impl IntoResponse {
    match app_state.query_sender.query_dashboard().await {
        Ok(response) => {
            // Format timestamp for filename
            let filename = format!(
                "fluxion_export_{}.json",
                response.timestamp.format("%Y%m%d_%H%M%S")
            );

            // Create comprehensive export structure optimized for Claude analysis
            let export_data = serde_json::json!({
                "metadata": {
                    "export_timestamp": response.timestamp,
                    "timezone": response.timezone,
                    "debug_mode": response.debug_mode,
                    "export_format_version": "1.0",
                    "description": "FluxION solar battery management system data export for analysis"
                },
                "consumption_stats": response.consumption_stats.as_ref().map(|stats| {
                    serde_json::json!({
                        "ema_kwh": stats.ema_kwh,
                        "ema_days": stats.ema_days,
                        "today_import_kwh": stats.today_import_kwh,
                        "yesterday_import_kwh": stats.yesterday_import_kwh,
                    })
                }),
                "system_health": {
                    "status": {
                        "inverter_connection": response.health.inverter_source,
                        "price_data_source": response.health.price_source,
                        "last_update": response.health.last_update,
                    },
                    "errors": response.health.errors,
                },
                "inverters": response.inverters.iter().map(|inv| {
                    serde_json::json!({
                        "identification": {
                            "id": inv.id,
                            "topology": inv.topology,
                            "online": inv.online,
                        },
                        "current_operation": {
                            "mode": inv.mode,
                            "mode_reason": inv.mode_reason,
                            "run_mode": inv.run_mode,
                            "error_code": inv.error_code,
                        },
                        "battery": {
                            "state_of_charge_percent": inv.battery_soc,
                            "power_watts": inv.battery_power_w,
                            "voltage_volts": inv.battery_voltage_v,
                            "current_amperes": inv.battery_current_a,
                            "temperature_celsius": inv.battery_temperature_c,
                            "capacity_kwh": inv.battery_capacity_kwh,
                            "energy_input_today_kwh": inv.battery_input_energy_today_kwh,
                            "energy_output_today_kwh": inv.battery_output_energy_today_kwh,
                        },
                        "grid": {
                            "power_watts": inv.grid_power_w,
                            "voltage_volts": inv.grid_voltage_v,
                            "frequency_hz": inv.grid_frequency_hz,
                            "import_power_watts": inv.grid_import_w,
                            "export_power_watts": inv.grid_export_w,
                            "import_today_kwh": inv.grid_import_today_kwh,
                            "export_today_kwh": inv.grid_export_today_kwh,
                        },
                        "solar_generation": {
                            "total_power_watts": inv.pv_power_w,
                            "pv1_power_watts": inv.pv1_power_w,
                            "pv2_power_watts": inv.pv2_power_w,
                            "energy_today_kwh": inv.daily_energy_kwh,
                            "energy_total_kwh": inv.total_energy_kwh,
                            "today_solar_energy_kwh": inv.today_solar_energy_kwh,
                            "total_solar_energy_kwh": inv.total_solar_energy_kwh,
                        },
                        "house_load": {
                            "load_watts": inv.house_load_w,
                        },
                        "inverter_internals": {
                            "temperature_celsius": inv.inverter_temperature_c,
                            "voltage_volts": inv.inverter_voltage_v,
                            "current_amperes": inv.inverter_current_a,
                            "power_watts": inv.inverter_power_w,
                            "frequency_hz": inv.inverter_frequency_hz,
                        },
                    })
                }).collect::<Vec<_>>(),
                "operation_schedule": response.schedule.as_ref().map(|sched| {
                    serde_json::json!({
                        "current_block": {
                            "mode": sched.current_mode,
                            "reason": sched.current_reason,
                            "strategy": sched.current_strategy,
                            "expected_profit_czk": sched.expected_profit,
                        },
                        "schedule_summary": {
                            "next_mode_change": sched.next_change,
                            "total_blocks_today": sched.blocks_today,
                            "target_soc_max_percent": sched.target_soc_max,
                            "target_soc_min_percent": sched.target_soc_min,
                            "total_expected_profit_czk": sched.total_expected_profit,
                        },
                    })
                }),
                "electricity_prices": response.prices.as_ref().map(|prices| {
                    serde_json::json!({
                        "current_price_czk_per_kwh": prices.current_price,
                        "price_statistics": {
                            "minimum_czk_per_kwh": prices.min_price,
                            "maximum_czk_per_kwh": prices.max_price,
                            "average_czk_per_kwh": prices.avg_price,
                        },
                        "price_blocks": prices.blocks.iter().map(|block| {
                            serde_json::json!({
                                "timestamp": block.timestamp,
                                "price_czk_per_kwh": block.price,
                                "operation_type": block.block_type,
                                "target_soc_percent": block.target_soc,
                                "strategy_name": block.strategy,
                                "expected_profit_czk": block.expected_profit,
                                "decision_reason": block.reason,
                            })
                        }).collect::<Vec<_>>(),
                    })
                }),
                "battery_history": {
                    "state_of_charge": response.battery_soc_history.as_ref().map(|history| {
                        history.iter().map(|point| {
                            serde_json::json!({
                                "timestamp": point.timestamp,
                                "soc_percent": point.soc,
                            })
                        }).collect::<Vec<_>>()
                    }),
                },
                "battery_predictions": {
                    "state_of_charge": response.battery_soc_prediction.as_ref().map(|prediction| {
                        prediction.iter().map(|point| {
                            serde_json::json!({
                                "timestamp": point.timestamp,
                                "predicted_soc_percent": point.soc,
                            })
                        }).collect::<Vec<_>>()
                    }),
                },
                "pv_generation_history": response.pv_generation_history.as_ref().map(|pv_history| {
                    pv_history.iter().map(|point| {
                        serde_json::json!({
                            "timestamp": point.timestamp,
                            "power_watts": point.power_w,
                        })
                    }).collect::<Vec<_>>()
                }),
            });

            let json_string = match serde_json::to_string_pretty(&export_data) {
                Ok(json) => json,
                Err(e) => {
                    error!("Failed to serialize export data: {}", e);
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

/// Config UI page handler
fn config_ui_handler(i18n: Arc<I18n>) -> impl Fn() -> std::future::Ready<Html<String>> + Clone {
    move || {
        let template = config_ui::ConfigTemplate { i18n: i18n.clone() };
        let html = template
            .render()
            .unwrap_or_else(|e| format!("<h1>Template error</h1><p>{e}</p>"));
        std::future::ready(Html(html))
    }
}
