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

use std::time::Duration;

use chrono::Utc;
use fluxion_core::web_bridge::WebQueryResponse;
use fluxion_core::WebQuerySender;
use fluxion_shared::heartbeat::{HeartbeatRequest, HeartbeatResponse, HeartbeatStatus};
use fluxion_shared::telemetry::{
    ClientSyncData, InstanceTelemetry, InverterTelemetry, ScheduleBlockTelemetry,
    ScheduleTelemetry, SocPredictionPoint, TelemetrySnapshot,
};
use tracing::{error, info, warn};

use crate::config::ServerHeartbeatConfig;
use crate::version::VERSION;

/// Spawns a background task that periodically sends heartbeats to the central server.
pub fn spawn_heartbeat_task(config: ServerHeartbeatConfig, query_sender: WebQuerySender) {
    info!(
        server_url = %config.server_url,
        instance_id = %config.instance_id,
        interval_seconds = config.interval_seconds,
        "Starting heartbeat client"
    );

    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let interval = Duration::from_secs(config.interval_seconds);
        let url = format!("{}/api/heartbeat", config.server_url.trim_end_matches('/'));
        let mut first_heartbeat = true;

        loop {
            // Query current system state for heartbeat payload
            let (strategy_name, battery_soc, telemetry, sync_data) =
                match query_sender.query_dashboard().await {
                    Ok(dashboard) => {
                        let strategy = dashboard
                            .schedule
                            .as_ref()
                            .and_then(|s| s.current_strategy.clone());
                        let soc = dashboard.inverters.first().map(|i| i.battery_soc);
                        let telemetry = build_telemetry_snapshot(&dashboard);
                        let sync = if first_heartbeat {
                            first_heartbeat = false;
                            Some(build_sync_data(&dashboard))
                        } else {
                            None
                        };
                        (strategy, soc, Some(telemetry), sync)
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to query dashboard for heartbeat");
                        (None, None, None, None)
                    }
                };

            let request = HeartbeatRequest {
                instance_id: config.instance_id.clone(),
                shared_secret: config.shared_secret.clone(),
                timestamp: Utc::now(),
                fluxion_version: VERSION.to_owned(),
                status: HeartbeatStatus {
                    friendly_name: config.friendly_name.clone(),
                    online: true,
                    strategy_name,
                    battery_soc,
                },
                telemetry,
                sync_data,
            };

            match client.post(&url).json(&request).send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        match resp.json::<HeartbeatResponse>().await {
                            Ok(hr) if hr.ok => {
                                info!("Heartbeat sent successfully");
                            }
                            Ok(hr) => {
                                warn!(message = ?hr.message, "Heartbeat rejected by server");
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to parse heartbeat response");
                            }
                        }
                    } else {
                        warn!(status = %resp.status(), "Heartbeat request failed");
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to send heartbeat");
                }
            }

            tokio::time::sleep(interval).await;
        }
    });
}

