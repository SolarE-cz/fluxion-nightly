// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Strategy simulator API endpoints.
//!
//! Provides REST API for the interactive strategy simulator,
//! allowing users to create synthetic test days, run simulations,
//! and compare strategy performance.

use askama::Template;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use fluxion_strategy_simulator::{
    ConsumptionProfile, PRICE_PRESETS, PriceScenario, SimulationConfig, SimulationEngine,
    SimulationState, SocOverride, StrategyInfo, SyntheticDayConfig,
    state::SimulationResultsSummary, strategies::StrategySelection,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;

/// State for simulator API handlers
#[derive(Clone)]
pub struct SimulatorState {
    /// Active simulations (in-memory)
    simulations: Arc<RwLock<HashMap<Uuid, SimulationState>>>,
    /// Simulation engine
    engine: Arc<SimulationEngine>,
}

impl std::fmt::Debug for SimulatorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatorState")
            .field("simulations_count", &self.simulations.read().len())
            .finish_non_exhaustive()
    }
}

impl SimulatorState {
    /// Create new simulator state
    #[must_use]
    pub fn new() -> Self {
        Self {
            simulations: Arc::new(RwLock::new(HashMap::new())),
            engine: Arc::new(SimulationEngine::new()),
        }
    }

    /// Clean up old simulations (call periodically)
    pub fn cleanup_old_simulations(&self, max_age_secs: i64) {
        let now = chrono::Utc::now();
        let mut sims = self.simulations.write();
        sims.retain(|_, sim| (now - sim.last_updated).num_seconds() < max_age_secs);
    }
}

impl Default for SimulatorState {
    fn default() -> Self {
        Self::new()
    }
}

// ============= HTML Template =============

/// Simulator page template
#[derive(Debug, Template)]
#[template(path = "simulator.html")]
pub struct SimulatorTemplate {
    pub ingress_path: String,
}

/// Extract ingress path from request headers
fn extract_ingress_path(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("X-Ingress-Path")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}

/// GET /simulator
/// Simulator page handler
pub async fn simulator_page_handler(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let ingress_path = extract_ingress_path(&headers);

    let template = SimulatorTemplate { ingress_path };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            Html(format!(
                "<html><body><h1>Error</h1><p>Failed to render template: {e}</p></body></html>"
            ))
            .into_response()
        }
    }
}

// ============= Request/Response Types =============

/// Response for presets endpoint
#[derive(Debug, Serialize)]
pub struct PresetsResponse {
    pub consumption_profiles: Vec<PresetInfo>,
    pub price_scenarios: Vec<PresetInfo>,
    pub strategies: Vec<StrategyInfo>,
}

/// Preset information
#[derive(Debug, Serialize)]
pub struct PresetInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Create simulation request
#[derive(Debug, Deserialize)]
pub struct CreateSimulationRequest {
    /// Date for simulation (optional, defaults to today)
    pub date: Option<chrono::NaiveDate>,
    /// Consumption profile ID (optional, defaults to "peak_based")
    /// Valid values: "peak_based", "constant", "residential"
    pub consumption_profile: Option<String>,
    /// Price scenario ID (optional, defaults to "usual_day")
    /// Valid values: "usual_day", "elevated_day", "volatile", "negative", "hdo_optimized"
    pub price_scenario: Option<String>,
    /// Initial SOC (optional, defaults to 50%)
    pub initial_soc: Option<f32>,
    /// Battery capacity (optional, defaults to 10 kWh)
    pub battery_capacity_kwh: Option<f32>,
    /// Strategies to enable
    pub strategies: Option<Vec<String>>,
    /// Include baselines (deprecated - use explicit strategy selection instead)
    #[expect(dead_code)]
    pub include_baselines: Option<bool>,
}

/// Convert consumption profile string ID to enum
fn parse_consumption_profile(id: &str) -> ConsumptionProfile {
    match id {
        "constant" => ConsumptionProfile::Constant { load_kw: 2.0 },
        "residential" => ConsumptionProfile::Residential {
            morning_load_kw: 3.0,
            midday_load_kw: 1.5,
            evening_load_kw: 4.0,
            night_load_kw: 0.8,
        },
        _ => ConsumptionProfile::default(), // Default peak-based profile
    }
}

/// Convert price scenario string ID to enum
fn parse_price_scenario(id: &str) -> PriceScenario {
    match id {
        "elevated_day" => PriceScenario::ElevatedDay,
        "volatile" => PriceScenario::Volatile,
        "negative" | "negative_prices" => PriceScenario::NegativePrices,
        "hdo_optimized" => PriceScenario::HdoOptimized,
        _ => PriceScenario::UsualDay, // Default (usual_day and others)
    }
}

