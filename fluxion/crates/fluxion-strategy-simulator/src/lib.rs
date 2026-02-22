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

//! Strategy Efficiency Test Suite for FluxION
//!
//! This crate provides a comprehensive simulation framework for evaluating
//! and comparing battery operation strategies using synthetic test days,
//! configurable consumption patterns, and realistic price scenarios.
//!
//! # Features
//!
//! - **Synthetic Day Generation**: Create test days with configurable consumption profiles
//! - **Price Scenarios**: Pre-defined price patterns (usual day, elevated, volatile, negative)
//! - **Multi-Strategy Comparison**: Compare V1-V4 strategies plus baselines
//! - **Interactive Simulation**: Step through days with real-time recalculation
//! - **Override System**: Modify SOC, load, and prices at any point
//!
//! # Example
//!
//! ```ignore
//! use fluxion_strategy_simulator::{
//!     SyntheticDayConfig, ConsumptionProfile, PriceScenario,
//!     SimulationConfig, SimulationEngine,
//! };
//!
//! let day_config = SyntheticDayConfig::default();
//! let sim_config = SimulationConfig::default();
//!
//! let engine = SimulationEngine::new();
//! let mut state = engine.create_simulation(day_config, sim_config)?;
//! engine.run_to_completion(&mut state)?;
//!
//! for (name, result) in &state.strategy_results {
//!     println!("{}: {} CZK", name, result.net_cost_czk);
//! }
//! ```

pub mod cli;
pub mod price_scenarios;
pub mod simulation_engine;
pub mod state;
pub mod strategies;
pub mod synthetic_data;

// Re-exports for convenience
pub use price_scenarios::{PRICE_PRESETS, PriceScenario, PriceScenarioPreset};
pub use simulation_engine::SimulationEngine;
pub use state::{
    SimulationConfig, SimulationOverrides, SimulationState, SocOverride, StrategySimulationResult,
};
pub use strategies::{NaiveSelfUseStrategy, NoBatteryBaseline, StrategyInfo, StrategyRegistry};
pub use synthetic_data::{
    ConsumptionProfile, SolarProfile, SyntheticBlock, SyntheticDay, SyntheticDayConfig,
    SyntheticDayGenerator,
};
