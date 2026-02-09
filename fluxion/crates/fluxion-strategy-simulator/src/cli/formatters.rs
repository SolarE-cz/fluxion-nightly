// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Output formatters for CLI simulation results.

use crate::state::{SimulationConfig, SimulationState};
use anyhow::Result;
use comfy_table::{Attribute, Cell, Color, Table, presets::UTF8_FULL};
use std::fs::File;
use std::io::Write;

/// Formatter for pretty ASCII tables
pub struct TableFormatter;

/// Formatter for CSV export
pub struct CsvFormatter;

impl TableFormatter {
    /// Format simulation results as a pretty table
    pub fn format_results(state: &SimulationState, config: &SimulationConfig) -> String {
        let mut output = String::new();

        // Create results table
        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_header(vec![
            Cell::new("Strategy").add_attribute(Attribute::Bold),
            Cell::new("Net Cost\n(CZK)").add_attribute(Attribute::Bold),
            Cell::new("Savings vs\nNo Battery").add_attribute(Attribute::Bold),
            Cell::new("Grid Import\n(kWh)").add_attribute(Attribute::Bold),
            Cell::new("Grid Export\n(kWh)").add_attribute(Attribute::Bold),
            Cell::new("Cycles").add_attribute(Attribute::Bold),
            Cell::new("Final SOC\n(%)").add_attribute(Attribute::Bold),
        ]);

        // Get no-battery cost for comparison
        let no_battery_cost = state
            .strategy_results
            .get("no_battery")
            .map(|r| r.net_cost_czk)
            .unwrap_or(0.0);

        // Get ranked strategies
        let ranked = state.ranked_strategies();

        // Add rows for each strategy
        for (id, result) in &ranked {
            let savings = no_battery_cost - result.net_cost_czk;
            let savings_percent = if no_battery_cost > 0.0 {
                (savings / no_battery_cost) * 100.0
            } else {
                0.0
            };

            let cycles = result.battery_cycles(config.battery_capacity_kwh);

            let savings_str = if id.as_str() == "no_battery" {
                "-".to_string()
            } else {
                format!("{:.2} ({:.1}%)", savings, savings_percent)
            };

            // Highlight best strategy
            let name_cell = if ranked.first().map(|(i, _)| *i) == Some(id) {
                Cell::new(&result.strategy_name)
                    .fg(Color::Green)
                    .add_attribute(Attribute::Bold)
            } else {
                Cell::new(&result.strategy_name)
            };

            table.add_row(vec![
                name_cell,
                Cell::new(format!("{:.2}", result.net_cost_czk)),
                Cell::new(savings_str),
                Cell::new(format!("{:.2}", result.total_grid_import_kwh)),
                Cell::new(format!("{:.2}", result.total_grid_export_kwh)),
                Cell::new(format!("{:.2}", cycles)),
                Cell::new(format!("{:.1}", result.current_soc)),
            ]);
        }

        output.push_str(&table.to_string());
        output.push('\n');

        // Add simulation info
        output.push_str(&format!(
            "Simulation complete: {} blocks (00:00 - 23:45)\n",
            state.current_block
        ));
        output.push_str(&format!(
            "Scenario: {} | Battery: {:.1} kWh | Initial SOC: {:.0}%\n",
            state.day.price_scenario_name, config.battery_capacity_kwh, state.day.initial_soc
        ));

        output
    }

    /// Format block-by-block decision log
    pub fn format_decision_log(state: &SimulationState) -> String {
        let mut output = String::new();
        output.push_str("\n=== Decision Log ===\n\n");

        // Sample key blocks (midnight, 7am, noon, 6pm, 11pm)
        let key_blocks = [0, 28, 48, 72, 92];

        for &block_idx in &key_blocks {
            if block_idx >= state.day.blocks.len() {
                continue;
            }

            let block = &state.day.blocks[block_idx];
            let time = format!("{:02}:{:02}", block_idx * 15 / 60, (block_idx * 15) % 60);

            output.push_str(&format!(
                "Block {} ({}) - Price: {:.2} CZK/kWh, Load: {:.2} kWh\n",
                block_idx, time, block.price_czk_per_kwh, block.consumption_kwh
            ));

            for result in state.strategy_results.values() {
                if let Some(eval) = result.evaluations.get(block_idx) {
                    let soc = result.soc_history.get(block_idx + 1).unwrap_or(&0.0);
                    output.push_str(&format!(
                        "  {}: {:?} ({:.0}% SOC) - \"{}\"\n",
                        result.strategy_name, eval.mode, soc, eval.reason
                    ));
                }
            }

            output.push('\n');
        }

        output
    }
}

impl CsvFormatter {
    /// Export block-by-block detailed data to CSV file
    pub fn format_detailed(
        state: &SimulationState,
        _config: &SimulationConfig,
        path: &str,
    ) -> Result<()> {
        let mut file = File::create(path)?;

        // Build CSV header
        let mut header = vec![
            "block".to_string(),
            "time".to_string(),
            "date".to_string(),
            "price_czk_per_kwh".to_string(),
            "consumption_kwh".to_string(),
            "solar_kwh".to_string(),
            "grid_fee_czk_per_kwh".to_string(),
            "hdo_low_tariff".to_string(),
        ];

        // Add columns for each strategy
        let ranked = state.ranked_strategies();
        for (id, _result) in &ranked {
            let prefix = id.as_str();
            header.push(format!("{}_mode", prefix));
            header.push(format!("{}_soc_percent", prefix));
            header.push(format!("{}_block_cost_czk", prefix));
            header.push(format!("{}_cumulative_cost_czk", prefix));
            header.push(format!("{}_reason", prefix));
        }

        writeln!(file, "{}", header.join(","))?;

        // Write data rows for each block
        for block_idx in 0..state.day.blocks.len() {
            let block = &state.day.blocks[block_idx];
            let time = format!("{:02}:{:02}", block_idx * 15 / 60, (block_idx * 15) % 60);

            let mut row = vec![
                block_idx.to_string(),
                time,
                state.day.date.to_string(),
                format!("{:.4}", block.price_czk_per_kwh),
                format!("{:.4}", block.consumption_kwh),
                format!("{:.4}", block.solar_kwh),
                format!("{:.4}", block.grid_fee_czk_per_kwh),
                if block.is_hdo_low_tariff {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
            ];

            // Add strategy data
            for (_id, result) in &ranked {
                if let Some(eval) = result.evaluations.get(block_idx) {
                    let soc = result.soc_history.get(block_idx + 1).unwrap_or(&0.0);
                    let cumulative_cost = result
                        .cumulative_cost_czk
                        .get(block_idx + 1)
                        .unwrap_or(&0.0);
                    let block_cost = eval.cost_czk - eval.revenue_czk;

                    // Escape reason string (handle commas and quotes)
                    let escaped_reason = if eval.reason.contains(',') || eval.reason.contains('"') {
                        format!("\"{}\"", eval.reason.replace('"', "\"\""))
                    } else {
                        eval.reason.clone()
                    };

                    row.push(format!("{:?}", eval.mode));
                    row.push(format!("{:.2}", soc));
                    row.push(format!("{:.4}", block_cost));
                    row.push(format!("{:.4}", cumulative_cost));
                    row.push(escaped_reason);
                } else {
                    // No evaluation for this block yet
                    row.push("".to_string());
                    row.push("".to_string());
                    row.push("".to_string());
                    row.push("".to_string());
                    row.push("".to_string());
                }
            }

            writeln!(file, "{}", row.join(","))?;
        }

        Ok(())
    }
}
