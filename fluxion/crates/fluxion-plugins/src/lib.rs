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

//! Fluxion Plugin System
//!
//! This crate provides the plugin infrastructure for Fluxion battery optimization strategies.
//!
//! ## Architecture
//!
//! - **PluginManager**: Coordinates strategy plugins and merges their decisions
//! - **Protocol Types**: JSON-serializable types for plugin communication
//! - **Built-in Adapters**: Wrappers for Rust-native strategies
//!
//! ## Plugin Interface
//!
//! Plugins implement the `Plugin` trait:
//! - `name()`: Unique plugin identifier
//! - `priority()`: Decision priority (0-100)
//! - `is_enabled()`: Whether the plugin is active
//! - `evaluate()`: Returns a `BlockDecision` for a given context
//!
//! ## External Plugins
//!
//! External plugins (Python, Go, etc.) communicate via HTTP/REST:
//! - POST to plugin's callback URL with `EvaluationRequest`
//! - Receive `BlockDecision` response

pub mod builtin;
pub mod manager;
pub mod protocol;

pub use builtin::*;
pub use manager::{Plugin, PluginManager};
pub use protocol::*;
