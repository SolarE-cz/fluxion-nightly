// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! CLI entry point for FluxION Strategy Simulator

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Parser;
use fluxion_strategy_simulator::{
    cli::{
        BatchArgs, BatchConfig, Cli, Commands, CompareArgs, CsvFormatter, DataLoader,
        JsonExportLoader, RunArgs, SqliteLoader, SyntheticLoader, TableFormatter,
    },
    price_scenarios::PriceScenario,
    simulation_engine::SimulationEngine,
    state::SimulationConfig,
    strategies::{StrategyRegistry, StrategySelection},
    synthetic_data::{ConsumptionProfile, SolarProfile, SyntheticDayConfig},
};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run(args) => run_command(args),
        Commands::Compare(args) => compare_command(args),
        Commands::Batch(args) => batch_command(args),
    }
}

fn run_command(args: RunArgs) -> Result<()> {
    // Validate arguments
    validate_run_args(&args)?;

    // Parse strategies
    let strategy_ids: Vec<String> = if args.strategies.to_lowercase() == "all" {
        vec![
            "winter_adaptive_v5".to_string(),
            "winter_adaptive_v4".to_string(),
            "winter_adaptive_v3".to_string(),
            "winter_adaptive_v2".to_string(),
            "winter_adaptive_v1".to_string(),
            "naive".to_string(),
            "no_battery".to_string(),
        ]
    } else {
        args.strategies
            .split(',')
            .map(|s| {
                let trimmed = s.trim().to_lowercase();
                match trimmed.as_str() {
                    "c10" => "winter_adaptive_c10".to_string(),
                    "c20" => "winter_adaptive_c20".to_string(),
                    "v20" => "winter_adaptive_v20".to_string(),
                    "v10" => "winter_adaptive_v10".to_string(),
                    "v9" => "winter_adaptive_v9".to_string(),
                    "v8" => "winter_adaptive_v8".to_string(),
                    "v7" => "winter_adaptive_v7".to_string(),
                    "v5" => "winter_adaptive_v5".to_string(),
                    "v4" => "winter_adaptive_v4".to_string(),
                    "v3" => "winter_adaptive_v3".to_string(),
                    "v2" => "winter_adaptive_v2".to_string(),
                    "v1" => "winter_adaptive_v1".to_string(),
                    "naive" => "naive".to_string(),
                    "no_battery" => "no_battery".to_string(),
                    _ => trimmed,
                }
            })
            .collect()
    };

    // Parse date if provided
    let date = args
        .date
        .as_ref()
        .map(|d| {
            NaiveDate::parse_from_str(d, "%Y-%m-%d").with_context(|| {
                format!(
                    "Invalid date format: '{}'\n\n\
                    Expected format: YYYY-MM-DD (e.g., 2026-01-15)\n\
                    Please check the date and try again.",
                    d
                )
            })
        })
        .transpose()?;

    // Create data loader based on input source
    let loader: Box<dyn DataLoader> = if let Some(db_path) = args.from_db {
        // SQLite database loader
        if date.is_none() {
            anyhow::bail!("--date is required when using --from-db");
        }

        Box::new(SqliteLoader::new(
            db_path,
            args.battery_capacity,
            args.initial_soc,
        ))
    } else if let Some(json_path) = args.from_json {
        // JSON export loader
        Box::new(JsonExportLoader::new(
            json_path,
            args.battery_capacity,
            args.initial_soc,
        ))
    } else {
        // Synthetic data loader
        let price_scenario = match args.scenario.to_lowercase().as_str() {
            "usual_day" | "usual" => PriceScenario::UsualDay,
            "elevated_day" | "elevated" => PriceScenario::ElevatedDay,
            "volatile" => PriceScenario::Volatile,
            "negative_prices" | "negative" => PriceScenario::NegativePrices,
            "hdo" | "hdo_optimized" => PriceScenario::HdoOptimized,
            _ => {
                eprintln!("Unknown scenario '{}'. Using 'usual_day'.", args.scenario);
                PriceScenario::UsualDay
            }
        };

        // Parse solar profile from CLI
        let solar_profile = match args.solar.to_lowercase().as_str() {
            "moderate" => SolarProfile::moderate(),
            "high" => SolarProfile::high(),
            _ => SolarProfile::none(),
        };

        let day_config = SyntheticDayConfig {
            date: chrono::Utc::now().date_naive(),
            consumption: ConsumptionProfile::default(),
            solar: solar_profile,
            price_scenario,
            initial_soc: args.initial_soc,
            battery_capacity_kwh: args.battery_capacity,
            hdo_periods: Some(vec![
                (0, 6),   // Night: 00:00-06:00
                (13, 15), // Midday: 13:00-15:00
                (20, 22), // Evening: 20:00-22:00
            ]),
            hdo_low_tariff_czk: 0.50,
            hdo_high_tariff_czk: 1.80,
        };

        Box::new(SyntheticLoader { config: day_config })
    };

    // Load strategy config overrides if provided
    let registry = if let Some(config_path) = &args.strategy_config {
        let overrides = load_strategy_config(config_path)?;
        Arc::new(StrategyRegistry::new_with_overrides(overrides)?)
    } else {
        Arc::new(StrategyRegistry::new_with_defaults())
    };

    // Load data
    let day = loader.load(date)?;

    // Build simulation config
    let mut strategies = Vec::new();
    let mut include_naive = false;
    let mut include_no_battery = false;

    for id in strategy_ids {
        if id == "naive" {
            include_naive = true;
        } else if id == "no_battery" {
            include_no_battery = true;
        } else {
            strategies.push(StrategySelection {
                strategy_id: id,
                enabled: true,
                config_overrides: None,
            });
        }
    }

    let sim_config = SimulationConfig {
        strategies,
        include_no_battery,
        include_naive,
        battery_capacity_kwh: day.battery_capacity_kwh,
        ..SimulationConfig::default()
    };

    // Create engine and run simulation (with custom registry if overrides provided)
    let engine = SimulationEngine::with_registry(registry);
    let mut state = engine.create_simulation_from_day(day, sim_config.clone())?;

    println!("Running simulation...");
    engine.run_to_completion(&mut state)?;

    // Determine output mode
    let output_mode = args.output.to_lowercase();

    // Validate csv_path if needed
    if (output_mode == "csv" || output_mode == "both") && args.csv_path.is_none() {
        anyhow::bail!("--csv-path is required when --output is 'csv' or 'both'");
    }

    // Display table output
    if output_mode == "table" || output_mode == "both" {
        println!("\n{}", TableFormatter::format_results(&state, &sim_config));
    }

    // Export CSV
    if output_mode == "csv" || output_mode == "both" {
        let csv_path = args.csv_path.as_ref().unwrap();
        CsvFormatter::format_detailed(&state, &sim_config, csv_path)
            .with_context(|| format!("Failed to write CSV to {}", csv_path))?;
        println!("CSV exported to: {}", csv_path);
    }

    // Show decision log if requested
    if args.decision_log {
        println!("{}", TableFormatter::format_decision_log(&state));
    }

    Ok(())
}

