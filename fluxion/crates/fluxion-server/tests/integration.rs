#![allow(clippy::float_cmp)]
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

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use chrono::Utc;
use serde_json::json;

use fluxion_server::config::{
    AuthSettings, DatabaseSettings, EmailSettings, HeartbeatSettings, ServerConfig, ServerSettings,
};
use fluxion_server::dashboard::{self, DashboardState};
use fluxion_server::db::Database;
use fluxion_server::heartbeat::{self, HeartbeatState};
use fluxion_server::notifications::EmailNotifier;

const TEST_SECRET: &str = "test-secret-for-integration-tests";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_config() -> ServerConfig {
    ServerConfig {
        server: ServerSettings {
            bind_address: "127.0.0.1".to_owned(),
            port: 0,
        },
        auth: AuthSettings {
            shared_secret: TEST_SECRET.to_owned(),
        },
        heartbeat: HeartbeatSettings::default(),
        email: EmailSettings {
            smtp_host: "localhost".to_owned(),
            smtp_port: 2525,
            smtp_username: "test".to_owned(),
            smtp_password: "test".to_owned(),
            from_address: "test@example.com".to_owned(),
            use_tls: false,
            admin_recipients: vec!["admin@example.com".to_owned()],
        },
        database: DatabaseSettings::default(),
    }
}

struct TestServer {
    port: u16,
    db: Arc<Database>,
    client: reqwest::Client,
}

impl TestServer {
    async fn start() -> Self {
        let config = Arc::new(test_config());
        let db = Arc::new(Database::open(":memory:").expect("Failed to open in-memory database"));
        let notifier =
            Arc::new(EmailNotifier::new(&config.email).expect("Failed to create test notifier"));

        let heartbeat_state = HeartbeatState {
            db: Arc::clone(&db),
            config: Arc::clone(&config),
            notifier,
        };

        let dashboard_state = DashboardState {
            db: Arc::clone(&db),
        };

        let app = Router::new()
            .route("/", get(dashboard::dashboard_handler))
            .with_state(dashboard_state)
            .route(
                "/api/heartbeat",
                post(heartbeat::heartbeat_handler).with_state(heartbeat_state),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind test listener");
        let port = listener.local_addr().expect("No local addr").port();

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("Test server error");
        });

        Self {
            port,
            db,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn post_heartbeat(&self, body: &serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url("/api/heartbeat"))
            .json(body)
            .send()
            .await
            .expect("Failed to send heartbeat request")
    }

    async fn get_dashboard(&self) -> reqwest::Response {
        self.client
            .get(self.url("/"))
            .send()
            .await
            .expect("Failed to fetch dashboard")
    }
}

fn basic_heartbeat(instance_id: &str) -> serde_json::Value {
    json!({
        "instance_id": instance_id,
        "shared_secret": TEST_SECRET,
        "timestamp": Utc::now().to_rfc3339(),
        "fluxion_version": "0.2.35",
        "status": {
            "online": true,
            "friendly_name": instance_id,
            "strategy_name": "Winter-Adaptive-V9",
            "battery_soc": 65.0
        }
    })
}

fn sample_telemetry() -> serde_json::Value {
    json!({
        "collected_at": Utc::now().to_rfc3339(),
        "inverters": [{
            "id": "inverter-1",
            "battery_soc": 65.0,
            "battery_temperature_c": 22.5,
            "battery_input_energy_today_kwh": 5.2,
            "battery_output_energy_today_kwh": 3.1,
            "grid_import_today_kwh": 8.7,
            "grid_export_today_kwh": 2.3,
            "today_solar_energy_kwh": 12.5,
            "total_solar_energy_kwh": 4500.0,
            "online": true,
            "run_mode": "Normal",
            "error_code": 0,
            "inverter_temperature_c": 35.0,
            "mode": "SelfUse",
            "actual_mode": "SelfUse",
            "mode_synced": true
        }],
        "instance": {
            "current_mode": "SelfUse",
            "current_reason": "Solar production covering demand",
            "current_strategy": "Winter-Adaptive-V9",
            "expected_profit": 2.5,
            "total_expected_profit": 45.0,
            "inverter_source": true,
            "price_source": true,
            "errors": [],
            "consumption_ema_kwh": 15.0,
            "today_import_kwh": 8.7,
            "yesterday_import_kwh": 12.3,
            "solar_forecast_total_today_kwh": 18.5,
            "solar_forecast_remaining_today_kwh": 6.0,
            "solar_forecast_tomorrow_kwh": 20.0,
            "solar_forecast_actual_today_kwh": 12.5,
            "solar_forecast_accuracy_percent": 92.0,
            "hdo_low_tariff_periods": [["00:00", "06:00"], ["20:00", "22:00"]],
            "hdo_low_tariff_czk": 0.5,
            "hdo_high_tariff_czk": 1.2
        }
    })
}

