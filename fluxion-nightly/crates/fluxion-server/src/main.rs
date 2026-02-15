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

use axum::Router;
use axum::routing::{get, post};
use tracing::info;
use tracing_subscriber::EnvFilter;

use fluxion_server::config::ServerConfig;
use fluxion_server::dashboard::{self, DashboardState};
use fluxion_server::db::Database;
use fluxion_server::heartbeat::{self, HeartbeatState};
use fluxion_server::monitor;
use fluxion_server::notifications::EmailNotifier;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("fluxion_server=info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "server_config.toml".to_owned());
    info!(path = %config_path, "Loading configuration");
    let config = Arc::new(ServerConfig::from_file(&config_path)?);

    let db = Arc::new(Database::open(&config.database.path)?);
    info!(path = %config.database.path, "Database opened");

    let notifier = Arc::new(EmailNotifier::new(&config.email)?);

    monitor::spawn_monitor(Arc::clone(&db), Arc::clone(&config), Arc::clone(&notifier));

    // Spawn telemetry cleanup task (runs every 24 hours)
    {
        let db = Arc::clone(&db);
        let retention_days = config.database.telemetry_retention_days;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(86400));
            loop {
                interval.tick().await;
                match db.cleanup_old_telemetry(retention_days) {
                    Ok(deleted) if deleted > 0 => {
                        info!(deleted, "Cleaned up old telemetry snapshots");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to clean up old telemetry");
                    }
                    _ => {}
                }
            }
        });
    }

    let heartbeat_state = HeartbeatState {
        db: Arc::clone(&db),
        config: Arc::clone(&config),
        notifier,
    };

    let dashboard_state = DashboardState {
        db: Arc::clone(&db),
    };

    let app = Router::new()
        .route("/", get(dashboard::dashboard_handler))
        .with_state(dashboard_state)
        .route(
            "/api/heartbeat",
            post(heartbeat::heartbeat_handler).with_state(heartbeat_state),
        );

    let addr = format!("{}:{}", config.server.bind_address, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("FluxION Server listening on {addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
