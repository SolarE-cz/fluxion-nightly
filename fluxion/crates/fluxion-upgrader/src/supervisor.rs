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

//! Process supervisor module for managing the fluxion child process

use crate::error::{Result, UpgraderError};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(30);
const CRASH_RETRY_DELAY: Duration = Duration::from_secs(5);
const CIRCUIT_BREAKER_LIMIT: usize = 5;
const CIRCUIT_BREAKER_WINDOW: Duration = Duration::from_secs(600); // 10 minutes

pub enum HealthStatus {
    Healthy {
        version: String,
        safe_to_upgrade: bool,
    },
    Unhealthy,
    Unreachable,
}

pub struct Supervisor {
    child: Option<Child>,
    binary_path: PathBuf,
    port: u16,
    crash_times: Vec<Instant>,
}

impl Supervisor {
    pub fn new(port: u16) -> Self {
        let binary_path = Self::resolve_binary_path();
        Self {
            child: None,
            binary_path,
            port,
            crash_times: Vec::new(),
        }
    }

    fn resolve_binary_path() -> PathBuf {
        // Check /data/fluxion first (downloaded binary)
        let primary = PathBuf::from("/data/fluxion");
        if primary.exists() {
            return primary;
        }

        // Fallback to bundled binary
        PathBuf::from("/usr/local/bin/fluxion-main")
    }

    /// Start fluxion binary as child process
    pub async fn start(&mut self) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }

        tracing::info!("Starting fluxion from {}", self.binary_path.display());

        let child = std::process::Command::new(&self.binary_path)
            .spawn()
            .map_err(|e| UpgraderError::Process(format!("Failed to start fluxion: {e}")))?;

        self.child = Some(child);

        // Wait a bit for the process to start
        tokio::time::sleep(Duration::from_secs(2)).await;

        if !self.is_running() {
            return Err(UpgraderError::Process(
                "Fluxion exited immediately".to_string(),
            ));
        }

        Ok(())
    }

    /// Graceful stop: SIGTERM -> 30s timeout -> SIGKILL
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            tracing::info!("Stopping fluxion (PID {})", child.id());

            // Try graceful shutdown first
            let pid = Pid::from_raw(child.id() as i32);
            let _ = signal::kill(pid, Signal::SIGTERM);

            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        tracing::info!("Fluxion stopped gracefully");
                        return Ok(());
                    }
                    Ok(None) => {
                        if start.elapsed() >= GRACEFUL_TIMEOUT {
                            tracing::warn!("Fluxion did not stop gracefully, killing");
                            let _ = child.kill();
                            let _ = child.wait();
                            return Ok(());
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        tracing::warn!("Error waiting for child: {e}");
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Restart (stop + start)
    pub async fn restart(&mut self) -> Result<()> {
        self.stop().await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.start().await
    }

    /// Check if child is still running
    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(_) => false,
            }
        } else {
            false
        }
    }

    /// Health check via HTTP
    pub async fn health_check(&self) -> Result<HealthStatus> {
        let url = format!("http://127.0.0.1:{}/api/upgrader/status", self.port);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| UpgraderError::Process(format!("Failed to build HTTP client: {e}")))?;

        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                #[derive(serde::Deserialize)]
                struct StatusResponse {
                    version: String,
                    healthy: bool,
                    safe_to_upgrade: bool,
                }

                match response.json::<StatusResponse>().await {
                    Ok(status) => {
                        if status.healthy {
                            Ok(HealthStatus::Healthy {
                                version: status.version,
                                safe_to_upgrade: status.safe_to_upgrade,
                            })
                        } else {
                            Ok(HealthStatus::Unhealthy)
                        }
                    }
                    Err(_) => Ok(HealthStatus::Unhealthy),
                }
            }
            Ok(_) => Ok(HealthStatus::Unhealthy),
            Err(_) => Ok(HealthStatus::Unreachable),
        }
    }

    /// Check if circuit breaker is triggered
    pub fn is_circuit_breaker_open(&mut self) -> bool {
        let now = Instant::now();
        // Remove crashes older than the window
        self.crash_times
            .retain(|&t| now.duration_since(t) < CIRCUIT_BREAKER_WINDOW);
        self.crash_times.len() >= CIRCUIT_BREAKER_LIMIT
    }

    /// Record a crash
    pub fn record_crash(&mut self) {
        self.crash_times.push(Instant::now());
        tracing::warn!(
            "Recorded crash. Total in window: {}/{}",
            self.crash_times.len(),
            CIRCUIT_BREAKER_LIMIT
        );
    }

    /// Reset circuit breaker
    pub fn reset_circuit_breaker(&mut self) {
        self.crash_times.clear();
    }

    /// Wait for child to exit, restarting if crashed
    pub async fn supervise(&mut self) -> Result<()> {
        loop {
            if let Some(mut child) = self.child.take() {
                match child.wait() {
                    Ok(status) => {
                        tracing::warn!("Fluxion exited with status: {status}");
                        self.record_crash();

                        if self.is_circuit_breaker_open() {
                            return Err(UpgraderError::Process(
                                "Circuit breaker triggered: too many crashes".to_string(),
                            ));
                        }

                        // Wait before restart
                        tokio::time::sleep(CRASH_RETRY_DELAY).await;

                        // Restart
                        self.start().await?;
                    }
                    Err(e) => {
                        tracing::error!("Error waiting for child: {e}");
                        return Err(UpgraderError::Process(format!("Child wait error: {e}")));
                    }
                }
            } else {
                // No child running, start one
                self.start().await?;
            }
        }
    }

    /// Check health with timeout
    pub async fn health_check_with_timeout(&self, timeout_secs: u64) -> Result<HealthStatus> {
        let timeout = Duration::from_secs(timeout_secs);
        let start = Instant::now();

        loop {
            match self.health_check().await {
                Ok(HealthStatus::Healthy { .. }) => {
                    return Ok(HealthStatus::Healthy {
                        version: "0.2.38".to_string(), // Placeholder
                        safe_to_upgrade: true,
                    });
                }
                Ok(_) => {
                    if start.elapsed() >= timeout {
                        return Err(UpgraderError::HealthCheckTimeout { timeout_secs });
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(_e) => {
                    if start.elapsed() >= timeout {
                        return Err(UpgraderError::HealthCheckTimeout { timeout_secs });
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}
