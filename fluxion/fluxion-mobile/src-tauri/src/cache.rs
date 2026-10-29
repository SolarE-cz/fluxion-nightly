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

//! UI bundle and data caching for offline-first experience.
//!
//! Manages local caching of:
//! - UI bundle HTML (the mobile template rendered by the server)
//! - Latest data snapshot JSON
//!
//! Cache files are stored in the app's private data directory:
//! - `cache/ui_bundle.html` — the full UI HTML
//! - `cache/ui_version.txt` — version string for quick comparison
//! - `cache/state.json` — last fetched data snapshot
//! - `cache/state_timestamp.txt` — ISO 8601 timestamp of last fetch

use chrono::{DateTime, Utc};
use std::path::PathBuf;
use tracing::error;

pub struct UiCache {
    cache_dir: PathBuf,
}

impl UiCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        // Ensure cache directory exists
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            error!("Failed to create cache dir: {e}");
        }
        Self { cache_dir }
    }

    /// Returns the cached UI bundle HTML, or None if not yet cached.
    pub fn load_ui_bundle(&self) -> Option<String> {
        let path = self.cache_dir.join("ui_bundle.html");
        std::fs::read_to_string(path).ok()
    }

    /// Writes a new UI bundle to disk, replacing the previous one.
    pub fn store_ui_bundle(&self, html: &str, version: &str) -> Result<(), String> {
        let bundle_path = self.cache_dir.join("ui_bundle.html");
        let version_path = self.cache_dir.join("ui_version.txt");

        std::fs::write(&bundle_path, html)
            .map_err(|e| format!("Failed to write UI bundle: {e}"))?;
        std::fs::write(&version_path, version)
            .map_err(|e| format!("Failed to write UI version: {e}"))?;

        Ok(())
    }

    /// Returns the cached version string, or None.
    pub fn cached_ui_version(&self) -> Option<String> {
        let path = self.cache_dir.join("ui_version.txt");
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
    }

    /// Returns the cached data JSON and its timestamp, or None.
    pub fn load_cached_data(&self) -> Option<(String, DateTime<Utc>)> {
        let data_path = self.cache_dir.join("state.json");
        let ts_path = self.cache_dir.join("state_timestamp.txt");

        let data = std::fs::read_to_string(data_path).ok()?;
        let ts_str = std::fs::read_to_string(ts_path).ok()?;
        let timestamp = ts_str.trim().parse::<DateTime<Utc>>().ok()?;

        Some((data, timestamp))
    }

    /// Writes the latest data snapshot to disk.
    pub fn store_data(&self, json: &str, timestamp: DateTime<Utc>) -> Result<(), String> {
        let data_path = self.cache_dir.join("state.json");
        let ts_path = self.cache_dir.join("state_timestamp.txt");

        std::fs::write(&data_path, json).map_err(|e| format!("Failed to write state data: {e}"))?;
        std::fs::write(&ts_path, timestamp.to_rfc3339())
            .map_err(|e| format!("Failed to write state timestamp: {e}"))?;

        Ok(())
    }

    /// Clear all cached data (used on unpair/reset).
    pub fn clear(&self) -> Result<(), String> {
        for name in &[
            "ui_bundle.html",
            "ui_version.txt",
            "state.json",
            "state_timestamp.txt",
        ] {
            let path = self.cache_dir.join(name);
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| format!("Failed to remove {name}: {e}"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_cache_roundtrip() {
        let dir = std::env::temp_dir().join("fluxion_cache_test");
        let _ = std::fs::remove_dir_all(&dir);

        let cache = UiCache::new(dir.clone());

        // Initially empty
        assert!(cache.load_ui_bundle().is_none());
        assert!(cache.cached_ui_version().is_none());
        assert!(cache.load_cached_data().is_none());

        // Store UI bundle
        cache
            .store_ui_bundle("<html>test</html>", "0.2.35")
            .unwrap();
        assert_eq!(cache.load_ui_bundle().unwrap(), "<html>test</html>");
        assert_eq!(cache.cached_ui_version().unwrap(), "0.2.35");

        // Store data
        let ts = Utc::now();
        cache.store_data(r#"{"battery_soc": 75}"#, ts).unwrap();
        let (data, loaded_ts) = cache.load_cached_data().unwrap();
        assert!(data.contains("battery_soc"));
        assert_eq!(loaded_ts.timestamp(), ts.timestamp());

        // Clear
        cache.clear().unwrap();
        assert!(cache.load_ui_bundle().is_none());
        assert!(cache.load_cached_data().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
