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

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Debug mode configuration resource
///
/// When debug mode is enabled (default: ON), the system will:
/// - Log all intended actions instead of executing them
/// - Not make any actual changes to inverter settings
/// - Provide detailed execution traces
///
/// This allows safe testing on production hardware.
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct DebugModeConfig {
    /// Whether debug mode is enabled
    pub enabled: bool,

    /// Log level for debug mode (overrides normal log level)
    pub log_level: DebugLogLevel,

    /// Whether to simulate successful execution (for testing schedulers)
    pub simulate_success: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugLogLevel {
    Trace,
    Debug,
    Info,
}

impl Default for DebugModeConfig {
    fn default() -> Self {
        Self {
            enabled: true, // Safe default - debug mode ON
            log_level: DebugLogLevel::Info,
            simulate_success: true,
        }
    }
}

impl DebugModeConfig {
    /// Create with debug mode enabled
    pub fn enabled() -> Self {
        Self::default()
    }

    /// Create with debug mode disabled (production mode)
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            log_level: DebugLogLevel::Info,
            simulate_success: false,
        }
    }

    /// Check if an action should be executed or just logged
    pub fn should_execute(&self) -> bool {
        !self.enabled
    }

    /// Check if debug mode is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Log a debug action (mode change, command, etc.)
    pub fn log_action(&self, action: &str, details: &str) {
        if self.enabled {
            match self.log_level {
                DebugLogLevel::Trace => tracing::trace!("🔍 DEBUG: {} | {}", action, details),
                DebugLogLevel::Debug => tracing::debug!("🔍 DEBUG: {} | {}", action, details),
                DebugLogLevel::Info => info!("🔍 DEBUG MODE: {} | {}", action, details),
            }
        }
    }

    /// Log that an action would be executed in production
    pub fn log_would_execute(&self, action: &str, target: &str, reason: &str) {
        if self.enabled {
            info!(
                "🔍 DEBUG MODE: Would execute '{}' on '{}' (reason: {})",
                action, target, reason
            );
        }
    }

    /// Log skipping execution due to debug mode
    pub fn log_skipped(&self, action: &str, reason: &str) {
        if self.enabled {
            info!("⏭️  DEBUG MODE: Skipped '{}' ({})", action, reason);
        }
    }

    /// Warn when debug mode is disabled (production mode)
    pub fn warn_production_mode() {
        warn!("⚠️  DEBUG MODE DISABLED - System will make REAL changes to inverter!");
        warn!("⚠️  Ensure configuration is correct before proceeding.");
    }
}

/// Result type for debug mode operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugExecutionResult {
    /// Action was executed in production mode
    Executed,

    /// Action was simulated in debug mode (reported as success)
    Simulated,

    /// Action was logged but not executed or simulated
    LoggedOnly,
}

impl DebugExecutionResult {
    /// Check if the action was successful (either executed or simulated)
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Executed | Self::Simulated)
    }

    /// Check if this was a simulation
    pub fn is_simulation(&self) -> bool {
        matches!(self, Self::Simulated)
    }
}

/// Helper macro for debug mode logging
///
/// Usage:
/// ```no_run
/// # use fluxion_core::debug_log;
/// # use fluxion_core::debug::DebugModeConfig;
/// # use bevy_ecs::prelude::*;
/// fn my_system(debug: Res<DebugModeConfig>) {
///     debug_log!(debug, "Setting mode", "ForceCharge on inverter_1");
/// }
/// ```
#[macro_export]
macro_rules! debug_log {
    ($debug:expr, $action:expr, $details:expr) => {
        if $debug.is_enabled() {
            $debug.log_action($action, $details);
        }
    };
}

/// Helper macro for conditional execution based on debug mode
///
/// Usage:
/// ```no_run
/// # use fluxion_core::debug_execute;
/// # use fluxion_core::debug::{DebugModeConfig, DebugExecutionResult};
/// # use bevy_ecs::prelude::*;
/// fn my_system(debug: Res<DebugModeConfig>) {
///     let result = debug_execute!(
///         debug,
///         "Change mode to ForceCharge",
///         {
///             // Production code - actually change mode
///             println!("Changing mode...");
///             DebugExecutionResult::Executed
///         }
///     );
/// }
/// ```
#[macro_export]
macro_rules! debug_execute {
    ($debug:expr, $action:expr, $production_code:block) => {{
        if $debug.should_execute() {
            // Production mode - execute
            $production_code
        } else {
            // Debug mode - log and simulate
            $debug.log_action($action, "Would execute in production mode");
            if $debug.simulate_success {
                $crate::debug::DebugExecutionResult::Simulated
            } else {
                $crate::debug::DebugExecutionResult::LoggedOnly
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_mode_default_enabled() {
        let config = DebugModeConfig::default();
        assert!(config.is_enabled());
        assert!(!config.should_execute());
    }

    #[test]
    fn test_debug_mode_disabled() {
        let config = DebugModeConfig::disabled();
        assert!(!config.is_enabled());
        assert!(config.should_execute());
    }

    #[test]
    fn test_debug_execution_result() {
        assert!(DebugExecutionResult::Executed.is_success());
        assert!(DebugExecutionResult::Simulated.is_success());
        assert!(!DebugExecutionResult::LoggedOnly.is_success());

        assert!(!DebugExecutionResult::Executed.is_simulation());
        assert!(DebugExecutionResult::Simulated.is_simulation());
        assert!(!DebugExecutionResult::LoggedOnly.is_simulation());
    }

    #[test]
    fn test_debug_mode_should_execute() {
        let debug_on = DebugModeConfig::enabled();
        assert!(!debug_on.should_execute());

        let debug_off = DebugModeConfig::disabled();
        assert!(debug_off.should_execute());
    }

    #[test]
    fn test_debug_log_levels() {
        let config = DebugModeConfig {
            enabled: true,
            log_level: DebugLogLevel::Debug,
            simulate_success: true,
        };

        // This test just ensures the methods compile and don't panic
        config.log_action("Test", "Details");
        config.log_would_execute("Action", "Target", "Reason");
        config.log_skipped("Action", "Reason");
    }
}