fn compare_command(args: CompareArgs) -> Result<()> {
    // Validate arguments
    validate_compare_args(&args)?;

    // Compare command is similar to run but with emphasis on comparison
    // Convert CompareArgs to RunArgs with adjusted defaults

    let run_args = RunArgs {
        scenario: args.scenario,
        strategies: args.strategies,
        initial_soc: args.initial_soc,
        battery_capacity: args.battery_capacity,
        decision_log: false, // Don't show decision log by default in compare mode
        from_db: args.from_db,
        from_json: args.from_json,
        date: args.date,
        output: args.output,
        csv_path: args.csv_path,
        solar: args.solar,
        strategy_config: args.strategy_config,
    };

    // Run the simulation
    run_command(run_args)?;

    // TODO: Add additional comparison-specific output (savings calculations, ranking)
    // This will be implemented in the formatters module

    Ok(())
}

fn batch_command(args: BatchArgs) -> Result<()> {
    // Validate arguments
    validate_batch_args(&args)?;

    // Load batch configuration from TOML
    let config = BatchConfig::from_file(&args.config)
        .with_context(|| format!("Failed to load batch config from {}", args.config))?;

    // Determine output directory
    let output_dir = args.output_dir.as_ref().unwrap_or(&config.output.csv_dir);

    // Create output directory if it doesn't exist
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output directory: {}", output_dir))?;

    println!(
        "Running batch simulation with {} scenarios...",
        config.scenarios.len()
    );
    println!("Output directory: {}\n", output_dir);

    // Run scenarios (sequentially for now, parallel implementation later)
    for (idx, scenario) in config.scenarios.iter().enumerate() {
        println!(
            "[{}/{}] Running scenario: {}",
            idx + 1,
            config.scenarios.len(),
            scenario.name
        );

        // Build RunArgs from scenario config
        let run_args =
            build_run_args_from_scenario(scenario, &config.simulation, output_dir, &config.output)?;

        // Run simulation
        match run_command(run_args) {
            Ok(_) => println!("  ✓ Completed\n"),
            Err(e) => {
                eprintln!("  ✗ Failed: {}\n", e);
                // Continue with remaining scenarios
            }
        }
    }

    println!("Batch simulation complete!");
    println!("Results saved to: {}", output_dir);

    Ok(())
}

