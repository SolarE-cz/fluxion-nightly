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

//! FluxION Upgrader - Entry point for the upgrader binary
//!
//! This binary runs as PID 1 in the Docker container, managing the fluxion
//! binary lifecycle and handling over-the-air updates via GitHub Releases.

use fluxion_upgrader::backup::{create_backup, remove_backup};
use fluxion_upgrader::calm_detector::wait_for_calm;
use fluxion_upgrader::config::load_config;
use fluxion_upgrader::release_checker::check_latest_release;
use fluxion_upgrader::state::{load_state, save_state};
use fluxion_upgrader::supervisor::Supervisor;
use fluxion_upgrader::telemetry::{UpgradeEvent, report_event};
use fluxion_upgrader::{UpgraderError, is_newer};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::{Instant, sleep};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fluxion_upgrader=debug".parse().unwrap()),
        )
        .init();

    info!("Starting FluxION Upgrader");

    // Load config
    let config = load_config()?;
    info!(
        "Loaded config: auto_update={}, release_branch={:?}",
        config.auto_update, config.release_branch
    );

    // Load state
    let mut state = load_state()?;
    info!("Loaded state: current_version={:?}", state.current_version);

    // Resolve binary path and supervisor
    let mut supervisor = Supervisor::new(config.fluxion_port);

    // Ensure data directory exists
    tokio::fs::create_dir_all("/data").await?;

    // Start fluxion
    supervisor.start().await?;
    info!("Fluxion started successfully");

    // Set up signal handling
    let shutdown_notify = Arc::new(Notify::new());
    let shutdown_notify_clone = shutdown_notify.clone();

    tokio::spawn(async move {
        setup_signal_handlers(shutdown_notify_clone).await;
    });

    // Main loop
    let mut check_interval = tokio::time::interval(Duration::from_secs(config.check_interval_secs));
    let mut backup_cleanup_interval = tokio::time::interval(Duration::from_secs(3600)); // Every hour

    loop {
        tokio::select! {
            _ = shutdown_notify.notified() => {
                info!("Shutdown signal received");
                supervisor.stop().await?;
                save_state(&state)?;
                info!("Shutting down");
                break;
            }
            _ = check_interval.tick() => {
                if !config.auto_update {
                    continue;
                }

                if let Err(e) = run_upgrade_cycle(&config, &mut supervisor, &mut state).await {
                    error!("Upgrade cycle error: {e}");
                }
            }
            _ = backup_cleanup_interval.tick() => {
                if let Err(e) = cleanup_backup_if_expired(&mut state).await {
                    warn!("Backup cleanup error: {e}");
                }
            }
        }
    }

    Ok(())
}