/// Map frontend strategy IDs to backend strategy IDs
fn map_strategy_id(frontend_id: &str) -> String {
    match frontend_id {
        "v4_global" => "winter_adaptive_v4".to_owned(),
        "v3_hdo" => "winter_adaptive_v3".to_owned(),
        "v2_advanced" => "winter_adaptive_v2".to_owned(),
        "v1_winter" => "winter_adaptive".to_owned(),
        "naive" => "naive".to_owned(),
        "no_battery" => "no_battery".to_owned(),
        other => other.to_owned(), // Pass through unknown IDs
    }
}
/// Step simulation request
#[derive(Debug, Deserialize)]
pub struct StepRequest {
    /// Number of blocks to step (defaults to 1)
    pub blocks: Option<usize>,
}

/// SOC override request
#[derive(Debug, Deserialize)]
pub struct SocOverrideRequest {
    /// Block index (optional, defaults to current)
    pub block_index: Option<usize>,
    /// New SOC value (0-100)
    pub soc_percent: f32,
    /// Apply to specific strategies (optional, defaults to all)
    pub strategy_ids: Option<Vec<String>>,
}

/// Load override request
#[derive(Debug, Deserialize)]
pub struct LoadOverrideRequest {
    /// Block index (optional, defaults to current block)
    pub block_index: Option<usize>,
    /// Load value in kWh
    pub load_kwh: f32,
}

/// Price override request
#[derive(Debug, Deserialize)]
pub struct PriceOverrideRequest {
    /// Block index (optional, defaults to current block)
    pub block_index: Option<usize>,
    /// Price value in CZK/kWh
    pub price_czk: f32,
}

/// Simulation snapshot for API responses
#[derive(Debug, Serialize)]
pub struct SimulationSnapshot {
    pub id: Uuid,
    pub current_block: usize,
    pub current_time: String,
    pub is_complete: bool,
    pub day: DaySnapshot,
    pub strategy_results: HashMap<String, StrategySnapshot>,
    pub overrides_active: bool,
    pub config: ConfigSnapshot,
}

/// Day snapshot with blocks
#[derive(Debug, Serialize)]
pub struct DaySnapshot {
    pub date: String,
    pub price_scenario: String,
    pub total_consumption_kwh: f32,
    pub total_solar_kwh: f32,
    pub initial_soc: f32,
    pub blocks: Vec<BlockSnapshot>,
}

/// Block snapshot for a single 15-minute period
#[derive(Debug, Serialize)]
pub struct BlockSnapshot {
    pub price_czk: f32,
    pub consumption_kwh: f32,
    pub solar_kwh: f32,
    pub is_hdo_low_tariff: bool,
}

/// Config snapshot
#[derive(Debug, Serialize)]
pub struct ConfigSnapshot {
    pub battery_capacity_kwh: f32,
}

/// Strategy snapshot
#[derive(Debug, Serialize)]
pub struct StrategySnapshot {
    pub name: String,
    pub current_soc: f32,
    pub current_mode: String,
    pub net_cost_czk: f32,
    pub total_grid_import_kwh: f32,
    pub total_grid_export_kwh: f32,
    pub last_reason: String,
    pub soc_history: Vec<f32>,
    pub cumulative_cost_czk: Vec<f32>,
}

impl SimulationSnapshot {
    fn from_state(state: &SimulationState) -> Self {
        let strategy_results: HashMap<String, StrategySnapshot> = state
            .strategy_results
            .iter()
            .map(|(id, result)| {
                (
                    id.clone(),
                    StrategySnapshot {
                        name: result.strategy_name.clone(),
                        current_soc: result.current_soc,
                        current_mode: format!("{:?}", result.current_mode),
                        net_cost_czk: result.net_cost_czk,
                        total_grid_import_kwh: result.total_grid_import_kwh,
                        total_grid_export_kwh: result.total_grid_export_kwh,
                        last_reason: result.last_reason.clone(),
                        soc_history: result.soc_history.clone(),
                        cumulative_cost_czk: result.cumulative_cost_czk.clone(),
                    },
                )
            })
            .collect();

        let blocks: Vec<BlockSnapshot> = state
            .day
            .blocks
            .iter()
            .map(|block| BlockSnapshot {
                price_czk: block.price_czk_per_kwh,
                consumption_kwh: block.consumption_kwh,
                solar_kwh: block.solar_kwh,
                is_hdo_low_tariff: block.is_hdo_low_tariff,
            })
            .collect();

        Self {
            id: state.id,
            current_block: state.current_block,
            current_time: state.current_time_str(),
            is_complete: state.is_complete(),
            day: DaySnapshot {
                date: state.day.date.to_string(),
                price_scenario: state.day.price_scenario_name.clone(),
                total_consumption_kwh: state.day.total_consumption_kwh,
                total_solar_kwh: state.day.total_solar_kwh,
                initial_soc: state.day.initial_soc,
                blocks,
            },
            strategy_results,
            overrides_active: state.overrides.has_overrides(),
            config: ConfigSnapshot {
                battery_capacity_kwh: state.config.battery_capacity_kwh,
            },
        }
    }
}

