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

//! HTTP/REST plugin implementation for external strategy providers.
//!
//! This module allows external strategies written in any language (Python, Go, etc.)
//! to communicate with Fluxion via HTTP/REST.

use crate::manager::Plugin;
use crate::protocol::{BlockDecision, EvaluationRequest, PluginManifest};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tracing::{debug, error, warn};

/// HTTP plugin that delegates evaluation to an external service
pub struct HttpPlugin {
    /// Plugin manifest (name, priority, etc.)
    manifest: PluginManifest,
    /// Callback URL for evaluation requests
    callback_url: String,
    /// HTTP client
    client: reqwest::blocking::Client,
    /// Request timeout
    timeout: Duration,
    /// Number of consecutive failures
    failure_count: AtomicU32,
    /// Maximum failures before auto-disable
    max_failures: u32,
}

impl std::fmt::Debug for HttpPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPlugin")
            .field("name", &self.manifest.name)
            .field("callback_url", &self.callback_url)
            .field("priority", &self.manifest.default_priority)
            .field("enabled", &self.manifest.enabled)
            .field("timeout", &self.timeout)
            .field("failure_count", &self.failure_count.load(Ordering::Relaxed))
            .field("max_failures", &self.max_failures)
            .finish_non_exhaustive()
    }
}

impl HttpPlugin {
    /// Create a new HTTP plugin
    ///
    /// # Arguments
    /// * `manifest` - Plugin manifest with name, priority, etc.
    /// * `callback_url` - URL to POST evaluation requests to
    #[must_use]
    pub fn new(manifest: PluginManifest, callback_url: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            manifest,
            callback_url,
            client,
            timeout: Duration::from_secs(5),
            failure_count: AtomicU32::new(0),
            max_failures: 3,
        }
    }

    /// Create a new HTTP plugin with custom settings
    #[must_use]
    pub fn with_settings(
        manifest: PluginManifest,
        callback_url: String,
        timeout: Duration,
        max_failures: u32,
    ) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            manifest,
            callback_url,
            client,
            timeout,
            failure_count: AtomicU32::new(0),
            max_failures,
        }
    }

    /// Get the callback URL
    pub fn callback_url(&self) -> &str {
        &self.callback_url
    }

    /// Get the current failure count
    pub fn failure_count(&self) -> u32 {
        self.failure_count.load(Ordering::Relaxed)
    }

    /// Reset the failure count (e.g., after a successful health check)
    pub fn reset_failures(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
    }

    /// Record a failure
    fn record_failure(&self) {
        let prev = self.failure_count.fetch_add(1, Ordering::Relaxed);
        if prev + 1 >= self.max_failures {
            warn!(
                "Plugin {} has {} consecutive failures, auto-disabling",
                self.manifest.name,
                prev + 1
            );
        }
    }

    /// Record a success (resets failure count)
    fn record_success(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
    }
}

impl Plugin for HttpPlugin {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn priority(&self) -> u8 {
        self.manifest.default_priority
    }

    fn is_enabled(&self) -> bool {
        // Disable if too many consecutive failures
        if self.failure_count.load(Ordering::Relaxed) >= self.max_failures {
            return false;
        }
        self.manifest.enabled
    }

    fn evaluate(&self, request: &EvaluationRequest) -> anyhow::Result<BlockDecision> {
        debug!(
            "HttpPlugin {} evaluating block at {}",
            self.manifest.name, request.block.block_start
        );

        // Make HTTP POST request to callback URL
        let response = self
            .client
            .post(&self.callback_url)
            .json(request)
            .timeout(self.timeout)
            .send();

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.json::<BlockDecision>() {
                        Ok(mut decision) => {
                            self.record_success();
                            // Ensure priority matches manifest
                            decision.priority = self.manifest.default_priority;
                            // Set strategy_name from manifest if not provided by external plugin
                            if decision.strategy_name.is_none() {
                                decision.strategy_name = Some(self.manifest.name.clone());
                            }
                            debug!(
                                "HttpPlugin {} returned decision: {:?}",
                                self.manifest.name, decision.mode
                            );
                            Ok(decision)
                        }
                        Err(e) => {
                            self.record_failure();
                            error!(
                                "HttpPlugin {} failed to parse response: {}",
                                self.manifest.name, e
                            );
                            Err(anyhow::anyhow!("Failed to parse response: {e}"))
                        }
                    }
                } else {
                    self.record_failure();
                    let status = resp.status();
                    let body = resp.text().unwrap_or_default();
                    error!(
                        "HttpPlugin {} returned error {}: {}",
                        self.manifest.name, status, body
                    );
                    Err(anyhow::anyhow!("HTTP error {status}: {body}"))
                }
            }
            Err(e) => {
                self.record_failure();
                error!("HttpPlugin {} request failed: {}", self.manifest.name, e);
                Err(anyhow::anyhow!("Request failed: {e}"))
            }
        }
    }
}

/// Registration request from an external plugin
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginRegistrationRequest {
    /// Plugin manifest
    pub manifest: PluginManifest,
    /// Callback URL for evaluation requests
    pub callback_url: String,
}

/// Response to a registration request
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginRegistrationResponse {
    /// Whether registration was successful
    pub success: bool,
    /// Error message if registration failed
    pub error: Option<String>,
    /// Assigned plugin ID (for future unregistration)
    pub plugin_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_manifest() -> PluginManifest {
        PluginManifest {
            name: "test-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            description: "Test plugin".to_owned(),
            default_priority: 50,
            enabled: true,
        }
    }

    #[test]
    fn test_http_plugin_creation() {
        let manifest = create_test_manifest();
        let plugin = HttpPlugin::new(manifest, "http://localhost:8080/evaluate".to_owned());

        assert_eq!(plugin.name(), "test-plugin");
        assert_eq!(plugin.priority(), 50);
        assert!(plugin.is_enabled());
        assert_eq!(plugin.failure_count(), 0);
    }

    #[test]
    fn test_failure_tracking() {
        let manifest = create_test_manifest();
        let plugin = HttpPlugin::with_settings(
            manifest,
            "http://localhost:8080/evaluate".to_owned(),
            Duration::from_secs(1),
            3,
        );

        assert!(plugin.is_enabled());
        assert_eq!(plugin.failure_count(), 0);

        // Record failures
        plugin.record_failure();
        assert_eq!(plugin.failure_count(), 1);
        assert!(plugin.is_enabled());

        plugin.record_failure();
        assert_eq!(plugin.failure_count(), 2);
        assert!(plugin.is_enabled());

        plugin.record_failure();
        assert_eq!(plugin.failure_count(), 3);
        assert!(!plugin.is_enabled()); // Should be disabled after 3 failures

        // Reset failures
        plugin.reset_failures();
        assert_eq!(plugin.failure_count(), 0);
        assert!(plugin.is_enabled());
    }

    #[test]
    fn test_success_resets_failures() {
        let manifest = create_test_manifest();
        let plugin = HttpPlugin::new(manifest, "http://localhost:8080/evaluate".to_owned());

        plugin.record_failure();
        plugin.record_failure();
        assert_eq!(plugin.failure_count(), 2);

        plugin.record_success();
        assert_eq!(plugin.failure_count(), 0);
    }
}
