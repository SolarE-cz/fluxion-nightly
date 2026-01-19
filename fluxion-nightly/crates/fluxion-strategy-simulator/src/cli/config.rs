// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! TOML configuration file parsing for batch simulation runs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

/// Root configuration structure for batch simulations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchConfig {
    /// Global simulation parameters
    pub simulation: SimulationParams,

    /// List of scenarios to run
    pub scenarios: Vec<ScenarioConfig>,

    /// Output configuration
    #[serde(default)]
    pub output: OutputConfig,
}

/// Global simulation parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationParams {
    /// Battery capacity in kWh
    #[serde(default = "default_battery_capacity")]
    pub battery_capacity_kwh: f32,

    /// Initial state of charge (0-100%)
    #[serde(default = "default_initial_soc")]
    pub initial_soc: f32,

    /// Strategies to simulate (e.g., ["winter_adaptive_v4", "naive", "no_battery"])
    pub strategies: Vec<String>,
}

/// Individual scenario configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    /// Human-readable name for this scenario
    pub name: String,

    /// Scenario type and source
    #[serde(flatten)]
    pub source: ScenarioSource,
}

/// Source of scenario data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScenarioSource {
    /// Use synthetic data with predefined scenarios
    #[serde(rename = "synthetic")]
    Synthetic {
        /// Price scenario name (usual_day, elevated_day, volatile, etc.)
        price_scenario: String,

        /// Consumption profile (optional, defaults to peak_based)
        #[serde(default = "default_consumption_profile")]
        consumption_profile: String,
    },

    /// Load from SQLite database
    #[serde(rename = "database")]
    Database {
        /// Path to SQLite database file
        db_path: String,

        /// Date to load (YYYY-MM-DD)
        date: String,
    },

    /// Load from JSON export file
    #[serde(rename = "json")]
    Json {
        /// Path to JSON export file
        json_path: String,
    },
}

/// Output configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Output format (table, csv, both)
    #[serde(default = "default_output_format")]
    pub format: String,

    /// Directory for CSV output files
    #[serde(default = "default_csv_dir")]
    pub csv_dir: String,

    /// Include decision log in output
    #[serde(default)]
    pub include_decision_log: bool,
}

// Default value functions
fn default_battery_capacity() -> f32 {
    10.0
}

fn default_initial_soc() -> f32 {
    50.0
}

fn default_consumption_profile() -> String {
    "peak_based".to_string()
}

fn default_output_format() -> String {
    "both".to_string()
}

fn default_csv_dir() -> String {
    "./batch_results".to_string()
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: default_output_format(),
            csv_dir: default_csv_dir(),
            include_decision_log: false,
        }
    }
}

impl BatchConfig {
    /// Load batch configuration from TOML file
    pub fn from_file(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;

        let config: BatchConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML config: {}", path))?;

        Ok(config)
    }

    /// Generate example batch config as TOML string
    pub fn example_toml() -> String {
        r#"# FluxION Strategy Simulator - Batch Configuration Example

[simulation]
battery_capacity_kwh = 10.0
initial_soc = 50.0
strategies = ["winter_adaptive_v4", "winter_adaptive_v3", "naive", "no_battery"]

# Scenario 1: Synthetic usual day
[[scenarios]]
name = "usual_day_test"
type = "synthetic"
price_scenario = "usual_day"
consumption_profile = "peak_based"

# Scenario 2: Synthetic volatile prices
[[scenarios]]
name = "volatile_test"
type = "synthetic"
price_scenario = "volatile"

# Scenario 3: Historical data from database
[[scenarios]]
name = "historical_2026_01_15"
type = "database"
db_path = "solax_data.db"
date = "2026-01-15"

# Scenario 4: JSON export
[[scenarios]]
name = "json_export_test"
type = "json"
json_path = "fluxion_export_20260115.json"

[output]
format = "both"          # table, csv, or both
csv_dir = "./batch_results"
include_decision_log = false
"#
        .to_string()
    }
}