// ============= API Handlers =============

/// GET /api/simulator/presets
/// Returns available presets for consumption, prices, and strategies
pub async fn presets_handler(State(state): State<SimulatorState>) -> impl IntoResponse {
    let consumption_profiles = vec![
        PresetInfo {
            id: "peak_based".to_owned(),
            name: "Peak-Based (Default)".to_owned(),
            description: "1 kWh base, 4 kWh peaks at 7-8, 10-11, 14-15, 17-18".to_owned(),
        },
        PresetInfo {
            id: "constant".to_owned(),
            name: "Constant Load".to_owned(),
            description: "Uniform consumption throughout the day".to_owned(),
        },
        PresetInfo {
            id: "residential".to_owned(),
            name: "Residential Pattern".to_owned(),
            description: "Morning/evening peaks, low midday and night".to_owned(),
        },
    ];

    let price_scenarios: Vec<PresetInfo> = PRICE_PRESETS
        .iter()
        .map(|p| PresetInfo {
            id: p.id.to_owned(),
            name: p.name.to_owned(),
            description: p.description.to_owned(),
        })
        .collect();

    let strategies: Vec<StrategyInfo> = state.engine.registry().list_strategies().to_vec();

    Json(PresetsResponse {
        consumption_profiles,
        price_scenarios,
        strategies,
    })
}

/// POST /api/simulator/create
/// Create a new simulation session
pub async fn create_simulation_handler(
    State(state): State<SimulatorState>,
    Json(request): Json<CreateSimulationRequest>,
) -> impl IntoResponse {
    info!("Creating new simulation");

    // Parse consumption profile and price scenario from string IDs
    let consumption_profile = request
        .consumption_profile
        .as_deref()
        .map(parse_consumption_profile)
        .unwrap_or_default();

    let price_scenario = request
        .price_scenario
        .as_deref()
        .map_or(PriceScenario::UsualDay, parse_price_scenario);

    // Build day config
    let day_config = SyntheticDayConfig {
        date: request
            .date
            .unwrap_or_else(|| chrono::Utc::now().date_naive()),
        consumption: consumption_profile,
        solar: fluxion_strategy_simulator::SolarProfile::None,
        price_scenario,
        initial_soc: request.initial_soc.unwrap_or(50.0),
        battery_capacity_kwh: request.battery_capacity_kwh.unwrap_or(10.0),
        hdo_periods: None,
        hdo_low_tariff_czk: 0.50,
        hdo_high_tariff_czk: 1.80,
    };

    // Build sim config - map frontend strategy IDs to backend IDs
    let selected_strategies = request
        .strategies
        .unwrap_or_else(|| vec!["v4_global".to_owned()]);

    // Check if baselines are explicitly selected
    let include_naive = selected_strategies.iter().any(|s| s == "naive");
    let include_no_battery = selected_strategies.iter().any(|s| s == "no_battery");

    // Filter out baseline IDs and map the rest to backend IDs
    let strategies: Vec<StrategySelection> = selected_strategies
        .into_iter()
        .filter(|id| id != "naive" && id != "no_battery")
        .map(|id| StrategySelection {
            strategy_id: map_strategy_id(&id),
            enabled: true,
            config_overrides: None,
        })
        .collect();

    let sim_config = SimulationConfig {
        strategies,
        include_no_battery,
        include_naive,
        battery_capacity_kwh: day_config.battery_capacity_kwh,
        ..SimulationConfig::default()
    };

    // Create simulation
    match state.engine.create_simulation(day_config, sim_config) {
        Ok(simulation) => {
            let id = simulation.id;
            let snapshot = SimulationSnapshot::from_state(&simulation);
            state.simulations.write().insert(id, simulation);

            info!("Created simulation {}", id);
            Json(snapshot).into_response()
        }
        Err(e) => {
            error!("Failed to create simulation: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": e.to_string()
                })),
            )
                .into_response()
        }
    }
}

