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
use std::collections::VecDeque;

/// Maximum number of history points to keep (48 hours at 15-minute intervals = 192 points)
const MAX_HISTORY_POINTS: usize = 192;

/// Single battery state of charge data point
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryHistoryPoint {
    pub timestamp: DateTime<Utc>,
    pub soc: f32,
    pub power_w: f32, // Battery power (positive = charging, negative = discharging)
    pub voltage_v: Option<f32>, // Battery voltage
}

/// Resource for storing battery SOC history over time
#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct BatteryHistory {
    /// Historical SOC data points (newest first)
    points: VecDeque<BatteryHistoryPoint>,
}

impl Default for BatteryHistory {
    fn default() -> Self {
        Self {
            points: VecDeque::with_capacity(MAX_HISTORY_POINTS),
        }
    }
}

impl BatteryHistory {
    /// Create a new empty battery history
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new data point to the history
    /// Automatically maintains the size limit
    pub fn add_point(&mut self, point: BatteryHistoryPoint) {
        tracing::info!(
            "📊 [BatteryHistory] Adding point: {:.1}% at {} (total will be: {})",
            point.soc,
            point.timestamp.format("%H:%M:%S"),
            (self.points.len() + 1).min(MAX_HISTORY_POINTS)
        );

        // Add to front (newest first)
        self.points.push_front(point);

        // Remove oldest points if we exceed the limit
        while self.points.len() > MAX_HISTORY_POINTS {
            self.points.pop_back();
        }
    }

    /// Get all history points (newest first)
    pub fn points(&self) -> &VecDeque<BatteryHistoryPoint> {
        &self.points
    }

    /// Get history points in chronological order (oldest first)
    pub fn points_chronological(&self) -> Vec<&BatteryHistoryPoint> {
        self.points.iter().rev().collect()
    }

    /// Get the most recent data point
    pub fn latest(&self) -> Option<&BatteryHistoryPoint> {
        self.points.front()
    }

    /// Get SOC at a specific timestamp (interpolated if needed)
    pub fn soc_at(&self, timestamp: DateTime<Utc>) -> Option<f32> {
        // Find surrounding points
        let mut before: Option<&BatteryHistoryPoint> = None;
        let mut after: Option<&BatteryHistoryPoint> = None;

        for point in self.points.iter() {
            if point.timestamp <= timestamp {
                before = Some(point);
                break;
            }
            after = Some(point);
        }

        match (before, after) {
            (Some(b), Some(a)) => {
                // Interpolate between two points
                let total_dur = (a.timestamp - b.timestamp).num_seconds() as f32;
                let elapsed = (timestamp - b.timestamp).num_seconds() as f32;
                let ratio = elapsed / total_dur;
                Some(b.soc + (a.soc - b.soc) * ratio)
            }
            (Some(b), None) => Some(b.soc), // Only have data after timestamp
            (None, Some(a)) => Some(a.soc), // Only have data before timestamp
            (None, None) => None,           // No data
        }
    }

    /// Get the number of stored points
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Check if history is empty
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Clear all history
    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Remove points older than the specified duration
    pub fn prune_older_than(&mut self, duration: chrono::Duration) {
        let cutoff = Utc::now() - duration;
        self.points.retain(|p| p.timestamp > cutoff);
    }

    /// Get average SOC over a time period
    pub fn average_soc_since(&self, since: DateTime<Utc>) -> Option<f32> {
        let relevant_points: Vec<_> = self
            .points
            .iter()
            .filter(|p| p.timestamp >= since)
            .collect();

        if relevant_points.is_empty() {
            return None;
        }

        let sum: f32 = relevant_points.iter().map(|p| p.soc).sum();
        Some(sum / relevant_points.len() as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_battery_history_add_point() {
        let mut history = BatteryHistory::new();
        assert!(history.is_empty());

        let point = BatteryHistoryPoint {
            timestamp: Utc::now(),
            soc: 50.0,
            power_w: 100.0,
            voltage_v: Some(52.0),
        };

        history.add_point(point);
        assert_eq!(history.len(), 1);
        assert_eq!(history.latest().unwrap().soc, 50.0);
    }

    #[test]
    fn test_battery_history_size_limit() {
        let mut history = BatteryHistory::new();

        // Add more than MAX_HISTORY_POINTS
        for i in 0..250 {
            let point = BatteryHistoryPoint {
                timestamp: Utc::now() + chrono::Duration::minutes(i),
                soc: 50.0 + i as f32,
                power_w: 100.0,
                voltage_v: None,
            };
            history.add_point(point);
        }

        // Should not exceed MAX_HISTORY_POINTS
        assert_eq!(history.len(), MAX_HISTORY_POINTS);
    }

    #[test]
    fn test_battery_history_chronological_order() {
        let mut history = BatteryHistory::new();
        let now = Utc::now();

        for i in 0..5 {
            let point = BatteryHistoryPoint {
                timestamp: now + chrono::Duration::minutes(i),
                soc: i as f32,
                power_w: 0.0,
                voltage_v: None,
            };
            history.add_point(point);
        }

        let chrono_points = history.points_chronological();
        assert_eq!(chrono_points.len(), 5);
        // Should be in order from oldest to newest
        assert_eq!(chrono_points[0].soc, 0.0);
        assert_eq!(chrono_points[4].soc, 4.0);
    }

    #[test]
    fn test_battery_history_prune() {
        let mut history = BatteryHistory::new();
        let now = Utc::now();

        // Add old points (all older than 48 hours)
        for i in 0..5 {
            let point = BatteryHistoryPoint {
                timestamp: now - chrono::Duration::hours(72) + chrono::Duration::hours(i),
                soc: i as f32,
                power_w: 0.0,
                voltage_v: None,
            };
            history.add_point(point);
        }

        // Add recent points (all within 48 hours)
        for i in 0..3 {
            let point = BatteryHistoryPoint {
                timestamp: now - chrono::Duration::hours(i),
                soc: 50.0 + i as f32,
                power_w: 0.0,
                voltage_v: None,
            };
            history.add_point(point);
        }

        assert_eq!(history.len(), 8);

        // Prune older than 48 hours
        history.prune_older_than(chrono::Duration::hours(48));

        // Should only have recent points
        assert_eq!(history.len(), 3);
    }
}