fn build_telemetry_snapshot(dashboard: &WebQueryResponse) -> TelemetrySnapshot {
    let inverters = dashboard
        .inverters
        .iter()
        .map(|inv| InverterTelemetry {
            id: inv.id.clone(),
            battery_soc: inv.battery_soc,
            battery_temperature_c: inv.battery_temperature_c,
            battery_input_energy_today_kwh: inv.battery_input_energy_today_kwh,
            battery_output_energy_today_kwh: inv.battery_output_energy_today_kwh,
            grid_import_today_kwh: inv.grid_import_today_kwh,
            grid_export_today_kwh: inv.grid_export_today_kwh,
            today_solar_energy_kwh: inv.today_solar_energy_kwh,
            total_solar_energy_kwh: inv.total_solar_energy_kwh,
            online: inv.online,
            run_mode: inv.run_mode.clone(),
            error_code: inv.error_code,
            inverter_temperature_c: inv.inverter_temperature_c,
            mode: inv.mode.clone(),
            actual_mode: inv.actual_mode.clone(),
            mode_synced: inv.mode_synced,
        })
        .collect();

    let schedule = dashboard.schedule.as_ref();
    let instance = InstanceTelemetry {
        current_mode: schedule.map_or_else(|| "unknown".to_owned(), |s| s.current_mode.clone()),
        current_reason: schedule
            .map_or_else(|| "no schedule".to_owned(), |s| s.current_reason.clone()),
        current_strategy: schedule.and_then(|s| s.current_strategy.clone()),
        expected_profit: schedule.and_then(|s| s.expected_profit),
        total_expected_profit: schedule.and_then(|s| s.total_expected_profit),
        inverter_source: dashboard.health.inverter_source,
        price_source: dashboard.health.price_source,
        errors: dashboard.health.errors.clone(),
        consumption_ema_kwh: dashboard
            .consumption_stats
            .as_ref()
            .and_then(|cs| cs.ema_kwh),
        today_import_kwh: dashboard
            .consumption_stats
            .as_ref()
            .and_then(|cs| cs.today_import_kwh),
        yesterday_import_kwh: dashboard
            .consumption_stats
            .as_ref()
            .and_then(|cs| cs.yesterday_import_kwh),
        solar_forecast_total_today_kwh: dashboard
            .solar_forecast
            .as_ref()
            .map_or(0.0, |sf| sf.total_today_kwh),
        solar_forecast_remaining_today_kwh: dashboard
            .solar_forecast
            .as_ref()
            .map_or(0.0, |sf| sf.remaining_today_kwh),
        solar_forecast_tomorrow_kwh: dashboard
            .solar_forecast
            .as_ref()
            .map_or(0.0, |sf| sf.tomorrow_kwh),
        solar_forecast_actual_today_kwh: dashboard
            .solar_forecast
            .as_ref()
            .and_then(|sf| sf.actual_today_kwh),
        solar_forecast_accuracy_percent: dashboard
            .solar_forecast
            .as_ref()
            .and_then(|sf| sf.accuracy_percent),
        hdo_low_tariff_periods: dashboard
            .hdo_schedule
            .as_ref()
            .map_or_else(Vec::new, |hdo| hdo.low_tariff_periods.clone()),
        hdo_low_tariff_czk: dashboard
            .hdo_schedule
            .as_ref()
            .map_or(0.0, |hdo| hdo.low_tariff_czk),
        hdo_high_tariff_czk: dashboard
            .hdo_schedule
            .as_ref()
            .map_or(0.0, |hdo| hdo.high_tariff_czk),
    };

    let schedule_telemetry = dashboard.prices.as_ref().map(|prices| {
        let blocks = prices
            .blocks
            .iter()
            .map(|b| ScheduleBlockTelemetry {
                timestamp: b.timestamp,
                price_czk: b.price,
                operation: b.block_type.clone(),
                target_soc: b.target_soc,
                strategy: b.strategy.clone(),
                expected_profit: b.expected_profit,
                reason: b.reason.clone(),
                is_historical: b.is_historical,
            })
            .collect::<Vec<_>>();

        ScheduleTelemetry {
            generated_at: dashboard
                .schedule
                .as_ref()
                .map_or_else(Utc::now, |s| s.schedule_generated_at),
            total_blocks: blocks.len(),
            total_expected_profit: dashboard
                .schedule
                .as_ref()
                .and_then(|s| s.total_expected_profit),
            blocks,
            price_min: prices.min_price,
            price_max: prices.max_price,
            price_avg: prices.avg_price,
            today_price_min: prices.today_min_price,
            today_price_max: prices.today_max_price,
            today_price_avg: prices.today_avg_price,
            today_price_median: prices.today_median_price,
            tomorrow_price_min: prices.tomorrow_min_price,
            tomorrow_price_max: prices.tomorrow_max_price,
            tomorrow_price_avg: prices.tomorrow_avg_price,
            tomorrow_price_median: prices.tomorrow_median_price,
        }
    });

    let soc_predictions = dashboard.battery_soc_prediction.as_ref().map(|preds| {
        preds
            .iter()
            .map(|p| SocPredictionPoint {
                timestamp: p.timestamp,
                predicted_soc: p.soc,
            })
            .collect()
    });

    TelemetrySnapshot {
        collected_at: Utc::now(),
        inverters,
        instance,
        schedule: schedule_telemetry,
        soc_predictions,
    }
}

fn build_sync_data(dashboard: &WebQueryResponse) -> ClientSyncData {
    ClientSyncData {
        battery_capacity_kwh: dashboard
            .inverters
            .first()
            .and_then(|i| i.battery_capacity_kwh),
        target_soc_max: dashboard
            .schedule
            .as_ref()
            .map_or(100.0, |s| s.target_soc_max),
        target_soc_min: dashboard
            .schedule
            .as_ref()
            .map_or(10.0, |s| s.target_soc_min),
    }
}
