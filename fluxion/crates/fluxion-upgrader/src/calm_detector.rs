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

//! Calm detector module - waits for safe-to-upgrade conditions

use crate::error::{Result, UpgraderError};
use std::time::Duration;

pub enum CalmResult {
    /// System is calm, safe to upgrade
    Calm,
    /// Timed out waiting for calm, forcing upgrade
    Timeout,
}

pub async fn wait_for_calm(port: u16, max_wait_hours: u64) -> Result<CalmResult> {
    let max_wait = Duration::from_secs(max_wait_hours * 3600);
    let poll_interval = Duration::from_secs(60);

    tracing::info!("Waiting for calm time (max wait: {}h)", max_wait_hours);

    let start = std::time::Instant::now();

    loop {
        // Check for timeout
        if start.elapsed() >= max_wait {
            tracing::warn!("Calm timeout reached, forcing upgrade");
            return Ok(CalmResult::Timeout);
        }

        // Check fluxion status
        if is_safe_to_upgrade(port).await? {
            tracing::info!("System is calm, safe to upgrade");
            return Ok(CalmResult::Calm);
        }

        tracing::debug!("System not calm yet, waiting...");
        tokio::time::sleep(poll_interval).await;
    }
}

async fn is_safe_to_upgrade(port: u16) -> Result<bool> {
    let url = format!("http://127.0.0.1:{}/api/upgrader/status", port);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| UpgraderError::Config(format!("Failed to build HTTP client: {e}")))?;

    #[derive(serde::Deserialize)]
    struct StatusResponse {
        safe_to_upgrade: bool,
    }

    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => {
            match response.json::<StatusResponse>().await {
                Ok(status) => Ok(status.safe_to_upgrade),
                Err(_) => Ok(false),
            }
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calm_result_variants() {
        // Test that CalmResult can be created
        let calm = CalmResult::Calm;
        let timeout = CalmResult::Timeout;

        assert!(matches!(calm, CalmResult::Calm));
        assert!(matches!(timeout, CalmResult::Timeout));
    }
}
