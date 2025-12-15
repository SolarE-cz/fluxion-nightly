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

use fluxion_i18n::{I18n, Language};

/// List of all translation keys that must exist in all languages
const REQUIRED_KEYS: &[&str] = &[
    // Main - Operating Modes
    "mode-self-use",
    "mode-force-charge",
    "mode-force-discharge",
    "mode-backup",
    "mode-feed-in-priority",
    "mode-off-grid",
    // Main - Topology
    "topology-independent",
    "topology-master",
    "topology-slave",
    // Main - Units
    "unit-percent",
    "unit-watt",
    "unit-kilowatt",
    "unit-kilowatt-hour",
    "unit-voltage",
    "unit-ampere",
    "unit-celsius",
    "unit-hertz",
    // Main - Status
    "status-online",
    "status-offline",
    "status-error",
    "status-warning",
    "status-ok",
    // Main - Common
    "yes",
    "no",
    "unknown",
    "not-available",
    // Web - Dashboard
    "dashboard-title",
    "dashboard-subtitle",
    // Web - Sections
    "section-inverter-status",
    "section-battery",
    "section-solar",
    "section-grid",
    "section-load",
    "section-schedule",
    "section-system",
    // Web - Inverter Status
    "inverter-status",
    "inverter-connection",
    "inverter-mode",
    "inverter-topology",
    "inverter-serial",
    "inverter-firmware",
    // Web - Battery
    "battery-soc",
    "battery-power",
    "battery-voltage",
    "battery-current",
    "battery-temperature",
    "battery-soh",
    "battery-charging",
    "battery-discharging",
    "battery-idle",
    "battery-energy-charged",
    "battery-energy-discharged",
    // Web - Solar (PV)
    "pv-power",
    "pv-power-total",
    "pv-string",
    "pv-voltage",
    "pv-current",
    "pv-energy-today",
    "pv-energy-total",
    // Web - Grid
    "grid-power",
    "grid-voltage",
    "grid-current",
    "grid-frequency",
    "grid-importing",
    "grid-exporting",
    "grid-energy-imported",
    "grid-energy-exported",
    "grid-energy-consumed",
    // Web - Load
    "load-power",
    "load-power-total",
    "load-house",
    // Web - Inverter Details
    "inverter-voltage-total",
    "inverter-current-total",
    "inverter-power-total",
    "inverter-frequency",
    // Web - Battery Extended
    "battery-capacity",
    "battery-input-today",
    "battery-output-today",
    // Web - Grid Extended
    "grid-import-power",
    "grid-export-power",
    "grid-import-today",
    "grid-export-today",
    // Web - Solar Extended
    "solar-energy-today",
    "solar-energy-total",
    // Web - Schedule
    "schedule-current-block",
    "schedule-next-block",
    "schedule-reason",
    "schedule-price",
    "schedule-starts-at",
    "schedule-ends-at",
    "schedule-no-data",
    // Web - System
    "system-debug-mode",
    "system-enabled",
    "system-disabled",
    "system-uptime",
    "system-version",
    "system-last-update",
    // Web - Errors & Messages
    "error-connection-lost",
    "error-no-data",
    "warning-high-temperature",
    "message-charging-from-grid",
    "message-exporting-to-grid",
    // Schedule - Reasons
    "reason-cheapest-block",
    "reason-peak-price",
    "reason-normal-operation",
    "reason-forced-charge",
    "reason-forced-discharge",
    "reason-backup-reserve",
    "reason-grid-limit",
    "reason-battery-protection",
    "reason-temperature-limit",
    "reason-manual-mode",
    // Schedule - States
    "state-charging",
    "state-discharging",
    "state-idle",
    "state-self-use",
    // Schedule - Time-related
    "time-now",
    "time-next",
    "time-in",
    "time-until",
    "time-from-to",
    // Schedule - Block information
    "block-duration",
    "block-energy",
    "block-savings",
];

#[test]
fn test_english_translations_complete() {
    use fluent::fluent_args;

    let i18n = I18n::new(Language::English).expect("Failed to load English translations");

    let mut missing_keys = Vec::new();

    for key in REQUIRED_KEYS {
        // Try without variables first
        let result = if i18n.get(key).is_err() {
            // Try with dummy variables for keys that need them
            i18n.format(
                key,
                Some(&fluent_args![
                    "count" => 1,
                    "master" => "test",
                    "number" => 1,
                    "price" => 0.0,
                    "currency" => "CZK",
                    "minutes" => 1,
                    "time" => "00:00",
                    "start" => "00:00",
                    "end" => "01:00",
                    "hours" => 1,
                    "energy" => 0.0,
                    "amount" => 0.0
                ]),
            )
        } else {
            i18n.get(key)
        };

        if result.is_err() {
            missing_keys.push(*key);
        }
    }

    assert!(
        missing_keys.is_empty(),
        "Missing English translations for keys: {:?}",
        missing_keys
    );
}

