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

//! Shared application state managed by Tauri.

use std::path::PathBuf;
use tokio::sync::RwLock;

use crate::cache::UiCache;
use crate::credentials::{AppSettings, StoredConnection};
use crate::tor::TorClient;

/// Central app state, managed by Tauri and shared across all IPC commands.
pub struct AppState {
    pub app_handle: tauri::AppHandle,
    pub cache: UiCache,
    pub tor: RwLock<TorClient>,
    pub connection: RwLock<Option<StoredConnection>>,
    pub settings: RwLock<AppSettings>,
}

impl AppState {
    pub fn new(app_handle: tauri::AppHandle, data_dir: PathBuf) -> Self {
        Self {
            app_handle,
            cache: UiCache::new(data_dir.join("cache")),
            tor: RwLock::new(TorClient::new(data_dir.join("tor"))),
            connection: RwLock::new(None),
            settings: RwLock::new(AppSettings::default()),
        }
    }
}