fn build_run_args_from_scenario(
    scenario: &fluxion_strategy_simulator::cli::config::ScenarioConfig,
    sim_params: &fluxion_strategy_simulator::cli::config::SimulationParams,
    output_dir: &str,
    output_config: &fluxion_strategy_simulator::cli::config::OutputConfig,
) -> Result<RunArgs> {
    use fluxion_strategy_simulator::cli::config::ScenarioSource;

    // Build strategy list
    let strategies = sim_params.strategies.join(",");

    // Determine data source and build RunArgs
    let (from_db, from_json, date, scenario_name) = match &scenario.source {
        ScenarioSource::Synthetic {
            price_scenario,
            consumption_profile: _,
        } => (None, None, None, price_scenario.clone()),
        ScenarioSource::Database { db_path, date } => (
            Some(db_path.clone()),
            None,
            Some(date.clone()),
            "".to_string(),
        ),
        ScenarioSource::Json { json_path } => (None, Some(json_path.clone()), None, "".to_string()),
    };

    // Build CSV path
    let csv_path = if output_config.format == "csv" || output_config.format == "both" {
        Some(format!("{}/{}.csv", output_dir, scenario.name))
    } else {
        None
    };

    // Determine output mode for batch
    let output = if output_config.format == "both" {
        // For batch mode, don't print tables to avoid clutter
        "csv".to_string()
    } else {
        output_config.format.clone()
    };

    Ok(RunArgs {
        scenario: scenario_name,
        strategies,
        initial_soc: sim_params.initial_soc,
        battery_capacity: sim_params.battery_capacity_kwh,
        decision_log: output_config.include_decision_log,
        from_db,
        from_json,
        date,
        output,
        csv_path,
        solar: "none".to_string(), // Batch mode uses scenario-defined solar (TODO: add to batch config)
        strategy_config: None,     // Batch mode doesn't support strategy config overrides yet
    })
}

