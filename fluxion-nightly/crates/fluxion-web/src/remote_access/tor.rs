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

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use tracing::{error, info, warn};

#[derive(Debug)]
pub struct TorManager {
    data_dir: PathBuf,
    torrc_path: PathBuf,
    child: Option<Child>,
    listen_port: u16,
}

impl TorManager {
    #[must_use]
    pub fn new(data_dir: &Path, listen_port: u16) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            torrc_path: data_dir.join("tor").join("torrc"),
            child: None,
            listen_port,
        }
    }

    /// Generate torrc content for the hidden service.
    fn generate_torrc(&self) -> String {
        let hidden_service_dir = self.data_dir.join("tor").join("hidden_service");
        let auth_dir = self.data_dir.join("tor").join("authorized_clients");

        format!(
            "DataDirectory {data_dir}\n\
             HiddenServiceDir {hs_dir}\n\
             HiddenServicePort 80 127.0.0.1:{port}\n\
             HiddenServiceVersion 3\n\
             ClientOnionAuthDir {auth_dir}\n",
            data_dir = self.data_dir.join("tor").join("data").display(),
            hs_dir = hidden_service_dir.display(),
            port = self.listen_port,
            auth_dir = auth_dir.display(),
        )
    }

    /// Write the torrc file to disk.
    fn write_torrc(&self) -> std::io::Result<()> {
        if let Some(parent) = self.torrc_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Ensure subdirectories exist
        std::fs::create_dir_all(self.data_dir.join("tor").join("data"))?;
        std::fs::create_dir_all(self.data_dir.join("tor").join("hidden_service"))?;
        std::fs::create_dir_all(self.data_dir.join("tor").join("authorized_clients"))?;

        let content = self.generate_torrc();
        std::fs::write(&self.torrc_path, content)
    }

    /// Start the Tor process. Returns error if already running or spawn fails.
    pub fn start(&mut self) -> std::io::Result<()> {
        if self.child.is_some() {
            warn!("Tor process already running, skipping start");
            return Ok(());
        }

        self.write_torrc()?;

        info!("Starting Tor with torrc at {}", self.torrc_path.display());
        let child = Command::new("tor")
            .arg("-f")
            .arg(&self.torrc_path)
            .spawn()
            .map_err(|e| {
                error!("Failed to start Tor: {e}");
                e
            })?;

        info!("Tor process started (PID: {})", child.id());
        self.child = Some(child);
        Ok(())
    }

    /// Stop the Tor process gracefully.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            info!("Stopping Tor process (PID: {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
            info!("Tor process stopped");
        }
    }

    /// Reload Tor configuration (sends SIGHUP). Used after adding/removing auth clients.
    pub fn reload(&self) -> std::io::Result<()> {
        if let Some(ref child) = self.child {
            let pid = child.id();
            info!("Reloading Tor (SIGHUP to PID {pid})");
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                #[expect(clippy::cast_possible_wrap)]
                kill(Pid::from_raw(pid as i32), Signal::SIGHUP)
                    .map_err(|e| std::io::Error::other(format!("SIGHUP failed: {e}")))?;
            }
            Ok(())
        } else {
            warn!("Cannot reload Tor: process not running");
            Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Tor process not running",
            ))
        }
    }

    /// Read the .onion address from the hidden service hostname file.
    #[must_use]
    pub fn read_onion_address(&self) -> Option<String> {
        let hostname_path = self
            .data_dir
            .join("tor")
            .join("hidden_service")
            .join("hostname");
        std::fs::read_to_string(hostname_path)
            .ok()
            .map(|s| s.trim().to_owned())
    }

    /// Check if the Tor process is running.
    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    warn!("Tor process has exited");
                    self.child = None;
                    false
                }
                Ok(None) => true,
                Err(e) => {
                    error!("Failed to check Tor process status: {e}");
                    false
                }
            }
        } else {
            false
        }
    }
}

impl Drop for TorManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_torrc() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = TorManager::new(tmp.path(), 8099);
        let torrc = manager.generate_torrc();

        assert!(torrc.contains("HiddenServicePort 80 127.0.0.1:8099"));
        assert!(torrc.contains("HiddenServiceVersion 3"));
        assert!(torrc.contains("HiddenServiceDir"));
        assert!(torrc.contains("DataDirectory"));
        assert!(torrc.contains("ClientOnionAuthDir"));
    }

    #[test]
    fn test_write_torrc_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = TorManager::new(tmp.path(), 8099);
        manager.write_torrc().unwrap();

        assert!(tmp.path().join("tor").join("torrc").exists());
        assert!(tmp.path().join("tor").join("data").exists());
        assert!(tmp.path().join("tor").join("hidden_service").exists());
        assert!(tmp.path().join("tor").join("authorized_clients").exists());
    }

    #[test]
    fn test_no_onion_address_before_start() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = TorManager::new(tmp.path(), 8099);
        assert!(manager.read_onion_address().is_none());
    }
}
