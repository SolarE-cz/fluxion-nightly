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

use bevy_ecs::prelude::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::utils::calculate_ema;
use crate::{
    components::*,
    config_events::{ConfigUpdateEvent, UserControlUpdateEvent},
    debug::DebugModeConfig,
    resources::{SystemConfig, TimezoneConfig},
};

/// Channel for web query requests
#[derive(Resource)]
pub struct WebQueryChannel {
    pub receiver: mpsc::UnboundedReceiver<WebQueryRequest>,
}

/// Channel for config update events
#[derive(Resource)]
pub struct ConfigUpdateChannel {
    pub receiver: mpsc::UnboundedReceiver<ConfigUpdateEvent>,
}

/// Channel for user control update events
#[derive(Resource)]
pub struct UserControlUpdateChannel {
    pub receiver: mpsc::UnboundedReceiver<UserControlUpdateEvent>,
}

/// Clonable sender for web queries
#[derive(Clone)]
pub struct WebQuerySender {
    sender: mpsc::UnboundedSender<WebQueryRequest>,
}

/// Clonable sender for config updates
#[derive(Clone)]
pub struct ConfigUpdateSender {
    sender: mpsc::UnboundedSender<ConfigUpdateEvent>,
}

/// Clonable sender for user control updates
#[derive(Clone)]
pub struct UserControlUpdateSender {
    sender: mpsc::UnboundedSender<UserControlUpdateEvent>,
}

impl std::fmt::Debug for WebQuerySender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebQuerySender").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ConfigUpdateSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigUpdateSender").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for UserControlUpdateSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserControlUpdateSender")
            .finish_non_exhaustive()
    }
}

impl WebQuerySender {
    /// Create a new sender/receiver pair
    pub fn new() -> (Self, WebQueryChannel) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, WebQueryChannel { receiver })
    }

    /// Request dashboard data
    pub async fn query_dashboard(&self) -> Result<WebQueryResponse, QueryError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        self.sender
            .send(WebQueryRequest {
                query_type: QueryType::Dashboard,
                response_tx,
            })
            .map_err(|_| QueryError::ChannelClosed)?;

        response_rx.await.map_err(|_| QueryError::ResponseTimeout)
    }

    /// Request health check data
    pub async fn query_health(&self) -> Result<SystemHealthData, QueryError> {
        let response = self.query_dashboard().await?;
        Ok(response.health)
    }
}

impl ConfigUpdateSender {
    /// Create a new sender/receiver pair
    pub fn new() -> (Self, ConfigUpdateChannel) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, ConfigUpdateChannel { receiver })
    }

    /// Send a config update event
    pub fn send_update(&self, event: ConfigUpdateEvent) -> Result<(), ConfigUpdateError> {
        self.sender
            .send(event)
            .map_err(|_| ConfigUpdateError::ChannelClosed)
    }
}

impl UserControlUpdateSender {
    /// Create a new sender/receiver pair
    pub fn new() -> (Self, UserControlUpdateChannel) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, UserControlUpdateChannel { receiver })
    }

    /// Send a user control update event
    pub fn send(&self, event: UserControlUpdateEvent) -> Result<(), UserControlUpdateError> {
        self.sender
            .send(event)
            .map_err(|_| UserControlUpdateError::ChannelClosed)
    }
}

/// Error when sending user control update fails
#[derive(Debug, Clone)]
pub enum UserControlUpdateError {
    ChannelClosed,
}

impl std::fmt::Display for UserControlUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserControlUpdateError::ChannelClosed => {
                write!(f, "user control update channel closed")
            }
        }
    }
}

/// Web query request from async web handlers to ECS
pub struct WebQueryRequest {
    pub query_type: QueryType,
    pub response_tx: tokio::sync::oneshot::Sender<WebQueryResponse>,
}

/// Types of queries the web UI can make
pub enum QueryType {
    Dashboard,
}

/// Battery SOC history point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySocHistoryPoint {
    pub timestamp: DateTime<Utc>,
    pub soc: f32,
}

/// Battery SOC prediction point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatterySocPredictionPoint {
    pub timestamp: DateTime<Utc>,
    pub soc: f32,
}

/// PV generation history point for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PvGenerationHistoryPoint {
    pub timestamp: DateTime<Utc>,
    pub power_w: f32,
}

/// Aggregated consumption statistics used by strategies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumptionStats {
    /// Historical EMA of daily consumption (kWh/day), if available
    pub ema_kwh: Option<f32>,
    /// Number of days used for EMA calculation
    pub ema_days: usize,
    /// Total grid import today (kWh), if available
    pub today_import_kwh: Option<f32>,
    /// Total grid import yesterday (kWh), if available
    pub yesterday_import_kwh: Option<f32>,
    /// Average hourly consumption profile (kWh per hour, 24 entries, index = hour of day)
    /// Averaged over last 7 days of historical data
    pub hourly_consumption_profile: Option<Vec<f32>>,
}

