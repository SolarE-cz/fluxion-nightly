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

//! FluxION Backtesting Engine
//!
//! This crate provides historical data analysis and strategy simulation capabilities
//! for the FluxION energy optimization system.
//!
//! ## Features
//!
//! - **Historical Data Browser**: Load and analyze actual plant data from SQLite
//! - **Strategy Simulation**: Run strategies against historical data
//! - **Cost Analysis**: Calculate grid costs, battery value, and savings
//! - **Comparison**: Compare actual vs simulated performance

pub mod actual;
pub mod db;
pub mod metrics;
pub mod simulation;
pub mod types;

pub use actual::analyze_actual_day;
pub use db::{DataSource, SqliteDataSource};
pub use metrics::{ComparisonDiff, calculate_comparison};
pub use simulation::simulate_day;
pub use types::*;