fn sample_sync_data() -> serde_json::Value {
    json!({
        "battery_capacity_kwh": 10.0,
        "target_soc_max": 95.0,
        "target_soc_min": 15.0
    })
}

// ---------------------------------------------------------------------------
// Heartbeat — basic protocol tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_returns_ok() {
    let server = TestServer::start().await;
    let resp = server.post_heartbeat(&basic_heartbeat("test-1")).await;

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["message"].is_null());
}

#[tokio::test]
async fn heartbeat_response_contains_server_time() {
    let server = TestServer::start().await;
    let before = Utc::now();
    let resp = server.post_heartbeat(&basic_heartbeat("test-1")).await;
    let after = Utc::now();

    let body: serde_json::Value = resp.json().await.unwrap();
    let server_time: chrono::DateTime<Utc> =
        serde_json::from_value(body["server_time"].clone()).unwrap();
    assert!(server_time >= before);
    assert!(server_time <= after);
}

#[tokio::test]
async fn heartbeat_invalid_secret_returns_401() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("test-1");
    body["shared_secret"] = json!("wrong-secret");

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 401);

    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["ok"], false);
    assert!(result["message"].as_str().is_some());
}

#[tokio::test]
async fn heartbeat_empty_secret_returns_401() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("test-1");
    body["shared_secret"] = json!("");

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 401);
}

// ---------------------------------------------------------------------------
// Heartbeat — client record creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_creates_client_record() {
    let server = TestServer::start().await;
    server.post_heartbeat(&basic_heartbeat("client-abc")).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].instance_id, "client-abc");
    assert_eq!(clients[0].status, "online");
    assert_eq!(clients[0].fluxion_version.as_deref(), Some("0.2.35"));
    assert_eq!(
        clients[0].strategy_name.as_deref(),
        Some("Winter-Adaptive-V9")
    );
}

#[tokio::test]
async fn heartbeat_stores_friendly_name() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("name-test");
    body["status"]["friendly_name"] = json!("My Home Battery");
    server.post_heartbeat(&body).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients[0].friendly_name.as_deref(), Some("My Home Battery"));
}

#[tokio::test]
async fn heartbeat_stores_battery_soc() {
    let server = TestServer::start().await;
    server.post_heartbeat(&basic_heartbeat("soc-test")).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients[0].battery_soc, Some(65.0));
}

#[tokio::test]
async fn heartbeat_marks_client_online() {
    let server = TestServer::start().await;
    server.post_heartbeat(&basic_heartbeat("status-test")).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients[0].status, "online");
}

// ---------------------------------------------------------------------------
// Heartbeat — telemetry storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_with_telemetry_stores_latest_json() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("telem-1");
    body["telemetry"] = sample_telemetry();

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 200);

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    assert!(clients[0].latest_telemetry_json.is_some());

    let telem_json = clients[0].latest_telemetry_json.as_ref().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(telem_json).unwrap();
    assert_eq!(parsed["instance"]["current_mode"], "SelfUse");
    assert_eq!(parsed["inverters"][0]["battery_soc"], 65.0);
}

#[tokio::test]
async fn heartbeat_with_telemetry_inserts_snapshot_row() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("snap-1");
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    assert_eq!(server.db.telemetry_snapshot_count("snap-1").unwrap(), 1);
}

#[tokio::test]
async fn multiple_telemetry_heartbeats_insert_multiple_snapshots() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("snap-multi");
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    body["timestamp"] = json!(Utc::now().to_rfc3339());
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    body["timestamp"] = json!(Utc::now().to_rfc3339());
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    assert_eq!(server.db.telemetry_snapshot_count("snap-multi").unwrap(), 3);
}

