// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Backtest API handlers for the web interface.
//!
//! This module provides REST API endpoints for the strategy backtesting feature.

use std::path::PathBuf;
use std::sync::Arc;

use askama::Template;
use axum::{
    Json,
    extract::{Path, State},
    response::{Html, IntoResponse},
};
use chrono::NaiveDate;
use fluxion_backtest::{
    BacktestMetadata, DataSource, DayAnalysis, SqliteDataSource, StrategyChoice,
    StrategyConfigOverrides, calculate_comparison, simulate_day,
};
use fluxion_i18n::I18n;
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

/// State for backtest handlers
#[derive(Clone, Debug)]
pub struct BacktestState {
    pub data_source: Arc<SqliteDataSource>,
    pub i18n: Arc<I18n>,
}

impl BacktestState {
    /// Create a new backtest state with the given database path
    #[must_use]
    pub fn new(db_path: PathBuf, i18n: Arc<I18n>) -> Self {
        Self {
            data_source: Arc::new(SqliteDataSource::new(db_path)),
            i18n,
        }
    }
}

/// Backtest page template
#[derive(Template)]
#[template(path = "backtest.html")]
pub struct BacktestTemplate {
    #[expect(dead_code)]
    pub i18n: Arc<I18n>,
    pub ingress_path: String,
    pub available_days: Vec<String>,
    pub strategies: Vec<StrategyInfoJson>,
}

/// Strategy info for JSON serialization
#[derive(Clone, Serialize, Deserialize)]
pub struct StrategyInfoJson {
    pub id: String,
    pub name: String,
    pub description: String,
    pub has_parameters: bool,
}

impl From<fluxion_backtest::StrategyInfo> for StrategyInfoJson {
    fn from(info: fluxion_backtest::StrategyInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            description: info.description,
            has_parameters: info.has_parameters,
        }
    }
}

/// Handler for the backtest page
pub async fn backtest_page_handler(
    State(state): State<BacktestState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    debug!("Backtest page requested");
    let ingress_path = extract_ingress_path(&headers);

    // Get available days
    let available_days = match state.data_source.get_available_days() {
        Ok(days) => days.iter().map(ToString::to_string).collect(),
        Err(e) => {
            error!("Failed to get available days: {}", e);
            vec![]
        }
    };

    let strategies: Vec<StrategyInfoJson> = BacktestMetadata::strategies()
        .into_iter()
        .map(Into::into)
        .collect();

    let template = BacktestTemplate {
        i18n: state.i18n,
        ingress_path,
        available_days,
        strategies,
    };

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

/// Response for the available days endpoint
#[derive(Serialize)]
pub struct AvailableDaysResponse {
    pub days: Vec<String>,
    pub strategies: Vec<StrategyInfoJson>,
}

/// Handler to get available days and strategies
pub async fn available_days_handler(State(state): State<BacktestState>) -> impl IntoResponse {
    debug!("Available days requested");

    match state.data_source.get_available_days() {
        Ok(days) => {
            let response = AvailableDaysResponse {
                days: days.iter().map(ToString::to_string).collect(),
                strategies: BacktestMetadata::strategies()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            };
            Json(response).into_response()
        }
        Err(e) => {
            error!("Failed to get available days: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get available days: {e}"),
            )
                .into_response()
        }
    }
}

/// Handler to get actual data for a specific day
pub async fn day_data_handler(
    State(state): State<BacktestState>,
    Path(date_str): Path<String>,
) -> impl IntoResponse {
    debug!("Day data requested for: {}", date_str);

    let date = match NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid date format: {e}"),
            )
                .into_response();
        }
    };

    match fluxion_backtest::analyze_actual_day(state.data_source.as_ref(), date) {
        Ok(analysis) => Json(analysis).into_response(),
        Err(e) => {
            error!("Failed to analyze day {}: {}", date, e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to analyze day: {e}"),
            )
                .into_response()
        }
    }
}

/// Request body for simulation endpoint
#[derive(Deserialize)]
pub struct SimulateRequest {
    pub date: String,
    pub strategy: String,
    #[serde(default)]
    pub config_overrides: Option<StrategyConfigOverrides>,
}

