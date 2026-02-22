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

//! Error types for the upgrader crate

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpgraderError {
    #[error("config error: {0}")]
    Config(String),

    #[error("state persistence error: {0}")]
    State(#[from] std::io::Error),

    #[error("state serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("release check failed: {0}")]
    ReleaseCheck(String),

    #[error("download failed: {0}")]
    Download(String),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("process error: {0}")]
    Process(String),

    #[error("health check failed after {timeout_secs}s")]
    HealthCheckTimeout { timeout_secs: u64 },

    #[error("rollback failed: {0}")]
    Rollback(String),

    #[error("version parse error: {0}")]
    VersionParse(String),

    #[error("backup error: {0}")]
    Backup(String),

    #[error("timeout waiting for calm time")]
    CalmTimeout,
}

pub type Result<T> = std::result::Result<T, UpgraderError>;
