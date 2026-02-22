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

//! Schedule locking utilities for strategy evaluation
//!
//! Provides mechanisms to lock mode decisions for time blocks to prevent
//! oscillation when strategies are re-evaluated frequently. This is used
//! by V2 and V3 strategies to ensure stable operation.
//!
//! ## Usage
//!
//! When a strategy decides on a mode for a block, it can lock that decision
//! (and optionally subsequent blocks) to prevent re-evaluation from changing
//! the mode mid-execution.

use chrono::{DateTime, Utc};
use fluxion_types::inverter::InverterOperationMode;

/// A locked schedule entry - mode decision that should not change
#[derive(Debug, Clone)]
pub struct LockedBlock {
    /// Start time of this block
    pub block_start: DateTime<Utc>,
    /// The locked mode for this block
    pub mode: InverterOperationMode,
    /// Reason for this mode (for logging)
    pub reason: String,
}

/// State for schedule locking to prevent oscillation
#[derive(Debug, Clone, Default)]
pub struct ScheduleLockState {
    /// Locked blocks - mode decisions that should not be recalculated
    pub locked_blocks: Vec<LockedBlock>,
}

impl ScheduleLockState {
    /// Check if a block is locked and return its mode if so
    pub fn get_locked_mode(
        &self,
        block_start: DateTime<Utc>,
    ) -> Option<(InverterOperationMode, String)> {
        self.locked_blocks
            .iter()
            .find(|b| b.block_start == block_start)
            .map(|b| (b.mode, format!("LOCKED: {}", b.reason)))
    }

    /// Lock modes for the specified blocks
    pub fn lock_blocks(&mut self, blocks: Vec<LockedBlock>) {
        // Clear old locks (blocks in the past)
        let now = Utc::now();
        self.locked_blocks.retain(|b| b.block_start >= now);

        // Add new locks, avoiding duplicates
        for block in blocks {
            if !self
                .locked_blocks
                .iter()
                .any(|b| b.block_start == block.block_start)
            {
                self.locked_blocks.push(block);
            }
        }
    }
}
