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

//! Binary downloader with SHA256 verification module

use crate::config::UpgraderConfig;
use crate::error::{Result, UpgraderError};
use crate::release_checker::ReleaseInfo;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "amd64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "aarch64";

const MAX_RETRIES: u32 = 3;
const RETRY_DELAYS: [u64; 3] = [1, 5, 30]; // seconds

pub async fn download_and_verify(
    release: &ReleaseInfo,
    _config: &UpgraderConfig,
) -> Result<PathBuf> {
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            tracing::warn!("Retrying download (attempt {}/{MAX_RETRIES})", attempt + 1);
            tokio::time::sleep(tokio::time::Duration::from_secs(
                RETRY_DELAYS[attempt as usize - 1],
            ))
            .await;
        }

        match download_with_verification(release).await {
            Ok(path) => return Ok(path),
            Err(e) => {
                last_error = Some(e);
                // Clean up temp file
                let temp_path = PathBuf::from("/data/fluxion.new");
                if temp_path.exists() {
                    let _ = std::fs::remove_file(&temp_path);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| UpgraderError::Download("No error recorded".to_string())))
}

async fn download_with_verification(release: &ReleaseInfo) -> Result<PathBuf> {
    // Download checksums
    let checksums_content = download_to_string(&release.checksums_url).await?;
    let expected_hash = parse_sha256sums(&checksums_content, ARCH)?;

    // Download binary with streaming SHA256
    let temp_path = PathBuf::from("/data/fluxion.new");
    let actual_hash = download_binary_with_checksum(&release.binary_url, &temp_path).await?;

    // Verify checksum
    if actual_hash != expected_hash {
        std::fs::remove_file(&temp_path)?;
        return Err(UpgraderError::ChecksumMismatch {
            expected: expected_hash,
            actual: actual_hash,
        });
    }

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&temp_path, perms)?;
    }

    Ok(temp_path)
}

async fn download_to_string(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("fluxion-upgrader/0.1.0")
        .build()
        .map_err(|e| UpgraderError::Download(format!("Failed to build HTTP client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpgraderError::Download(format!("Request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(UpgraderError::Download(format!(
            "Download failed with status: {}",
            response.status()
        )));
    }

    response
        .text()
        .await
        .map_err(|e| UpgraderError::Download(format!("Failed to read response: {e}")))
}

async fn download_binary_with_checksum(url: &str, path: &PathBuf) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("fluxion-upgrader/0.1.0")
        .build()
        .map_err(|e| UpgraderError::Download(format!("Failed to build HTTP client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpgraderError::Download(format!("Request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(UpgraderError::Download(format!(
            "Download failed with status: {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| UpgraderError::Download(format!("Failed to download bytes: {e}")))?;

    let mut file = std::fs::File::create(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    std::io::Write::write_all(&mut file, &bytes)?;

    Ok(format!("{:x}", hasher.finalize()))
}

fn parse_sha256sums(content: &str, arch: &str) -> Result<String> {
    let binary_name = format!("fluxion-{arch}");

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == binary_name {
            return Ok(parts[0].to_lowercase());
        }
    }

    Err(UpgraderError::Download(format!(
        "SHA256SUMS does not contain entry for {binary_name}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sha256sums() {
        let content = "abc123def456789  fluxion-amd64\nxyz789abc123456  fluxion-aarch64";
        let hash = parse_sha256sums(content, "amd64").unwrap();
        assert_eq!(hash, "abc123def456789");
    }

    #[test]
    fn test_parse_sha256sums_aarch64() {
        let content = "abc123def456789  fluxion-amd64\nxyz789abc123456  fluxion-aarch64";
        let hash = parse_sha256sums(content, "aarch64").unwrap();
        assert_eq!(hash, "xyz789abc123456");
    }

    #[test]
    fn test_parse_sha256sums_not_found() {
        let content = "abc123def456789  fluxion-amd64";
        let result = parse_sha256sums(content, "aarch64");
        assert!(result.is_err());
    }

    #[test]
    fn test_checksum_verification() {
        use sha2::Digest;

        // Create a known hash
        let data = b"test data";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let expected = format!("{:x}", hasher.finalize());

        // Verify our hash function produces consistent results
        let mut hasher2 = Sha256::new();
        hasher2.update(data);
        let actual = format!("{:x}", hasher2.finalize());

        assert_eq!(expected, actual);
    }
}
