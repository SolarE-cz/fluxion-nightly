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

use thiserror::Error;

/// Home Assistant API error types
#[derive(Error, Debug)]
pub enum HaError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("HA API returned error status {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("Entity not found: {0}")]
    EntityNotFound(String),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("JSON parsing error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Service call failed: {service} - {reason}")]
    ServiceCallFailed { service: String, reason: String },

    #[error("Connection timeout")]
    Timeout,

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Configuration error: {0}")]
    ConfigError(String),
}

pub type HaResult<T> = Result<T, HaError>;
