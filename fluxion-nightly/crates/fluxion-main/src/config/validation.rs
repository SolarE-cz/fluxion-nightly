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

use serde::{Deserialize, Serialize};

/// Validation result with detailed field-level errors and warnings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    /// Whether the configuration is valid
    pub valid: bool,
    /// Validation errors (prevent config from being used)
    pub errors: Vec<ValidationIssue>,
    /// Validation warnings (config can be used but may not be optimal)
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationResult {
    /// Create a successful validation result
    pub fn success() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Create a validation result with errors
    pub fn with_errors(errors: Vec<ValidationIssue>) -> Self {
        Self {
            valid: false,
            errors,
            warnings: Vec::new(),
        }
    }

    /// Add an error to the result
    pub fn add_error(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.valid = false;
        self.errors.push(ValidationIssue {
            field: field.into(),
            message: message.into(),
            severity: ValidationSeverity::Error,
        });
    }

    /// Add a warning to the result
    pub fn add_warning(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(ValidationIssue {
            field: field.into(),
            message: message.into(),
            severity: ValidationSeverity::Warning,
        });
    }

    /// Check if there are any errors
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Merge another validation result into this one
    pub fn merge(&mut self, other: ValidationResult) {
        self.valid = self.valid && other.valid;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }
}

/// A validation issue (error or warning)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// Field path (e.g., "control.min_battery_soc")
    pub field: String,
    /// Human-readable error message
    pub message: String,
    /// Severity of the issue
    pub severity: ValidationSeverity,
}

/// Severity level of a validation issue
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationSeverity {
    /// Prevents config from being used
    Error,
    /// Config can be used but may not be optimal
    Warning,
}
