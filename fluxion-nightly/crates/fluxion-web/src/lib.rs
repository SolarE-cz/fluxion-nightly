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
pub mod remote_access;
mod routes;
mod simulator;
mod user_control_api;
mod validation;

pub use backtest::BacktestState;
pub use config_api::ConfigApiState;
pub use plugin_api::PluginApiState;
pub use remote_access::{
    MobileApiState, RemoteAccessApiState, mobile_api_routes, remote_access_routes,
};
use routes::{DashboardTemplate, LiveDataTemplate, display_timezone};
pub use simulator::SimulatorState;
pub use user_control_api::{UserControlApiState, UserControlUpdateSender};

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
use chrono::{Local, NaiveTime, Offset};
use fluxion_core::{ConfigUpdateSender, WebQueryResponse, WebQuerySender};
use fluxion_i18n::I18n;
use fluxion_types::UserControlState;
use parking_lot::RwLock;
use serde::Serialize;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::{StreamExt, wrappers::IntervalStream};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, trace};

/// Application state for web handlers
#[derive(Clone)]
pub struct AppState {
    pub query_sender: WebQuerySender,
    pub i18n: Arc<I18n>,
    /// User control state for dashboard rendering
    pub user_control_state: Option<Arc<RwLock<UserControlState>>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("query_sender", &"<WebQuerySender>")
            .field("i18n", &self.i18n)
            .field("user_control_state", &self.user_control_state.is_some())
            .finish()
    }
}

/// Configuration for scheduled daily data export
/// Exports full day's data at a specified time for debugging purposes
#[derive(Clone, Debug)]
pub struct ScheduledExportConfig {
    /// Directory to save export files (e.g., "/data/exports")
    pub export_dir: PathBuf,
    /// Time of day to run the export (default: 23:55)
    pub export_time: NaiveTime,
}

impl Default for ScheduledExportConfig {
    fn default() -> Self {
        Self {
            // Use relative path for portability (works in both dev and HA addon)
            export_dir: PathBuf::from("./data/exports"),
            // 23:55 - 5 minutes before midnight to capture full day's data
            export_time: NaiveTime::from_hms_opt(23, 55, 0).expect("valid time"),
        }
    }
}

