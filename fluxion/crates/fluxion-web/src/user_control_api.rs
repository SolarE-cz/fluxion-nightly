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

//! User control API endpoints for managing FluxION runtime behavior.
//!
//! Provides endpoints for:
//! - Enabling/disabling FluxION mode changes
//! - Setting charge/discharge restrictions
//! - Managing fixed time slot overrides

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use fluxion_core::{UserControlChangeType, UserControlPersistence, UserControlUpdateEvent};
use fluxion_types::user_control::FixedTimeSlot;
use fluxion_types::{InverterOperationMode, UserControlState};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

/// Channel sender type for user control updates to ECS
pub type UserControlUpdateSender = fluxion_core::UserControlUpdateSender;

/// Shared state for user control API endpoints
#[derive(Clone)]
pub struct UserControlApiState {
    /// User control state in memory
    pub state: Arc<RwLock<UserControlState>>,
    /// Path to persistent state file
    pub persistence_path: String,
    /// Optional sender for user control updates to ECS
    pub update_sender: Option<UserControlUpdateSender>,
}

impl std::fmt::Debug for UserControlApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserControlApiState")
            .field("state", &"<RwLock>")
            .field("persistence_path", &self.persistence_path)
            .field("update_sender", &self.update_sender.is_some())
            .finish()
    }
}

impl UserControlApiState {
    /// Create new user control API state
    pub fn new(
        state: UserControlState,
        persistence_path: impl Into<String>,
        update_sender: Option<UserControlUpdateSender>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
            persistence_path: persistence_path.into(),
            update_sender,
        }
    }
}

// ==================== GET /api/user-control ====================

/// Response for GET /api/user-control
#[derive(Serialize)]
pub struct GetUserControlResponse {
    pub enabled: bool,
    pub disallow_charge: bool,
    pub disallow_discharge: bool,
    pub fixed_time_slots: Vec<FixedTimeSlotResponse>,
    pub last_modified: Option<String>,
}

/// Fixed time slot in API response format
#[derive(Serialize)]
pub struct FixedTimeSlotResponse {
    pub id: String,
    pub from: String,
    pub to: String,
    pub mode: String,
    pub note: Option<String>,
    pub created_at: String,
}

impl From<&FixedTimeSlot> for FixedTimeSlotResponse {
    fn from(slot: &FixedTimeSlot) -> Self {
        Self {
            id: slot.id.clone(),
            from: slot.from.to_rfc3339(),
            to: slot.to.to_rfc3339(),
            mode: format!("{:?}", slot.mode),
            note: slot.note.clone(),
            created_at: slot.created_at.to_rfc3339(),
        }
    }
}

/// GET /api/user-control - Get current user control state
pub async fn get_user_control(
    State(state): State<UserControlApiState>,
) -> Json<GetUserControlResponse> {
    let mut current_state = state.state.read().clone();
    current_state.cleanup_expired_slots(); // Clean up on read

    Json(GetUserControlResponse {
        enabled: current_state.enabled,
        disallow_charge: current_state.disallow_charge,
        disallow_discharge: current_state.disallow_discharge,
        fixed_time_slots: current_state
            .fixed_time_slots
            .iter()
            .map(FixedTimeSlotResponse::from)
            .collect(),
        last_modified: current_state.last_modified.map(|t| t.to_rfc3339()),
    })
}

// ==================== PUT /api/user-control/enabled ====================

/// Request for PUT /api/user-control/enabled
#[derive(Deserialize)]
pub struct SetEnabledRequest {
    pub enabled: bool,
}

/// Response for PUT /api/user-control/enabled
#[derive(Serialize)]
pub struct SetEnabledResponse {
    pub success: bool,
    pub enabled: bool,
}

/// PUT /api/user-control/enabled - Set FluxION enabled state
pub async fn set_enabled(
    State(state): State<UserControlApiState>,
    Json(request): Json<SetEnabledRequest>,
) -> Result<Json<SetEnabledResponse>, StatusCode> {
    let new_state = {
        let mut user_state = state.state.write();
        user_state.enabled = request.enabled;
        user_state.last_modified = Some(Utc::now());
        user_state.clone()
    };

    info!(
        "🎛️ User control: FluxION {}",
        if request.enabled {
            "ENABLED"
        } else {
            "DISABLED"
        }
    );

    persist_and_notify(&state, &new_state, UserControlChangeType::EnabledChanged)?;

    Ok(Json(SetEnabledResponse {
        success: true,
        enabled: request.enabled,
    }))
}

