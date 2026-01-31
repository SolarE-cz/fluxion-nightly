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

//! Adapter for wrapping Rust-native EconomicStrategy implementations as plugins.

use crate::manager::Plugin;
use crate::protocol::{BlockDecision, EvaluationRequest};
use fluxion_types::config::ControlConfig;
use fluxion_types::pricing::TimeBlockPrice;
use std::fmt::Debug;

/// Trait alias for existing strategy implementations
pub trait EconomicStrategy: Send + Sync {
    /// Get the name of this strategy
    fn name(&self) -> &str;

    /// Evaluate this strategy for a given time block
    fn evaluate(&self, context: &EvaluationContext<'_>) -> BlockEvaluation;

    /// Check if this strategy is enabled
    fn is_enabled(&self) -> bool {
        true
    }
}

/// Evaluation context for Rust strategies (matches fluxion-core::strategy::EvaluationContext)
#[derive(Debug)]
pub struct EvaluationContext<'a> {
    /// Price information for this block
    pub price_block: &'a TimeBlockPrice,
    /// Control configuration
    pub control_config: &'a ControlConfig,
    /// Current battery SOC (%)
    pub current_battery_soc: f32,
    /// Solar forecast for this block (kWh)
    pub solar_forecast_kwh: f32,
    /// Consumption forecast for this block (kWh)
    pub consumption_forecast_kwh: f32,
    /// Grid export price (CZK/kWh)
    pub grid_export_price_czk_per_kwh: f32,
    /// All price blocks for analysis
    pub all_price_blocks: Option<&'a [TimeBlockPrice]>,
    /// Backup discharge minimum SOC (%)
    pub backup_discharge_min_soc: f32,
    /// Grid import today (kWh)
    pub grid_import_today_kwh: Option<f32>,
    /// Consumption today (kWh)
    pub consumption_today_kwh: Option<f32>,
    /// Average hourly consumption profile (kWh per hour, 24 entries, index = hour of day)
    pub hourly_consumption_profile: Option<&'a [f32; 24]>,
}

/// Block evaluation result (matches fluxion-core::strategy::BlockEvaluation)
#[derive(Debug)]
pub struct BlockEvaluation {
    /// Block start time
    pub block_start: chrono::DateTime<chrono::Utc>,
    /// Duration in minutes
    pub duration_minutes: u32,
    /// Recommended mode
    pub mode: fluxion_types::inverter::InverterOperationMode,
    /// Expected revenue (CZK)
    pub revenue_czk: f32,
    /// Expected cost (CZK)
    pub cost_czk: f32,
    /// Net profit (CZK)
    pub net_profit_czk: f32,
    /// Reason for decision
    pub reason: String,
    /// Strategy name
    pub strategy_name: String,
    /// Decision UID
    pub decision_uid: Option<String>,
}

/// Wraps a Rust EconomicStrategy as a Plugin
#[derive(Debug)]
pub struct RustStrategyAdapter<S: EconomicStrategy + Debug> {
    strategy: S,
    priority: u8,
    control_config: ControlConfig,
}

impl<S: EconomicStrategy + Debug> RustStrategyAdapter<S> {
    /// Create a new adapter
    pub fn new(strategy: S, priority: u8, control_config: ControlConfig) -> Self {
        Self {
            strategy,
            priority,
            control_config,
        }
    }
}

impl<S: EconomicStrategy + Debug + 'static> Plugin for RustStrategyAdapter<S> {
    fn name(&self) -> &str {
        self.strategy.name()
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn is_enabled(&self) -> bool {
        self.strategy.is_enabled()
    }

    fn evaluate(&self, request: &EvaluationRequest) -> anyhow::Result<BlockDecision> {
        // Convert EvaluationRequest to EvaluationContext
        let price_block = TimeBlockPrice {
            block_start: request.block.block_start,
            duration_minutes: request.block.duration_minutes,
            price_czk_per_kwh: request.block.price_czk_per_kwh,
            effective_price_czk_per_kwh: request.block.effective_price_czk_per_kwh,
        };

        let all_blocks: Vec<TimeBlockPrice> = request
            .all_blocks
            .iter()
            .map(|b| TimeBlockPrice {
                block_start: b.block_start,
                duration_minutes: b.duration_minutes,
                price_czk_per_kwh: b.price_czk_per_kwh,
                effective_price_czk_per_kwh: b.effective_price_czk_per_kwh,
            })
            .collect();

        let context = EvaluationContext {
            price_block: &price_block,
            control_config: &self.control_config,
            current_battery_soc: request.battery.current_soc_percent,
            solar_forecast_kwh: request.forecast.solar_kwh,
            consumption_forecast_kwh: request.forecast.consumption_kwh,
            grid_export_price_czk_per_kwh: request.forecast.grid_export_price_czk_per_kwh,
            all_price_blocks: Some(&all_blocks),
            backup_discharge_min_soc: request.backup_discharge_min_soc,
            grid_import_today_kwh: request.historical.grid_import_today_kwh,
            consumption_today_kwh: request.historical.consumption_today_kwh,
            hourly_consumption_profile: None,
        };

        // Call the strategy
        let eval = self.strategy.evaluate(&context);

        // Convert BlockEvaluation to BlockDecision
        Ok(BlockDecision {
            block_start: eval.block_start,
            duration_minutes: eval.duration_minutes,
            mode: eval.mode.into(),
            reason: eval.reason,
            priority: self.priority,
            strategy_name: Some(eval.strategy_name),
            confidence: None,
            expected_profit_czk: Some(eval.net_profit_czk),
            decision_uid: eval.decision_uid,
        })
    }
}

/// Helper to convert protocol types to fluxion-types
impl From<&EvaluationRequest> for Vec<TimeBlockPrice> {
    fn from(request: &EvaluationRequest) -> Self {
        request
            .all_blocks
            .iter()
            .map(|b| TimeBlockPrice {
                block_start: b.block_start,
                duration_minutes: b.duration_minutes,
                price_czk_per_kwh: b.price_czk_per_kwh,
                effective_price_czk_per_kwh: b.effective_price_czk_per_kwh,
            })
            .collect()
    }
}