/// Validate RunArgs for correctness
fn validate_run_args(args: &RunArgs) -> Result<()> {
    // Validate SOC range
    if args.initial_soc < 0.0 || args.initial_soc > 100.0 {
        anyhow::bail!(
            "Invalid initial SOC: {}%. Must be between 0 and 100.",
            args.initial_soc
        );
    }

    // Validate battery capacity
    if args.battery_capacity <= 0.0 {
        anyhow::bail!(
            "Invalid battery capacity: {} kWh. Must be greater than 0.",
            args.battery_capacity
        );
    }

    // Check for conflicting data sources
    let source_count = [args.from_db.is_some(), args.from_json.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();

    if source_count > 1 {
        anyhow::bail!(
            "Conflicting data sources. Please use only one of: --from-db, --from-json, or synthetic (default)."
        );
    }

    // Validate file paths exist
    if let Some(db_path) = &args.from_db
        && !std::path::Path::new(db_path).exists()
    {
        anyhow::bail!(
            "Database file not found: {}\n\nPlease check the path and try again.",
            db_path
        );
    }

    if let Some(json_path) = &args.from_json
        && !std::path::Path::new(json_path).exists()
    {
        anyhow::bail!(
            "JSON export file not found: {}\n\nPlease check the path and try again.",
            json_path
        );
    }

    // Validate strategy config file if provided
    if let Some(config_path) = &args.strategy_config
        && !std::path::Path::new(config_path).exists()
    {
        anyhow::bail!(
            "Strategy config file not found: {}\n\nPlease check the path and try again.",
            config_path
        );
    }

    Ok(())
}

/// Validate CompareArgs by converting and validating as RunArgs
fn validate_compare_args(args: &CompareArgs) -> Result<()> {
    // Validate SOC range
    if args.initial_soc < 0.0 || args.initial_soc > 100.0 {
        anyhow::bail!(
            "Invalid initial SOC: {}%. Must be between 0 and 100.",
            args.initial_soc
        );
    }

    // Validate battery capacity
    if args.battery_capacity <= 0.0 {
        anyhow::bail!(
            "Invalid battery capacity: {} kWh. Must be greater than 0.",
            args.battery_capacity
        );
    }

    // Check for conflicting data sources
    let source_count = [args.from_db.is_some(), args.from_json.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();

    if source_count > 1 {
        anyhow::bail!(
            "Conflicting data sources. Please use only one of: --from-db, --from-json, or synthetic (default)."
        );
    }

    // Validate file paths exist
    if let Some(db_path) = &args.from_db
        && !std::path::Path::new(db_path).exists()
    {
        anyhow::bail!(
            "Database file not found: {}\n\nPlease check the path and try again.",
            db_path
        );
    }

    if let Some(json_path) = &args.from_json
        && !std::path::Path::new(json_path).exists()
    {
        anyhow::bail!(
            "JSON export file not found: {}\n\nPlease check the path and try again.",
            json_path
        );
    }

    // Validate strategy config file if provided
    if let Some(config_path) = &args.strategy_config
        && !std::path::Path::new(config_path).exists()
    {
        anyhow::bail!(
            "Strategy config file not found: {}\n\nPlease check the path and try again.",
            config_path
        );
    }

    Ok(())
}

/// Validate BatchArgs
fn validate_batch_args(args: &BatchArgs) -> Result<()> {
    // Validate config file exists
    if !std::path::Path::new(&args.config).exists() {
        anyhow::bail!(
            "Configuration file not found: {}\n\n\
            Please create a batch config file. See docs/CLI_SIMULATOR_REFERENCE.md for format.",
            args.config
        );
    }

    // Validate parallel count (warn if > 0 since not implemented yet)
    if args.parallel > 0 {
        eprintln!("Warning: Parallel execution is not yet implemented. Running sequentially.");
    }

    Ok(())
}

/// Load strategy config overrides from a TOML file.
///
/// The TOML file has sections keyed by strategy ID:
/// ```toml
/// [winter_adaptive_c10]
/// daylight_start_hour = 9
/// daylight_end_hour = 15
/// ```
///
/// Returns a map of strategy_id -> config as serde_json::Value.
fn load_strategy_config(path: &str) -> Result<HashMap<String, serde_json::Value>> {
    if !std::path::Path::new(path).exists() {
        anyhow::bail!(
            "Strategy config file not found: {}\n\nPlease check the path and try again.",
            path
        );
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read strategy config from {}", path))?;

    let table: toml::Table =
        toml::from_str(&content).with_context(|| format!("Failed to parse TOML from {}", path))?;

    let mut overrides = HashMap::new();

    for (key, value) in table {
        // Convert TOML value to JSON value for serde compatibility
        let json_value = toml_to_json(value);
        overrides.insert(key, json_value);
    }

    Ok(overrides)
}

/// Convert a TOML value to a serde_json::Value.
fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => {
            let map: serde_json::Map<String, serde_json::Value> = table
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
    }
}