#[tokio::test]
async fn telemetry_json_roundtrips_to_typed_struct() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("roundtrip");
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    let clients = server.db.get_all_clients().unwrap();
    let json_str = clients[0].latest_telemetry_json.as_ref().unwrap();

    let snapshot: fluxion_shared::telemetry::TelemetrySnapshot =
        serde_json::from_str(json_str).unwrap();
    assert_eq!(snapshot.inverters.len(), 1);
    assert_eq!(snapshot.inverters[0].battery_soc, 65.0);
    assert_eq!(snapshot.inverters[0].grid_import_today_kwh, Some(8.7));
    assert_eq!(snapshot.instance.current_mode, "SelfUse");
    assert_eq!(snapshot.instance.solar_forecast_total_today_kwh, 18.5);
    assert_eq!(snapshot.instance.errors.len(), 0);
}

// ---------------------------------------------------------------------------
// Heartbeat — sync data storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_with_sync_data_stores_capacity() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("sync-1");
    body["sync_data"] = sample_sync_data();

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 200);

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients[0].battery_capacity_kwh, Some(10.0));
    assert_eq!(clients[0].target_soc_max, Some(95.0));
    assert_eq!(clients[0].target_soc_min, Some(15.0));
}

#[tokio::test]
async fn sync_data_without_battery_capacity() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("no-cap");
    body["sync_data"] = json!({
        "battery_capacity_kwh": null,
        "target_soc_max": 90.0,
        "target_soc_min": 10.0
    });
    server.post_heartbeat(&body).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients[0].battery_capacity_kwh, None);
    assert_eq!(clients[0].target_soc_max, Some(90.0));
    assert_eq!(clients[0].target_soc_min, Some(10.0));
}

// ---------------------------------------------------------------------------
// Heartbeat — full payload (telemetry + sync data)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_full_payload_stores_everything() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("full-1");
    body["telemetry"] = sample_telemetry();
    body["sync_data"] = sample_sync_data();

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 200);

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    // Telemetry stored
    assert!(clients[0].latest_telemetry_json.is_some());
    assert_eq!(server.db.telemetry_snapshot_count("full-1").unwrap(), 1);
    // Sync data stored
    assert_eq!(clients[0].battery_capacity_kwh, Some(10.0));
    assert_eq!(clients[0].target_soc_max, Some(95.0));
    assert_eq!(clients[0].target_soc_min, Some(15.0));
}

// ---------------------------------------------------------------------------
// Heartbeat — upsert and multi-client behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_heartbeats_same_client_upserts() {
    let server = TestServer::start().await;

    let mut body1 = basic_heartbeat("multi-1");
    body1["status"]["battery_soc"] = json!(50.0);
    server.post_heartbeat(&body1).await;

    let mut body2 = basic_heartbeat("multi-1");
    body2["status"]["battery_soc"] = json!(75.0);
    server.post_heartbeat(&body2).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1, "should be one client (upserted)");
    assert_eq!(clients[0].battery_soc, Some(75.0), "SOC should be latest");
    assert_eq!(
        server.db.heartbeat_count("multi-1").unwrap(),
        2,
        "two heartbeat log entries"
    );
}

#[tokio::test]
async fn multiple_different_clients() {
    let server = TestServer::start().await;

    server.post_heartbeat(&basic_heartbeat("alpha")).await;
    server.post_heartbeat(&basic_heartbeat("beta")).await;
    server.post_heartbeat(&basic_heartbeat("gamma")).await;

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 3);

    let ids: Vec<&str> = clients.iter().map(|c| c.instance_id.as_str()).collect();
    assert!(ids.contains(&"alpha"));
    assert!(ids.contains(&"beta"));
    assert!(ids.contains(&"gamma"));
}

