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

//! Telemetry module for reporting upgrade events

use crate::error::Result;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub enum UpgradeEvent {
    UpgradeStarted {
        from_version: String,
        to_version: String,
    },
    UpgradeCompleted {
        from_version: String,
        to_version: String,
        duration_secs: u64,
    },
    UpgradeFailed {
        from_version: String,
        to_version: String,
        error: String,
    },
    RollbackStarted {
        from_version: String,
        to_version: String,
    },
    RollbackCompleted {
        restored_version: String,
    },
}

#[allow(dead_code)]
const TELEMETRY_TIMEOUT: Duration = Duration::from_secs(5);

/// Report an upgrade event to the telemetry server
///
/// This is a fire-and-forget operation: errors are logged but never block
/// the upgrade flow.
pub async fn report_event(event: UpgradeEvent) -> Result<()> {
    // TODO: Server endpoint to be added later
    // For now, just log the event
    let event_json = serde_json::to_string(&event)?;
    tracing::info!("Telemetry event: {event_json}");

    // Future implementation will POST to fluxion-server
    // let server_url = std::env::var("FLUXION_SERVER_URL")
    //     .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    // let url = format!("{}/api/upgrade-event", server_url);
    //
    // let client = reqwest::Client::builder()
    //     .timeout(TELEMETRY_TIMEOUT)
    //     .build()?;
    //
    // match client.post(&url).json(&event).send().await {
    //     Ok(response) if response.status().is_success() => {
    //         tracing::debug!("Telemetry event reported successfully");
    //     }
    //     Ok(response) => {
    //         tracing::warn!("Telemetry server returned status: {}", response.status());
    //     }
    //     Err(e) => {
    //         tracing::warn!("Failed to report telemetry event: {e}");
    //     }
    // }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upgrade_event_serialization() {
        let event = UpgradeEvent::UpgradeStarted {
            from_version: "0.2.38".to_string(),
            to_version: "0.2.39".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("UpgradeStarted"));
        assert!(json.contains("0.2.38"));
        assert!(json.contains("0.2.39"));
    }

    #[test]
    fn test_upgrade_completed_serialization() {
        let event = UpgradeEvent::UpgradeCompleted {
            from_version: "0.2.38".to_string(),
            to_version: "0.2.39".to_string(),
            duration_secs: 120,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("UpgradeCompleted"));
        assert!(json.contains("120"));
    }

    #[test]
    fn test_rollback_completed_serialization() {
        let event = UpgradeEvent::RollbackCompleted {
            restored_version: "0.2.38".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("RollbackCompleted"));
    }

    #[tokio::test]
    async fn test_report_event() {
        let event = UpgradeEvent::UpgradeStarted {
            from_version: "0.2.38".to_string(),
            to_version: "0.2.39".to_string(),
        };

        // This should not fail even without a server
        let result = report_event(event).await;
        assert!(result.is_ok());
    }
}