#[test]
fn test_czech_translations_complete() {
    use fluent::fluent_args;

    let i18n = I18n::new(Language::Czech).expect("Failed to load Czech translations");

    let mut missing_keys = Vec::new();

    for key in REQUIRED_KEYS {
        // Try without variables first
        let result = if i18n.get(key).is_err() {
            // Try with dummy variables for keys that need them
            i18n.format(
                key,
                Some(&fluent_args![
                    "count" => 1,
                    "master" => "test",
                    "number" => 1,
                    "price" => 0.0,
                    "currency" => "Kč",
                    "minutes" => 1,
                    "time" => "00:00",
                    "start" => "00:00",
                    "end" => "01:00",
                    "hours" => 1,
                    "energy" => 0.0,
                    "amount" => 0.0
                ]),
            )
        } else {
            i18n.get(key)
        };

        if result.is_err() {
            missing_keys.push(*key);
        }
    }

    assert!(
        missing_keys.is_empty(),
        "Missing Czech translations for keys: {:?}",
        missing_keys
    );
}

#[test]
fn test_translations_not_empty() {
    use fluent::fluent_args;

    let languages = vec![Language::English, Language::Czech];

    for lang in languages {
        let i18n = I18n::new(lang).expect("Failed to load translations");

        for key in REQUIRED_KEYS {
            let translation = i18n.get(key).unwrap_or_else(|_| {
                // Try with dummy variables
                i18n.format(
                    key,
                    Some(&fluent_args![
                        "count" => 1,
                        "master" => "test",
                        "number" => 1,
                        "price" => 0.0,
                        "currency" => "CZK",
                        "minutes" => 1,
                        "time" => "00:00",
                        "start" => "00:00",
                        "end" => "01:00",
                        "hours" => 1,
                        "energy" => 0.0,
                        "amount" => 0.0
                    ]),
                )
                .unwrap_or_else(|_| panic!("Missing key: {}", key))
            });
            assert!(
                !translation.is_empty(),
                "Empty translation for key '{}' in language {:?}",
                key,
                lang
            );
        }
    }
}

#[test]
fn test_english_is_default_fallback() {
    // English should always work
    let i18n = I18n::new(Language::English);
    assert!(i18n.is_ok(), "English translations must be available");
}

#[test]
fn test_language_switching() {
    // Test that we can create multiple i18n instances with different languages
    let en = I18n::new(Language::English).expect("Failed to load English");
    let cs = I18n::new(Language::Czech).expect("Failed to load Czech");

    // Verify they produce different translations
    let key = "mode-self-use";
    let en_text = en.get(key).expect("Missing English translation");
    let cs_text = cs.get(key).expect("Missing Czech translation");

    assert_ne!(
        en_text, cs_text,
        "English and Czech translations should differ"
    );
    assert_eq!(en_text, "Self Use");
    assert_eq!(cs_text, "Vlastní spotřeba");
}

#[test]
fn test_variable_interpolation() {
    use fluent::fluent_args;

    let i18n = I18n::new(Language::English).expect("Failed to load English");

    // Test price formatting with variables
    let result = i18n.format(
        "reason-cheapest-block",
        Some(&fluent_args![
            "price" => 0.123,
            "currency" => "Kč"
        ]),
    );

    assert!(result.is_ok(), "Variable interpolation should work");
    let text = result.unwrap();
    assert!(text.contains("0.123"), "Should contain price value");
    assert!(text.contains("Kč"), "Should contain currency");
}

#[test]
fn test_pluralization_czech() {
    use fluent::fluent_args;

    let i18n = I18n::new(Language::Czech).expect("Failed to load Czech");

    // Test Czech plural forms (one, few, other)
    let cases = vec![
        (1, "minutu"), // one
        (2, "minuty"), // few
        (5, "minut"),  // other
    ];

    for (count, expected_form) in cases {
        let result = i18n.format("time-in", Some(&fluent_args!["minutes" => count]));

        assert!(
            result.is_ok(),
            "Pluralization should work for count {}",
            count
        );
        let text = result.unwrap();
        assert!(
            text.contains(expected_form),
            "For count {} expected '{}' but got '{}'",
            count,
            expected_form,
            text
        );
    }
}
