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

pub mod config;
pub mod health;
pub mod history;
pub mod inverter;
pub mod pricing;
pub mod scheduling;
pub mod user_control;
pub mod web;

// Re-export common types for convenience
pub use config::{ControlConfig, SystemConfig};
pub use health::{HealthStatus, SystemHealthData};
pub use history::{ConsumptionHistory, ConsumptionHistoryConfig};
pub use inverter::{Inverter, InverterOperationMode, InverterType};
pub use pricing::{PriceAnalysis, SpotPriceData};
pub use scheduling::{BlockDebugInfo, OperationSchedule, ScheduledMode, StrategyEvaluation};
pub use user_control::{FixedTimeSlot, UserControlState};