async fn run_upgrade_cycle(
    config: &fluxion_upgrader::UpgraderConfig,
    supervisor: &mut Supervisor,
    state: &mut fluxion_upgrader::UpgraderState,
) -> Result<(), UpgraderError> {
    info!("Running upgrade cycle");

    // Check for new release
    state.last_check_at = Some(chrono::Utc::now());
    let release = match check_latest_release(config).await? {
        Some(r) => r,
        None => {
            warn!("No release found");
            return Ok(());
        }
    };

    info!("Latest release: {}", release.version);

    // Check if upgrade is needed
    let current_version = state
        .current_version
        .as_deref()
        .unwrap_or("0.0.0")
        .to_string();
    if !is_newer(&current_version, &release.version)? {
        info!("Already on latest version: {}", current_version);
        return Ok(());
    }

    // Check if we should skip this version (too many failures)
    if let Some(ref failed_version) = state.failed_version
        && failed_version == &release.version
        && state.consecutive_failures >= 3
    {
        warn!("Skipping version {}, too many failures", release.version);
        return Ok(());
    }

    info!(
        "New version available: {} (current: {})",
        release.version, current_version
    );

    // Download binary
    info!("Downloading version {}", release.version);
    let binary_path = fluxion_upgrader::downloader::download_and_verify(&release, config).await?;
    info!("Binary downloaded and verified");

    // Wait for calm time
    let calm_result = wait_for_calm(config.fluxion_port, config.max_calm_wait_hours).await?;
    match calm_result {
        fluxion_upgrader::calm_detector::CalmResult::Calm => {
            info!("System is calm, proceeding with upgrade");
        }
        fluxion_upgrader::calm_detector::CalmResult::Timeout => {
            warn!("Calm timeout reached, forcing upgrade");
        }
    }

    // Create backup
    info!("Creating backup");
    let backup = create_backup(&current_version)?;
    state.backup_version = Some(backup.version.clone());
    state.backup_path = Some(backup.path.to_string_lossy().to_string());
    save_state(state)?;

    // Report upgrade started
    let _ = report_event(UpgradeEvent::UpgradeStarted {
        from_version: current_version.to_string(),
        to_version: release.version.clone(),
    })
    .await;

    // Perform upgrade
    info!(
        "Performing upgrade from {} to {}",
        current_version, release.version
    );
    let upgrade_start = Instant::now();

    // Stop fluxion
    supervisor.stop().await?;

    // Replace binary
    let new_binary_path = std::path::Path::new("/data/fluxion");
    tokio::fs::rename(&binary_path, new_binary_path).await?;

    // Start new fluxion
    supervisor.start().await?;

    // Health check with timeout
    info!("Checking health of new version");
    match supervisor.health_check_with_timeout(120).await {
        Ok(fluxion_upgrader::supervisor::HealthStatus::Healthy { .. }) => {
            let duration = upgrade_start.elapsed().as_secs();
            info!("Upgrade successful in {}s", duration);

            // Update state
            state.current_version = Some(release.version.clone());
            state.last_upgrade_at = Some(chrono::Utc::now());
            state.upgrade_success_since = Some(chrono::Utc::now());
            state.consecutive_failures = 0;
            save_state(state)?;

            // Report upgrade completed
            let _ = report_event(UpgradeEvent::UpgradeCompleted {
                from_version: current_version.to_string(),
                to_version: release.version.clone(),
                duration_secs: duration,
            })
            .await;
        }
        Err(e) => {
            error!("Health check failed after upgrade: {e}");

            // Report upgrade failed
            let _ = report_event(UpgradeEvent::UpgradeFailed {
                from_version: current_version.to_string(),
                to_version: release.version.clone(),
                error: e.to_string(),
            })
            .await;

            // Perform rollback
            warn!("Performing rollback to {}", current_version);
            let _ = report_event(UpgradeEvent::RollbackStarted {
                from_version: release.version.clone(),
                to_version: current_version.to_string(),
            })
            .await;

            if let Err(rollback_err) = perform_rollback(supervisor, state).await {
                error!("Rollback failed: {rollback_err}");
            } else {
                let _ = report_event(UpgradeEvent::RollbackCompleted {
                    restored_version: current_version.to_string(),
                })
                .await;
            }
        }
        Ok(fluxion_upgrader::supervisor::HealthStatus::Unhealthy)
        | Ok(fluxion_upgrader::supervisor::HealthStatus::Unreachable) => {
            error!("Fluxion is unhealthy after upgrade");
            // Trigger rollback
            warn!("Performing rollback to {}", current_version);
            let _ = perform_rollback(supervisor, state).await;
        }
    }

    Ok(())
}

async fn cleanup_backup_if_expired(
    state: &mut fluxion_upgrader::UpgraderState,
) -> Result<(), UpgraderError> {
    if state.upgrade_success_since.is_none() {
        return Ok(());
    }

    // Check if we've been stable for 24 hours
    let success_duration =
        chrono::Utc::now().signed_duration_since(state.upgrade_success_since.unwrap());
    if success_duration.num_hours() < 24 {
        return Ok(());
    }

    // Remove backup if exists
    if let Some(ref backup_path) = state.backup_path {
        let path = std::path::Path::new(backup_path);
        if path.exists() {
            remove_backup(path)?;
            state.backup_path = None;
            state.backup_version = None;
            save_state(state)?;
            info!("Backup removed after 24h of stable operation");
        }
    }

    Ok(())
}

async fn perform_rollback(
    supervisor: &mut Supervisor,
    state: &mut fluxion_upgrader::UpgraderState,
) -> Result<(), UpgraderError> {
    // Save failed version
    if let Some(ref current) = state.current_version {
        state.failed_version = Some(current.clone());
    }

    // Use the rollback module
    fluxion_upgrader::rollback::perform_rollback(supervisor, state).await?;

    // Report rollback completed
    let _ = report_event(UpgradeEvent::RollbackCompleted {
        restored_version: state.current_version.clone().unwrap_or_default(),
    })
    .await;

    Ok(())
}

async fn setup_signal_handlers(shutdown_notify: Arc<Notify>) {
    let _shutdown = Arc::clone(&shutdown_notify);

    // SIGTERM handler
    unsafe {
        let handler = SigHandler::Handler(sigterm_handler);
        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());
        let _ = signal::sigaction(Signal::SIGTERM, &action);
    }

    // SIGHUP handler
    unsafe {
        let handler = SigHandler::Handler(sighup_handler);
        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());
        let _ = signal::sigaction(Signal::SIGHUP, &action);
    }

    // Keep the task alive
    loop {
        sleep(Duration::from_secs(3600)).await;
    }
}

extern "C" fn sigterm_handler(_signal: nix::libc::c_int) {
    info!("SIGTERM received");
    std::process::exit(0);
}

extern "C" fn sighup_handler(_signal: nix::libc::c_int) {
    info!("SIGHUP received - fluxion should restart");
    // The main loop handles this by checking for process status
}