// ==================== PUT /api/user-control/restrictions ====================

/// Request for PUT /api/user-control/restrictions
#[derive(Deserialize)]
pub struct SetRestrictionsRequest {
    pub disallow_charge: Option<bool>,
    pub disallow_discharge: Option<bool>,
}

/// Response for PUT /api/user-control/restrictions
#[derive(Serialize)]
pub struct SetRestrictionsResponse {
    pub success: bool,
    pub disallow_charge: bool,
    pub disallow_discharge: bool,
}

/// PUT /api/user-control/restrictions - Set charge/discharge restrictions
pub async fn set_restrictions(
    State(state): State<UserControlApiState>,
    Json(request): Json<SetRestrictionsRequest>,
) -> Result<Json<SetRestrictionsResponse>, StatusCode> {
    let (disallow_charge, disallow_discharge, new_state) = {
        let mut user_state = state.state.write();
        if let Some(dc) = request.disallow_charge {
            user_state.disallow_charge = dc;
        }
        if let Some(dd) = request.disallow_discharge {
            user_state.disallow_discharge = dd;
        }
        user_state.last_modified = Some(Utc::now());
        (
            user_state.disallow_charge,
            user_state.disallow_discharge,
            user_state.clone(),
        )
    };

    info!(
        "🎛️ User control restrictions: disallow_charge={}, disallow_discharge={}",
        disallow_charge, disallow_discharge
    );

    persist_and_notify(
        &state,
        &new_state,
        UserControlChangeType::RestrictionsChanged,
    )?;

    Ok(Json(SetRestrictionsResponse {
        success: true,
        disallow_charge,
        disallow_discharge,
    }))
}

// ==================== POST /api/user-control/slots ====================

/// Request for POST /api/user-control/slots
#[derive(Deserialize)]
pub struct CreateSlotRequest {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub mode: String, // "SelfUse", "ForceCharge", "ForceDischarge", "BackUpMode", "NoChargeNoDischarge"
    pub note: Option<String>,
}

/// Response for slot operations
#[derive(Serialize)]
pub struct SlotResponse {
    pub success: bool,
    pub slot: Option<FixedTimeSlotResponse>,
    pub error: Option<String>,
}

/// POST /api/user-control/slots - Create a new fixed time slot
pub async fn create_slot(
    State(state): State<UserControlApiState>,
    Json(request): Json<CreateSlotRequest>,
) -> Result<Json<SlotResponse>, StatusCode> {
    // Parse mode
    let mode = parse_operation_mode(&request.mode).ok_or_else(|| {
        error!("Invalid mode: {}", request.mode);
        StatusCode::BAD_REQUEST
    })?;

    // Validate times
    if request.from >= request.to {
        error!("Invalid time range: from >= to");
        return Err(StatusCode::BAD_REQUEST);
    }

    let slot = FixedTimeSlot::new(request.from, request.to, mode, request.note);

    let new_state = {
        let mut user_state = state.state.write();
        user_state.fixed_time_slots.push(slot.clone());
        user_state.last_modified = Some(Utc::now());
        user_state.clone()
    };

    info!(
        "🎛️ User control: Created fixed slot {} ({:?}) from {} to {}",
        slot.id,
        slot.mode,
        slot.from.format("%H:%M"),
        slot.to.format("%H:%M")
    );

    persist_and_notify(&state, &new_state, UserControlChangeType::SlotAdded)?;

    Ok(Json(SlotResponse {
        success: true,
        slot: Some(FixedTimeSlotResponse::from(&slot)),
        error: None,
    }))
}

// ==================== PUT /api/user-control/slots/:id ====================

/// Request for PUT /api/user-control/slots/:id
#[derive(Deserialize)]
pub struct UpdateSlotRequest {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub mode: Option<String>,
    pub note: Option<String>,
}

