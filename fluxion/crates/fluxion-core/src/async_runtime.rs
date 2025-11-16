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

use bevy_ecs::prelude::Resource;
use std::future::Future;

/// Resource that provides access to async task spawning
/// Uses tokio runtime which is required for reqwest HTTP client
#[derive(Resource, Clone)]
pub struct AsyncRuntime;

impl AsyncRuntime {
    /// Create a new AsyncRuntime
    pub fn new() -> Self {
        Self
    }

    /// Spawn an async task using tokio
    /// Returns a JoinHandle that can be detached
    pub fn spawn<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T>
    where
        T: Send + 'static,
    {
        tokio::spawn(future)
    }
}

impl Default for AsyncRuntime {
    fn default() -> Self {
        Self::new()
    }
}
