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
use fluxion_core::WebQuerySender;
use fluxion_shared::heartbeat::{HeartbeatRequest, HeartbeatResponse, HeartbeatStatus};
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

        loop {
            // Query current system state for heartbeat payload
            let (strategy_name, battery_soc) = match query_sender.query_dashboard().await {
                Ok(dashboard) => {
                    let strategy = dashboard
                        .schedule
                        .as_ref()
                        .and_then(|s| s.current_strategy.clone());
                    let soc = dashboard.inverters.first().map(|i| i.battery_soc);
                    (strategy, soc)
                }
                Err(e) => {
                    warn!(error = %e, "Failed to query dashboard for heartbeat");
                    (None, None)
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
                telemetry: None,
                sync_data: None,
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
