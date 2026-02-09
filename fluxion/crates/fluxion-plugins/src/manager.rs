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

//! Plugin manager for coordinating strategy plugins.

use crate::protocol::{BlockDecision, EvaluationRequest, OperationMode};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// Trait for strategy plugins
pub trait Plugin: Send + Sync {
    /// Get the plugin name
    fn name(&self) -> &str;

    /// Get the plugin priority (0-100, higher wins)
    fn priority(&self) -> u8;

    /// Check if the plugin is enabled
    fn is_enabled(&self) -> bool;

    /// Evaluate a block and return a decision
    fn evaluate(&self, request: &EvaluationRequest) -> anyhow::Result<BlockDecision>;
}

/// Plugin registration entry
struct PluginEntry {
    plugin: Arc<dyn Plugin>,
    enabled: bool,
    priority_override: Option<u8>,
}

impl std::fmt::Debug for PluginEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEntry")
            .field("name", &self.plugin.name())
            .field("enabled", &self.enabled)
            .field("priority_override", &self.priority_override)
            .finish()
    }
}

impl PluginEntry {
    fn effective_priority(&self) -> u8 {
        self.priority_override
            .unwrap_or_else(|| self.plugin.priority())
    }
}

/// Manages strategy plugins and coordinates evaluation
#[derive(Debug)]
pub struct PluginManager {
    plugins: HashMap<String, PluginEntry>,
    /// Fallback mode when no plugins are available or all fail
    fallback_mode: OperationMode,
    /// Timeout for plugin evaluation in milliseconds
    evaluation_timeout_ms: u64,
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManager {
    /// Create a new plugin manager
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            fallback_mode: OperationMode::SelfUse,
            evaluation_timeout_ms: 5000,
        }
    }

    /// Set the fallback mode when no plugins produce a result
    pub fn set_fallback_mode(&mut self, mode: OperationMode) {
        self.fallback_mode = mode;
    }

    /// Set the evaluation timeout in milliseconds
    pub fn set_evaluation_timeout(&mut self, timeout_ms: u64) {
        self.evaluation_timeout_ms = timeout_ms;
    }

    /// Register a plugin
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        let name = plugin.name().to_owned();
        debug!(
            "Registering plugin: {} (priority: {})",
            name,
            plugin.priority()
        );
        self.plugins.insert(
            name,
            PluginEntry {
                plugin,
                enabled: true,
                priority_override: None,
            },
        );
    }

    /// Enable or disable a plugin
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(entry) = self.plugins.get_mut(name) {
            entry.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Override a plugin's priority
    pub fn set_priority(&mut self, name: &str, priority: u8) -> bool {
        if let Some(entry) = self.plugins.get_mut(name) {
            entry.priority_override = Some(priority);
            true
        } else {
            false
        }
    }

    /// Get list of registered plugins
    #[must_use]
    pub fn list_plugins(&self) -> Vec<(&str, u8, bool)> {
        self.plugins
            .iter()
            .map(|(name, entry)| {
                (
                    name.as_str(),
                    entry.effective_priority(),
                    entry.enabled && entry.plugin.is_enabled(),
                )
            })
            .collect()
    }

    /// Evaluate all enabled plugins and return their decisions
    pub fn evaluate_all(&self, request: &EvaluationRequest) -> Vec<BlockDecision> {
        let mut decisions = Vec::new();

        for (name, entry) in &self.plugins {
            if !entry.enabled || !entry.plugin.is_enabled() {
                continue;
            }

            match entry.plugin.evaluate(request) {
                Ok(mut decision) => {
                    // Apply priority override if set
                    if let Some(priority) = entry.priority_override {
                        decision.priority = priority;
                    }
                    decisions.push(decision);
                }
                Err(e) => {
                    warn!("Plugin {} failed evaluation: {}", name, e);
                }
            }
        }

        decisions
    }

    /// Merge multiple decisions into a single decision using priority
    ///
    /// Decision priority rules:
    /// 1. Highest priority wins
    /// 2. If tied, highest confidence wins
    /// 3. If still tied, highest expected profit wins
    #[must_use]
    pub fn merge_decisions(
        &self,
        mut decisions: Vec<BlockDecision>,
        request: &EvaluationRequest,
    ) -> BlockDecision {
        if decisions.is_empty() {
            // Fallback decision
            return BlockDecision {
                block_start: request.block.block_start,
                duration_minutes: request.block.duration_minutes,
                mode: self.fallback_mode,
                reason: "No strategy plugins available".to_owned(),
                priority: 0,
                strategy_name: Some("Fallback".to_owned()),
                confidence: None,
                expected_profit_czk: None,
                decision_uid: Some("fallback:no_plugins".to_owned()),
            };
        }

        // Sort by priority (desc), then confidence (desc), then profit (desc)
        decisions.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| {
                    let a_conf = a.confidence.unwrap_or(0.0);
                    let b_conf = b.confidence.unwrap_or(0.0);
                    b_conf
                        .partial_cmp(&a_conf)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let a_profit = a.expected_profit_czk.unwrap_or(0.0);
                    let b_profit = b.expected_profit_czk.unwrap_or(0.0);
                    b_profit
                        .partial_cmp(&a_profit)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        decisions.into_iter().next().expect("decisions not empty")
    }

    /// Evaluate all plugins and return the merged decision
    #[must_use]
    pub fn evaluate(&self, request: &EvaluationRequest) -> BlockDecision {
        let decisions = self.evaluate_all(request);
        self.merge_decisions(decisions, request)
    }
}
