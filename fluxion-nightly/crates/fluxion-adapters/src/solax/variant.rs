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

//! SolaX vs SolaX Ultra detection (brief H12).
//!
//! The configured vendor selects the entity mapper, but the two variants name their SOC sensor
//! differently. At startup each configured inverter is probed in HA: a clear answer overrides the
//! configuration, an ambiguous one (both or neither sensor) keeps it.

use fluxion_core::InverterType;
use tracing::{info, warn};

use crate::ha::client::HomeAssistantClient;
use crate::ha::errors::HaResult;

/// SOC sensor that only the SolaX Ultra has
fn ultra_soc_entity(prefix: &str) -> String {
    format!("sensor.{prefix}_battery_total_capacity_charge")
}

/// SOC sensor of the standard SolaX
fn solax_soc_entity(prefix: &str) -> String {
    format!("sensor.{prefix}_battery_capacity")
}

/// Detect the variant from which probe entities exist. Returns `None` when both or neither exist.
pub fn detect_variant(exists: impl Fn(&str) -> bool, prefix: &str) -> Option<InverterType> {
    match (
        exists(&ultra_soc_entity(prefix)),
        exists(&solax_soc_entity(prefix)),
    ) {
        (true, false) => Some(InverterType::SolaxUltra),
        (false, true) => Some(InverterType::Solax),
        _ => None,
    }
}

/// Probe HA for the existing variant entities of one inverter
async fn probe_existing(client: &HomeAssistantClient, prefix: &str) -> HaResult<Vec<String>> {
    let mut found = Vec::new();
    for entity in [ultra_soc_entity(prefix), solax_soc_entity(prefix)] {
        if client.get_state_optional(&entity).await?.is_some() {
            found.push(entity);
        }
    }
    Ok(found)
}

/// Variant to use for one inverter: the detected one when the probe is clear, else the configured
/// one. An unreachable HA (after the client's retries) keeps the configured variant.
pub async fn detect_inverter_type(
    client: &HomeAssistantClient,
    prefix: &str,
    configured: InverterType,
) -> InverterType {
    let found = match probe_existing(client, prefix).await {
        Ok(found) => found,
        Err(e) => {
            warn!(
                "⚠️ SolaX variant probe for {} failed ({}); using configured {}",
                prefix,
                e,
                configured.display_name()
            );
            return configured;
        }
    };
    let detected = detect_variant(|entity| found.iter().any(|f| f == entity), prefix);
    info!(
        "🔎 SolaX variant for {}: detected {} (configured {})",
        prefix,
        detected.map_or("unknown", |v| v.display_name()),
        configured.display_name()
    );
    match detected {
        Some(variant) if variant != configured => {
            warn!(
                "⚠️ SolaX variant for {} differs from the configuration; using detected {}",
                prefix,
                variant.display_name()
            );
            variant
        }
        Some(variant) => variant,
        None => configured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const HYBRID: &str = include_str!("../../tests/fixtures/solax_hybrid_entity_ids.txt");
    const ULTRA: &str = include_str!("../../tests/fixtures/solax_ultra_parallel_entity_ids.txt");

    fn entities(text: &str) -> HashSet<&str> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn detect_in(text: &str, prefix: &str) -> Option<InverterType> {
        let ids = entities(text);
        detect_variant(|entity| ids.contains(entity), prefix)
    }

    /// Entities the mapper reads on every poll, plus the history and mode entities
    fn core_reads(variant: InverterType, prefix: &str) -> Vec<String> {
        let mapper = crate::solax::create_entity_mapper(variant);
        let mut ids = vec![
            mapper.get_battery_soc_entity(prefix),
            mapper.get_grid_power_entity(prefix),
            mapper.get_battery_power_entity(prefix),
            mapper.get_pv_power_entity(prefix),
            mapper.get_export_limit_entity(prefix),
            mapper.get_work_mode_entity(prefix),
        ];
        ids.extend(mapper.get_manual_mode_entity(prefix));
        ids.extend(mapper.get_house_load_entity(prefix));
        ids.extend(mapper.get_grid_import_today_entity(prefix));
        ids.extend(mapper.get_battery_output_energy_today_entity(prefix));
        ids
    }

    #[test]
    fn x3_hybrid_detects_solax() {
        assert_eq!(detect_in(HYBRID, "solax"), Some(InverterType::Solax));
    }

    #[test]
    fn ultra_parallel_masters_and_slave_detect_ultra() {
        assert_eq!(
            detect_in(ULTRA, "solax_80_master"),
            Some(InverterType::SolaxUltra)
        );
        assert_eq!(
            detect_in(ULTRA, "solax_81_slave"),
            Some(InverterType::SolaxUltra)
        );
    }

    #[test]
    fn normal_solax_in_ultra_site_detects_solax() {
        assert_eq!(
            detect_in(ULTRA, "solax_53_other"),
            Some(InverterType::Solax)
        );
    }

    #[test]
    fn both_or_neither_probe_keeps_configuration() {
        assert_eq!(detect_variant(|_| true, "solax"), None);
        assert_eq!(detect_variant(|_| false, "solax"), None);
    }

    #[test]
    fn chosen_mapper_core_reads_exist_in_fixture() {
        let cases = [
            (HYBRID, "solax"),
            (ULTRA, "solax_80_master"),
            (ULTRA, "solax_81_slave"),
            (ULTRA, "solax_53_other"),
        ];
        for (text, prefix) in cases {
            let variant = detect_in(text, prefix).expect("fixture detects a variant");
            let ids = entities(text);
            for entity in core_reads(variant, prefix) {
                assert!(
                    ids.contains(entity.as_str()),
                    "{variant:?} mapper reads {entity}, not in the fixture for {prefix}"
                );
            }
        }
    }

    #[tokio::test]
    async fn probe_uses_detected_variant_and_falls_back_on_failure() {
        let mut server = mockito::Server::new_async().await;
        let _ultra = server
            .mock("GET", "/api/states/sensor.solax_80_master_battery_total_capacity_charge")
            .with_status(200)
            .with_body(r#"{"entity_id":"sensor.solax_80_master_battery_total_capacity_charge","state":"50","attributes":{},"last_changed":"2025-10-02T10:00:00Z","last_updated":"2025-10-02T10:00:00Z"}"#)
            .create_async()
            .await;
        let _no_solax = server
            .mock("GET", "/api/states/sensor.solax_80_master_battery_capacity")
            .with_status(404)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(server.url(), "t").unwrap();
        assert_eq!(
            detect_inverter_type(&client, "solax_80_master", InverterType::Solax).await,
            InverterType::SolaxUltra
        );

        let mut down = mockito::Server::new_async().await;
        let _error = down
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .create_async()
            .await;
        let client = HomeAssistantClient::new(down.url(), "t").unwrap();
        assert_eq!(
            detect_inverter_type(&client, "solax", InverterType::SolaxUltra).await,
            InverterType::SolaxUltra
        );
    }
}
