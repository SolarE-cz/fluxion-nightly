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

//! GitHub API release checking module

use crate::config::UpgraderConfig;
use crate::error::{Result, UpgraderError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "amd64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "aarch64";

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub binary_url: String,
    pub checksums_url: String,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    published_at: DateTime<Utc>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

const USER_AGENT: &str = "fluxion-upgrader/0.1.0";

pub async fn check_latest_release(config: &UpgraderConfig) -> Result<Option<ReleaseInfo>> {
    let repo = config.release_branch.github_repo();
    let base_url = config
        .api_base_url
        .as_deref()
        .unwrap_or("https://api.github.com");
    let url = format!("{base_url}/repos/{repo}/releases/latest");

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| UpgraderError::ReleaseCheck(format!("Failed to build HTTP client: {e}")))?;

    let mut request = client.get(&url);

    // Add auth token for private staging repo
    if config.release_branch.is_private() {
        if let Some(ref token) = config.staging_token {
            request = request.bearer_auth(token);
        } else {
            return Err(UpgraderError::ReleaseCheck(
                "Staging token required for private repo".to_string(),
            ));
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| UpgraderError::ReleaseCheck(format!("Request failed: {e}")))?;

    // Check rate limit
    if let Some(remaining) = response.headers().get("x-ratelimit-remaining")
        && let Ok(remaining_str) = remaining.to_str()
        && let Ok(remaining_int) = remaining_str.parse::<u32>()
        && remaining_int < 10
    {
        tracing::warn!("GitHub rate limit low: {remaining_int} remaining");
    }

    if response.status().is_client_error() || response.status().is_server_error() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error body".to_string());
        return Err(UpgraderError::ReleaseCheck(format!(
            "GitHub API error {status}: {body}"
        )));
    }

    let release: GithubRelease = response
        .json()
        .await
        .map_err(|e| UpgraderError::ReleaseCheck(format!("Failed to parse response: {e}")))?;

    // Find the correct asset for our architecture
    let binary_asset = release
        .assets
        .iter()
        .find(|a| a.name == format!("fluxion-{ARCH}"))
        .ok_or_else(|| {
            UpgraderError::ReleaseCheck(format!("No binary found for architecture: {ARCH}"))
        })?;

    let checksums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .ok_or_else(|| UpgraderError::ReleaseCheck("No SHA256SUMS asset found".to_string()))?;

    Ok(Some(ReleaseInfo {
        tag: release.tag_name.clone(),
        version: crate::version::version_from_tag(&release.tag_name).to_string(),
        binary_url: binary_asset.browser_download_url.clone(),
        checksums_url: checksums_asset.browser_download_url.clone(),
        published_at: release.published_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Server, ServerGuard};
    use serde_json::json;

    fn setup_mock_release(server: &ServerGuard) -> GithubRelease {
        GithubRelease {
            tag_name: "v0.2.39".to_string(),
            html_url: format!("{}test/release", server.url()),
            published_at: Utc::now(),
            assets: vec![
                GithubAsset {
                    name: format!("fluxion-{ARCH}"),
                    browser_download_url: format!("{}/fluxion-{}", server.url(), ARCH),
                },
                GithubAsset {
                    name: "SHA256SUMS".to_string(),
                    browser_download_url: format!("{}/SHA256SUMS", server.url()),
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_check_latest_release_success() {
        let mut server = Server::new_async().await;
        let release = setup_mock_release(&server);

        let mock = server
            .mock("GET", "/repos/SolarE-cz/fluxion-nightly/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&serde_json::to_string(&release).unwrap())
            .create_async()
            .await;

        let config = UpgraderConfig {
            api_base_url: Some(server.url()),
            ..Default::default()
        };
        let result = check_latest_release(&config).await;

        assert!(result.is_ok());
        let release_info = result.unwrap().unwrap();
        assert_eq!(release_info.tag, "v0.2.39");
        assert_eq!(release_info.version, "0.2.39");

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_check_latest_release_no_assets() {
        let mut server = Server::new_async().await;
        let release = GithubRelease {
            tag_name: "v0.2.39".to_string(),
            html_url: format!("{}test/release", server.url()),
            published_at: Utc::now(),
            assets: vec![],
        };

        let mock = server
            .mock("GET", "/repos/SolarE-cz/fluxion-nightly/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&serde_json::to_string(&release).unwrap())
            .create_async()
            .await;

        let config = UpgraderConfig {
            api_base_url: Some(server.url()),
            ..Default::default()
        };
        let result = check_latest_release(&config).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            UpgraderError::ReleaseCheck(_)
        ));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_check_latest_release_rate_limited() {
        let mut server = Server::new_async().await;

        let mock = server
            .mock("GET", "/repos/SolarE-cz/fluxion-nightly/releases/latest")
            .with_status(403)
            .with_header("content-type", "application/json")
            .with_header("x-ratelimit-remaining", "0")
            .with_body(json!({"message": "API rate limit exceeded"}).to_string())
            .create_async()
            .await;

        let config = UpgraderConfig {
            api_base_url: Some(server.url()),
            ..Default::default()
        };
        let result = check_latest_release(&config).await;

        assert!(result.is_err());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_check_latest_release_private_repo() {
        let mut server = Server::new_async().await;
        let release = setup_mock_release(&server);

        let mock = server
            .mock("GET", "/repos/SolarE-cz/fluxion-staging/releases/latest")
            .match_header("authorization", "Bearer test-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&serde_json::to_string(&release).unwrap())
            .create_async()
            .await;

        let config = UpgraderConfig {
            release_branch: crate::config::ReleaseBranch::Staging,
            staging_token: Some("test-token".to_string()),
            api_base_url: Some(server.url()),
            ..Default::default()
        };
        let result = check_latest_release(&config).await;

        assert!(result.is_ok());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_check_latest_release_private_repo_no_token() {
        let _server = Server::new_async().await;

        let config = UpgraderConfig {
            release_branch: crate::config::ReleaseBranch::Staging,
            staging_token: None,
            ..Default::default()
        };
        let result = check_latest_release(&config).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            UpgraderError::ReleaseCheck(_)
        ));
    }
}