/// HDO (High/Low tariff) schedule information for chart display
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HdoScheduleInfo {
    /// Low tariff periods for today: (start "HH:MM", end "HH:MM")
    pub low_tariff_periods: Vec<(String, String)>,
    /// Low tariff grid fee in CZK/kWh
    pub low_tariff_czk: f32,
    /// High tariff grid fee in CZK/kWh
    pub high_tariff_czk: f32,
    /// Last update timestamp (ISO 8601)
    pub last_updated: Option<String>,
}

/// Pricing fees configuration for chart display
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PricingFees {
    /// Spot buy fee in CZK/kWh
    pub buy_fee_czk: f32,
    /// Spot sell fee in CZK/kWh
    pub sell_fee_czk: f32,
}

/// Solar forecast data for dashboard display
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SolarForecastInfo {
    /// Total solar production forecast for today (kWh)
    pub total_today_kwh: f32,
    /// Remaining solar production forecast for today (kWh)
    pub remaining_today_kwh: f32,
    /// Solar production forecast for tomorrow (kWh)
    pub tomorrow_kwh: f32,
    /// Actual solar energy generated today (kWh) - from inverter
    pub actual_today_kwh: Option<f32>,
    /// Accuracy: actual vs predicted (e.g., +5.2% means actual is 5.2% higher than predicted)
    /// Only available if both actual and predicted > 0
    pub accuracy_percent: Option<f32>,
    /// Whether data is available (sensors discovered)
    pub available: bool,
}

/// Response containing ECS component data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebQueryResponse {
    pub timestamp: DateTime<Utc>,
    pub debug_mode: bool,
    pub inverters: Vec<InverterData>,
    pub schedule: Option<ScheduleData>,
    pub prices: Option<PriceData>,
    pub health: SystemHealthData,
    pub timezone: Option<String>,
    pub battery_soc_history: Option<Vec<BatterySocHistoryPoint>>,
    pub battery_soc_prediction: Option<Vec<BatterySocPredictionPoint>>,
    pub pv_generation_history: Option<Vec<PvGenerationHistoryPoint>>,
    /// Aggregated consumption statistics (EMA, imports)
    pub consumption_stats: Option<ConsumptionStats>,
    /// HDO (grid tariff) schedule for chart display
    pub hdo_schedule: Option<HdoScheduleInfo>,
    /// Pricing fees from config for chart display
    pub pricing_fees: Option<PricingFees>,
    /// Solar forecast data
    pub solar_forecast: Option<SolarForecastInfo>,
}

/// Inverter component data bundle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterData {
    // Core identification
    pub id: String,
    pub topology: String,

    // Current mode (FluxION's internal planned mode)
    pub mode: String,
    pub mode_reason: String,
    // Actual mode reported by the inverter hardware
    pub actual_mode: Option<String>,
    // Whether actual mode matches the planned mode
    pub mode_synced: bool,

    // Battery
    pub battery_soc: f32,
    pub battery_power_w: f32,
    pub battery_voltage_v: f32,
    pub battery_current_a: f32,
    pub battery_temperature_c: f32,

    // Grid
    pub grid_power_w: f32,
    pub grid_voltage_v: f32,
    pub grid_frequency_hz: f32,

    // PV Generation
    pub pv_power_w: f32,
    pub pv1_power_w: f32,
    pub pv2_power_w: f32,
    pub daily_energy_kwh: f32,
    pub total_energy_kwh: f32,

    // Status
    pub online: bool,
    pub run_mode: String,
    pub error_code: u16,
    pub inverter_temperature_c: f32,

    // Extended data from RawInverterState
    pub house_load_w: Option<f32>,
    pub grid_import_w: Option<f32>,
    pub grid_export_w: Option<f32>,
    pub grid_import_today_kwh: Option<f32>,
    pub grid_export_today_kwh: Option<f32>,
    pub inverter_frequency_hz: Option<f32>,
    pub inverter_voltage_v: Option<f32>,
    pub inverter_current_a: Option<f32>,
    pub inverter_power_w: Option<f32>,
    pub battery_capacity_kwh: Option<f32>,
    pub battery_input_energy_today_kwh: Option<f32>,
    pub battery_output_energy_today_kwh: Option<f32>,
    pub today_solar_energy_kwh: Option<f32>,
    pub total_solar_energy_kwh: Option<f32>,
    /// Grid import EMA (historical average consumption per day in kWh)
    pub grid_import_ema_kwh: Option<f32>,
}

