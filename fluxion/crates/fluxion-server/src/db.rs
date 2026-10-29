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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::path::Path;
use std::sync::Mutex;

use fluxion_shared::telemetry::{ScheduleTelemetry, SocPredictionPoint, TelemetrySnapshot};

#[derive(Debug)]
pub struct Database {
    conn: Mutex<rusqlite::Connection>,
}

#[derive(Debug, Clone)]
pub struct ClientRecord {
    pub instance_id: String,
    pub friendly_name: Option<String>,
    pub last_seen: DateTime<Utc>,
    pub status: String,
    pub fluxion_version: Option<String>,
    pub strategy_name: Option<String>,
    pub battery_soc: Option<f32>,
    pub latest_telemetry_json: Option<String>,
    pub latest_telemetry_at: Option<String>,
    pub battery_capacity_kwh: Option<f32>,
    pub target_soc_max: Option<f32>,
    pub target_soc_min: Option<f32>,
}

impl Database {
    #[expect(
        clippy::too_many_lines,
        reason = "schema initialization with migrations"
    )]
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create database directory: {}", parent.display())
            })?;
        }

        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("Failed to open database: {path}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clients (
                instance_id    TEXT PRIMARY KEY,
                friendly_name  TEXT,
                first_seen     TEXT NOT NULL,
                last_seen      TEXT NOT NULL,
                status         TEXT NOT NULL DEFAULT 'online',
                fluxion_version TEXT,
                strategy_name  TEXT,
                battery_soc    REAL,
                extra_data     TEXT
            );

            CREATE TABLE IF NOT EXISTS heartbeat_log (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id    TEXT NOT NULL,
                received_at    TEXT NOT NULL,
                payload        TEXT NOT NULL,
                FOREIGN KEY (instance_id) REFERENCES clients(instance_id)
            );

            CREATE TABLE IF NOT EXISTS notification_log (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id    TEXT NOT NULL,
                event_type     TEXT NOT NULL,
                sent_at        TEXT NOT NULL,
                recipients     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS telemetry_snapshots (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id             TEXT NOT NULL,
                timestamp               TEXT NOT NULL,
                battery_soc             REAL,
                grid_import_today_kwh   REAL,
                grid_export_today_kwh   REAL,
                today_solar_energy_kwh  REAL,
                current_mode            TEXT,
                current_strategy        TEXT,
                snapshot_json           TEXT NOT NULL,
                FOREIGN KEY (instance_id) REFERENCES clients(instance_id)
            );

            CREATE INDEX IF NOT EXISTS idx_telemetry_instance_time
                ON telemetry_snapshots(instance_id, timestamp DESC);

            CREATE TABLE IF NOT EXISTS schedule_blocks (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id       TEXT NOT NULL,
                snapshot_id       INTEGER NOT NULL,
                block_ts          TEXT NOT NULL,
                price_czk         REAL,
                operation         TEXT,
                target_soc        REAL,
                strategy          TEXT,
                expected_profit   REAL,
                reason            TEXT,
                is_historical     BOOLEAN,
                FOREIGN KEY (instance_id) REFERENCES clients(instance_id),
                FOREIGN KEY (snapshot_id) REFERENCES telemetry_snapshots(id)
            );

            CREATE INDEX IF NOT EXISTS idx_schedule_blocks_instance_ts
                ON schedule_blocks(instance_id, block_ts);
            CREATE INDEX IF NOT EXISTS idx_schedule_blocks_strategy
                ON schedule_blocks(strategy);
            CREATE INDEX IF NOT EXISTS idx_schedule_blocks_snapshot
                ON schedule_blocks(snapshot_id);

            CREATE TABLE IF NOT EXISTS soc_predictions (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id       TEXT NOT NULL,
                snapshot_id       INTEGER NOT NULL,
                prediction_ts     TEXT NOT NULL,
                predicted_soc     REAL NOT NULL,
                FOREIGN KEY (instance_id) REFERENCES clients(instance_id),
                FOREIGN KEY (snapshot_id) REFERENCES telemetry_snapshots(id)
            );

            CREATE INDEX IF NOT EXISTS idx_soc_predictions_instance_ts
                ON soc_predictions(instance_id, prediction_ts);",
        )
        .context("Failed to initialize database schema")?;

        // Migrate clients table: add telemetry and sync columns (ignore if already exist)
        let migration_columns = [
            "latest_telemetry_json TEXT",
            "latest_telemetry_at TEXT",
            "battery_capacity_kwh REAL",
            "target_soc_max REAL",
            "target_soc_min REAL",
        ];
        for col_def in &migration_columns {
            let sql = format!("ALTER TABLE clients ADD COLUMN {col_def}");
            // Ignore "duplicate column" errors — column already exists
            let _ = conn.execute_batch(&sql);
        }

        // Migrate telemetry_snapshots: add schedule/price/consumption columns
        let telemetry_columns = [
            "total_expected_profit REAL",
            "schedule_blocks_count INTEGER",
            "price_min REAL",
            "price_max REAL",
            "price_avg REAL",
            "consumption_ema_kwh REAL",
            "solar_forecast_today_kwh REAL",
            "solar_actual_today_kwh REAL",
        ];
        for col_def in &telemetry_columns {
            let sql = format!("ALTER TABLE telemetry_snapshots ADD COLUMN {col_def}");
            let _ = conn.execute_batch(&sql);
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_client(
        &self,
        instance_id: &str,
        friendly_name: Option<&str>,
        version: Option<&str>,
        strategy: Option<&str>,
        soc: Option<f32>,
        extra: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO clients (instance_id, friendly_name, first_seen, last_seen, status, fluxion_version, strategy_name, battery_soc, extra_data)
             VALUES (?1, ?2, ?3, ?3, 'online', ?4, ?5, ?6, ?7)
             ON CONFLICT(instance_id) DO UPDATE SET
                friendly_name = COALESCE(?2, friendly_name),
                last_seen = ?3,
                status = 'online',
                fluxion_version = COALESCE(?4, fluxion_version),
                strategy_name = COALESCE(?5, strategy_name),
                battery_soc = COALESCE(?6, battery_soc),
                extra_data = COALESCE(?7, extra_data)",
            params![instance_id, friendly_name, now, version, strategy, soc, extra],
        )?;

        Ok(())
    }

    pub fn get_all_clients(&self) -> Result<Vec<ClientRecord>> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT instance_id, friendly_name, last_seen, status, fluxion_version, strategy_name, battery_soc,
                    latest_telemetry_json, latest_telemetry_at, battery_capacity_kwh, target_soc_max, target_soc_min
             FROM clients ORDER BY last_seen DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ClientRecord {
                    instance_id: row.get(0)?,
                    friendly_name: row.get(1)?,
                    last_seen: row.get(2)?,
                    status: row.get(3)?,
                    fluxion_version: row.get(4)?,
                    strategy_name: row.get(5)?,
                    battery_soc: row.get(6)?,
                    latest_telemetry_json: row.get(7)?,
                    latest_telemetry_at: row.get(8)?,
                    battery_capacity_kwh: row.get(9)?,
                    target_soc_max: row.get(10)?,
                    target_soc_min: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update_client_status(&self, instance_id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute(
            "UPDATE clients SET status = ?1 WHERE instance_id = ?2",
            params![status, instance_id],
        )?;
        Ok(())
    }

    pub fn log_heartbeat(&self, instance_id: &str, payload_json: &str) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO heartbeat_log (instance_id, received_at, payload) VALUES (?1, ?2, ?3)",
            params![instance_id, now, payload_json],
        )?;
        Ok(())
    }

    pub fn log_notification(
        &self,
        instance_id: &str,
        event_type: &str,
        recipients: &[String],
    ) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let now = Utc::now().to_rfc3339();
        let recipients_json = serde_json::to_string(recipients)?;
        conn.execute(
            "INSERT INTO notification_log (instance_id, event_type, sent_at, recipients) VALUES (?1, ?2, ?3, ?4)",
            params![instance_id, event_type, now, recipients_json],
        )?;
        Ok(())
    }

    pub fn last_notification_for(
        &self,
        instance_id: &str,
        event_type: &str,
    ) -> Option<DateTime<Utc>> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.query_row(
            "SELECT sent_at FROM notification_log WHERE instance_id = ?1 AND event_type = ?2 ORDER BY sent_at DESC LIMIT 1",
            params![instance_id, event_type],
            |row| row.get(0),
        )
        .ok()
    }

    pub fn insert_telemetry_snapshot(
        &self,
        instance_id: &str,
        snapshot: &TelemetrySnapshot,
    ) -> Result<i64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let timestamp = snapshot.collected_at.to_rfc3339();
        let snapshot_json = serde_json::to_string(snapshot)?;

        // Extract key columns from first inverter for dashboard queries
        let first_inv = snapshot.inverters.first();
        let battery_soc = first_inv.map(|i| i.battery_soc);
        let grid_import = first_inv.and_then(|i| i.grid_import_today_kwh);
        let grid_export = first_inv.and_then(|i| i.grid_export_today_kwh);
        let solar = first_inv.and_then(|i| i.today_solar_energy_kwh);
        let current_mode = Some(&snapshot.instance.current_mode);
        let current_strategy = snapshot.instance.current_strategy.as_deref();

        // Extract schedule/price/consumption columns
        let total_expected_profit = snapshot
            .schedule
            .as_ref()
            .and_then(|s| s.total_expected_profit);
        let schedule_blocks_count = snapshot
            .schedule
            .as_ref()
            .map(|s| i64::try_from(s.total_blocks).unwrap_or(i64::MAX));
        let price_min = snapshot.schedule.as_ref().map(|s| s.price_min);
        let price_max = snapshot.schedule.as_ref().map(|s| s.price_max);
        let price_avg = snapshot.schedule.as_ref().map(|s| s.price_avg);
        let consumption_ema_kwh = snapshot.instance.consumption_ema_kwh;
        let solar_forecast_today_kwh = Some(snapshot.instance.solar_forecast_total_today_kwh);
        let solar_actual_today_kwh = snapshot.instance.solar_forecast_actual_today_kwh;

        conn.execute(
            "INSERT INTO telemetry_snapshots (instance_id, timestamp, battery_soc, grid_import_today_kwh, grid_export_today_kwh, today_solar_energy_kwh, current_mode, current_strategy, snapshot_json, total_expected_profit, schedule_blocks_count, price_min, price_max, price_avg, consumption_ema_kwh, solar_forecast_today_kwh, solar_actual_today_kwh)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![instance_id, timestamp, battery_soc, grid_import, grid_export, solar, current_mode, current_strategy, snapshot_json, total_expected_profit, schedule_blocks_count, price_min, price_max, price_avg, consumption_ema_kwh, solar_forecast_today_kwh, solar_actual_today_kwh],
        )?;

        Ok(conn.last_insert_rowid())
    }

    pub fn update_latest_telemetry(&self, instance_id: &str, json: &str) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE clients SET latest_telemetry_json = ?1, latest_telemetry_at = ?2 WHERE instance_id = ?3",
            params![json, now, instance_id],
        )?;
        Ok(())
    }

    pub fn update_client_sync_data(
        &self,
        instance_id: &str,
        battery_capacity_kwh: Option<f32>,
        target_soc_max: f32,
        target_soc_min: f32,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        conn.execute(
            "UPDATE clients SET battery_capacity_kwh = ?1, target_soc_max = ?2, target_soc_min = ?3 WHERE instance_id = ?4",
            params![battery_capacity_kwh, target_soc_max, target_soc_min, instance_id],
        )?;
        Ok(())
    }

    pub fn cleanup_old_telemetry(&self, retention_days: u32) -> Result<u64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.to_rfc3339();
        let deleted = conn.execute(
            "DELETE FROM telemetry_snapshots WHERE timestamp < ?1",
            params![cutoff_str],
        )?;
        Ok(deleted as u64)
    }

    pub fn heartbeat_count(&self, instance_id: &str) -> Result<u64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let count: u64 = conn.query_row(
            "SELECT COUNT(*) FROM heartbeat_log WHERE instance_id = ?1",
            params![instance_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn telemetry_snapshot_count(&self, instance_id: &str) -> Result<u64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let count: u64 = conn.query_row(
            "SELECT COUNT(*) FROM telemetry_snapshots WHERE instance_id = ?1",
            params![instance_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn insert_schedule_blocks(
        &self,
        instance_id: &str,
        snapshot_id: i64,
        schedule: &ScheduleTelemetry,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let mut stmt = conn.prepare(
            "INSERT INTO schedule_blocks (instance_id, snapshot_id, block_ts, price_czk, operation, target_soc, strategy, expected_profit, reason, is_historical)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;

        for block in &schedule.blocks {
            stmt.execute(params![
                instance_id,
                snapshot_id,
                block.timestamp.to_rfc3339(),
                block.price_czk,
                block.operation,
                block.target_soc,
                block.strategy,
                block.expected_profit,
                block.reason,
                block.is_historical,
            ])?;
        }

        Ok(())
    }

    pub fn insert_soc_predictions(
        &self,
        instance_id: &str,
        snapshot_id: i64,
        predictions: &[SocPredictionPoint],
    ) -> Result<()> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let mut stmt = conn.prepare(
            "INSERT INTO soc_predictions (instance_id, snapshot_id, prediction_ts, predicted_soc)
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        for pred in predictions {
            stmt.execute(params![
                instance_id,
                snapshot_id,
                pred.timestamp.to_rfc3339(),
                pred.predicted_soc,
            ])?;
        }

        Ok(())
    }

    pub fn cleanup_old_schedule_blocks(&self, retention_days: u32) -> Result<u64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.to_rfc3339();
        let deleted = conn.execute(
            "DELETE FROM schedule_blocks WHERE block_ts < ?1",
            params![cutoff_str],
        )?;
        Ok(deleted as u64)
    }

    pub fn cleanup_old_soc_predictions(&self, retention_days: u32) -> Result<u64> {
        let conn = self.conn.lock().expect("database mutex poisoned");
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.to_rfc3339();
        let deleted = conn.execute(
            "DELETE FROM soc_predictions WHERE prediction_ts < ?1",
            params![cutoff_str],
        )?;
        Ok(deleted as u64)
    }
}