/// Handler to run a strategy simulation
pub async fn simulate_handler(
    State(state): State<BacktestState>,
    Json(request): Json<SimulateRequest>,
) -> impl IntoResponse {
    debug!(
        "Simulation requested for {} with strategy {}",
        request.date, request.strategy
    );

    let date = match NaiveDate::parse_from_str(&request.date, "%Y-%m-%d") {
        Ok(d) => d,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid date format: {e}"),
            )
                .into_response();
        }
    };

    let strategy = match request.strategy.as_str() {
        "actual" => StrategyChoice::Actual,
        "self_use" => StrategyChoice::SelfUse,
        "winter_adaptive" => StrategyChoice::WinterAdaptive,
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Unknown strategy: {}", request.strategy),
            )
                .into_response();
        }
    };

    match simulate_day(
        state.data_source.as_ref(),
        date,
        &strategy,
        request.config_overrides.as_ref(),
    ) {
        Ok(analysis) => Json(analysis).into_response(),
        Err(e) => {
            error!("Failed to simulate day {}: {}", date, e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to simulate: {e}"),
            )
                .into_response()
        }
    }
}

/// Request body for comparison endpoint
#[derive(Deserialize)]
pub struct CompareRequest {
    pub date: String,
    pub left_strategy: String,
    pub right_strategy: String,
    #[serde(default)]
    pub left_overrides: Option<StrategyConfigOverrides>,
    #[serde(default)]
    pub right_overrides: Option<StrategyConfigOverrides>,
}

/// Response for comparison endpoint
#[derive(Serialize)]
pub struct CompareResponse {
    pub left: DayAnalysis,
    pub right: DayAnalysis,
    pub comparison: ComparisonInfo,
}

#[derive(Serialize)]
pub struct ComparisonInfo {
    pub cost_diff_czk: f64,
    pub savings_percent: f64,
    pub battery_value_diff_czk: f64,
    pub summary: String,
}

/// Handler to compare two strategies
pub async fn compare_handler(
    State(state): State<BacktestState>,
    Json(request): Json<CompareRequest>,
) -> impl IntoResponse {
    debug!(
        "Comparison requested: {} vs {} for {}",
        request.left_strategy, request.right_strategy, request.date
    );

    let date = match NaiveDate::parse_from_str(&request.date, "%Y-%m-%d") {
        Ok(d) => d,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid date format: {e}"),
            )
                .into_response();
        }
    };

    let parse_strategy = |s: &str| -> Result<StrategyChoice, String> {
        match s {
            "actual" => Ok(StrategyChoice::Actual),
            "self_use" => Ok(StrategyChoice::SelfUse),
            "winter_adaptive" => Ok(StrategyChoice::WinterAdaptive),
            _ => Err(format!("Unknown strategy: {s}")),
        }
    };

    let left_strategy = match parse_strategy(&request.left_strategy) {
        Ok(s) => s,
        Err(e) => return (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
    };

    let right_strategy = match parse_strategy(&request.right_strategy) {
        Ok(s) => s,
        Err(e) => return (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
    };

    // Run both simulations
    let left = match simulate_day(
        state.data_source.as_ref(),
        date,
        &left_strategy,
        request.left_overrides.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Left simulation failed: {e}"),
            )
                .into_response();
        }
    };

    let right = match simulate_day(
        state.data_source.as_ref(),
        date,
        &right_strategy,
        request.right_overrides.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Right simulation failed: {e}"),
            )
                .into_response();
        }
    };

    // Calculate comparison
    let diff = calculate_comparison(&left, &right);

    let summary = if diff.savings_percent > 0.0 {
        format!(
            "{} saves {:.0} CZK ({:.1}%) vs {}",
            right.strategy, -diff.cost_diff_czk, diff.savings_percent, left.strategy
        )
    } else if diff.savings_percent < 0.0 {
        format!(
            "{} costs {:.0} CZK ({:.1}%) more than {}",
            right.strategy,
            diff.cost_diff_czk,
            diff.savings_percent.abs(),
            left.strategy
        )
    } else {
        format!("{} and {} have equal costs", left.strategy, right.strategy)
    };

    let response = CompareResponse {
        left,
        right,
        comparison: ComparisonInfo {
            cost_diff_czk: diff.cost_diff_czk,
            savings_percent: diff.savings_percent,
            battery_value_diff_czk: diff.battery_value_diff_czk,
            summary,
        },
    };

    Json(response).into_response()
}

/// Extract ingress path from request headers
fn extract_ingress_path(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("X-Ingress-Path")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}
