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
use tracing::{debug, info, warn};

use crate::{InverterDataSourceResource, PriceDataSourceResource, components::*};

/// Initialize health checker resource
/// Replaces the complex channel spawning with a simple resource
pub fn setup_health_checker(
    commands: &mut Commands,
    inverter_source: &InverterDataSourceResource,
    price_source: &PriceDataSourceResource,
) {
    let health_checker = crate::resources::HealthChecker::new(
        inverter_source.0.clone(),
        price_source.0.clone(),
        300, // 5 minutes interval
    );

    commands.insert_resource(health_checker);
    info!("✅ Health checker initialized with 5-minute check interval");
}

/// Simplified system that checks health using direct cache access
/// Replaces the complex channel polling with on-demand checking
pub fn check_health_system(
    health_checker: Res<crate::resources::HealthChecker>,
    mut health_status_query: Query<&mut HealthStatus>,
    mut commands: Commands,
) {
    // Only check if needed (non-blocking check)
    if !health_checker.should_check() {
        return;
    }

    debug!("🏥 Checking health status...");

    // Get current health status for all sources
    let health_results = health_checker.check_all();

    for (source_name, is_healthy) in health_results {
        if is_healthy {
            debug!("✅ {} is healthy", source_name);
        } else {
            warn!("⚠️ {} is unhealthy", source_name);
        }

        // Update or create HealthStatus component
        let mut found = false;
        for mut status in health_status_query.iter_mut() {
            if status.source_name == source_name {
                status.is_healthy = is_healthy;
                status.last_check = chrono::Utc::now();
                if !is_healthy {
                    status
                        .recent_errors
                        .push(format!("Health check failed at {}", chrono::Utc::now()));
                    // Keep only last 10 errors
                    if status.recent_errors.len() > 10 {
                        status.recent_errors.drain(0..1);
                    }
                } else {
                    // Clear errors on successful check
                    status.recent_errors.clear();
                }
                found = true;
                break;
            }
        }

        if !found {
            // Create new HealthStatus entity
            commands.spawn(HealthStatus {
                source_name: source_name.clone(),
                is_healthy,
                last_check: chrono::Utc::now(),
                recent_errors: Vec::new(),
            });
            info!("📊 Created health status entity for {}", source_name);
        }
    }
}
