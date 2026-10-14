// SPDX-License-Identifier: CC-BY-NC-ND-4.0

//! GitHub release checker

use crate::config::ReleaseBranch;
use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::{Deserialize, Serialize};

/// GitHub release asset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    /// Asset name (e.g., "fluxion-amd64")
    pub name: String,
    /// Download URL
    pub browser_download_url: String,
    /// Asset size in bytes
    pub size: i64,
}

/// GitHub release information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// Tag name (e.g., "v0.2.38")
    pub tag_name: String,
    /// Version without the 'v' prefix
    #[serde(skip)]
    pub version: String,
    /// Release name
    pub name: Option<String>,
    /// Release notes
    pub body: Option<String>,
    /// Whether this is a prerelease
    #[serde(default)]
    pub prerelease: bool,
    /// Release assets
    pub assets: Vec<ReleaseAsset>,
    /// Published timestamp
    pub published_at: String,
}

impl Release {
    /// Create a new Release from tag name
    pub fn from_tag(tag: impl Into<String>) -> Self {
        let tag = tag.into();
        let version = tag.strip_prefix('v').unwrap_or(&tag).to_string();

        Self {
            version,
            tag_name: tag,
            name: None,
            body: None,
            prerelease: false,
            assets: Vec::new(),
            published_at: Utc::now().to_rfc3339(),
        }
    }

    /// Get the asset for a specific architecture
    pub fn get_asset(&self, arch: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name.contains(arch))
    }

    /// Check if this release has the SHA256SUMS file
    pub fn has_checksums(&self) -> bool {
        self.assets.iter().any(|a| a.name == "SHA256SUMS")
    }

    /// Get the SHA256SUMS asset URL
    pub fn get_checksums_url(&self) -> Option<String> {
        self.assets
            .iter()
            .find(|a| a.name == "SHA256SUMS")
            .map(|a| a.browser_download_url.clone())
    }
}

/// Release checker for GitHub releases
pub struct ReleaseChecker {
    client: reqwest::Client,
    owner: String,
    repo: String,
    token: Option<String>,
}

impl ReleaseChecker {
    /// Create a new release checker for a repository
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("fluxion-upgrader/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            owner: owner.into(),
            repo: repo.into(),
            token: None,
        }
    }

    /// Create a new release checker for a release branch
    pub fn from_branch(branch: ReleaseBranch, token: Option<String>) -> Self {
        Self::new(branch.repo_owner(), branch.repo_name()).with_token(token)
    }

    /// Set authentication token for private repos
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Get the latest release from GitHub
    pub async fn get_latest(&self) -> Result<Release> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            self.owner, self.repo
        );

        let mut request = self
            .client
            .get(&url)
            .header(ACCEPT, "application/vnd.github.v3+json");

        if let Some(ref token) = self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request.send().await.context("Failed to fetch release")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to fetch release: {} - {}", status, text);
        }

        let mut release: Release = response.json().await.context("Failed to parse release")?;

        // Strip 'v' prefix from version
        release.version = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_string();

        Ok(release)
    }

    /// Get all releases from GitHub
    pub async fn get_all(&self) -> Result<Vec<Release>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases",
            self.owner, self.repo
        );

        let mut request = self
            .client
            .get(&url)
            .header(ACCEPT, "application/vnd.github.v3+json");

        if let Some(ref token) = self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request.send().await.context("Failed to fetch releases")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to fetch releases: {} - {}", status, text);
        }

        let mut releases: Vec<Release> =
            response.json().await.context("Failed to parse releases")?;

        // Strip 'v' prefix from versions
        for release in &mut releases {
            release.version = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name)
                .to_string();
        }

        Ok(releases)
    }

    /// Get a specific release by tag
    pub async fn get_by_tag(&self, tag: &str) -> Result<Release> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/tags/{}",
            self.owner, self.repo, tag
        );

        let mut request = self
            .client
            .get(&url)
            .header(ACCEPT, "application/vnd.github.v3+json");

        if let Some(ref token) = self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request.send().await.context("Failed to fetch release")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to fetch release: {} - {}", status, text);
        }

        let mut release: Release = response.json().await.context("Failed to parse release")?;

        // Strip 'v' prefix from version
        release.version = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_string();

        Ok(release)
    }

    /// Download an asset to a file
    pub async fn download_asset(&self, url: &str, path: &str) -> Result<()> {
        let mut request = self.client.get(url);

        if let Some(ref token) = self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request.send().await.context("Failed to download asset")?;

        if !response.status().is_success() {
            let status = response.status();
            anyhow::bail!("Failed to download asset: {}", status);
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read asset bytes")?;

        tokio::fs::write(path, bytes)
            .await
            .context("Failed to write asset file")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_release_from_tag() {
        let release = Release::from_tag("v0.2.38");
        assert_eq!(release.tag_name, "v0.2.38");
        assert_eq!(release.version, "0.2.38");
    }

    #[test]
    fn test_release_from_tag_no_prefix() {
        let release = Release::from_tag("0.2.38");
        assert_eq!(release.tag_name, "0.2.38");
        assert_eq!(release.version, "0.2.38");
    }

    #[test]
    fn test_release_get_asset() {
        let mut release = Release::from_tag("v0.2.38");
        release.assets.push(ReleaseAsset {
            name: "fluxion-amd64".to_string(),
            browser_download_url: "https://example.com/fluxion-amd64".to_string(),
            size: 1000,
        });

        assert!(release.get_asset("amd64").is_some());
        assert!(release.get_asset("aarch64").is_none());
    }

    #[test]
    fn test_release_has_checksums() {
        let mut release = Release::from_tag("v0.2.38");
        assert!(!release.has_checksums());

        release.assets.push(ReleaseAsset {
            name: "SHA256SUMS".to_string(),
            browser_download_url: "https://example.com/SHA256SUMS".to_string(),
            size: 100,
        });

        assert!(release.has_checksums());
    }

    #[test]
    fn test_release_checker_new() {
        let checker = ReleaseChecker::new("SolarE-cz", "fluxion");
        assert_eq!(checker.owner, "SolarE-cz");
        assert_eq!(checker.repo, "fluxion");
        assert!(checker.token.is_none());
    }

    #[test]
    fn test_release_checker_with_token() {
        let checker =
            ReleaseChecker::new("SolarE-cz", "fluxion").with_token(Some("token".to_string()));
        assert_eq!(checker.token, Some("token".to_string()));
    }

    #[test]
    fn test_release_checker_from_branch() {
        let checker = ReleaseChecker::from_branch(ReleaseBranch::Nightly, None);
        assert_eq!(checker.owner, "SolarE-cz");
        assert_eq!(checker.repo, "fluxion-nightly");
        assert!(checker.token.is_none());
    }

    #[test]
    fn test_release_checker_from_branch_staging() {
        let checker =
            ReleaseChecker::from_branch(ReleaseBranch::Staging, Some("token".to_string()));
        assert_eq!(checker.owner, "SolarE-cz");
        assert_eq!(checker.repo, "fluxion-staging");
        assert_eq!(checker.token, Some("token".to_string()));
    }
}
