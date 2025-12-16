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

// Test module for charging optimization analysis
// Compares current vs proposed algorithm for overnight charging

/// Represents a price block for analysis
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PriceBlock {
    ts: i64,
    price: f32,
    op: String,    // 'c' = charge, 's' = self-use, 'd' = discharge
    state: String, // WA, CFC, SU, etc.
}

/// Analyze charging schedule and calculate costs
#[derive(Debug, Default)]
#[allow(dead_code)]
struct ScheduleAnalysis {
    /// Blocks where charging is scheduled
    charge_blocks: Vec<PriceBlock>,
    /// Blocks where battery is depleted and grid is used
    grid_usage_blocks: Vec<PriceBlock>,
    /// Total charging cost (price * energy)
    charging_cost: f32,
    /// Total grid usage cost during depleted periods
    grid_usage_cost: f32,
    /// Hours spent at minimum SOC
    hours_at_min_soc: f32,
    /// Average charging price
    avg_charge_price: f32,
    /// Cheapest available blocks that could have been used
    missed_cheap_blocks: Vec<PriceBlock>,
}

/// Simulates battery SOC over time
#[allow(dead_code)]
fn simulate_schedule(
    blocks: &[PriceBlock],
    initial_soc: f32,
    battery_kwh: f32,
    charge_rate_kw: f32,
    consumption_kw: f32,
    min_soc: f32,
) -> (Vec<(i64, f32)>, f32) {
    let mut soc = initial_soc;
    let mut soc_history = Vec::new();
    let mut grid_import_kwh = 0.0;

    for block in blocks {
        soc_history.push((block.ts, soc));

        let energy_change = if block.op == "c" {
            // Charging: add energy (15 min = 0.25 hours)
            charge_rate_kw * 0.25
        } else {
            // Consumption: remove energy
            -(consumption_kw * 0.25)
        };

        // Calculate SOC change
        let soc_change = (energy_change / battery_kwh) * 100.0;
        soc = (soc + soc_change).clamp(min_soc, 100.0);

        // If SOC at minimum and not charging, grid is used
        if soc <= min_soc && block.op != "c" {
            grid_import_kwh += consumption_kw * 0.25;
        }
    }

    (soc_history, grid_import_kwh)
}

/// Find optimal charging blocks using improved algorithm
/// Key insight: We want to select blocks that give us:
/// 1. Lowest total charging cost
/// 2. While preferring consecutive blocks (to avoid inverter cycling)
/// 3. And ensuring we charge enough to avoid grid usage during expensive hours
fn find_optimal_charge_blocks(
    blocks: &[PriceBlock],
    initial_soc: f32,
    target_soc: f32,
    battery_kwh: f32,
    charge_rate_kw: f32,
    deadline_hours: f32,
) -> Vec<usize> {
    // Calculate energy needed
    let energy_needed = battery_kwh * (target_soc - initial_soc) / 100.0;
    if energy_needed <= 0.0 {
        return Vec::new();
    }

    // Calculate blocks needed
    let charge_per_block = charge_rate_kw * 0.25;
    let blocks_needed = (energy_needed / charge_per_block).ceil() as usize;

    // Find deadline index
    let deadline_blocks = (deadline_hours * 4.0) as usize; // 4 blocks per hour
    let deadline_idx = blocks.len().min(deadline_blocks);

    // Get eligible blocks (within deadline), sorted by price
    let mut eligible: Vec<(usize, f32)> = blocks[..deadline_idx]
        .iter()
        .enumerate()
        .map(|(i, b)| (i, b.price))
        .collect();

    eligible.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // STRATEGY: First, take the absolutely cheapest blocks
    // Then check if we can form better consecutive runs by including slightly more expensive blocks

    let cheapest_price = eligible.first().map(|(_, p)| *p).unwrap_or(0.0);
    let tolerance = 0.20; // 20% tolerance for consecutive grouping
    let threshold = cheapest_price * (1.0 + tolerance);

    // Get all blocks within tolerance, sorted by index for run detection
    let mut within_tolerance: Vec<(usize, f32)> = eligible
        .iter()
        .filter(|(_, p)| *p <= threshold)
        .cloned()
        .collect();
    within_tolerance.sort_by_key(|(idx, _)| *idx);

    // Find consecutive runs within tolerance blocks
    let mut runs: Vec<Vec<(usize, f32)>> = Vec::new();
    let mut current_run: Vec<(usize, f32)> = Vec::new();

    for (idx, price) in &within_tolerance {
        if current_run.is_empty() || *idx == current_run.last().unwrap().0 + 1 {
            current_run.push((*idx, *price));
        } else {
            if !current_run.is_empty() {
                runs.push(current_run);
            }
            current_run = vec![(*idx, *price)];
        }
    }
    if !current_run.is_empty() {
        runs.push(current_run);
    }

    // Score runs by: (length >= 2 preferred, then avg price)
    runs.sort_by(|a, b| {
        let a_len_ok = a.len() >= 2;
        let b_len_ok = b.len() >= 2;
        if a_len_ok != b_len_ok {
            return b_len_ok.cmp(&a_len_ok);
        }
        // Same category: prefer cheaper average
        let a_avg: f32 = a.iter().map(|(_, p)| p).sum::<f32>() / a.len() as f32;
        let b_avg: f32 = b.iter().map(|(_, p)| p).sum::<f32>() / b.len() as f32;
        a_avg.partial_cmp(&b_avg).unwrap()
    });

    // Greedily select from best runs, but cap each run's contribution
    // to encourage diversity (don't take 20 blocks from one run if we only need 8)
    let mut selected: Vec<usize> = Vec::new();

    for run in &runs {
        if selected.len() >= blocks_needed {
            break;
        }
        // Take blocks from this run (up to remaining needed)
        for (idx, _) in run {
            if selected.len() >= blocks_needed {
                break;
            }
            if !selected.contains(idx) {
                selected.push(*idx);
            }
        }
    }

    // If we still need more, fall back to cheapest blocks not yet selected
    if selected.len() < blocks_needed {
        for (idx, _) in &eligible {
            if selected.len() >= blocks_needed {
                break;
            }
            if !selected.contains(idx) {
                selected.push(*idx);
            }
        }
    }

    selected
}

