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

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::ServerConfig;
use crate::db::Database;
use crate::notifications::EmailNotifier;

pub fn spawn_monitor(
    db: Arc<Database>,
    config: Arc<ServerConfig>,
    notifier: Arc<EmailNotifier>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        info!("Heartbeat monitor started (checking every 60s)");

        loop {
            interval.tick().await;

            let clients = match db.get_all_clients() {
                Ok(c) => c,
                Err(e) => {
                    error!(error = %e, "Monitor: failed to fetch clients");
                    continue;
                }
            };

            let now = Utc::now();
            let expected_secs = config.heartbeat.expected_interval_secs;
            let threshold_secs = expected_secs * u64::from(config.heartbeat.miss_threshold);
            let warning_secs = expected_secs * 2;

            for client in &clients {
                let elapsed: u64 = now
                    .signed_duration_since(client.last_seen)
                    .num_seconds()
                    .try_into()
                    .unwrap_or(0);

                if elapsed > threshold_secs && client.status != "offline" {
                    // Transition to offline
                    if let Err(e) = db.update_client_status(&client.instance_id, "offline") {
                        error!(error = %e, instance_id = %client.instance_id, "Failed to update status to offline");
                        continue;
                    }

                    warn!(
                        instance_id = %client.instance_id,
                        elapsed_secs = elapsed,
                        threshold_secs = threshold_secs,
                        "Client marked offline"
                    );

                    // Check dedup: don't send if notified within the last hour
                    let recently_notified = db
                        .last_notification_for(&client.instance_id, "offline")
                        .is_some_and(|last| now.signed_duration_since(last).num_seconds() < 3600);

                    if !recently_notified {
                        let friendly_name = client
                            .friendly_name
                            .as_deref()
                            .unwrap_or(&client.instance_id);
                        let last_seen = client.last_seen.to_rfc3339();

                        if let Err(e) = notifier
                            .send_offline_alert(&client.instance_id, friendly_name, &last_seen)
                            .await
                        {
                            error!(error = %e, "Failed to send offline alert");
                        }

                        if let Err(e) = db.log_notification(
                            &client.instance_id,
                            "offline",
                            &notifier.recipients(),
                        ) {
                            error!(error = %e, "Failed to log offline notification");
                        }
                    }
                } else if elapsed > warning_secs && client.status == "online" {
                    // Transition to warning
                    if let Err(e) = db.update_client_status(&client.instance_id, "warning") {
                        error!(error = %e, instance_id = %client.instance_id, "Failed to update status to warning");
                    } else {
                        info!(
                            instance_id = %client.instance_id,
                            elapsed_secs = elapsed,
                            "Client status changed to warning"
                        );
                    }
                }
            }
        }
    })
}
