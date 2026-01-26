// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! CLI argument definitions using clap.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fluxion-sim")]
#[command(author, version, about = "FluxION Strategy Simulator CLI")]
#[command(
    long_about = "Fast CLI for testing battery management strategies against various scenarios.\n\
    \nSupports synthetic scenarios, historical data from SQLite databases, and JSON exports.\n\
    Ideal for rapid iteration during strategy development and regression testing.\n\
    \nExamples:\n  \
    fluxion-sim run                        # Quick test with default scenario\n  \
    fluxion-sim compare --strategies all   # Compare all strategies\n  \
    fluxion-sim batch --config test.toml   # Run multiple scenarios"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run a single simulation with specified strategies and scenario
    #[command(
        long_about = "Run a simulation with one or more strategies against a scenario.\n\
        \nData Sources (choose one):\n  \
        - Synthetic: --scenario <name> (usual_day, volatile, elevated, negative, hdo)\n  \
        - Database: --from-db <path> --date <YYYY-MM-DD>\n  \
        - JSON Export: --from-json <path>\n\
        \nExamples:\n  \
        fluxion-sim run\n  \
        fluxion-sim run --scenario volatile --strategies v4,v3\n  \
        fluxion-sim run --from-db data.db --date 2026-01-15 --output both --csv-path results.csv"
    )]
    Run(RunArgs),

    /// Compare multiple strategies with emphasis on savings and ranking
    #[command(
        long_about = "Compare multiple strategies side-by-side with savings calculations.\n\
        \nDefaults to comparing ALL strategies (v1-v4, naive, no_battery).\n\
        Shows savings vs baseline and optionally ranks by chosen metric.\n\
        \nExamples:\n  \
        fluxion-sim compare\n  \
        fluxion-sim compare --scenario volatile\n  \
        fluxion-sim compare --from-json export.json --rank-by cycles"
    )]
    Compare(CompareArgs),

    /// Run batch simulations from TOML configuration file
    #[command(
        long_about = "Execute multiple simulation scenarios defined in a TOML config file.\n\
        \nEnables systematic testing across different scenarios with consistent parameters.\n\
        Results are saved to CSV files in the specified output directory.\n\
        \nExamples:\n  \
        fluxion-sim batch --config scenarios.toml\n  \
        fluxion-sim batch --config test.toml --output-dir ./results"
    )]
    Batch(BatchArgs),
}

#[derive(Parser)]
pub struct RunArgs {
    /// Price scenario name (usual_day, volatile, elevated, negative, hdo)
    #[arg(
        long,
        default_value = "usual_day",
        help = "Synthetic price scenario to simulate",
        long_help = "Available scenarios:\n  \
          - usual_day: Typical Czech spot prices (2-5 CZK/kWh)\n  \
          - volatile: High price volatility (0.5-8 CZK/kWh)\n  \
          - elevated: Consistently high prices (4-7 CZK/kWh)\n  \
          - negative: Includes negative price periods\n  \
          - hdo: HDO tariff structure\n\
          \nIgnored when using --from-db or --from-json"
    )]
    pub scenario: String,

    /// Comma-separated strategy IDs or "all"
    #[arg(
        long,
        default_value = "v4,naive,no_battery",
        help = "Strategies to test (v4, v3, v2, v1, naive, no_battery, all)",
        long_help = "Strategy shortcuts:\n  \
          - v4: winter_adaptive_v4 (current default)\n  \
          - v3: winter_adaptive_v3\n  \
          - v2: winter_adaptive_v2\n  \
          - v1: winter_adaptive_v1\n  \
          - naive: simple self-consumption\n  \
          - no_battery: baseline without battery\n  \
          - all: test all strategies\n\
          \nExamples: v4,naive or all"
    )]
    pub strategies: String,

    /// Initial battery state of charge (0-100%)
    #[arg(
        long,
        default_value_t = 50.0,
        help = "Starting battery SOC percentage (must be 0-100)"
    )]
    pub initial_soc: f32,

    /// Battery capacity in kWh
    #[arg(
        long,
        default_value_t = 10.0,
        help = "Battery capacity in kilowatt-hours (must be > 0)"
    )]
    pub battery_capacity: f32,

    /// Show detailed block-by-block decision reasoning
    #[arg(
        long,
        default_value_t = false,
        help = "Display decision log with strategy reasoning for each block"
    )]
    pub decision_log: bool,

    /// Load historical data from SQLite database (requires --date)
    #[arg(
        long,
        value_name = "PATH",
        help = "Path to solax_data.db database",
        long_help = "Load real historical data from SQLite database.\n\
          Requires --date to specify which day to simulate.\n\
          \nExample: --from-db solax_data.db --date 2026-01-15"
    )]
    pub from_db: Option<String>,

    /// Load data from JSON export file
    #[arg(
        long,
        value_name = "PATH",
        help = "Path to FluxION JSON export file",
        long_help = "Load data from a JSON export generated by FluxION web interface.\n\
          Export format includes prices, consumption, and battery config.\n\
          \nExample: --from-json fluxion_export_20260115.json"
    )]
    pub from_json: Option<String>,

    /// Date to simulate (YYYY-MM-DD format, required with --from-db)
    #[arg(
        long,
        value_name = "YYYY-MM-DD",
        help = "Date to load from database",
        long_help = "Specify which day to load when using --from-db.\n\
          Format: YYYY-MM-DD (e.g., 2026-01-15)\n\
          \nRequired when using --from-db, ignored otherwise."
    )]
    pub date: Option<String>,

    /// Output format: table, csv, or both
    #[arg(long, default_value = "table",
          value_parser = ["table", "csv", "both"],
          help = "How to display results")]
    pub output: String,

    /// CSV file path (required when output is csv or both)
    #[arg(
        long,
        value_name = "PATH",
        help = "Where to save detailed CSV results",
        long_help = "Path for CSV export with block-by-block details.\n\
          Required when --output is 'csv' or 'both'.\n\
          \nExample: --csv-path results.csv"
    )]
    pub csv_path: Option<String>,

    /// Solar generation profile (none, moderate, high)
    #[arg(
        long,
        default_value = "none",
        value_parser = ["none", "moderate", "high"],
        help = "Solar generation profile for synthetic scenarios",
        long_help = "Solar generation profiles:\n  \
          - none: No solar (winter/cloudy day testing)\n  \
          - moderate: Spring/fall typical (~3kW peak, 7am-6pm, ~12 kWh/day)\n  \
          - high: Summer day (~5kW peak, 5am-9pm, ~25 kWh/day)\n\
          \nIgnored when using --from-db or --from-json"
    )]
    pub solar: String,
}