/// Calculate total cost for a schedule
#[allow(dead_code)]
fn calculate_schedule_cost(
    blocks: &[PriceBlock],
    charge_indices: &[usize],
    initial_soc: f32,
    battery_kwh: f32,
    charge_rate_kw: f32,
    consumption_kw: f32,
    min_soc: f32,
) -> (f32, f32, f32) {
    // Returns: (charging_cost, grid_usage_cost, hours_at_min_soc)
    let charge_per_block = charge_rate_kw * 0.25;
    let consumption_per_block = consumption_kw * 0.25;

    let mut soc = initial_soc;
    let mut charging_cost = 0.0;
    let mut grid_usage_cost = 0.0;
    let mut blocks_at_min = 0;

    for (i, block) in blocks.iter().enumerate() {
        if charge_indices.contains(&i) {
            // Charging
            let energy_added = charge_per_block.min((100.0 - soc) * battery_kwh / 100.0);
            charging_cost += energy_added * block.price;
            soc += (energy_added / battery_kwh) * 100.0;
            soc = soc.min(100.0);
        } else {
            // Consuming
            let energy_needed = consumption_per_block;
            let battery_can_provide = ((soc - min_soc) * battery_kwh / 100.0).max(0.0);

            if battery_can_provide >= energy_needed {
                // Battery covers consumption
                soc -= (energy_needed / battery_kwh) * 100.0;
            } else {
                // Grid needed
                let grid_energy = energy_needed - battery_can_provide;
                grid_usage_cost += grid_energy * block.price;
                soc = min_soc;
                blocks_at_min += 1;
            }
        }
    }

    let hours_at_min = blocks_at_min as f32 * 0.25;
    (charging_cost, grid_usage_cost, hours_at_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consecutive_run_detection() {
        // Test that we correctly identify consecutive cheap blocks
        let prices = vec![
            2.3, 2.1, 1.9, // Initial (1.9 is cheap)
            2.4, 2.3, // Gap
            2.0, 1.95, 1.96, 1.99, 1.97, 1.94, 2.0, // Long cheap run
            2.5, 2.6, // Expensive
            1.9, 1.95, // Another cheap spot
        ];

        let blocks: Vec<PriceBlock> = prices
            .iter()
            .enumerate()
            .map(|(i, &price)| PriceBlock {
                ts: i as i64 * 900,
                price,
                op: "s".to_string(),
                state: "WA".to_string(),
            })
            .collect();

        let optimal = find_optimal_charge_blocks(
            &blocks, 22.0, // initial_soc
            90.0, // target_soc
            24.0, // battery_kwh
            10.0, // charge_rate_kw
            4.0,  // deadline_hours
        );

        println!("Prices: {:?}", prices);
        println!("Optimal charge indices: {:?}", optimal);
        println!(
            "Optimal charge prices: {:?}",
            optimal.iter().map(|&i| prices[i]).collect::<Vec<_>>()
        );

        // Check that the long run (indices 5-11) is preferred
        // The algorithm should pick mostly from the cheap run
        let in_cheap_run = optimal.iter().filter(|&&i| (5..=11).contains(&i)).count();
        assert!(
            in_cheap_run >= 5,
            "Should pick at least 5 blocks from the long cheap run (01:30-03:45 equivalent)"
        );
    }
}