#[tokio::test]
async fn heartbeat_logs_increase_per_client() {
    let server = TestServer::start().await;

    // 3 heartbeats from client A
    for _ in 0..3 {
        server.post_heartbeat(&basic_heartbeat("client-a")).await;
    }
    // 2 heartbeats from client B
    for _ in 0..2 {
        server.post_heartbeat(&basic_heartbeat("client-b")).await;
    }

    assert_eq!(server.db.heartbeat_count("client-a").unwrap(), 3);
    assert_eq!(server.db.heartbeat_count("client-b").unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Heartbeat — backward compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_backward_compat_no_telemetry_fields() {
    let server = TestServer::start().await;
    let body = json!({
        "instance_id": "old-client",
        "shared_secret": TEST_SECRET,
        "timestamp": Utc::now().to_rfc3339(),
        "fluxion_version": "0.1.0",
        "status": {
            "online": true
        }
    });

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 200);

    let clients = server.db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].instance_id, "old-client");
    assert_eq!(clients[0].fluxion_version.as_deref(), Some("0.1.0"));
    assert!(clients[0].latest_telemetry_json.is_none());
    assert!(clients[0].battery_capacity_kwh.is_none());
    assert_eq!(server.db.telemetry_snapshot_count("old-client").unwrap(), 0);
}

#[tokio::test]
async fn heartbeat_without_optional_status_fields() {
    let server = TestServer::start().await;
    let body = json!({
        "instance_id": "minimal",
        "shared_secret": TEST_SECRET,
        "timestamp": Utc::now().to_rfc3339(),
        "fluxion_version": "0.2.35",
        "status": {
            "online": true
        }
    });

    let resp = server.post_heartbeat(&body).await;
    assert_eq!(resp.status(), 200);

    let clients = server.db.get_all_clients().unwrap();
    assert!(clients[0].friendly_name.is_none());
    assert!(clients[0].strategy_name.is_none());
    assert!(clients[0].battery_soc.is_none());
}

// ---------------------------------------------------------------------------
// Dashboard — rendering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dashboard_renders_empty_state() {
    let server = TestServer::start().await;

    let resp = server.get_dashboard().await;
    assert_eq!(resp.status(), 200);

    let html = resp.text().await.unwrap();
    assert!(html.contains("FluxION Server"));
    assert!(html.contains("No instances registered"));
}

#[tokio::test]
async fn dashboard_shows_client_after_heartbeat() {
    let server = TestServer::start().await;

    let mut body = basic_heartbeat("dash-test");
    body["status"]["friendly_name"] = json!("My Solar System");
    server.post_heartbeat(&body).await;

    let resp = server.get_dashboard().await;
    let html = resp.text().await.unwrap();
    assert!(html.contains("My Solar System"));
    assert!(html.contains("0.2.35"));
    assert!(!html.contains("No instances registered"));
}

#[tokio::test]
async fn dashboard_shows_multiple_clients() {
    let server = TestServer::start().await;

    let mut body_a = basic_heartbeat("house-a");
    body_a["status"]["friendly_name"] = json!("House Alpha");
    server.post_heartbeat(&body_a).await;

    let mut body_b = basic_heartbeat("house-b");
    body_b["status"]["friendly_name"] = json!("House Beta");
    server.post_heartbeat(&body_b).await;

    let resp = server.get_dashboard().await;
    let html = resp.text().await.unwrap();
    assert!(html.contains("House Alpha"));
    assert!(html.contains("House Beta"));
}

#[tokio::test]
async fn dashboard_shows_telemetry_sections() {
    let server = TestServer::start().await;

    let mut body = basic_heartbeat("telem-dash");
    body["status"]["friendly_name"] = json!("Telem House");
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    let resp = server.get_dashboard().await;
    let html = resp.text().await.unwrap();
    assert!(html.contains("Telem House"));
    assert!(
        html.contains("Today&#x27;s Energy") || html.contains("Today's Energy"),
        "energy section header"
    );
    assert!(html.contains("Grid Import"));
    assert!(html.contains("8.7"));
    assert!(html.contains("Solar Forecast"));
    assert!(html.contains("18.5"));
}

#[tokio::test]
async fn dashboard_shows_sync_data() {
    let server = TestServer::start().await;

    let mut body = basic_heartbeat("sync-dash");
    body["telemetry"] = sample_telemetry();
    body["sync_data"] = sample_sync_data();
    server.post_heartbeat(&body).await;

    let resp = server.get_dashboard().await;
    let html = resp.text().await.unwrap();
    assert!(html.contains("Battery Capacity"));
    assert!(html.contains("10.0"));
    assert!(html.contains("SOC Limits"));
}