/// GET /api/simulator/{id}
/// Get current simulation state
pub async fn get_simulation_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let sims = state.simulations.read();

    if let Some(simulation) = sims.get(&id) {
        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// POST /api/simulator/{id}/step
/// Step simulation forward by N blocks
pub async fn step_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
    Json(request): Json<StepRequest>,
) -> impl IntoResponse {
    let blocks = request.blocks.unwrap_or(1);

    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        if let Err(e) = state.engine.step(simulation, blocks) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// POST /api/simulator/{id}/run
/// Run simulation to completion
pub async fn run_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        if let Err(e) = state.engine.run_to_completion(simulation) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// GET /api/simulator/{id}/results
/// Get final results summary
pub async fn results_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let sims = state.simulations.read();

    if let Some(simulation) = sims.get(&id) {
        let summary = SimulationResultsSummary::from_state(simulation);
        Json(summary).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// PUT /api/simulator/{id}/override/soc
/// Override SOC at current or specified block
pub async fn override_soc_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
    Json(request): Json<SocOverrideRequest>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        let override_spec = SocOverride {
            block_index: request.block_index.unwrap_or(simulation.current_block),
            soc_percent: request.soc_percent,
            strategy_ids: request.strategy_ids,
        };

        if let Err(e) = state.engine.apply_soc_override(simulation, override_spec) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        // Return updated simulation state
        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// PUT /api/simulator/{id}/override/load
/// Override consumption at current or specified block
pub async fn override_load_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
    Json(request): Json<LoadOverrideRequest>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        let block_index = request.block_index.unwrap_or(simulation.current_block);
        let overrides = vec![(block_index, request.load_kwh)];

        if let Err(e) = state.engine.apply_load_override(simulation, overrides) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        // Return updated simulation state
        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// PUT /api/simulator/{id}/override/price
/// Override price at current or specified block
pub async fn override_price_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
    Json(request): Json<PriceOverrideRequest>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        let block_index = request.block_index.unwrap_or(simulation.current_block);
        let overrides = vec![(block_index, request.price_czk)];

        if let Err(e) = state.engine.apply_price_override(simulation, overrides) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        // Return updated simulation state
        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// POST /api/simulator/{id}/reset
/// Reset simulation to block 0
pub async fn reset_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if let Some(simulation) = sims.get_mut(&id) {
        if let Err(e) = state.engine.clear_overrides(simulation) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }

        Json(SimulationSnapshot::from_state(simulation)).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// DELETE /api/simulator/{id}
/// Delete simulation session
pub async fn delete_handler(
    State(state): State<SimulatorState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let mut sims = state.simulations.write();

    if sims.remove(&id).is_some() {
        Json(serde_json::json!({"success": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}

/// GET /api/simulator/blocks/{id}/{block}
/// Get detailed info for a specific block
#[expect(clippy::integer_division)]
pub async fn block_detail_handler(
    State(state): State<SimulatorState>,
    Path((id, block_idx)): Path<(Uuid, usize)>,
) -> impl IntoResponse {
    let sims = state.simulations.read();

    if let Some(simulation) = sims.get(&id) {
        if block_idx >= simulation.day.blocks.len() {
            return (StatusCode::BAD_REQUEST, "Block index out of range").into_response();
        }

        let block = &simulation.day.blocks[block_idx];

        // Get evaluations for this block from each strategy
        let mut strategy_decisions: HashMap<String, serde_json::Value> = HashMap::new();

        for (strategy_id, result) in &simulation.strategy_results {
            if let Some(eval) = result.evaluations.get(block_idx) {
                strategy_decisions.insert(
                    strategy_id.clone(),
                    serde_json::json!({
                        "mode": format!("{:?}", eval.mode),
                        "reason": eval.reason,
                        "cost_czk": eval.cost_czk,
                        "revenue_czk": eval.revenue_czk,
                        "soc_after": result.soc_history.get(block_idx + 1),
                    }),
                );
            }
        }

        Json(serde_json::json!({
            "index": block_idx,
            "timestamp": block.timestamp.to_rfc3339(),
            "time": format!("{:02}:{:02}", block_idx / 4, (block_idx % 4) * 15),
            "consumption_kwh": block.consumption_kwh,
            "solar_kwh": block.solar_kwh,
            "spot_price_czk": block.price_czk_per_kwh,
            "grid_fee_czk": block.grid_fee_czk_per_kwh,
            "effective_price_czk": block.effective_price_czk_per_kwh,
            "is_hdo_low_tariff": block.is_hdo_low_tariff,
            "strategy_decisions": strategy_decisions,
        }))
        .into_response()
    } else {
        (StatusCode::NOT_FOUND, "Simulation not found").into_response()
    }
}
