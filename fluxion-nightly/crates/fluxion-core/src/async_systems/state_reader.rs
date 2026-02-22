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
use tracing::{debug, info, trace, warn};

use crate::{InverterDataSourceResource, components::*};

/// Interval for collecting battery and PV history (in seconds)
/// Collects data every 15 minutes to build historical charts
const HISTORY_COLLECTION_INTERVAL_SECS: u64 = 15 * 60;

/// Initialize inverter state reader resource and timer
/// Replaces the complex channel spawning with simple resources
pub fn setup_inverter_state_reader(
    commands: &mut Commands,
    inverter_source: &InverterDataSourceResource,
) {
    let state_reader = crate::resources::InverterStateReader::new(inverter_source.0.clone());
    let state_timer = crate::resources::StateReadTimer::new(5); // 5 seconds interval

    commands.insert_resource(state_reader);
    commands.insert_resource(state_timer);
    info!("✅ Inverter state reader initialized with 5-second read interval");
}

/// Simplified system that reads inverter states using direct resource access
/// Replaces the complex channel polling with timer-based direct reads
pub fn read_inverter_states_system(
    state_reader: Res<crate::resources::InverterStateReader>,
    state_timer: ResMut<crate::resources::StateReadTimer>,
    mut commands: Commands,
    mut inverters: Query<(Entity, &Inverter, Option<&mut RawInverterState>)>,
) {
    use chrono::Utc;

    // Check if it's time to read (non-blocking timer check)
    if !state_timer.should_read() {
        return;
    }
    state_timer.mark_read();

    debug!("📡 Reading inverter states...");

    // Read state for each inverter directly
    for (entity, inverter, existing_state) in inverters.iter_mut() {
        match state_reader.read_state(&inverter.id) {
            Ok(state) => {
                debug!(
                    "✅ Retrieved state for {}: SOC={:.1}%",
                    inverter.id, state.battery_soc
                );

                let raw_state = RawInverterState {
                    state,
                    last_updated: Utc::now(),
                };

                // Update or insert the RawInverterState component
                if let Some(mut existing) = existing_state {
                    *existing = raw_state;
                } else {
                    commands.entity(entity).insert(raw_state);
                }
            }
            Err(e) => {
                warn!("❌ Failed to read state for {}: {}", inverter.id, e);
            }
        }
    }
}

type InverterComponentsQuery<'a> = (
    Entity,
    &'a RawInverterState,
    // Core components
    Option<&'a mut BatteryStatus>,
    Option<&'a mut GridPower>,
    Option<&'a mut PowerGeneration>,
    Option<&'a mut InverterStatus>,
    // Extended components
    Option<&'a mut ExtendedPv>,
    Option<&'a mut EpsStatus>,
    Option<&'a mut BatteryExtended>,
    Option<&'a mut GridTotals>,
    Option<&'a mut ThreePhase>,
    Option<&'a mut Temperatures>,
);

