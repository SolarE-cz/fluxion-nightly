// SPDX-License-Identifier: CC-BY-NC-ND-4.0

//! Process supervisor for managing the child FluxION process

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use tracing::{info, warn};

/// Process supervisor for managing a child FluxION process
pub struct ProcessSupervisor {
    child: Arc<Mutex<Option<Child>>>,
    binary_path: Arc<PathBuf>,
}

impl ProcessSupervisor {
    /// Create a new process supervisor
    pub fn new(binary_path: &Path) -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            binary_path: Arc::new(binary_path.to_path_buf()),
        }
    }

    /// Start the child process
    pub fn start(&self) -> Result<()> {
        let mut child_guard = self.child.lock();

        // Check if already running
        if child_guard.is_some() {
            warn!("Child process is already running");
            return Ok(());
        }

        // Check if binary exists
        let binary_path = if self.binary_path.exists() {
            self.binary_path.as_ref().to_path_buf()
        } else {
            // Fallback to bundled binary
            let fallback = PathBuf::from("/usr/local/bin/fluxion-main");
            info!(
                "Binary {} not found, using fallback {}",
                self.binary_path.display(),
                fallback.display()
            );
            fallback
        };

        if !binary_path.exists() {
            anyhow::bail!(
                "Neither downloaded binary ({}) nor fallback ({}) exists",
                self.binary_path.display(),
                binary_path.display()
            );
        }

        info!("Starting child process: {}", binary_path.display());

        let child = Command::new(&binary_path)
            .spawn()
            .context(format!("Failed to start {}", binary_path.display()))?;

        *child_guard = Some(child);
        info!(
            "Child process started with PID: {}",
            child_guard.as_ref().unwrap().id()
        );

        Ok(())
    }

    /// Stop the child process
    pub fn stop(&self) -> Result<()> {
        let mut child_guard = self.child.lock();

        let mut child = child_guard.take().context("No child process is running")?;

        info!("Stopping child process PID: {}", child.id());

        // Try graceful shutdown first
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;

            let pid = Pid::from_raw(child.id() as i32);
            if let Err(e) = signal::kill(pid, Signal::SIGTERM) {
                warn!("Failed to send SIGTERM: {}", e);
            }
        }

        #[cfg(not(unix))]
        {
            // On Windows, just kill the process
        }

        // Wait for process to exit with timeout
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    info!("Child process stopped gracefully");
                    return Ok(());
                }
                Ok(None) => {
                    if start.elapsed() > timeout {
                        warn!("Child process did not stop gracefully, killing");
                        child.kill()?;
                        let _ = child.wait();
                        info!("Child process killed");
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    warn!("Error checking child status: {}", e);
                    child.kill()?;
                    let _ = child.wait();
                    info!("Child process killed due to error");
                    return Ok(());
                }
            }
        }
    }

    /// Restart the child process
    pub fn restart(&self) -> Result<()> {
        info!("Restarting child process");

        self.stop()?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        self.start()?;

        Ok(())
    }

    /// Check if the child process is running
    pub fn is_running(&self) -> bool {
        let child_guard = self.child.lock();
        child_guard.is_some()
    }

    /// Get the child process ID
    pub fn pid(&self) -> Option<u32> {
        let child_guard = self.child.lock();
        child_guard.as_ref().map(|c| c.id())
    }

    /// Replace the binary and restart
    pub async fn replace_and_restart(&self, new_binary: &Path) -> Result<()> {
        info!(
            "Replacing binary {} with {}",
            self.binary_path.display(),
            new_binary.display()
        );

        // Stop the child process
        self.stop()?;

        // Ensure parent directory exists
        if let Some(parent) = self.binary_path.as_ref().parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        // Copy new binary to location
        tokio::fs::copy(new_binary, self.binary_path.as_ref())
            .await
            .with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    new_binary.display(),
                    self.binary_path.display()
                )
            })?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(self.binary_path.as_ref())
                .await?
                .permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(self.binary_path.as_ref(), perms).await?;
        }

        info!("Binary replaced, restarting");
        self.start()?;

        Ok(())
    }

    /// Wait for the child process to exit
    pub fn wait(&self) -> Result<()> {
        let mut child_guard = self.child.lock();
        if let Some(mut child) = child_guard.take() {
            child.wait().context("Failed to wait for child process")?;
            Ok(())
        } else {
            anyhow::bail!("No child process to wait for");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_process_supervisor_new() {
        let temp = NamedTempFile::new().unwrap();
        let supervisor = ProcessSupervisor::new(temp.path());
        assert_eq!(*supervisor.binary_path, temp.path().to_path_buf());
        assert!(!supervisor.is_running());
    }

    #[test]
    fn test_process_supervisor_is_running_no_child() {
        let temp = NamedTempFile::new().unwrap();
        let supervisor = ProcessSupervisor::new(temp.path());
        assert!(!supervisor.is_running());
    }

    #[test]
    fn test_process_supervisor_pid_no_child() {
        let temp = NamedTempFile::new().unwrap();
        let supervisor = ProcessSupervisor::new(temp.path());
        assert!(supervisor.pid().is_none());
    }
}