/// PUT /api/user-control/slots/:id - Update an existing fixed time slot
pub async fn update_slot(
    State(state): State<UserControlApiState>,
    Path(slot_id): Path<String>,
    Json(request): Json<UpdateSlotRequest>,
) -> Result<Json<SlotResponse>, StatusCode> {
    let (updated_slot, new_state) = {
        let mut user_state = state.state.write();

        // Find slot index first to avoid borrow issues
        let slot_idx = user_state
            .fixed_time_slots
            .iter()
            .position(|s| s.id == slot_id)
            .ok_or(StatusCode::NOT_FOUND)?;

        // Scope the mutable borrow of the slot
        {
            let slot = &mut user_state.fixed_time_slots[slot_idx];
            if let Some(from) = request.from {
                slot.from = from;
            }
            if let Some(to) = request.to {
                slot.to = to;
            }
            if let Some(mode_str) = &request.mode {
                slot.mode = parse_operation_mode(mode_str).ok_or(StatusCode::BAD_REQUEST)?;
            }
            if request.note.is_some() {
                slot.note.clone_from(&request.note);
            }

            // Validate times
            if slot.from >= slot.to {
                return Err(StatusCode::BAD_REQUEST);
            }
        }

        user_state.last_modified = Some(Utc::now());
        (
            user_state.fixed_time_slots[slot_idx].clone(),
            user_state.clone(),
        )
    };

    info!("🎛️ User control: Updated fixed slot {}", slot_id);

    persist_and_notify(&state, &new_state, UserControlChangeType::SlotModified)?;

    Ok(Json(SlotResponse {
        success: true,
        slot: Some(FixedTimeSlotResponse::from(&updated_slot)),
        error: None,
    }))
}

// ==================== DELETE /api/user-control/slots/:id ====================

/// Response for DELETE /api/user-control/slots/:id
#[derive(Serialize)]
pub struct DeleteSlotResponse {
    pub success: bool,
}

/// DELETE /api/user-control/slots/:id - Delete a fixed time slot
pub async fn delete_slot(
    State(state): State<UserControlApiState>,
    Path(slot_id): Path<String>,
) -> Result<Json<DeleteSlotResponse>, StatusCode> {
    let new_state = {
        let mut user_state = state.state.write();
        let initial_len = user_state.fixed_time_slots.len();
        user_state.fixed_time_slots.retain(|s| s.id != slot_id);

        if user_state.fixed_time_slots.len() == initial_len {
            return Err(StatusCode::NOT_FOUND);
        }

        user_state.last_modified = Some(Utc::now());
        user_state.clone()
    };

    info!("🎛️ User control: Deleted fixed slot {}", slot_id);

    persist_and_notify(&state, &new_state, UserControlChangeType::SlotRemoved)?;

    Ok(Json(DeleteSlotResponse { success: true }))
}

// ==================== Helper Functions ====================

/// Parse operation mode from string
fn parse_operation_mode(mode_str: &str) -> Option<InverterOperationMode> {
    match mode_str {
        "SelfUse" => Some(InverterOperationMode::SelfUse),
        "ForceCharge" => Some(InverterOperationMode::ForceCharge),
        "ForceDischarge" => Some(InverterOperationMode::ForceDischarge),
        "BackUpMode" => Some(InverterOperationMode::BackUpMode),
        "NoChargeNoDischarge" => Some(InverterOperationMode::NoChargeNoDischarge),
        _ => None,
    }
}

/// Persist state to disk and notify ECS
fn persist_and_notify(
    api_state: &UserControlApiState,
    new_state: &UserControlState,
    change_type: UserControlChangeType,
) -> Result<(), StatusCode> {
    // Persist to disk
    let persistence = UserControlPersistence::new(&api_state.persistence_path);
    if let Err(e) = persistence.save(new_state) {
        error!("Failed to persist user control state: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Send update event to ECS
    if let Some(sender) = &api_state.update_sender {
        let event = UserControlUpdateEvent::new(new_state.clone(), change_type);
        if let Err(e) = sender.send(event) {
            error!("Failed to send user control update event: {}", e);
            // Don't fail the request - state is persisted, just ECS update failed
        }
    }

    Ok(())
}