/// System that decomposes RawInverterState into individual ECS components
/// This ensures BatteryStatus, GridPower, and PowerGeneration components are always up-to-date
/// Also populates extended components if data is available
/// Also collects battery SOC history for visualization
pub fn decompose_inverter_state(
    mut inverters: Query<InverterComponentsQuery>,
    mut commands: Commands,
    mut battery_history: ResMut<BatteryHistory>,
    mut pv_history: ResMut<PvHistory>,
    mut last_history_update: Local<Option<std::time::Instant>>,
) {
    for (
        entity,
        raw_state,
        battery,
        grid,
        pv,
        status,
        ext_pv,
        eps,
        bat_ext,
        grid_tot,
        three_phase,
        temps,
    ) in inverters.iter_mut()
    {
        let state = &raw_state.state;

        // Update or insert BatteryStatus
        let battery_status = BatteryStatus {
            soc_percent: state.battery_soc as u16,
            voltage_v: state.battery_voltage_v.unwrap_or(0.0),
            current_a: state.battery_current_a.unwrap_or(0.0),
            power_w: state.battery_power_w as i32,
            temperature_c: state.battery_temperature_c.unwrap_or(0.0),
            cycles: 0, // Not available in GenericInverterState
        };

        if let Some(mut existing) = battery {
            *existing = battery_status;
        } else {
            commands.entity(entity).insert(battery_status);
        }

        // Collect battery SOC history (every 15 minutes)
        let now = std::time::Instant::now();
        let should_collect = last_history_update
            .map(|last| now.duration_since(last).as_secs() >= HISTORY_COLLECTION_INTERVAL_SECS)
            .unwrap_or(true);

        if should_collect {
            let history_point = BatteryHistoryPoint {
                timestamp: chrono::Utc::now(),
                soc: state.battery_soc,
                power_w: state.battery_power_w,
                voltage_v: Some(state.battery_voltage_v.unwrap_or(0.0)),
            };

            battery_history.add_point(history_point);

            // Also collect PV generation history at the same interval
            let pv_history_point = PvHistoryPoint {
                timestamp: chrono::Utc::now(),
                power_w: state.pv_power_w,
                pv1_power_w: state.pv1_power_w,
                pv2_power_w: state.pv2_power_w,
            };

            pv_history.add_point(pv_history_point);
            *last_history_update = Some(now);

            info!(
                "📊 Collected history: Battery {:.1}% ({:.0}W), PV {:.0}W (battery: {} pts, PV: {} pts)",
                state.battery_soc,
                state.battery_power_w,
                state.pv_power_w,
                battery_history.len(),
                pv_history.len()
            );
        } else {
            let elapsed_secs = last_history_update
                .map(|last| now.duration_since(last).as_secs())
                .unwrap_or(0);
            trace!(
                "⏱️ Battery history: {} points, next collection in {} seconds",
                battery_history.len(),
                HISTORY_COLLECTION_INTERVAL_SECS.saturating_sub(elapsed_secs)
            );
        }

        // Update or insert GridPower
        let grid_power = GridPower {
            export_power_w: state.grid_power_w as i32,
            grid_frequency_hz: state.inverter_frequency_hz.unwrap_or(0.0),
            grid_voltage_v: state.inverter_voltage_v.unwrap_or(0.0),
        };

        if let Some(mut existing) = grid {
            *existing = grid_power;
        } else {
            commands.entity(entity).insert(grid_power);
        }

        // Update or insert PowerGeneration
        let power_gen = PowerGeneration {
            current_power_w: state.pv_power_w as u16,
            daily_energy_kwh: state.today_solar_energy_kwh.unwrap_or(0.0),
            total_energy_kwh: state.total_solar_energy_kwh.unwrap_or(0.0),
            pv1_power_w: state.pv1_power_w.unwrap_or(0.0) as u16,
            pv2_power_w: state.pv2_power_w.unwrap_or(0.0) as u16,
        };

        if let Some(mut existing) = pv {
            *existing = power_gen;
        } else {
            commands.entity(entity).insert(power_gen);
        }

        // Update or insert InverterStatus
        let inv_status = InverterStatus {
            // Future: Map work_mode from GenericInverterState to RunMode enum
            // Currently GenericInverterState doesn't expose work_mode,
            // but vendor-specific state does. Consider adding to generic state.
            run_mode: RunMode::Normal,
            error_code: state.fault_code.unwrap_or(0),
            temperature_c: state.inverter_temperature_c.unwrap_or(0.0),
            last_update: Some(raw_state.last_updated),
            connection_healthy: state.online,
        };

        if let Some(mut existing) = status {
            *existing = inv_status;
        } else {
            commands.entity(entity).insert(inv_status);
        }

        // ============= Extended Components (Optional) =============

        // ExtendedPv - PV3/PV4 strings (if available)
        if state.pv3_power_w.is_some() || state.pv4_power_w.is_some() {
            let extended_pv = ExtendedPv {
                pv3_voltage_v: 0.0, // Not in GenericInverterState
                pv3_current_a: 0.0,
                pv3_power_w: state.pv3_power_w.unwrap_or(0.0) as u16,
                pv4_voltage_v: 0.0,
                pv4_current_a: 0.0,
                pv4_power_w: state.pv4_power_w.unwrap_or(0.0) as u16,
            };

            if let Some(mut existing) = ext_pv {
                *existing = extended_pv;
            } else {
                commands.entity(entity).insert(extended_pv);
            }
        }

        // EpsStatus - Emergency Power Supply (if available)
        if state.eps_voltage_v.is_some()
            || state.eps_current_a.is_some()
            || state.eps_power_w.is_some()
        {
            let eps_status = EpsStatus {
                voltage_v: state.eps_voltage_v.unwrap_or(0.0),
                current_a: state.eps_current_a.unwrap_or(0.0),
                power_w: state.eps_power_w.unwrap_or(0.0) as i16,
                frequency_hz: 0.0, // Not in GenericInverterState
            };

            if let Some(mut existing) = eps {
                *existing = eps_status;
            } else {
                commands.entity(entity).insert(eps_status);
            }
        }

        // BatteryExtended - BMS detailed data
        let battery_extended = BatteryExtended {
            output_energy_total_kwh: state.battery_output_energy_total_kwh.unwrap_or(0.0),
            output_energy_today_kwh: state.battery_output_energy_today_kwh.unwrap_or(0.0),
            input_energy_total_kwh: state.battery_input_energy_total_kwh.unwrap_or(0.0),
            input_energy_today_kwh: state.battery_input_energy_today_kwh.unwrap_or(0.0),
            pack_number: 0, // Not in GenericInverterState
            state_of_health_percent: state.battery_soh_percent.unwrap_or(100.0) as u16,
            bms_charge_max_current_a: state.bms_charge_max_current_a.unwrap_or(0.0),
            bms_discharge_max_current_a: state.bms_discharge_max_current_a.unwrap_or(0.0),
            bms_capacity_ah: 0, // Not in GenericInverterState
            board_temperature_c: state.board_temperature_c.unwrap_or(0.0),
            boost_temperature_c: state.boost_temperature_c.unwrap_or(0.0),
        };

        if let Some(mut existing) = bat_ext {
            *existing = battery_extended;
        } else {
            commands.entity(entity).insert(battery_extended);
        }

        // GridTotals - Lifetime energy totals
        let grid_totals = GridTotals {
            export_total_kwh: state.grid_export_total_kwh.unwrap_or(0.0),
            import_total_kwh: state.grid_import_total_kwh.unwrap_or(0.0),
            today_yield_kwh: state.today_yield_kwh.unwrap_or(0.0),
            total_yield_kwh: state.total_yield_kwh.unwrap_or(0.0),
        };

        if let Some(mut existing) = grid_tot {
            *existing = grid_totals;
        } else {
            commands.entity(entity).insert(grid_totals);
        }

        // ThreePhase - Per-phase data (if available)
        if state.l1_voltage_v.is_some()
            || state.l2_voltage_v.is_some()
            || state.l3_voltage_v.is_some()
        {
            let three_phase_data = ThreePhase {
                l1_voltage_v: state.l1_voltage_v.unwrap_or(0.0),
                l1_current_a: state.l1_current_a.unwrap_or(0.0),
                l1_power_w: state.l1_power_w.unwrap_or(0.0) as i16,
                l1_frequency_hz: 0.0, // Not in GenericInverterState
                l2_voltage_v: state.l2_voltage_v.unwrap_or(0.0),
                l2_current_a: state.l2_current_a.unwrap_or(0.0),
                l2_power_w: state.l2_power_w.unwrap_or(0.0) as i16,
                l2_frequency_hz: 0.0,
                l3_voltage_v: state.l3_voltage_v.unwrap_or(0.0),
                l3_current_a: state.l3_current_a.unwrap_or(0.0),
                l3_power_w: state.l3_power_w.unwrap_or(0.0) as i16,
                l3_frequency_hz: 0.0,
            };

            if let Some(mut existing) = three_phase {
                *existing = three_phase_data;
            } else {
                commands.entity(entity).insert(three_phase_data);
            }
        }

        // Temperatures - Consolidated temperature monitoring
        let temperatures = Temperatures {
            inverter_c: state.inverter_temperature_c.unwrap_or(0.0),
            battery_c: state.battery_temperature_c.unwrap_or(0.0),
            board_c: state.board_temperature_c.unwrap_or(0.0),
            boost_c: state.boost_temperature_c.unwrap_or(0.0),
        };

        if let Some(mut existing) = temps {
            *existing = temperatures;
        } else {
            commands.entity(entity).insert(temperatures);
        }
    }
}
