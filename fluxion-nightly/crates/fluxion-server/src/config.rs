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

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    #[serde(default)]
    pub heartbeat: HeartbeatSettings,
    pub email: EmailSettings,
    #[serde(default)]
    pub database: DatabaseSettings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSettings {
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthSettings {
    pub shared_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatSettings {
    #[serde(default = "default_expected_interval_secs")]
    pub expected_interval_secs: u64,
    #[serde(default = "default_miss_threshold")]
    pub miss_threshold: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailSettings {
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub from_address: String,
    #[serde(default = "default_use_tls")]
    pub use_tls: bool,
    pub admin_recipients: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    #[serde(default = "default_db_path")]
    pub path: String,
    #[serde(default = "default_telemetry_retention_days")]
    pub telemetry_retention_days: u32,
}

fn default_bind_address() -> String {
    "0.0.0.0".to_owned()
}

fn default_port() -> u16 {
    8100
}

fn default_expected_interval_secs() -> u64 {
    300
}

fn default_miss_threshold() -> u32 {
    3
}

fn default_smtp_port() -> u16 {
    587
}

fn default_use_tls() -> bool {
    true
}

fn default_db_path() -> String {
    "./data/fluxion-server.db".to_owned()
}

fn default_telemetry_retention_days() -> u32 {
    30
}

impl Default for HeartbeatSettings {
    fn default() -> Self {
        Self {
            expected_interval_secs: default_expected_interval_secs(),
            miss_threshold: default_miss_threshold(),
        }
    }
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            telemetry_retention_days: default_telemetry_retention_days(),
        }
    }
}

impl ServerConfig {
    pub fn from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(Path::new(path))
            .with_context(|| format!("Failed to read config file: {path}"))?;
        let config: Self =
            toml::from_str(&content).with_context(|| "Failed to parse config TOML")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.auth.shared_secret.is_empty()
            || self.auth.shared_secret == "change-me-to-a-strong-random-secret"
        {
            bail!("auth.shared_secret must be set to a strong random value");
        }
        if self.email.smtp_host.is_empty() {
            bail!("email.smtp_host must be set");
        }
        if self.email.admin_recipients.is_empty() {
            bail!("email.admin_recipients must contain at least one address");
        }
        Ok(())
    }
}