/// Spawn background task for scheduled daily data export
/// Runs at the configured time each day and saves export data to a file
#[expect(clippy::integer_division)]
fn spawn_scheduled_export_task(query_sender: WebQuerySender, config: ScheduledExportConfig) {
    tokio::spawn(async move {
        info!(
            "📅 Scheduled export enabled: will export at {} to {:?}",
            config.export_time.format("%H:%M"),
            config.export_dir
        );

        // Create export directory if it doesn't exist
        if let Err(e) = tokio::fs::create_dir_all(&config.export_dir).await {
            error!(
                "❌ Failed to create export directory {:?}: {}",
                config.export_dir, e
            );
            return;
        }

        loop {
            // Calculate time until next scheduled export
            let now = Local::now();
            let today_export_time = now
                .date_naive()
                .and_time(config.export_time)
                .and_local_timezone(Local)
                .single();

            let next_export = if let Some(today_time) = today_export_time {
                if now.time() < config.export_time {
                    // Export time is still ahead today
                    today_time
                } else {
                    // Export time has passed today, schedule for tomorrow
                    let tomorrow = now.date_naive() + chrono::Duration::days(1);
                    tomorrow
                        .and_time(config.export_time)
                        .and_local_timezone(Local)
                        .single()
                        .expect("valid tomorrow time")
                }
            } else {
                // Fallback: try tomorrow if today's time conversion fails
                let tomorrow = now.date_naive() + chrono::Duration::days(1);
                tomorrow
                    .and_time(config.export_time)
                    .and_local_timezone(Local)
                    .single()
                    .expect("valid tomorrow time")
            };

            let duration_until_export = (next_export - now)
                .to_std()
                .unwrap_or(Duration::from_secs(60));

            info!(
                "📅 Next scheduled export at {} (in {} hours {} minutes)",
                next_export.format("%Y-%m-%d %H:%M:%S"),
                duration_until_export.as_secs() / 3600,
                (duration_until_export.as_secs() % 3600) / 60
            );

            // Sleep until export time
            tokio::time::sleep(duration_until_export).await;

            // Perform the export
            info!("📦 Starting scheduled daily export...");

            match query_sender.query_dashboard().await {
                Ok(response) => {
                    // Generate filename with date
                    let filename = format!(
                        "fluxion_daily_{}.json",
                        Local::now().format("%Y%m%d_%H%M%S")
                    );
                    let filepath = config.export_dir.join(&filename);

                    // Create compact export data (reusing existing function)
                    let export_data = create_compact_export(&response);

                    match serde_json::to_string_pretty(&export_data) {
                        Ok(json_string) => match tokio::fs::write(&filepath, &json_string).await {
                            Ok(()) => {
                                info!(
                                    "✅ Daily export saved: {} ({} bytes)",
                                    filepath.display(),
                                    json_string.len()
                                );
                            }
                            Err(e) => {
                                error!("❌ Failed to write export file: {}", e);
                            }
                        },
                        Err(e) => {
                            error!("❌ Failed to serialize export data: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("❌ Failed to query dashboard for scheduled export: {}", e);
                }
            }

            // Small delay to avoid potential double-execution edge cases
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
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
/// * `backtest_state` - Backtest on Home Assistant history; `None` serves the routes with a
///   "not available" error
/// * `plugin_api_state` - Optional plugin API state for plugin management
/// * `scheduled_export_config` - Optional config for daily scheduled exports (for debugging)
/// * `user_control_api_state` - Optional user control API state for user override features
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
#[expect(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines)]
pub async fn start_web_server(
    query_sender: WebQuerySender,
    i18n: Arc<I18n>,
    port: u16,
    config_json: serde_json::Value,
    config_update_sender: Option<ConfigUpdateSender>,
    backtest_state: Option<BacktestState>,
    plugin_api_state: Option<PluginApiState>,
    scheduled_export_config: Option<ScheduledExportConfig>,
    user_control_api_state: Option<UserControlApiState>,
    remote_access_state: Option<RemoteAccessApiState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Spawn scheduled export task if configured
    if let Some(export_config) = scheduled_export_config {
        spawn_scheduled_export_task(query_sender.clone(), export_config);
    }

    // Extract user control state from API state for dashboard rendering
    let user_control_state = user_control_api_state
        .as_ref()
        .map(|uc| Arc::clone(&uc.state));

    // Pre-clone values needed for mobile API routes (before they're moved)
    let mobile_query_sender = query_sender.clone();
    let mobile_i18n = i18n.clone();
    let mobile_uc_api = user_control_api_state.clone();

    let app_state = AppState {
        query_sender,
        i18n: i18n.clone(),
        user_control_state,
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

    // Backtest routes are always registered; without a state they answer "not available"
    let backtest_state = backtest_state.unwrap_or_else(|| {
        BacktestState::unavailable(i18n.clone(), "the backtest needs Home Assistant history")
    });
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
            get(backtest::day_handler).with_state(backtest_state),
        );

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
                axum::routing::put(plugin_api::update_enabled_handler).with_state(plugin_state),
            );
    }

    // Add strategy simulator routes
    {
        info!("🧪 Strategy Simulator API enabled");
        let simulator_state = simulator::SimulatorState::new();
        app = app
            // Simulator page
            .route("/simulator", get(simulator::simulator_page_handler))
            // API endpoints
            .route(
                "/api/simulator/presets",
                get(simulator::presets_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/create",
                axum::routing::post(simulator::create_simulation_handler)
                    .with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}",
                get(simulator::get_simulation_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/step",
                axum::routing::post(simulator::step_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/run",
                axum::routing::post(simulator::run_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/results",
                get(simulator::results_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/override/soc",
                axum::routing::put(simulator::override_soc_handler)
                    .with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/override/load",
                axum::routing::put(simulator::override_load_handler)
                    .with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/override/price",
                axum::routing::put(simulator::override_price_handler)
                    .with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}/reset",
                axum::routing::post(simulator::reset_handler).with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/{id}",
                axum::routing::delete(simulator::delete_handler)
                    .with_state(simulator_state.clone()),
            )
            .route(
                "/api/simulator/blocks/{id}/{block}",
                get(simulator::block_detail_handler).with_state(simulator_state),
            );
    }

    // Add user control API routes if state is provided
    if let Some(uc_state) = user_control_api_state {
        info!("🎛️ User Control API enabled");
        app = app
            .route(
                "/api/user-control",
                get(user_control_api::get_user_control).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/enabled",
                axum::routing::put(user_control_api::set_enabled).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/restrictions",
                axum::routing::put(user_control_api::set_restrictions).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/default-mode",
                axum::routing::put(user_control_api::set_default_mode).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/slots",
                axum::routing::post(user_control_api::create_slot).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/slots/{id}",
                axum::routing::put(user_control_api::update_slot).with_state(uc_state.clone()),
            )
            .route(
                "/api/user-control/slots/{id}",
                axum::routing::delete(user_control_api::delete_slot).with_state(uc_state),
            );
    }

    let mut app = app
        .layer(CorsLayer::permissive()) // Allow HA Ingress
        .with_state(app_state);

    // Add remote access routes (self-contained state, merged after main state)
    if let Some(ra_state) = remote_access_state {
        info!("Remote Access API enabled");
        app = app.merge(remote_access_routes(ra_state));

        // Add mobile-facing API routes (served over Tor to mobile devices)
        let mobile_state = MobileApiState {
            query_sender: mobile_query_sender.clone(),
            i18n: mobile_i18n.clone(),
            user_control_api_state: mobile_uc_api.clone(),
            ui_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        app = app.merge(mobile_api_routes(mobile_state));
    }

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

    // Get user control state for dashboard rendering
    let user_control = app_state
        .user_control_state
        .as_ref()
        .map(|uc| uc.read().clone());

    match app_state.query_sender.query_dashboard().await {
        Ok(response) => {
            let template = DashboardTemplate::from_query_response(
                response,
                app_state.i18n.clone(),
                ingress_path,
                user_control,
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
                    // User control state not needed for live data updates (fetched separately via JS)
                    let dashboard = DashboardTemplate::from_query_response(
                        response,
                        app_state.i18n.clone(),
                        String::new(),
                        None,
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
                        consumption_stats: dashboard.consumption_stats,
                        solar_forecast: dashboard.solar_forecast,
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
    /// Block start instants (RFC 3339, UTC), aligned with `labels`
    block_starts_utc: Vec<String>,
    prices: Vec<f32>,
    modes: Vec<String>,
    strategies: Vec<Option<String>>,
    profits: Vec<Option<f32>>,
    current_time_label: Option<String>,
    current_battery_soc: Option<f32>,
    /// Predicted SOC per block, so a refresh follows the default-mode switch (brief H19)
    battery_soc_prediction: Vec<routes::SocPredictionPoint>,
    // Stacked bar data for HDO display
    spot_prices: Vec<f32>,
    grid_fees: Vec<f32>,
    tariff_types: Vec<String>,
    is_historical: Vec<bool>,
    reasons: Vec<Option<String>>,
    decision_uids: Vec<Option<String>>,
    /// Total effective price: spot + grid_fee + buy_fees
    effective_prices: Vec<f32>,
    /// Hourly consumption profile (kWh per hour, index = UTC hour of day)
    #[serde(skip_serializing_if = "Option::is_none")]
    hourly_consumption_profile: Option<Vec<f32>>,
    /// True when an HDO schedule is known, so `tariff_types` is real
    tariff_known: bool,
    /// Actual PV power (kW) per block from PV history, aligned with `labels`
    pv_actual_kw: Vec<Option<f32>>,
    /// Forecast PV power (kW) per not-yet-ended block, aligned with `labels`
    solar_forecast_kw: Vec<Option<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    solar_forecast_remaining_today_kwh: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    solar_forecast_tomorrow_kwh: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    utc_offset_minutes: Option<i32>,
}

/// Build the chart refresh JSON from a dashboard response; `None` when there is no price data
fn build_chart_data_json(response: &WebQueryResponse, i18n: Arc<I18n>) -> Option<ChartDataJson> {
    // User control state not needed for chart data
    let template = DashboardTemplate::from_query_response(
        response.clone(),
        i18n,
        String::new(), // JSON endpoint doesn't need ingress path
        None,
    );

    // Extract battery SOC from first inverter
    let current_battery_soc = response.inverters.first().map(|inv| inv.battery_soc);

    // Extract solar forecast data
    let solar_remaining = response
        .solar_forecast
        .as_ref()
        .filter(|sf| sf.available)
        .map(|sf| sf.remaining_today_kwh);
    let solar_tomorrow = response
        .solar_forecast
        .as_ref()
        .filter(|sf| sf.available)
        .map(|sf| sf.tomorrow_kwh);

    // Calculate UTC offset from timezone string
    let utc_offset_minutes = display_timezone(response).0.map(|tz| {
        let now = chrono::Utc::now().with_timezone(&tz);
        #[expect(clippy::integer_division)]
        {
            now.offset().fix().local_minus_utc() / 60
        }
    });

    // Extract chart data from template
    let chart = template.prices?.chart_data;
    Some(ChartDataJson {
        labels: chart.labels,
        block_starts_utc: chart.block_starts_utc,
        prices: chart.prices,
        modes: chart.modes,
        strategies: chart.strategies,
        profits: chart.profits,
        current_time_label: chart.current_time_label,
        current_battery_soc,
        battery_soc_prediction: chart.battery_soc_prediction,
        // Stacked bar data
        spot_prices: chart.spot_prices,
        grid_fees: chart.grid_fees,
        tariff_types: chart.tariff_types,
        is_historical: chart.is_historical,
        reasons: chart.reasons,
        decision_uids: chart.decision_uids,
        effective_prices: chart.effective_prices,
        hourly_consumption_profile: chart.hourly_consumption_profile,
        tariff_known: chart.tariff_known,
        pv_actual_kw: chart.pv_actual_kw,
        solar_forecast_kw: chart.solar_forecast_kw,
        solar_forecast_remaining_today_kwh: solar_remaining,
        solar_forecast_tomorrow_kwh: solar_tomorrow,
        utc_offset_minutes,
    })
}

/// Chart data endpoint - returns JSON for chart updates (once per minute)
async fn chart_data_handler(State(app_state): State<AppState>) -> impl IntoResponse {
    match app_state.query_sender.query_dashboard().await {
        Ok(response) => match build_chart_data_json(&response, app_state.i18n.clone()) {
            Some(chart_json) => Json(chart_json).into_response(),
            None => (axum::http::StatusCode::NO_CONTENT, "").into_response(),
        },
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
        "backup" => "b",
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use fluxion_core::web_bridge::{
        ConsumptionStats, HdoScheduleInfo, PriceBlockData, PriceData, SystemHealthData,
        TimezoneSource,
    };

    fn block(ts: &str, solar_forecast_kw: Option<f32>) -> PriceBlockData {
        let timestamp: DateTime<Utc> = ts.parse().unwrap();
        PriceBlockData {
            timestamp,
            price: 2.0,
            effective_buy_price: None,
            block_type: "self-use".to_owned(),
            target_soc: None,
            strategy: None,
            expected_profit: None,
            reason: None,
            decision_uid: None,
            debug_info: None,
            is_historical: false,
            solar_forecast_kw,
            backup_min_soc_pct: None,
        }
    }

    fn backup_block(ts: &str) -> PriceBlockData {
        PriceBlockData {
            block_type: "backup".to_owned(),
            strategy: Some("Self-Use".to_owned()),
            backup_min_soc_pct: Some(25),
            ..block(ts, None)
        }
    }

    #[test]
    fn effective_back_up_blocks_show_mode_with_min_soc_on_render_and_refresh() {
        let mut charge = block("2026-10-16T10:00:00Z", None);
        charge.block_type = "charge".to_owned();
        let mut planned_backup = block("2026-10-16T10:30:00Z", None);
        planned_backup.block_type = "backup".to_owned();
        let blocks = vec![
            charge,
            backup_block("2026-10-16T10:15:00Z"),
            planned_backup,
            block("2026-10-16T10:45:00Z", None),
        ];
        let mut resp = response(blocks, None);
        resp.battery_soc_prediction =
            Some(vec![fluxion_core::web_bridge::BatterySocPredictionPoint {
                timestamp: "2026-10-16T10:15:00Z".parse().unwrap(),
                soc: 25.0,
            }]);

        // Refresh (/chart-data)
        let json = serde_json::to_value(build_chart_data_json(&resp, i18n()).unwrap()).unwrap();
        assert_eq!(
            json["modes"],
            serde_json::json!([
                "Force Charge",
                "Back Up Mode (25%)",
                "Back Up Mode",
                "Self-Use"
            ])
        );
        assert_eq!(json["battery_soc_prediction"][0]["soc"], 25.0);

        // First render embeds the same labels; the page colours them with the Back Up colour
        let html = DashboardTemplate::from_query_response(resp, i18n(), String::new(), None)
            .render()
            .unwrap();
        assert!(html.contains(r#""Back Up Mode (25%)""#));
        assert!(html.contains("mode.startsWith('Back Up Mode')"));
        assert!(!html.contains("mode === 'Back Up Mode'"));
    }

    fn response(blocks: Vec<PriceBlockData>, hdo: Option<HdoScheduleInfo>) -> WebQueryResponse {
        WebQueryResponse {
            timestamp: Utc::now(),
            debug_mode: false,
            inverters: vec![],
            schedule: None,
            prices: Some(PriceData {
                current_price: 2.0,
                min_price: 2.0,
                max_price: 2.0,
                avg_price: 2.0,
                blocks,
                today_min_price: 2.0,
                today_max_price: 2.0,
                today_avg_price: 2.0,
                today_median_price: 2.0,
                tomorrow_min_price: None,
                tomorrow_max_price: None,
                tomorrow_avg_price: None,
                tomorrow_median_price: None,
            }),
            health: SystemHealthData {
                inverter_source: true,
                price_source: true,
                last_update: Utc::now(),
                errors: vec![],
            },
            timezone: Some("Europe/Prague".to_owned()),
            timezone_source: TimezoneSource::HomeAssistant,
            battery_soc_history: None,
            battery_soc_prediction: None,
            pv_generation_history: None,
            consumption_stats: Some(ConsumptionStats {
                ema_kwh: None,
                ema_days: 7,
                today_import_kwh: None,
                yesterday_import_kwh: None,
                hourly_consumption_profile: Some(vec![0.5; 24]),
            }),
            hdo_schedule: hdo,
            pricing_fees: None,
            solar_forecast: None,
        }
    }

    fn i18n() -> Arc<I18n> {
        Arc::new(I18n::new(fluxion_i18n::Language::English).unwrap())
    }

    /// Start the real web server on a free local port and return that port
    async fn start_test_server(backtest_state: Option<BacktestState>) -> u16 {
        let port = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (query_sender, _query_channel) = WebQuerySender::new();
        tokio::spawn(async move {
            let _ = start_web_server(
                query_sender,
                i18n(),
                port,
                serde_json::json!({}),
                None,
                backtest_state,
                None,
                None,
                None,
                None,
            )
            .await;
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("web server did not start on port {port}");
    }

    /// Send a raw HTTP/1.1 GET and return the full response text
    async fn http_get(port: u16, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    /// JSON body of a raw HTTP response
    fn json_body(response: &str) -> serde_json::Value {
        let body = response.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    mod backtest_history {
        use chrono::{DateTime, Duration, TimeZone, Utc};
        use fluxion_core::backtest::{
            BacktestEntities, BacktestSite, HistoryError, HistoryReader, HistoryState,
        };
        use fluxion_types::config::{
            ControlConfig, PriceSchedule, PricingConfig, WinterReserveConfigCore,
        };

        pub const TZ: chrono_tz::Tz = chrono_tz::Europe::Prague;

        /// Three days after the replayed day
        pub fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 10, 23, 12, 0, 0).unwrap()
        }

        pub fn site() -> BacktestSite {
            BacktestSite {
                timezone: TZ,
                entities: BacktestEntities {
                    soc: "sensor.solax_battery_capacity".to_owned(),
                    house_load: "sensor.solax_house_load".to_owned(),
                    pv: vec!["sensor.solax_pv_power_total".to_owned()],
                    grid_import: "sensor.solax_grid_import".to_owned(),
                    grid_export: "sensor.solax_grid_export".to_owned(),
                    spot_price: "sensor.spot_price".to_owned(),
                    buy_price: None,
                    hdo: None,
                },
                pricing: PricingConfig {
                    spot_price_entity: "sensor.spot_price".to_owned(),
                    tomorrow_price_entity: None,
                    buy_price_entity: None,
                    use_spot_prices_to_buy: true,
                    use_spot_prices_to_sell: true,
                    fixed_buy_price_czk: PriceSchedule::Flat(0.0),
                    fixed_sell_price_czk: PriceSchedule::Flat(0.0),
                    spot_buy_fee_czk: 0.4,
                    spot_sell_fee_czk: 0.3,
                    hdo_sensor_entity: String::new(),
                    hdo_low_tariff_czk: 0.5,
                    hdo_high_tariff_czk: 1.8,
                },
                control: ControlConfig::default(),
                winter_reserve: WinterReserveConfigCore::default(),
            }
        }

        /// Mocked HA history: hourly load and price shapes from 11 days before `now()`.
        #[derive(Debug)]
        pub struct MockHistory;

        fn series(entity: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<HistoryState> {
            let first = now() - Duration::days(11);
            let mut out = Vec::new();
            let mut t = first;
            while t <= end {
                let hour = t
                    .with_timezone(&TZ)
                    .format("%H")
                    .to_string()
                    .parse::<u32>()
                    .unwrap();
                let value = match entity {
                    "sensor.solax_battery_capacity" => 40.0,
                    "sensor.solax_house_load" => {
                        if (17..21).contains(&hour) {
                            2500.0
                        } else {
                            600.0
                        }
                    }
                    "sensor.solax_pv_power_total" => {
                        if (10..15).contains(&hour) {
                            800.0
                        } else {
                            0.0
                        }
                    }
                    "sensor.solax_grid_import" => {
                        if (17..21).contains(&hour) {
                            2500.0
                        } else {
                            300.0
                        }
                    }
                    "sensor.solax_grid_export" => 0.0,
                    "sensor.spot_price" => {
                        if (17..21).contains(&hour) {
                            6.0
                        } else if hour < 5 {
                            1.0
                        } else {
                            3.0
                        }
                    }
                    _ => return Vec::new(),
                };
                if t >= start || t + Duration::hours(1) > start {
                    out.push(HistoryState::from_raw(
                        t.max(start),
                        &value.to_string(),
                        None,
                    ));
                }
                t += Duration::hours(1);
            }
            out
        }

        #[async_trait::async_trait]
        impl HistoryReader for MockHistory {
            async fn history(
                &self,
                entity_id: &str,
                start: DateTime<Utc>,
                end: DateTime<Utc>,
                _with_attributes: bool,
            ) -> Result<Vec<HistoryState>, HistoryError> {
                Ok(series(entity_id, start, end))
            }
        }

        /// `MockHistory` that counts house-load reads (one per day load) and is slow
        #[derive(Debug)]
        pub struct CountingHistory {
            pub loads: std::sync::atomic::AtomicUsize,
            /// How long each house-load read takes
            pub delay: std::time::Duration,
        }

        impl Default for CountingHistory {
            fn default() -> Self {
                Self::with_delay(std::time::Duration::from_millis(200))
            }
        }

        impl CountingHistory {
            pub fn with_delay(delay: std::time::Duration) -> Self {
                Self {
                    loads: std::sync::atomic::AtomicUsize::new(0),
                    delay,
                }
            }

            pub fn count(&self) -> usize {
                self.loads.load(std::sync::atomic::Ordering::SeqCst)
            }
        }

        #[async_trait::async_trait]
        impl HistoryReader for CountingHistory {
            async fn history(
                &self,
                entity_id: &str,
                start: DateTime<Utc>,
                end: DateTime<Utc>,
                with_attributes: bool,
            ) -> Result<Vec<HistoryState>, HistoryError> {
                if entity_id == "sensor.solax_house_load" {
                    self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(self.delay).await;
                }
                MockHistory
                    .history(entity_id, start, end, with_attributes)
                    .await
            }
        }

        /// `MockHistory` whose HDO entity lookup (`/api/states`) fails
        #[derive(Debug)]
        pub struct HdoLookupFails;

        #[async_trait::async_trait]
        impl HistoryReader for HdoLookupFails {
            async fn history(
                &self,
                entity_id: &str,
                start: DateTime<Utc>,
                end: DateTime<Utc>,
                with_attributes: bool,
            ) -> Result<Vec<HistoryState>, HistoryError> {
                MockHistory
                    .history(entity_id, start, end, with_attributes)
                    .await
            }

            async fn resolve_hdo_entity(
                &self,
                _configured: &str,
            ) -> Result<Option<String>, HistoryError> {
                Err(HistoryError::Status(500))
            }
        }

        /// HA refuses every request
        #[derive(Debug)]
        pub struct RefusingHistory;

        #[async_trait::async_trait]
        impl HistoryReader for RefusingHistory {
            async fn history(
                &self,
                _entity_id: &str,
                _start: DateTime<Utc>,
                _end: DateTime<Utc>,
                _with_attributes: bool,
            ) -> Result<Vec<HistoryState>, HistoryError> {
                Err(HistoryError::Unauthorized)
            }
        }
    }

    fn backtest_state(reader: Arc<dyn fluxion_core::backtest::HistoryReader>) -> BacktestState {
        BacktestState::new(reader, Ok(backtest_history::site()), i18n())
            .with_clock(backtest_history::now)
    }

    #[tokio::test]
    async fn backtest_routes_registered_without_state() {
        let port = start_test_server(None).await;
        let response = http_get(port, "/api/backtest/days").await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(json_body(&response)["error"].is_string());
        let page = http_get(port, "/backtest").await;
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    }

    #[tokio::test]
    async fn simulator_always_links_backtest() {
        let port = start_test_server(None).await;
        let response = http_get(port, "/simulator").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("href=\"/backtest\""));
    }

    #[tokio::test]
    async fn backtest_day_returns_three_series_and_totals() {
        let state = backtest_state(Arc::new(backtest_history::MockHistory));
        let port = start_test_server(Some(state)).await;

        let days = json_body(&http_get(port, "/api/backtest/days").await);
        let listed: Vec<&str> = days["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["date"].as_str().unwrap())
            .collect();
        assert!(listed.contains(&"2026-10-20"), "{days}");

        let response = http_get(port, "/api/backtest/day/2026-10-20").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let report = json_body(&response);
        assert_eq!(report["labels"].as_array().unwrap().len(), 96);
        for series in ["actual", "self_use", "winter_reserve"] {
            assert_eq!(
                report[series]["soc_pct"].as_array().unwrap().len(),
                96,
                "{series}"
            );
            assert_eq!(report[series]["import_kwh"].as_array().unwrap().len(), 96);
        }
        assert_eq!(
            report["winter_reserve"]["mode"].as_array().unwrap().len(),
            96
        );
        for totals in ["totals_actual", "totals_self_use", "totals_winter_reserve"] {
            assert!(report[totals]["cost_czk"].is_number(), "{totals}");
        }
        assert_eq!(report["data"]["usable_blocks"], 96);
        assert_eq!(report["data"]["history_days"], 7);
        assert!((report["fees"]["spot_buy_fee_czk"].as_f64().unwrap() - 0.4).abs() < 1e-6);
        assert!(report["summary"].as_str().unwrap().contains("2026-10-20"));
        // Recorded import: 300 W (0.075 kWh per block) at spot 1.0 for 00-05 (20 blocks) and 3.0
        // for 60 blocks, 2500 W (0.625 kWh) at 6.0 for 17-21 (16 blocks); buy = spot + 0.4 trade
        // fee + 1.8 high tariff (no HDO schedule), no export
        let actual = report["totals_actual"]["cost_czk"].as_f64().unwrap();
        let expected = 0.075 * 20.0 * 3.2 + 0.075 * 60.0 * 5.2 + 0.625 * 16.0 * 8.2;
        assert!((actual - expected).abs() < 0.01, "{actual} vs {expected}");

        // Second request is served from the cache: identical body
        let again = json_body(&http_get(port, "/api/backtest/day/2026-10-20").await);
        assert_eq!(again, report);
    }

    #[tokio::test]
    async fn backtest_uses_the_live_back_up_floor_and_says_so() {
        let user_control = Arc::new(RwLock::new(UserControlState {
            default_mode: fluxion_types::DefaultMode::Backup,
            backup_min_soc_pct: 45,
            ..Default::default()
        }));
        let state = backtest_state(Arc::new(backtest_history::MockHistory))
            .with_user_control(Arc::clone(&user_control));
        let port = start_test_server(Some(state)).await;
        let min = |series: &serde_json::Value| {
            series
                .as_array()
                .unwrap()
                .iter()
                .filter_map(serde_json::Value::as_f64)
                .fold(f64::MAX, f64::min)
        };

        let backup = json_body(&http_get(port, "/api/backtest/day/2026-10-20").await);
        assert_eq!(backup["floor"]["source"], "back_up_default_mode");
        assert!((backup["floor"]["min_soc_pct"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!(
            min(&backup["self_use"]["soc_pct"]) >= 45.0 - 1e-3,
            "{}",
            backup["self_use"]["soc_pct"]
        );
        assert!(min(&backup["winter_reserve"]["soc_pct"]) >= 45.0 - 1e-3);

        // Switching the dashboard to Self-Use replays with the control minimum (not the cache)
        user_control.write().default_mode = fluxion_types::DefaultMode::SelfUse;
        let self_use = json_body(&http_get(port, "/api/backtest/day/2026-10-20").await);
        assert_eq!(self_use["floor"]["source"], "control_min_battery_soc");
        assert!(min(&self_use["self_use"]["soc_pct"]) < 45.0);
    }

    #[tokio::test]
    async fn backtest_out_of_range_dates_are_clear_400s() {
        let state = backtest_state(Arc::new(backtest_history::MockHistory));
        let port = start_test_server(Some(state)).await;
        for date in ["-262143-01-01", "2019-12-31", "2026-10-25", "+262142-12-31"] {
            let response = http_get(port, &format!("/api/backtest/day/{date}")).await;
            assert!(response.starts_with("HTTP/1.1 400"), "{date}: {response}");
            assert!(json_body(&response)["error"].is_string(), "{date}");
        }
        // The server survived (no panic took the handler down)
        let ok = http_get(port, "/api/backtest/day/2026-10-20").await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    }

    #[tokio::test]
    async fn backtest_loads_one_day_at_a_time_and_reuses_the_result() {
        let reader = Arc::new(backtest_history::CountingHistory::default());
        let state =
            backtest_state(Arc::clone(&reader) as Arc<dyn fluxion_core::backtest::HistoryReader>);
        let port = start_test_server(Some(state)).await;
        let (a, b) = tokio::join!(
            http_get(port, "/api/backtest/day/2026-10-20"),
            http_get(port, "/api/backtest/day/2026-10-20")
        );
        assert!(a.starts_with("HTTP/1.1 200") && b.starts_with("HTTP/1.1 200"));
        assert_eq!(json_body(&a), json_body(&b));
        assert_eq!(reader.loads.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 2026-10-22: its next day has not ended at now (2026-10-23 12:00), so the horizon is
        // cut; the result is still cached (briefly, see the TTL test)
        for _ in 0..2 {
            let r = http_get(port, "/api/backtest/day/2026-10-22").await;
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        }
        assert_eq!(reader.count(), 2);
    }

    /// State whose clock is `backtest_history::now()` plus the seconds in the returned cell
    fn backtest_state_with_offset_clock(
        reader: Arc<dyn fluxion_core::backtest::HistoryReader>,
    ) -> (BacktestState, Arc<std::sync::atomic::AtomicI64>) {
        let offset = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let clock_offset = Arc::clone(&offset);
        let state = BacktestState::new(reader, Ok(backtest_history::site()), i18n()).with_clock(
            move || {
                backtest_history::now()
                    + chrono::Duration::seconds(
                        clock_offset.load(std::sync::atomic::Ordering::SeqCst),
                    )
            },
        );
        (state, offset)
    }

    #[tokio::test]
    async fn backtest_incomplete_day_is_cached_for_15_minutes_complete_day_stays() {
        let reader = Arc::new(backtest_history::CountingHistory::with_delay(
            std::time::Duration::ZERO,
        ));
        let (state, offset) = backtest_state_with_offset_clock(
            Arc::clone(&reader) as Arc<dyn fluxion_core::backtest::HistoryReader>
        );
        let port = start_test_server(Some(state)).await;
        let set = |secs: i64| offset.store(secs, std::sync::atomic::Ordering::SeqCst);
        let get = |date: &'static str| async move {
            let r = http_get(port, &format!("/api/backtest/day/{date}")).await;
            assert!(r.starts_with("HTTP/1.1 200"), "{date}: {r}");
        };

        // 2026-10-22 is incomplete at now (its next day has not ended); 2026-10-20 is complete
        get("2026-10-22").await;
        get("2026-10-20").await;
        assert_eq!(reader.count(), 2);
        // Within 15 minutes both come from the cache
        set(14 * 60 + 59);
        get("2026-10-22").await;
        get("2026-10-20").await;
        assert_eq!(reader.count(), 2);
        // After 15 minutes the incomplete day is read again (and cached again), the complete
        // one is not
        set(15 * 60);
        get("2026-10-22").await;
        get("2026-10-20").await;
        assert_eq!(reader.count(), 3);
        get("2026-10-22").await;
        assert_eq!(reader.count(), 3);
        set(10 * 3600);
        get("2026-10-20").await;
        assert_eq!(reader.count(), 3);
    }

    #[tokio::test]
    async fn backtest_waiting_for_another_load_is_bounded_with_a_clear_503() {
        let reader = Arc::new(backtest_history::CountingHistory::with_delay(
            std::time::Duration::from_millis(1500),
        ));
        let state =
            backtest_state(Arc::clone(&reader) as Arc<dyn fluxion_core::backtest::HistoryReader>)
                .with_load_wait(std::time::Duration::from_millis(200));
        let port = start_test_server(Some(state)).await;
        let first = tokio::spawn(http_get(port, "/api/backtest/day/2026-10-20"));
        // Let the first request take the permit
        while reader.count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let waited = std::time::Instant::now();
        let turned_away = http_get(port, "/api/backtest/day/2026-10-19").await;
        assert!(waited.elapsed() < std::time::Duration::from_millis(1200));
        assert!(turned_away.starts_with("HTTP/1.1 503"), "{turned_away}");
        let message = json_body(&turned_away)["error"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(message, "another backtest day is loading, try again");
        for leak in [
            "/home/",
            "/data/",
            "http://",
            "token",
            "Bearer",
            "supervisor",
        ] {
            assert!(!turned_away.contains(leak), "{leak} in {turned_away}");
        }
        // The first load finishes normally; the turned-away day never loaded
        let first = first.await.unwrap();
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert_eq!(reader.count(), 1);
        // Once the permit is free the other day loads
        let later = http_get(port, "/api/backtest/day/2026-10-19").await;
        assert!(later.starts_with("HTTP/1.1 200"), "{later}");
        assert_eq!(reader.count(), 2);
    }

    #[tokio::test]
    async fn backtest_failed_hdo_lookup_falls_back_to_high_tariff() {
        let site = || {
            let mut site = backtest_history::site();
            site.entities.hdo = Some("sensor.hdo_schedule".to_owned());
            site
        };
        let state = |reader: Arc<dyn fluxion_core::backtest::HistoryReader>| {
            BacktestState::new(reader, Ok(site()), i18n()).with_clock(backtest_history::now)
        };
        let port = start_test_server(Some(state(Arc::new(backtest_history::HdoLookupFails)))).await;
        let response = http_get(port, "/api/backtest/day/2026-10-20").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let report = json_body(&response);
        assert_eq!(report["data"]["hdo_source"], "lookup failed (high tariff)");
        assert_eq!(report["data"]["missing_per_field"]["hdo"], 96);
        assert!(
            report["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("could not be looked up")),
            "{}",
            report["notes"]
        );
        // Priced like the day whose HDO sensor recorded no schedule (spot + 0.4 trade fee + 1.8
        // high tariff)
        let reference = json_body(
            &http_get(
                start_test_server(Some(state(Arc::new(backtest_history::MockHistory)))).await,
                "/api/backtest/day/2026-10-20",
            )
            .await,
        );
        assert_eq!(reference["data"]["hdo_source"], "none (high tariff)");
        assert_eq!(report["buy_czk"], reference["buy_czk"]);
        assert_eq!(
            report["totals_actual"]["cost_czk"],
            reference["totals_actual"]["cost_czk"]
        );
    }

    #[tokio::test]
    async fn backtest_ha_error_is_clear_json_without_paths_or_tokens() {
        let state = backtest_state(Arc::new(backtest_history::RefusingHistory));
        let port = start_test_server(Some(state)).await;
        for path in ["/api/backtest/day/2026-10-20", "/api/backtest/days"] {
            let response = http_get(port, path).await;
            assert!(response.starts_with("HTTP/1.1 502"), "{response}");
            let body = json_body(&response);
            let message = body["error"].as_str().unwrap();
            assert!(message.contains("Home Assistant"), "{message}");
            for leak in [
                "/home/",
                "/data/",
                "http://",
                "token",
                "Bearer",
                "supervisor",
            ] {
                assert!(!response.contains(leak), "{leak} in {response}");
            }
        }
        // A bad date and an unfinished day are clear 400s
        let bad = http_get(port, "/api/backtest/day/20-10-2026").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        let today = http_get(port, "/api/backtest/day/2026-10-23").await;
        assert!(today.starts_with("HTTP/1.1 400"), "{today}");
    }

    #[test]
    fn chart_json_carries_solar_forecast_series() {
        let blocks = vec![
            block("2026-10-16T10:00:00Z", None),
            block("2026-10-16T10:15:00Z", Some(3.2)),
            block("2026-10-16T10:30:00Z", Some(0.0)),
        ];
        let chart = build_chart_data_json(&response(blocks, None), i18n()).unwrap();
        let json = serde_json::to_value(&chart).unwrap();

        let forecast: Vec<Option<f64>> =
            serde_json::from_value(json["solar_forecast_kw"].clone()).unwrap();
        assert_eq!(forecast.len(), 3);
        assert_eq!(forecast[0], None);
        assert!((forecast[1].unwrap() - 3.2).abs() < 1e-6);
        assert_eq!(forecast[2], Some(0.0));
        // Aligned with the labels and UTC block starts
        assert_eq!(json["labels"].as_array().unwrap().len(), 3);
        assert_eq!(json["block_starts_utc"][1], "2026-10-16T10:15:00Z");
        assert_eq!(json["pv_actual_kw"], serde_json::json!([null, null, null]));
        assert_eq!(
            json["hourly_consumption_profile"].as_array().unwrap().len(),
            24
        );
        // No HDO schedule: tariff types are a default, not a known tariff
        assert_eq!(json["tariff_known"], false);
    }

    #[test]
    fn dashboard_renders_chart_series_helper_and_toggles() {
        let blocks = vec![block("2026-10-16T10:00:00Z", Some(1.0))];
        let html = DashboardTemplate::from_query_response(
            response(blocks, None),
            i18n(),
            String::new(),
            None,
        )
        .render()
        .unwrap();
        assert!(html.contains("window.FluxionChartSeries"));
        assert!(html.contains(r#"id="toggleSolarForecast""#));
        assert!(html.contains(r#"id="toggleConsumptionProfile""#));
        assert!(html.contains(r#""solar_forecast_kw":[1.0]"#));
    }

    #[test]
    fn chart_json_marks_tariff_known_with_zero_fees() {
        let hdo = HdoScheduleInfo {
            low_tariff_periods: vec![("00:00".to_owned(), "12:00".to_owned())],
            low_tariff_czk: 0.0,
            high_tariff_czk: 0.0,
            last_updated: None,
        };
        // 10:00Z = 12:00 Prague (high), 09:45Z = 11:45 Prague (low)
        let blocks = vec![
            block("2026-10-16T09:45:00Z", None),
            block("2026-10-16T10:00:00Z", None),
        ];
        let chart = build_chart_data_json(&response(blocks, Some(hdo)), i18n()).unwrap();
        let json = serde_json::to_value(&chart).unwrap();

        assert_eq!(json["tariff_known"], true);
        assert_eq!(json["tariff_types"], serde_json::json!(["low", "high"]));
        assert_eq!(json["grid_fees"], serde_json::json!([0.0, 0.0]));
    }
}