#[tokio::test]
async fn dashboard_shows_no_telemetry_for_old_client() {
    let server = TestServer::start().await;

    let body = json!({
        "instance_id": "legacy",
        "shared_secret": TEST_SECRET,
        "timestamp": Utc::now().to_rfc3339(),
        "fluxion_version": "0.1.0",
        "status": {
            "online": true,
            "friendly_name": "Legacy System"
        }
    });
    server.post_heartbeat(&body).await;

    let resp = server.get_dashboard().await;
    let html = resp.text().await.unwrap();
    assert!(html.contains("Legacy System"));
    assert!(html.contains("No telemetry data"));
}

// ---------------------------------------------------------------------------
// Telemetry — data integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telemetry_with_errors_stored_correctly() {
    let server = TestServer::start().await;

    let mut body = basic_heartbeat("errors-test");
    let mut telem = sample_telemetry();
    telem["instance"]["errors"] = json!(["Inverter timeout", "Price API unreachable"]);
    body["telemetry"] = telem;
    server.post_heartbeat(&body).await;

    let clients = server.db.get_all_clients().unwrap();
    let json_str = clients[0].latest_telemetry_json.as_ref().unwrap();
    let snapshot: fluxion_shared::telemetry::TelemetrySnapshot =
        serde_json::from_str(json_str).unwrap();
    assert_eq!(snapshot.instance.errors.len(), 2);
    assert_eq!(snapshot.instance.errors[0], "Inverter timeout");
    assert_eq!(snapshot.instance.errors[1], "Price API unreachable");
}

#[tokio::test]
async fn telemetry_with_multiple_inverters() {
    let server = TestServer::start().await;

    let mut body = basic_heartbeat("multi-inv");
    body["telemetry"] = json!({
        "collected_at": Utc::now().to_rfc3339(),
        "inverters": [
            {
                "id": "inv-1",
                "battery_soc": 80.0,
                "battery_temperature_c": 20.0,
                "battery_input_energy_today_kwh": null,
                "battery_output_energy_today_kwh": null,
                "grid_import_today_kwh": 5.0,
                "grid_export_today_kwh": 1.0,
                "today_solar_energy_kwh": 10.0,
                "total_solar_energy_kwh": null,
                "online": true,
                "run_mode": "Normal",
                "error_code": 0,
                "inverter_temperature_c": 30.0,
                "mode": "SelfUse",
                "actual_mode": null,
                "mode_synced": true
            },
            {
                "id": "inv-2",
                "battery_soc": 45.0,
                "battery_temperature_c": 23.0,
                "battery_input_energy_today_kwh": null,
                "battery_output_energy_today_kwh": null,
                "grid_import_today_kwh": 3.0,
                "grid_export_today_kwh": 0.5,
                "today_solar_energy_kwh": 8.0,
                "total_solar_energy_kwh": null,
                "online": true,
                "run_mode": "Normal",
                "error_code": 0,
                "inverter_temperature_c": 32.0,
                "mode": "GridCharge",
                "actual_mode": "GridCharge",
                "mode_synced": true
            }
        ],
        "instance": {
            "current_mode": "SelfUse",
            "current_reason": "test",
            "current_strategy": null,
            "expected_profit": null,
            "total_expected_profit": null,
            "inverter_source": true,
            "price_source": true,
            "errors": [],
            "consumption_ema_kwh": null,
            "today_import_kwh": null,
            "yesterday_import_kwh": null,
            "solar_forecast_total_today_kwh": 0.0,
            "solar_forecast_remaining_today_kwh": 0.0,
            "solar_forecast_tomorrow_kwh": 0.0,
            "solar_forecast_actual_today_kwh": null,
            "solar_forecast_accuracy_percent": null,
            "hdo_low_tariff_periods": [],
            "hdo_low_tariff_czk": 0.0,
            "hdo_high_tariff_czk": 0.0
        }
    });
    server.post_heartbeat(&body).await;

    let clients = server.db.get_all_clients().unwrap();
    let snapshot: fluxion_shared::telemetry::TelemetrySnapshot =
        serde_json::from_str(clients[0].latest_telemetry_json.as_ref().unwrap()).unwrap();
    assert_eq!(snapshot.inverters.len(), 2);
    assert_eq!(snapshot.inverters[0].id, "inv-1");
    assert_eq!(snapshot.inverters[0].battery_soc, 80.0);
    assert_eq!(snapshot.inverters[1].id, "inv-2");
    assert_eq!(snapshot.inverters[1].battery_soc, 45.0);
}

