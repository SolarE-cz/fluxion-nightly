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

//! FluxION Upgrader - A lightweight process supervisor and binary updater
//!
//! This crate runs as PID 1 in the Docker container, managing the `fluxion` binary
//! lifecycle and handling over-the-air updates via GitHub Releases.

pub mod backup;
pub mod calm_detector;
pub mod config;
pub mod downloader;
pub mod error;
pub mod release_checker;
pub mod rollback;
pub mod state;
pub mod supervisor;
pub mod telemetry;
pub mod version;

pub use config::{ReleaseBranch, UpgraderConfig};
pub use error::UpgraderError;
pub use state::UpgraderState;
pub use version::{is_newer, parse_version, version_from_tag};