/// Schedule component data
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

/// Price component data with chart
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceData {
    pub current_price: f32,
    pub min_price: f32,
    pub max_price: f32,
    pub avg_price: f32,
    pub blocks: Vec<PriceBlockData>,
    // Today's price statistics
    pub today_min_price: f32,
    pub today_max_price: f32,
    pub today_avg_price: f32,
    pub today_median_price: f32,
    // Tomorrow's price statistics (None if not yet available)
    pub tomorrow_min_price: Option<f32>,
    pub tomorrow_max_price: Option<f32>,
    pub tomorrow_avg_price: Option<f32>,
    pub tomorrow_median_price: Option<f32>,
}

/// Individual price block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBlockData {
    pub timestamp: DateTime<Utc>,
    pub price: f32,
    pub block_type: String,           // "charge", "discharge", "self-use"
    pub target_soc: Option<f32>,      // Target SOC for charge/discharge blocks
    pub strategy: Option<String>,     // Strategy that chose this mode
    pub expected_profit: Option<f32>, // Expected profit for this block (CZK)
    pub reason: Option<String>,       // Detailed reason for the decision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_uid: Option<String>, // Decision UID for debugging (e.g., "winter_adaptive_v2:scheduled_charge")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_info: Option<crate::strategy::BlockDebugInfo>, // Debug info (only when log_level=debug)
    pub is_historical: bool, // True if block is in the past (shows regenerated schedule, not actual history)
}

/// System health data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealthData {
    pub inverter_source: bool,
    pub price_source: bool,
    pub last_update: DateTime<Utc>,
    pub errors: Vec<String>,
}

/// Query error types
#[derive(Debug)]
pub enum QueryError {
    ChannelClosed,
    ResponseTimeout,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelClosed => write!(f, "Query channel closed"),
            Self::ResponseTimeout => write!(f, "Response timeout"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Config update error types
#[derive(Debug)]
pub enum ConfigUpdateError {
    ChannelClosed,
}

impl std::fmt::Display for ConfigUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelClosed => write!(f, "Config update channel closed"),
        }
    }
}

impl std::error::Error for ConfigUpdateError {}

type InverterQuery<'a> = (
    &'a Inverter,
    &'a CurrentMode,
    Option<&'a BatteryStatus>,
    Option<&'a GridPower>,
    Option<&'a PowerGeneration>,
    Option<&'a InverterStatus>,
    Option<&'a RawInverterState>,
);

/// Extract strategy name and expected profit from reason string
/// Format: "Strategy - reason (expected profit: X.XX CZK)"
fn extract_strategy_info(reason: &str) -> (Option<String>, Option<f32>) {
    // Try to extract strategy name (before first " - ")
    let strategy = reason.split(" - ").next().map(|s| s.trim().to_string());

    // Try to extract profit (in parentheses at end)
    let profit = if let Some(start) = reason.rfind("expected profit: ") {
        let profit_str = &reason[start + 17..]; // Skip "expected profit: "
        if let Some(end) = profit_str.find(" CZK") {
            profit_str[..end].trim().parse::<f32>().ok()
        } else {
            None
        }
    } else {
        None
    };

    (strategy, profit)
}

/// ECS system that processes web query requests
#[allow(clippy::too_many_arguments)]
pub fn web_query_system(
    debug_config: Res<DebugModeConfig>,
    system_config: Res<SystemConfig>,
    timezone_config: Option<Res<TimezoneConfig>>,
    mut channel: ResMut<WebQueryChannel>,
    inverters: Query<InverterQuery>,
    schedule: Query<&OperationSchedule>,
    price_data: Query<&SpotPriceData>,
    price_analysis: Query<&PriceAnalysis>,
    battery_history: Res<BatteryHistory>,
    pv_history: Res<PvHistory>,
    consumption_history: Option<Res<ConsumptionHistory>>,
    consumption_history_config: Option<Res<ConsumptionHistoryConfig>>,
    hdo_data: Option<Res<crate::async_systems::HdoScheduleData>>,
    solar_forecast: Option<Res<crate::async_systems::SolarForecastData>>,
) {
    // Process all pending queries
    while let Ok(request) = channel.receiver.try_recv() {
        trace!(
            "Processing web query: {:?}",
            std::any::type_name_of_val(&request.query_type)
        );

        let response = match request.query_type {
            QueryType::Dashboard => build_dashboard_response(
                &debug_config,
                &system_config,
                timezone_config.as_deref(),
                &inverters,
                &schedule,
                &price_data,
                &price_analysis,
                &battery_history,
                &pv_history,
                consumption_history.as_deref(),
                consumption_history_config.as_deref(),
                hdo_data.as_deref(),
                solar_forecast.as_deref(),
            ),
        };

        // Send response (ignore if receiver dropped)
        let _ = request.response_tx.send(response);
    }
}