// ---------------------------------------------------------------------------
// Telemetry cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telemetry_cleanup_preserves_recent_snapshots() {
    let server = TestServer::start().await;
    let mut body = basic_heartbeat("cleanup-test");
    body["telemetry"] = sample_telemetry();
    server.post_heartbeat(&body).await;

    assert_eq!(
        server.db.telemetry_snapshot_count("cleanup-test").unwrap(),
        1
    );

    // Cleanup with 30-day retention should keep the just-inserted snapshot
    let deleted = server.db.cleanup_old_telemetry(30).unwrap();
    assert_eq!(deleted, 0);

    // Snapshot still present
    assert_eq!(
        server.db.telemetry_snapshot_count("cleanup-test").unwrap(),
        1
    );
}

// ---------------------------------------------------------------------------
// Database — direct unit tests
// ---------------------------------------------------------------------------

#[test]
fn database_opens_in_memory() {
    let db = Database::open(":memory:").unwrap();
    let clients = db.get_all_clients().unwrap();
    assert!(clients.is_empty());
}

#[test]
fn database_upsert_and_get_clients() {
    let db = Database::open(":memory:").unwrap();

    db.upsert_client(
        "inst-1",
        Some("Test House"),
        Some("0.2.35"),
        Some("V9"),
        Some(55.0),
        None,
    )
    .unwrap();

    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].instance_id, "inst-1");
    assert_eq!(clients[0].friendly_name.as_deref(), Some("Test House"));
    assert_eq!(clients[0].battery_soc, Some(55.0));
}

#[test]
fn database_upsert_updates_existing() {
    let db = Database::open(":memory:").unwrap();

    db.upsert_client(
        "inst-1",
        Some("House A"),
        Some("0.2.34"),
        None,
        Some(40.0),
        None,
    )
    .unwrap();
    db.upsert_client(
        "inst-1",
        Some("House A Updated"),
        Some("0.2.35"),
        None,
        Some(80.0),
        None,
    )
    .unwrap();

    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].friendly_name.as_deref(), Some("House A Updated"));
    assert_eq!(clients[0].fluxion_version.as_deref(), Some("0.2.35"));
    assert_eq!(clients[0].battery_soc, Some(80.0));
}

#[test]
fn database_heartbeat_logging() {
    let db = Database::open(":memory:").unwrap();

    db.upsert_client("inst-1", None, None, None, None, None)
        .unwrap();
    db.log_heartbeat("inst-1", r#"{"test": true}"#).unwrap();
    db.log_heartbeat("inst-1", r#"{"test": true, "seq": 2}"#)
        .unwrap();

    assert_eq!(db.heartbeat_count("inst-1").unwrap(), 2);
    assert_eq!(db.heartbeat_count("other").unwrap(), 0);
}

#[test]
fn database_status_update() {
    let db = Database::open(":memory:").unwrap();

    db.upsert_client("inst-1", None, None, None, None, None)
        .unwrap();
    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients[0].status, "online");

    db.update_client_status("inst-1", "offline").unwrap();
    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients[0].status, "offline");

    db.update_client_status("inst-1", "online").unwrap();
    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients[0].status, "online");
}

#[test]
fn database_sync_data_update() {
    let db = Database::open(":memory:").unwrap();

    db.upsert_client("inst-1", None, None, None, None, None)
        .unwrap();
    db.update_client_sync_data("inst-1", Some(10.0), 95.0, 15.0)
        .unwrap();

    let clients = db.get_all_clients().unwrap();
    assert_eq!(clients[0].battery_capacity_kwh, Some(10.0));
    assert_eq!(clients[0].target_soc_max, Some(95.0));
    assert_eq!(clients[0].target_soc_min, Some(15.0));
}

#[test]
fn database_notification_logging() {
    let db = Database::open(":memory:").unwrap();

    assert!(db.last_notification_for("inst-1", "offline").is_none());

    db.log_notification("inst-1", "offline", &["admin@example.com".to_owned()])
        .unwrap();

    assert!(db.last_notification_for("inst-1", "offline").is_some());
    assert!(db.last_notification_for("inst-1", "recovery").is_none());
}
