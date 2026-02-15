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

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use chrono::Utc;
use tracing::error;

use crate::db::Database;
use fluxion_shared::telemetry::TelemetrySnapshot;

#[derive(Debug, Clone)]
pub struct DashboardState {
    pub db: Arc<Database>,
}

#[derive(Debug, Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub clients: Vec<DashboardClient>,
    pub total: usize,
    pub online: usize,
    pub warning: usize,
    pub offline: usize,
    pub server_time: String,
}

#[derive(Debug)]
pub struct DashboardClient {
    pub instance_id: String,
    pub friendly_name: String,
    pub status: String,
    pub fluxion_version: String,
    pub strategy_name: String,
    pub battery_soc: Option<f32>,
    pub last_seen_relative: String,
    pub last_seen: String,
    pub telemetry: Option<ClientTelemetryDisplay>,
    pub battery_capacity_kwh: Option<f32>,
    pub target_soc_max: Option<f32>,
    pub target_soc_min: Option<f32>,
}

#[derive(Debug)]
pub struct ClientTelemetryDisplay {
    // Energy today (cumulative)
    pub grid_import_today_kwh: Option<f32>,
    pub grid_export_today_kwh: Option<f32>,
    pub solar_today_kwh: Option<f32>,
    pub battery_charge_today_kwh: Option<f32>,
    pub battery_discharge_today_kwh: Option<f32>,
    // Status
    pub battery_temperature_c: Option<f32>,
    pub inverter_temperature_c: Option<f32>,
    pub current_mode: Option<String>,
    pub current_strategy: Option<String>,
    pub inverter_online: Option<bool>,
    pub mode_synced: Option<bool>,
    // Solar forecast
    pub solar_forecast_today_kwh: Option<f32>,
    pub solar_forecast_remaining_kwh: Option<f32>,
    pub solar_forecast_accuracy: Option<f32>,
    // Health
    pub errors: Vec<String>,
}

fn parse_telemetry_display(json: &str) -> Option<ClientTelemetryDisplay> {
    let snapshot: TelemetrySnapshot = serde_json::from_str(json).ok()?;

    let inv = snapshot.inverters.first();

    Some(ClientTelemetryDisplay {
        grid_import_today_kwh: inv.and_then(|i| i.grid_import_today_kwh),
        grid_export_today_kwh: inv.and_then(|i| i.grid_export_today_kwh),
        solar_today_kwh: inv.and_then(|i| i.today_solar_energy_kwh),
        battery_charge_today_kwh: inv.and_then(|i| i.battery_input_energy_today_kwh),
        battery_discharge_today_kwh: inv.and_then(|i| i.battery_output_energy_today_kwh),
        battery_temperature_c: inv.map(|i| i.battery_temperature_c),
        inverter_temperature_c: inv.map(|i| i.inverter_temperature_c),
        current_mode: Some(snapshot.instance.current_mode),
        current_strategy: snapshot.instance.current_strategy,
        inverter_online: inv.map(|i| i.online),
        mode_synced: inv.map(|i| i.mode_synced),
        solar_forecast_today_kwh: Some(snapshot.instance.solar_forecast_total_today_kwh),
        solar_forecast_remaining_kwh: Some(snapshot.instance.solar_forecast_remaining_today_kwh),
        solar_forecast_accuracy: snapshot.instance.solar_forecast_accuracy_percent,
        errors: snapshot.instance.errors,
    })
}

#[expect(
    clippy::integer_division,
    reason = "integer truncation is intentional for relative time display"
)]
fn format_relative_time(seconds: i64) -> String {
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3600 {
        let mins = seconds / 60;
        if mins == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{mins} minutes ago")
        }
    } else if seconds < 86400 {
        let hours = seconds / 3600;
        if hours == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = seconds / 86400;
        if days == 1 {
            "1 day ago".to_owned()
        } else {
            format!("{days} days ago")
        }
    }
}

#[expect(clippy::unused_async, reason = "axum handler must be async")]
pub async fn dashboard_handler(State(state): State<DashboardState>) -> impl IntoResponse {
    let now = Utc::now();

    let clients = match state.db.get_all_clients() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to fetch clients for dashboard");
            return Html("<h1>Error loading dashboard</h1>".to_owned());
        }
    };

    let online = clients.iter().filter(|c| c.status == "online").count();
    let warning = clients.iter().filter(|c| c.status == "warning").count();
    let offline = clients.iter().filter(|c| c.status == "offline").count();

    let dashboard_clients: Vec<DashboardClient> = clients
        .iter()
        .map(|c| {
            let elapsed = now.signed_duration_since(c.last_seen).num_seconds().max(0);
            let telemetry = c
                .latest_telemetry_json
                .as_deref()
                .and_then(parse_telemetry_display);
            DashboardClient {
                instance_id: c.instance_id.clone(),
                friendly_name: c
                    .friendly_name
                    .clone()
                    .unwrap_or_else(|| c.instance_id.clone()),
                status: c.status.clone(),
                fluxion_version: c
                    .fluxion_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
                strategy_name: c.strategy_name.clone().unwrap_or_else(|| "—".to_owned()),
                battery_soc: c.battery_soc,
                last_seen_relative: format_relative_time(elapsed),
                last_seen: c.last_seen.to_rfc3339(),
                telemetry,
                battery_capacity_kwh: c.battery_capacity_kwh,
                target_soc_max: c.target_soc_max,
                target_soc_min: c.target_soc_min,
            }
        })
        .collect();

    let template = DashboardTemplate {
        total: dashboard_clients.len(),
        online,
        warning,
        offline,
        clients: dashboard_clients,
        server_time: now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    };

    match template.render() {
        Ok(html) => Html(html),
        Err(e) => {
            error!(error = %e, "Template render error");
            Html(format!("<h1>Error rendering dashboard: {e}</h1>"))
        }
    }
}
