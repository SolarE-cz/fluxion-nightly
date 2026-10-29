// SPDX-License-Identifier: CC-BY-NC-ND-4.0

//! Calm time detector for safe upgrade timing

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;
use tracing::{debug, info};

/// Fluxion status from the health check endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FluxionStatus {
    /// Version
    pub version: String,

    /// Current mode
    pub mode: String,

    /// Health status
    pub healthy: bool,

    /// Safe to upgrade flag
    pub safe_to_upgrade: bool,
}

/// Calm time detector
pub struct CalmDetector {
    client: Client,
    port: u16,
    check_interval: StdDuration,
    max_wait: Duration,
}

impl CalmDetector {
    /// Create a new calm detector
    pub fn new(port: u16, max_wait_hours: u32) -> Self {
        Self {
            client: Client::builder()
                .timeout(StdDuration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            port,
            check_interval: StdDuration::from_secs(30), // Check every 30 seconds
            max_wait: Duration::hours(max_wait_hours as i64),
        }
    }

    /// Set custom check interval
    pub fn with_check_interval(mut self, interval: StdDuration) -> Self {
        self.check_interval = interval;
        self
    }

    /// Wait for calm time before upgrading
    pub async fn wait_for_calm(&self) -> Result<()> {
        let start = Utc::now();
        let max_end = start + self.max_wait;

        info!(
            "Waiting for calm time (max wait: {} hours)",
            self.max_wait.num_hours()
        );

        loop {
            let status = self.check_status().await?;

            if status.safe_to_upgrade {
                info!("System calm, safe to upgrade");
                return Ok(());
            }

            let now = Utc::now();
            if now >= max_end {
                info!("Max calm wait time reached, proceeding anyway");
                return Ok(());
            }

            let remaining = max_end - now;
            debug!(
                "System not calm yet, {} hours remaining, will retry in {} seconds",
                remaining.num_hours(),
                self.check_interval.as_secs()
            );

            tokio::time::sleep(self.check_interval).await;
        }
    }

    /// Check the Fluxion status
    pub async fn check_status(&self) -> Result<FluxionStatus> {
        let url = format!("http://localhost:{}/api/upgrader/status", self.port);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to check Fluxion status")?;

        if !response.status().is_success() {
            anyhow::bail!("Health check failed with status: {}", response.status());
        }

        let status: FluxionStatus = response
            .json()
            .await
            .context("Failed to parse Fluxion status")?;

        Ok(status)
    }

    /// Perform a health check with timeout
    pub async fn health_check(&self, timeout: StdDuration) -> Result<FluxionStatus> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            match self.check_status().await {
                Ok(status) => {
                    if status.healthy {
                        return Ok(status);
                    }
                    anyhow::bail!("Fluxion is not healthy");
                }
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e.context("Health check timeout"));
                    }
                    debug!("Health check failed, retrying: {}", e);
                    tokio::time::sleep(StdDuration::from_secs(5)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calm_detector_new() {
        let detector = CalmDetector::new(8099, 6);
        assert_eq!(detector.port, 8099);
        assert_eq!(detector.max_wait.num_hours(), 6);
        assert_eq!(detector.check_interval.as_secs(), 30);
    }

    #[test]
    fn test_calm_detector_with_check_interval() {
        let detector = CalmDetector::new(8099, 6).with_check_interval(StdDuration::from_secs(60));
        assert_eq!(detector.check_interval.as_secs(), 60);
    }

    #[test]
    fn test_fluxion_status_serialization() {
        let status = FluxionStatus {
            version: "0.2.38".to_string(),
            mode: "all_selfuse".to_string(),
            healthy: true,
            safe_to_upgrade: true,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"healthy\":true"));
        assert!(json.contains("\"safe_to_upgrade\":true"));
    }

    #[test]
    fn test_fluxion_status_deserialization() {
        let json = r#"{
            "version": "0.2.38",
            "mode": "all_selfuse",
            "healthy": true,
            "safe_to_upgrade": true
        }"#;

        let status: FluxionStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.version, "0.2.38");
        assert_eq!(status.mode, "all_selfuse");
        assert!(status.healthy);
        assert!(status.safe_to_upgrade);
    }
}