///Build dashboard response from ECS queries
#[allow(clippy::too_many_arguments)]
fn build_dashboard_response(
    debug_config: &DebugModeConfig,
    system_config: &SystemConfig,
    timezone_config: Option<&TimezoneConfig>,
    inverters: &Query<InverterQuery>,
    schedule: &Query<&OperationSchedule>,
    price_data: &Query<&SpotPriceData>,
    price_analysis: &Query<&PriceAnalysis>,
    battery_history: &BatteryHistory,
    pv_history: &PvHistory,
    consumption_history: Option<&ConsumptionHistory>,
    consumption_history_config: Option<&ConsumptionHistoryConfig>,
    hdo_data: Option<&crate::async_systems::HdoScheduleData>,
    solar_forecast_data: Option<&crate::async_systems::SolarForecastData>,
) -> WebQueryResponse {
    let now = Utc::now();

    // Compute consumption statistics (EMA and imports) early for use in inverter data
    let consumption_stats = {
        // Determine EMA window (days) from config if available, otherwise default to 7
        let ema_days = consumption_history_config
            .map(|cfg| cfg.ema_days)
            .unwrap_or(7);

        let mut ema_kwh: Option<f32> = None;
        let mut today_import_kwh: Option<f32> = None;
        let mut yesterday_import_kwh: Option<f32> = None;

        if let Some(history) = consumption_history {
            // EMA based on historical daily consumption values
            let values = history.consumption_values();
            if history.has_sufficient_data(ema_days) {
                ema_kwh = calculate_ema(&values, ema_days);
            }

            // Today and yesterday imports from history summaries (newest first)
            let today_date = now.date_naive();
            let yesterday_date = today_date.pred_opt().unwrap_or(today_date);

            for summary in history.summaries().iter() {
                let date = summary.date.date_naive();
                if date == today_date && today_import_kwh.is_none() {
                    today_import_kwh = Some(summary.grid_import_kwh);
                } else if date == yesterday_date && yesterday_import_kwh.is_none() {
                    yesterday_import_kwh = Some(summary.grid_import_kwh);
                }

                if today_import_kwh.is_some() && yesterday_import_kwh.is_some() {
                    break;
                }
            }
        }

        // Extract hourly profile if available
        let hourly_consumption_profile = consumption_history
            .and_then(|h| h.hourly_profile())
            .map(|p| p.hourly_avg_kwh.to_vec());

        // Only include stats if we have at least some meaningful data
        if ema_kwh.is_some() || today_import_kwh.is_some() || yesterday_import_kwh.is_some() {
            Some(ConsumptionStats {
                ema_kwh,
                ema_days,
                today_import_kwh,
                yesterday_import_kwh,
                hourly_consumption_profile,
            })
        } else {
            None
        }
    };

    // Extract EMA for use in inverter data
    let grid_import_ema = consumption_stats.as_ref().and_then(|stats| stats.ema_kwh);

    // Query inverter data
    let inverter_data: Vec<InverterData> = inverters
        .iter()
        .map(|(inv, mode, battery, grid, pv, status, raw_state)| {
            InverterData {
                // Core identification
                id: inv.id.clone(),
                topology: get_topology_string(&inv.id, system_config),

                // Current mode (planned by FluxION)
                mode: format!("{}", mode.mode),
                mode_reason: mode.reason.clone(),
                // Actual mode from inverter hardware
                actual_mode: raw_state.map(|r| format!("{}", r.state.work_mode)),
                // Whether actual mode matches planned mode
                mode_synced: raw_state
                    .map(|r| r.state.work_mode == mode.mode)
                    .unwrap_or(false),

                // Battery
                battery_soc: battery.map(|b| b.soc_percent as f32).unwrap_or(0.0),
                battery_power_w: battery.map(|b| b.power_w as f32).unwrap_or(0.0),
                battery_voltage_v: battery.map(|b| b.voltage_v).unwrap_or(0.0),
                battery_current_a: battery.map(|b| b.current_a).unwrap_or(0.0),
                battery_temperature_c: battery.map(|b| b.temperature_c).unwrap_or(0.0),

                // Grid
                grid_power_w: grid.map(|g| g.export_power_w as f32).unwrap_or(0.0),
                grid_voltage_v: grid.map(|g| g.grid_voltage_v).unwrap_or(0.0),
                grid_frequency_hz: grid.map(|g| g.grid_frequency_hz).unwrap_or(0.0),

                // PV Generation
                pv_power_w: pv.map(|p| p.current_power_w as f32).unwrap_or(0.0),
                pv1_power_w: pv.map(|p| p.pv1_power_w as f32).unwrap_or(0.0),
                pv2_power_w: pv.map(|p| p.pv2_power_w as f32).unwrap_or(0.0),
                daily_energy_kwh: pv.map(|p| p.daily_energy_kwh).unwrap_or(0.0),
                total_energy_kwh: pv.map(|p| p.total_energy_kwh).unwrap_or(0.0),

                // Status
                online: status.map(|s| s.connection_healthy).unwrap_or(false),
                run_mode: status
                    .map(|s| format!("{:?}", s.run_mode))
                    .unwrap_or_else(|| "Unknown".to_string()),
                error_code: status.map(|s| s.error_code).unwrap_or(0),
                inverter_temperature_c: status.map(|s| s.temperature_c).unwrap_or(0.0),

                // Extended data from RawInverterState
                house_load_w: raw_state.and_then(|r| r.state.house_load_w),
                grid_import_w: raw_state.and_then(|r| r.state.grid_import_w),
                grid_export_w: raw_state.and_then(|r| r.state.grid_export_w),
                grid_import_today_kwh: raw_state.and_then(|r| r.state.grid_import_today_kwh),
                grid_export_today_kwh: raw_state.and_then(|r| r.state.grid_export_today_kwh),
                inverter_frequency_hz: raw_state.and_then(|r| r.state.inverter_frequency_hz),
                inverter_voltage_v: raw_state.and_then(|r| r.state.inverter_voltage_v),
                inverter_current_a: raw_state.and_then(|r| r.state.inverter_current_a),
                inverter_power_w: raw_state.and_then(|r| r.state.inverter_power_w),
                battery_capacity_kwh: raw_state.and_then(|r| r.state.battery_capacity_kwh),
                battery_input_energy_today_kwh: raw_state
                    .and_then(|r| r.state.battery_input_energy_today_kwh),
                battery_output_energy_today_kwh: raw_state
                    .and_then(|r| r.state.battery_output_energy_today_kwh),
                today_solar_energy_kwh: raw_state.and_then(|r| r.state.today_solar_energy_kwh),
                total_solar_energy_kwh: raw_state.and_then(|r| r.state.total_solar_energy_kwh),
                grid_import_ema_kwh: grid_import_ema,
            }
        })
        .collect();

    // Query schedule data
    let schedule_data = schedule.single().ok().and_then(|sched| {
        sched.get_current_mode(now).map(|current| {
            // Find next change
            let next_change = sched
                .scheduled_blocks
                .iter()
                .find(|block| block.block_start > now)
                .map(|block| block.block_start);

            // Extract strategy and profit from reason string
            // Format: "Strategy - reason (expected profit: X.XX CZK)"
            let (strategy, profit) = extract_strategy_info(&current.reason);

            // Calculate total expected profit from all blocks
            let total_profit = sched
                .scheduled_blocks
                .iter()
                .filter_map(|block| extract_strategy_info(&block.reason).1)
                .sum::<f32>();

            // Calculate schedule metadata
            let total_blocks = sched.scheduled_blocks.len();
            let schedule_hours = total_blocks as f32 / 4.0;
            let schedule_ends_at = sched.scheduled_blocks.last().map(|b| b.block_start);

            ScheduleData {
                current_mode: format!("{}", current.mode),
                current_reason: current.reason.clone(),
                current_strategy: strategy,
                expected_profit: profit,
                next_change,
                blocks_today: sched.scheduled_blocks.len(),
                target_soc_max: system_config.control_config.max_battery_soc,
                target_soc_min: system_config.control_config.min_battery_soc,
                total_expected_profit: Some(total_profit),
                total_blocks_scheduled: total_blocks,
                schedule_hours,
                schedule_generated_at: sched.generated_at,
                schedule_ends_at,
            }
        })
    });

    // Query price data and enrich with schedule info
    let price_data_result =
        price_data
            .single()
            .ok()
            .zip(price_analysis.single().ok())
            .map(|(prices, analysis)| {
                // Get schedule for matching blocks with strategy info
                let sched = schedule.single().ok();

                // Build price blocks with classification
                let blocks: Vec<PriceBlockData> = prices
                    .time_block_prices
                    .iter()
                    .enumerate()
                    .map(|(idx, block)| {
                        // CRITICAL: Match schedule blocks by TIMESTAMP, not array index
                        // This is essential because schedule may have filtered past blocks,
                        // causing index misalignment with price data blocks.
                        let (
                            block_type,
                            target_soc,
                            strategy,
                            profit,
                            reason,
                            decision_uid,
                            debug_info,
                        ) = sched
                            .and_then(|s| {
                                // Find the scheduled block that matches this price block's timestamp
                                s.scheduled_blocks
                                    .iter()
                                    .find(|sb| sb.block_start == block.block_start)
                            })
                            .map(|sb| {
                                let (strat, prof) = extract_strategy_info(&sb.reason);
                                let block_type_str = match sb.mode {
                                    InverterOperationMode::ForceCharge => "charge",
                                    InverterOperationMode::ForceDischarge => "discharge",
                                    InverterOperationMode::SelfUse => "self-use",
                                    InverterOperationMode::BackUpMode => "backup",
                                };
                                let target_soc = match sb.mode {
                                    InverterOperationMode::ForceCharge => {
                                        Some(system_config.control_config.max_battery_soc)
                                    }
                                    InverterOperationMode::ForceDischarge => {
                                        Some(system_config.control_config.min_battery_soc)
                                    }
                                    InverterOperationMode::SelfUse
                                    | InverterOperationMode::BackUpMode => None,
                                };
                                (
                                    block_type_str.to_string(),
                                    target_soc,
                                    strat,
                                    prof,
                                    Some(sb.reason.clone()),
                                    sb.decision_uid.clone(),
                                    sb.debug_info.clone(),
                                )
                            })
                            .unwrap_or_else(|| {
                                // Fallback: No matching schedule block found
                                // Check if this is a charge/discharge block from analysis
                                if analysis.charge_blocks.contains(&idx) {
                                    (
                                        "charge".to_string(),
                                        Some(system_config.control_config.max_battery_soc),
                                        Some("Time-Aware Charge".to_string()),
                                        None,
                                        Some(format!(
                                            "Time-Aware Charge - Cheapest block ({:.3} CZK/kWh)",
                                            block.price_czk_per_kwh
                                        )),
                                        None,
                                        None,
                                    )
                                } else if analysis.discharge_blocks.contains(&idx) {
                                    (
                                        "discharge".to_string(),
                                        Some(system_config.control_config.min_battery_soc),
                                        Some("Winter-Peak-Discharge".to_string()),
                                        None,
                                        Some(format!(
                                            "Winter-Peak-Discharge - Peak price ({:.3} CZK/kWh)",
                                            block.price_czk_per_kwh
                                        )),
                                        None,
                                        None,
                                    )
                                } else {
                                    // Default to self-use with strategy name
                                    (
                                        "self-use".to_string(),
                                        None,
                                        Some("Self-Use".to_string()),
                                        None,
                                        Some(format!(
                                            "Self-Use - Normal operation ({:.3} CZK/kWh)",
                                            block.price_czk_per_kwh
                                        )),
                                        None,
                                        None,
                                    )
                                }
                            });

                        PriceBlockData {
                            timestamp: block.block_start,
                            price: block.price_czk_per_kwh,
                            block_type,
                            target_soc,
                            strategy,
                            expected_profit: profit,
                            reason,
                            decision_uid,
                            debug_info,
                            is_historical: block.block_start < now, // Mark past blocks as historical (regenerated, not actual)
                        }
                    })
                    .collect();

                // Find current price (closest to now)
                let current_price = prices
                    .time_block_prices
                    .iter()
                    .min_by_key(|b| (b.block_start - now).num_seconds().abs())
                    .map(|b| b.price_czk_per_kwh)
                    .unwrap_or(0.0);

                // Separate today and tomorrow prices based on dates
                let today_date = now.date_naive();
                let tomorrow_date = today_date + chrono::Duration::days(1);

                let today_prices: Vec<f32> = prices
                    .time_block_prices
                    .iter()
                    .filter(|b| b.block_start.date_naive() == today_date)
                    .map(|b| b.price_czk_per_kwh)
                    .collect();

                let tomorrow_prices: Vec<f32> = prices
                    .time_block_prices
                    .iter()
                    .filter(|b| b.block_start.date_naive() == tomorrow_date)
                    .map(|b| b.price_czk_per_kwh)
                    .collect();

                // Calculate today's statistics
                let today_min_price = today_prices
                    .iter()
                    .copied()
                    .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0);
                let today_max_price = today_prices
                    .iter()
                    .copied()
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0);
                let today_avg_price = if !today_prices.is_empty() {
                    today_prices.iter().sum::<f32>() / today_prices.len() as f32
                } else {
                    0.0
                };
                let today_median_price = calculate_median(&today_prices);

                // Calculate tomorrow's statistics (may not be available yet)
                let (
                    tomorrow_min_price,
                    tomorrow_max_price,
                    tomorrow_avg_price,
                    tomorrow_median_price,
                ) = if !tomorrow_prices.is_empty() {
                    let min = tomorrow_prices
                        .iter()
                        .copied()
                        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                        .unwrap_or(0.0);
                    let max = tomorrow_prices
                        .iter()
                        .copied()
                        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                        .unwrap_or(0.0);
                    let avg = tomorrow_prices.iter().sum::<f32>() / tomorrow_prices.len() as f32;
                    let median = calculate_median(&tomorrow_prices);
                    (Some(min), Some(max), Some(avg), Some(median))
                } else {
                    (None, None, None, None)
                };

                PriceData {
                    current_price,
                    min_price: analysis.price_range.min_czk_per_kwh,
                    max_price: analysis.price_range.max_czk_per_kwh,
                    avg_price: analysis.price_range.avg_czk_per_kwh,
                    blocks,
                    today_min_price,
                    today_max_price,
                    today_avg_price,
                    today_median_price,
                    tomorrow_min_price,
                    tomorrow_max_price,
                    tomorrow_avg_price,
                    tomorrow_median_price,
                }
            });

    // Build health status
    let has_inverter_data = !inverter_data.is_empty();
    let has_price_data = price_data.single().is_ok();

    let health = SystemHealthData {
        inverter_source: has_inverter_data,
        price_source: has_price_data,
        last_update: now,
        errors: vec![],
    };

    // Fallback for today's import from live inverter data if history is missing
    let consumption_stats = if let Some(mut stats) = consumption_stats {
        if stats.today_import_kwh.is_none() {
            let live_today_import: f32 = inverter_data
                .iter()
                .filter_map(|inv| inv.grid_import_today_kwh)
                .sum();
            if live_today_import > 0.0 {
                stats.today_import_kwh = Some(live_today_import);
            }
        }
        Some(stats)
    } else {
        None
    };

    // Convert battery history to response format
    let battery_soc_history = if !battery_history.is_empty() {
        let history_points: Vec<BatterySocHistoryPoint> = battery_history
            .points_chronological()
            .iter()
            .map(|point| BatterySocHistoryPoint {
                timestamp: point.timestamp,
                soc: point.soc,
            })
            .collect();

        debug!(
            "📊 Sending {} battery history points to web (oldest: {:.1}%, newest: {:.1}%)",
            history_points.len(),
            history_points.first().map(|p| p.soc).unwrap_or(0.0),
            history_points.last().map(|p| p.soc).unwrap_or(0.0)
        );

        Some(history_points)
    } else {
        debug!("📊 No battery history available yet");
        None
    };

    // Convert PV generation history to response format
    let pv_generation_history = if !pv_history.is_empty() {
        let pv_points: Vec<PvGenerationHistoryPoint> = pv_history
            .points_chronological()
            .iter()
            .map(|point| PvGenerationHistoryPoint {
                timestamp: point.timestamp,
                power_w: point.power_w,
            })
            .collect();

        debug!(
            "☀️ Sending {} PV history points to web (oldest: {:.0}W, newest: {:.0}W)",
            pv_points.len(),
            pv_points.first().map(|p| p.power_w).unwrap_or(0.0),
            pv_points.last().map(|p| p.power_w).unwrap_or(0.0)
        );

        Some(pv_points)
    } else {
        debug!("☀️ No PV generation history available yet");
        None
    };

    trace!(
        "Built dashboard response: {} inverters, schedule={}, prices={}, battery_history={}, pv_history={}",
        inverter_data.len(),
        schedule_data.is_some(),
        price_data_result.is_some(),
        battery_soc_history.is_some(),
        pv_generation_history.is_some()
    );

    // Calculate battery SOC predictions based on schedule
    let battery_soc_prediction = schedule.single().ok().and_then(|sched| {
        // Get current battery SOC from first inverter
        let current_soc = inverter_data
            .first()
            .map(|inv| inv.battery_soc)
            .unwrap_or(50.0);

        // Get current house load and PV power for accurate self-use predictions
        let house_load_w = inverter_data.first().and_then(|inv| inv.house_load_w);
        let pv_power_w = inverter_data.first().map(|inv| inv.pv_power_w);

        // Generate prediction using configured charge/discharge rates
        let prediction = crate::components::predict_battery_soc(
            sched,
            &system_config.control_config,
            current_soc,
            Some(system_config.control_config.max_battery_charge_rate_kw), // Use configured charge rate
            Some(system_config.control_config.max_battery_charge_rate_kw), // Use same for discharge
            house_load_w,
            pv_power_w,
        );

        if prediction.is_empty() {
            debug!("📈 No battery SOC predictions generated (empty schedule)");
            None
        } else {
            let prediction_points: Vec<BatterySocPredictionPoint> = prediction
                .points()
                .iter()
                .map(|point| BatterySocPredictionPoint {
                    timestamp: point.timestamp,
                    soc: point.soc_percent,
                })
                .collect();

            debug!(
                "📈 Generated {} battery SOC predictions (start: {:.1}%, end: {:.1}%)",
                prediction_points.len(),
                prediction_points.first().map(|p| p.soc).unwrap_or(0.0),
                prediction_points.last().map(|p| p.soc).unwrap_or(0.0)
            );

            Some(prediction_points)
        }
    });

    // Build HDO schedule info for chart display
    let hdo_schedule = hdo_data.map(|hdo| {
        // Log HDO data being sent to web
        if hdo.low_tariff_periods.is_empty() {
            warn!(
                "⚠️ HDO schedule has 0 low tariff periods! last_updated: {:?}",
                hdo.last_updated
            );
        } else {
            debug!(
                "📊 HDO schedule for web: {} periods, low={:.2} CZK, high={:.2} CZK",
                hdo.low_tariff_periods.len(),
                hdo.low_tariff_czk,
                hdo.high_tariff_czk
            );
        }
        HdoScheduleInfo {
            low_tariff_periods: hdo.low_tariff_periods.clone(),
            low_tariff_czk: hdo.low_tariff_czk,
            high_tariff_czk: hdo.high_tariff_czk,
            last_updated: hdo.last_updated.map(|t| t.to_rfc3339()),
        }
    });

    // Build pricing fees from config
    let pricing_fees = Some(PricingFees {
        buy_fee_czk: system_config.pricing_config.spot_buy_fee_czk,
        sell_fee_czk: system_config.pricing_config.spot_sell_fee_czk,
    });

    // Build solar forecast info
    // Get actual solar energy today from inverters (sum all inverters)
    let actual_solar_today: Option<f32> = {
        let sum: f32 = inverter_data
            .iter()
            .filter_map(|inv| inv.today_solar_energy_kwh)
            .sum();
        if sum > 0.0 { Some(sum) } else { None }
    };

    let solar_forecast = solar_forecast_data.map(|sf| {
        let has_data = sf.total_today_kwh > 0.0
            || sf.remaining_today_kwh > 0.0
            || sf.tomorrow_kwh > 0.0;

        // Calculate accuracy: (actual - predicted) / predicted * 100
        // Positive = actual is higher than predicted, negative = actual is lower
        let accuracy_percent = match (actual_solar_today, sf.total_today_kwh > 0.0) {
            (Some(actual), true) => {
                let diff_percent = ((actual - sf.total_today_kwh) / sf.total_today_kwh) * 100.0;
                Some(diff_percent)
            }
            _ => None,
        };

        debug!(
            "☀️ Solar forecast for web: predicted={:.1} kWh, actual={:?} kWh, remaining={:.1} kWh, tomorrow={:.1} kWh, accuracy={:?}%",
            sf.total_today_kwh, actual_solar_today, sf.remaining_today_kwh, sf.tomorrow_kwh, accuracy_percent
        );

        SolarForecastInfo {
            total_today_kwh: sf.total_today_kwh,
            remaining_today_kwh: sf.remaining_today_kwh,
            tomorrow_kwh: sf.tomorrow_kwh,
            actual_today_kwh: actual_solar_today,
            accuracy_percent,
            available: has_data,
        }
    });

    if solar_forecast.is_none() {
        debug!("☀️ Solar forecast resource not available in ECS");
    }

    WebQueryResponse {
        timestamp: now,
        debug_mode: debug_config.is_enabled(),
        inverters: inverter_data,
        schedule: schedule_data,
        prices: price_data_result,
        health,
        timezone: timezone_config
            .and_then(|tz| tz.timezone.clone())
            .or_else(|| system_config.system_config.timezone.clone()),
        battery_soc_history,
        battery_soc_prediction,
        pv_generation_history,
        consumption_stats,
        hdo_schedule,
        pricing_fees,
        solar_forecast,
    }
}

/// Calculate median of a slice of f32 values
fn calculate_median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// Helper to determine topology string from config
fn get_topology_string(inverter_id: &str, config: &SystemConfig) -> String {
    if let Some(inv_config) = config.inverters.iter().find(|i| i.id == inverter_id) {
        match &inv_config.topology {
            crate::resources::InverterTopology::Independent => "Independent".to_string(),
            crate::resources::InverterTopology::Master { slave_ids } => {
                format!("Master ({} slaves)", slave_ids.len())
            }
            crate::resources::InverterTopology::Slave { master_id } => {
                format!("Slave of {}", master_id)
            }
        }
    } else {
        "Unknown".to_string()
    }
}