#[derive(Parser)]
pub struct CompareArgs {
    /// Price scenario name (usual_day, volatile, elevated, negative, hdo)
    #[arg(
        long,
        default_value = "usual_day",
        help = "Synthetic price scenario to simulate"
    )]
    pub scenario: String,

    /// Comma-separated strategy IDs or "all"
    #[arg(
        long,
        default_value = "all",
        help = "Strategies to compare (defaults to all)",
        long_help = "Defaults to comparing ALL strategies (v1-v4, naive, no_battery).\n\
          Use comma-separated list to compare subset: v4,v3,naive"
    )]
    pub strategies: String,

    /// Initial battery state of charge (0-100%)
    #[arg(
        long,
        default_value_t = 50.0,
        help = "Starting battery SOC percentage (must be 0-100)"
    )]
    pub initial_soc: f32,

    /// Battery capacity in kWh
    #[arg(
        long,
        default_value_t = 10.0,
        help = "Battery capacity in kilowatt-hours (must be > 0)"
    )]
    pub battery_capacity: f32,

    /// Load historical data from SQLite database (requires --date)
    #[arg(long, value_name = "PATH", help = "Path to solax_data.db database")]
    pub from_db: Option<String>,

    /// Load data from JSON export file
    #[arg(long, value_name = "PATH", help = "Path to FluxION JSON export file")]
    pub from_json: Option<String>,

    /// Date to simulate (YYYY-MM-DD format, required with --from-db)
    #[arg(long, value_name = "YYYY-MM-DD", help = "Date to load from database")]
    pub date: Option<String>,

    /// Output format: table, csv, or both
    #[arg(long, default_value = "table",
          value_parser = ["table", "csv", "both"],
          help = "How to display results")]
    pub output: String,

    /// CSV file path (required when output is csv or both)
    #[arg(long, value_name = "PATH", help = "Where to save detailed CSV results")]
    pub csv_path: Option<String>,

    /// Show savings calculations vs no_battery baseline
    #[arg(
        long,
        default_value_t = true,
        help = "Display savings percentages vs baseline"
    )]
    pub show_savings: bool,

    /// Metric to rank strategies by
    #[arg(long, default_value = "cost",
          value_parser = ["cost", "cycles", "import", "export"],
          help = "Sort strategies by: cost, cycles, import, or export")]
    pub rank_by: String,

    /// Solar generation profile (none, moderate, high)
    #[arg(
        long,
        default_value = "none",
        value_parser = ["none", "moderate", "high"],
        help = "Solar generation profile for synthetic scenarios",
        long_help = "Solar generation profiles:\n  \
          - none: No solar (winter/cloudy day testing)\n  \
          - moderate: Spring/fall typical (~3kW peak, 7am-6pm, ~12 kWh/day)\n  \
          - high: Summer day (~5kW peak, 5am-9pm, ~25 kWh/day)\n\
          \nIgnored when using --from-db or --from-json"
    )]
    pub solar: String,
}

#[derive(Parser)]
pub struct BatchArgs {
    /// Path to TOML configuration file
    #[arg(
        long,
        value_name = "PATH",
        help = "Path to batch configuration file",
        long_help = "TOML file defining multiple scenarios to test.\n\
          See docs/CLI_SIMULATOR_REFERENCE.md for config format.\n\
          \nExample: --config batch_scenarios.toml"
    )]
    pub config: String,

    /// Number of parallel simulations (0 = sequential, not yet implemented)
    #[arg(
        long,
        default_value_t = 0,
        help = "Parallel execution (0 = sequential, default)"
    )]
    pub parallel: usize,

    /// Output directory for CSV files (overrides config setting)
    #[arg(
        long,
        value_name = "PATH",
        help = "Directory for CSV output files",
        long_help = "Directory where CSV results will be saved.\n\
          Overrides output.csv_dir from config file if specified.\n\
          \nExample: --output-dir ./batch_results"
    )]
    pub output_dir: Option<String>,
}
