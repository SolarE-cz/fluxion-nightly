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

//! Rollback module for restoring previous versions

use crate::error::{Result, UpgraderError};
use crate::state::UpgraderState;
use crate::supervisor::Supervisor;
use std::fs;
use std::path::Path;

/// Restore all files from backup and restart the old binary
pub async fn perform_rollback(
    supervisor: &mut Supervisor,
    state: &mut UpgraderState,
) -> Result<()> {
    tracing::warn!("Starting rollback to previous version");

    let backup_path = state
        .backup_path
        .as_ref()
        .ok_or_else(|| UpgraderError::Rollback("No backup path in state".to_string()))?;

    let backup_dir = Path::new(backup_path);
    if !backup_dir.exists() {
        return Err(UpgraderError::Rollback(format!(
            "Backup directory not found: {}",
            backup_dir.display()
        )));
    }

    // Stop current (broken) fluxion process
    tracing::info!("Stopping current fluxion process");
    supervisor.stop().await?;

    // Restore fluxion binary
    let fluxion_src = backup_dir.join("fluxion");
    let fluxion_dst = Path::new("/data/fluxion");
    if fluxion_src.exists() {
        fs::copy(&fluxion_src, fluxion_dst)?;
        tracing::info!("Restored fluxion binary");
    } else {
        return Err(UpgraderError::Rollback(
            "No fluxion binary in backup".to_string(),
        ));
    }

    // Restore config.json if it was backed up
    let config_src = backup_dir.join("config.json");
    let config_dst = Path::new("/data/config.json");
    if config_src.exists() {
        fs::copy(&config_src, config_dst)?;
        tracing::info!("Restored config.json");
    }

    // Restore user_control.json if it was backed up
    let user_control_src = backup_dir.join("user_control.json");
    let user_control_dst = Path::new("/data/user_control.json");
    if user_control_src.exists() {
        fs::copy(&user_control_src, user_control_dst)?;
        tracing::info!("Restored user_control.json");
    }

    // Restore tor directory if it was backed up
    let tor_src = backup_dir.join("tor");
    let tor_dst = Path::new("/data/tor");
    if tor_src.exists() {
        if tor_dst.exists() {
            fs::remove_dir_all(tor_dst)?;
        }
        copy_dir_recursive(&tor_src, tor_dst)?;
        tracing::info!("Restored tor directory");
    }

    // Start old binary
    tracing::info!("Starting rolled-back fluxion");
    supervisor.start().await?;

    // Verify health check passes
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let health_status = supervisor.health_check().await?;

    match health_status {
        crate::supervisor::HealthStatus::Healthy { .. } => {
            tracing::info!("Rollback successful, fluxion is healthy");
        }
        _ => {
            return Err(UpgraderError::Rollback(
                "Rollback failed - fluxion not healthy after restart".to_string(),
            ));
        }
    }

    // Update state
    if let Some(ref backup_version) = state.backup_version {
        state.current_version = Some(backup_version.clone());
    }
    state.consecutive_failures += 1;

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }

    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_dir_recursive() {
        let src = tempfile::TempDir::new().unwrap();
        let dst = tempfile::TempDir::new().unwrap();

        // Create nested structure
        let subdir = src.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        fs::write(src.path().join("file1.txt"), b"content1").unwrap();
        fs::write(subdir.join("file2.txt"), b"content2").unwrap();

        // Copy
        copy_dir_recursive(src.path(), dst.path()).unwrap();

        // Verify
        assert!(dst.path().join("file1.txt").exists());
        assert!(dst.path().join("subdir").exists());
        assert!(dst.path().join("subdir/file2.txt").exists());
    }

    #[test]
    fn test_copy_dir_nonexistent_src() {
        let dst = tempfile::TempDir::new().unwrap();
        let src = Path::new("/nonexistent/path");
        let result = copy_dir_recursive(src, dst.path());
        assert!(result.is_ok());
    }
}
