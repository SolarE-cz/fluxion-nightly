// SPDX-License-Identifier: CC-BY-NC-ND-4.0

//! Main upgrader that orchestrates the upgrade process

use crate::{
    backup::BackupManager, calm::CalmDetector, config::Config, downloader::Downloader,
    process::ProcessSupervisor, release::ReleaseChecker, state::State,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::{interval, sleep};
use tracing::{error, info};

/// Main upgrader that manages the upgrade lifecycle
pub struct Upgrader {
    config: Config,
    state: State,
    state_path: PathBuf,
    data_dir: PathBuf,
    release_checker: ReleaseChecker,
    downloader: Downloader,
    process_supervisor: ProcessSupervisor,
    calm_detector: CalmDetector,
    backup_manager: BackupManager,
}

impl Upgrader {
    /// Create a new upgrader instance
    pub async fn new(config: Config) -> Result<Self> {
        let data_dir = PathBuf::from("/data");
        let state_path = data_dir.join("upgrader_state.json");
        let binary_path = data_dir.join("fluxion");

        // Load or create state
        let state = State::load_or_create(&state_path, "0.2.38")?;

        // Create release checker
        let release_checker =
            ReleaseChecker::from_branch(config.release_branch, config.staging_token.clone());

        // Create calm detector
        let calm_detector = CalmDetector::new(config.fluxion_port, config.max_calm_wait_hours);

        // Create backup manager
        let backup_manager = BackupManager::new(&data_dir);

        // Create process supervisor
        let process_supervisor = ProcessSupervisor::new(&binary_path);

        Ok(Self {
            config,
            state,
            state_path,
            data_dir,
            release_checker,
            downloader: Downloader::new(),
            process_supervisor,
            calm_detector,
            backup_manager,
        })
    }

    /// Run the upgrader main loop
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting FluxION upgrader...");

        // Start the child process
        self.process_supervisor.start()?;
        info!("FluxION child process started");

        // Main update loop
        let mut check_interval = interval(Duration::from_secs(self.config.check_interval_secs));
        loop {
            check_interval.tick().await;

            if !self.config.auto_update {
                continue;
            }

            if let Err(e) = self.check_and_upgrade().await {
                error!("Upgrade check failed: {}", e);
            }
        }
    }

    /// Check for updates and perform upgrade if available
    async fn check_and_upgrade(&mut self) -> Result<()> {
        // Record check time
        self.state.record_check();

        // Get latest release
        let release = self.release_checker.get_latest().await?;
        info!("Latest release: {}", release.tag_name);

        // Check if we should skip this version
        if self.state.should_skip_version(&release.version, 3) {
            info!(
                "Skipping version {} due to previous failures",
                release.version
            );
            return Ok(());
        }

        // Check if we're already on this version
        if release.version == self.state.current_version {
            info!("Already on latest version: {}", release.version);
            return Ok(());
        }

        info!(
            "New version available: {} (current: {})",
            release.version, self.state.current_version
        );

        // Download binary
        self.download_binary(&release).await?;

        // Wait for calm time
        self.calm_detector.wait_for_calm().await?;

        // Perform upgrade
        self.perform_upgrade(&release).await?;

        Ok(())
    }

    /// Download the binary for a release
    async fn download_binary(&self, release: &crate::release::Release) -> Result<()> {
        info!("Downloading binaries for release {}", release.tag_name);

        // Determine architecture
        let arch = if cfg!(target_arch = "aarch64") || cfg!(target_arch = "arm") {
            "aarch64"
        } else {
            "amd64"
        };

        // Get asset for this architecture
        let asset = release
            .get_asset(arch)
            .ok_or_else(|| anyhow::anyhow!("No binary found for architecture {}", arch))?;

        // Download checksums
        let checksums_url = release
            .get_checksums_url()
            .ok_or_else(|| anyhow::anyhow!("Release does not contain SHA256SUMS"))?;

        let checksums_path = self.data_dir.join("SHA256SUMS");
        self.downloader
            .download(&checksums_url, &checksums_path)
            .await?;

        // Parse checksums
        let checksums_content = tokio::fs::read_to_string(&checksums_path).await?;
        let checksums = Downloader::parse_checksums(&checksums_content)?;
        let expected_checksum = Downloader::find_checksum(&checksums, &asset.name)
            .ok_or_else(|| anyhow::anyhow!("Checksum not found for {}", asset.name))?;

        info!(
            "Expected checksum for {}: {}",
            asset.name, expected_checksum
        );

        info!(
            "Expected checksum for {}: {}",
            asset.name, expected_checksum
        );

        // Download and verify binary
        let temp_binary_path = self.data_dir.join(format!("fluxion-{}.tmp", arch));
        self.downloader
            .download_and_verify(
                &asset.browser_download_url,
                &temp_binary_path,
                &Some(expected_checksum),
            )
            .await?;

        // Move to final location
        let binary_path = self.data_dir.join("fluxion");
        tokio::fs::rename(&temp_binary_path, &binary_path)
            .await
            .context("Failed to move binary to final location")?;

        info!("Binary downloaded and verified: {}", binary_path.display());

        Ok(())
    }

    /// Perform the upgrade
    async fn perform_upgrade(&mut self, release: &crate::release::Release) -> Result<()> {
        info!("Performing upgrade to {}", release.tag_name);

        // Create backup
        let backup_dir = self.backup_manager.create_backup_dir().await?;

        // Backup binary
        let binary_path = self.data_dir.join("fluxion");
        if binary_path.exists() {
            self.backup_manager
                .backup_file(&binary_path, &backup_dir)
                .await?;
        }

        // Backup config
        let config_path = self.data_dir.join("config.json");
        if config_path.exists() {
            self.backup_manager
                .backup_file(&config_path, &backup_dir)
                .await?;
        }

        // Backup user_control.json if exists
        let user_control_path = self.data_dir.join("user_control.json");
        if user_control_path.exists() {
            self.backup_manager
                .backup_file(&user_control_path, &backup_dir)
                .await?;
        }

        // Replace and restart
        let new_binary_path = self.data_dir.join("fluxion");
        self.process_supervisor
            .replace_and_restart(&new_binary_path)
            .await?;

        // Wait for health check
        sleep(Duration::from_secs(5)).await;
        match self
            .calm_detector
            .health_check(Duration::from_secs(120))
            .await
        {
            Ok(status) if status.healthy => {
                info!("Upgrade completed successfully to {}", release.tag_name);

                // Record successful upgrade
                let current_version = self.state.current_version.clone();
                self.state.record_upgrade(
                    &release.version,
                    &current_version,
                    backup_dir.display().to_string(),
                );
                self.state.save(&self.state_path)?;

                Ok(())
            }
            Ok(_) => {
                error!("Health check failed: FluxION not healthy");
                self.perform_rollback(backup_dir.to_path_buf()).await?;
                anyhow::bail!("Upgrade failed: FluxION not healthy after restart");
            }
            Err(e) => {
                error!("Health check failed: {}", e);
                self.perform_rollback(backup_dir.to_path_buf()).await?;
                anyhow::bail!("Upgrade failed: {}", e);
            }
        }
    }

    /// Perform rollback to previous version
    async fn perform_rollback(&mut self, backup_dir: PathBuf) -> Result<()> {
        info!("Performing rollback from backup");

        let backup_version = self
            .state
            .backup_version
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        // Restore binary
        let binary_path = self.data_dir.join("fluxion");
        self.backup_manager
            .restore_backup(&backup_dir, &binary_path)
            .await?;

        // Restart process
        self.process_supervisor.restart()?;

        // Record rollback
        self.state.record_rollback(&backup_version);
        self.state.save(&self.state_path)?;

        info!("Rollback completed to version {}", backup_version);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upgrader_binary_path() {
        let data_dir = PathBuf::from("/data");
        let binary_path = data_dir.join("fluxion");
        assert_eq!(binary_path, PathBuf::from("/data/fluxion"));
    }

    #[test]
    fn test_upgrader_state_path() {
        let data_dir = PathBuf::from("/data");
        let state_path = data_dir.join("upgrader_state.json");
        assert_eq!(state_path, PathBuf::from("/data/upgrader_state.json"));
    }
}
