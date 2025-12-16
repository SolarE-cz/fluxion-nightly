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

use bevy_ecs::prelude::Component;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::inverter::InverterOperationMode;

// ============= Scheduling Components (FluxION MVP) =============

/// A scheduled mode for a specific time block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledMode {
    /// Start time of this block
    pub block_start: DateTime<Utc>,

    /// Duration of this block (typically 15 minutes)
    pub duration_minutes: u32,

    /// Target inverter(s) for this command
    /// Empty = all inverters, Some(ids) = specific inverters only
    pub target_inverters: Option<Vec<String>>,

    /// Operation mode for this block
    pub mode: InverterOperationMode,

    /// Human-readable reason for this mode
    /// Human-readable reason for this mode
    pub reason: String,

    /// Debug info captured during scheduling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_info: Option<BlockDebugInfo>,
}

/// Debug information about strategy evaluation for a block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEvaluation {
    /// Name of the strategy
    pub strategy_name: String,

    /// Operation mode this strategy recommends
    pub mode: InverterOperationMode,

    /// Net profit score from this strategy (CZK)
    pub net_profit_czk: f32,

    /// Detailed reasoning for this strategy's decision
    pub reason: String,
}

/// Debug information captured during block scheduling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDebugInfo {
    /// All strategies that were evaluated for this block
    pub evaluated_strategies: Vec<StrategyEvaluation>,

    /// Explanation of why the winning strategy was chosen
    pub winning_reason: String,

    /// Key conditions that were checked
    pub conditions: Vec<String>,
}

/// Generated operation schedule for an inverter or set of inverters
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct OperationSchedule {
    /// Block-by-block mode assignments (15-minute granularity)
    pub scheduled_blocks: Vec<ScheduledMode>,

    /// When this schedule was generated
    pub generated_at: DateTime<Utc>,

    /// What price data version this schedule is based on
    pub based_on_price_version: DateTime<Utc>,
}

impl Default for OperationSchedule {
    fn default() -> Self {
        Self {
            scheduled_blocks: Vec::new(),
            generated_at: Utc::now(),
            based_on_price_version: Utc::now(),
        }
    }
}

impl OperationSchedule {
    /// Get the scheduled mode for the current time
    pub fn get_current_mode(&self, now: DateTime<Utc>) -> Option<&ScheduledMode> {
        self.scheduled_blocks.iter().find(|block| {
            let block_end =
                block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
            now >= block.block_start && now < block_end
        })
    }

    /// Get the scheduled mode for a specific time
    pub fn get_mode_at(&self, time: DateTime<Utc>) -> Option<&ScheduledMode> {
        self.scheduled_blocks.iter().find(|block| {
            let block_end =
                block.block_start + chrono::Duration::minutes(block.duration_minutes as i64);
            time >= block.block_start && time < block_end
        })
    }

    /// Check if schedule needs regeneration based on price data version
    pub fn needs_regeneration(&self, price_data_version: DateTime<Utc>) -> bool {
        self.based_on_price_version != price_data_version
    }
}

/// Current active mode for an inverter
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct CurrentMode {
    /// Current operation mode
    pub mode: InverterOperationMode,

    /// When this mode was set
    pub set_at: DateTime<Utc>,

    /// Why this mode was set
    pub reason: String,
}

impl Default for CurrentMode {
    fn default() -> Self {
        Self {
            mode: InverterOperationMode::SelfUse,
            // Set to far past to avoid debounce blocking initial mode changes
            set_at: Utc::now() - chrono::Duration::hours(24),
            reason: "Initial state".to_string(),
        }
    }
}

/// Schedule component data (for Web API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleData {
    pub current_mode: String,
    pub current_reason: String,
    pub current_strategy: Option<String>, // Strategy that chose this mode
    pub expected_profit: Option<f32>,     // Expected profit for current block (CZK)
    pub next_change: Option<DateTime<Utc>>,
    pub blocks_today: usize,
    pub target_soc_max: f32,                // Max battery SOC for charging
    pub target_soc_min: f32,                // Min battery SOC for discharging
    pub total_expected_profit: Option<f32>, // Total expected profit for all blocks (CZK)

    // Schedule metadata for transparency
    pub total_blocks_scheduled: usize, // Total blocks in schedule
    pub schedule_hours: f32,           // Hours of schedule data
    pub schedule_generated_at: DateTime<Utc>, // When schedule was created
    pub schedule_ends_at: Option<DateTime<Utc>>, // When schedule data ends
}
