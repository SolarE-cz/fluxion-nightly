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

use bevy_ecs::prelude::Component;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============= Pricing Components (FluxION MVP) =============

/// Spot price data from HA price integration
/// Uses 15-minute time blocks for granular scheduling
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct SpotPriceData {
    /// Array of time block prices (CZK/kWh)
    /// 15-minute blocks: 96 blocks per day, 96-140 blocks available (24-35 hours)
    pub time_block_prices: Vec<TimeBlockPrice>,

    /// Block duration in minutes (15 for current API)
    pub block_duration_minutes: u32,

    /// Timestamp when this data was fetched from HA
    pub fetched_at: DateTime<Utc>,

    /// HA entity last_updated timestamp
    pub ha_last_updated: DateTime<Utc>,
}

impl Default for SpotPriceData {
    fn default() -> Self {
        Self {
            time_block_prices: Vec::new(),
            block_duration_minutes: 15,
            fetched_at: Utc::now(),
            ha_last_updated: Utc::now(),
        }
    }
}

/// A single time block with price
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBlockPrice {
    /// Start time of this block
    pub block_start: DateTime<Utc>,

    /// Duration of this block (typically 15 minutes)
    pub duration_minutes: u32,

    /// Price for this time block (CZK/kWh)
    pub price_czk_per_kwh: f32,
}

/// Fixed price data when spot prices are disabled
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct FixedPriceData {
    /// Time block prices for buy (96 for 15-minute blocks, or 24 for hourly)
    /// Will be expanded to 96 blocks if only 24 provided
    pub buy_prices: Vec<f32>,

    /// Time block prices for sell (96 for 15-minute blocks, or 24 for hourly)
    /// Will be expanded to 96 blocks if only 24 provided
    pub sell_prices: Vec<f32>,

    /// Block duration in minutes
    pub block_duration_minutes: u32,
}

impl Default for FixedPriceData {
    fn default() -> Self {
        Self {
            buy_prices: vec![0.05; 24], // Default 24 hourly values
            sell_prices: vec![0.08; 24],
            block_duration_minutes: 60, // Hourly by default
        }
    }
}

impl FixedPriceData {
    /// Expand hourly prices (24 values) to 15-minute blocks (96 values)
    pub fn expand_to_15min_blocks(&mut self) {
        if self.buy_prices.len() == 24 {
            self.buy_prices = self
                .buy_prices
                .iter()
                .flat_map(|&price| vec![price; 4]) // Each hour = 4 blocks
                .collect();
        }

        if self.sell_prices.len() == 24 {
            self.sell_prices = self
                .sell_prices
                .iter()
                .flat_map(|&price| vec![price; 4])
                .collect();
        }

        self.block_duration_minutes = 15;
    }
}

/// Result of price analysis - identifies cheap/expensive blocks
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct PriceAnalysis {
    /// Indices of time blocks for force-charging (cheapest)
    pub charge_blocks: Vec<usize>,

    /// Indices of time blocks for force-discharging (most expensive)
    pub discharge_blocks: Vec<usize>,

    /// Price statistics
    pub price_range: PriceRange,

    /// When this analysis was generated
    pub analyzed_at: DateTime<Utc>,
}

impl Default for PriceAnalysis {
    fn default() -> Self {
        Self {
            charge_blocks: Vec::new(),
            discharge_blocks: Vec::new(),
            price_range: PriceRange::default(),
            analyzed_at: Utc::now(),
        }
    }
}

/// Price statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceRange {
    pub min_czk_per_kwh: f32,
    pub max_czk_per_kwh: f32,
    pub avg_czk_per_kwh: f32,
}

impl Default for PriceRange {
    fn default() -> Self {
        Self {
            min_czk_per_kwh: 0.0,
            max_czk_per_kwh: 0.0,
            avg_czk_per_kwh: 0.0,
        }
    }
}

/// Price component data with chart (for Web API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceData {
    pub current_price: f32,
    pub min_price: f32,
    pub max_price: f32,
    pub avg_price: f32,
    pub blocks: Vec<PriceBlockData>,
    // Today's price statistics
    pub today_min_price: f32,
    pub today_max_price: f32,
    pub today_avg_price: f32,
    pub today_median_price: f32,
    // Tomorrow's price statistics (None if not yet available)
    pub tomorrow_min_price: Option<f32>,
    pub tomorrow_max_price: Option<f32>,
    pub tomorrow_avg_price: Option<f32>,
    pub tomorrow_median_price: Option<f32>,
}

/// Compact representation of decision reasons to save space in exports
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DecisionReason {
    /// Self-Use - Normal operation (price)
    SelfUseNormal { price: f32 },

    /// Winter-Adaptive - Standard operation
    WinterAdaptiveStandard,

    /// Winter-Adaptive - Charging for horizon/target
    WinterAdaptiveCharge {
        price: f32,
        avg_price: f32,
        expected_profit: f32,
    },

    /// Winter-Adaptive - Urgent charge for today
    WinterAdaptiveUrgentCharge {
        price: f32,
        avg_price: f32,
        expected_profit: f32,
    },

    /// Winter-Adaptive - Discharging during expensive block
    WinterAdaptiveDischarge { price: f32, expected_profit: f32 },

    /// Winter-Adaptive - Holding charge during cheap block
    WinterAdaptiveHold {
        current_price: f32,
        threshold_price: f32,
        expected_profit: f32,
    },

    /// Winter-Adaptive - Preserving for tomorrow
    WinterAdaptivePreserve {
        tomorrow_avg: f32,
        current_price: f32,
        expected_profit: f32,
    },

    /// Winter-Peak-Discharge - Peak price discharge
    WinterPeakDischarge { price: f32 },

    /// Time-Aware Charge - Cheapest block
    TimeAwareCharge { price: f32 },

    /// Merged with adjacent charge blocks for EEPROM protection
    EepromMergedGap { gap_filled: u8, max_gap: u8 },

    /// Extended sequence for EEPROM protection
    EepromExtendedSequence { mode: String, min_blocks: u8 },

    /// Custom reason (fallback for new patterns)
    Custom { reason: String },
}

impl DecisionReason {
    /// Create from legacy string format for backward compatibility
    pub fn from_string(reason: &str) -> Self {
        // Parse common patterns
        if reason.starts_with("Self-Use - Normal operation")
            && let Some(price) = extract_price_from_reason(reason)
        {
            return Self::SelfUseNormal { price };
        }

        if reason == "Winter-Adaptive - Standard operation (expected profit: 0.00 CZK)" {
            return Self::WinterAdaptiveStandard;
        }

        if reason.starts_with("Winter-Adaptive - Charging for horizon/target")
            && let Some((price, avg, profit)) = extract_charge_info(reason)
        {
            return Self::WinterAdaptiveCharge {
                price,
                avg_price: avg,
                expected_profit: profit,
            };
        }

        if reason.starts_with("Winter-Adaptive - Urgent charge for today")
            && let Some((price, avg, profit)) = extract_charge_info(reason)
        {
            return Self::WinterAdaptiveUrgentCharge {
                price,
                avg_price: avg,
                expected_profit: profit,
            };
        }

        if reason.starts_with("Winter-Adaptive - Discharging during expensive block")
            && let Some((price, profit)) = extract_discharge_info(reason)
        {
            return Self::WinterAdaptiveDischarge {
                price,
                expected_profit: profit,
            };
        }

        if reason.starts_with("Winter-Adaptive - Holding charge during cheap block")
            && let Some((current, threshold, profit)) = extract_hold_info(reason)
        {
            return Self::WinterAdaptiveHold {
                current_price: current,
                threshold_price: threshold,
                expected_profit: profit,
            };
        }

        if reason.starts_with("Winter-Adaptive - Preserving for tomorrow")
            && let Some((tomorrow_avg, current, profit)) = extract_preserve_info(reason)
        {
            return Self::WinterAdaptivePreserve {
                tomorrow_avg,
                current_price: current,
                expected_profit: profit,
            };
        }

        if reason.starts_with("Winter-Peak-Discharge - Peak price")
            && let Some(price) = extract_price_from_reason(reason)
        {
            return Self::WinterPeakDischarge { price };
        }

        if reason.starts_with("Time-Aware Charge - Cheapest block")
            && let Some(price) = extract_price_from_reason(reason)
        {
            return Self::TimeAwareCharge { price };
        }

        if reason.starts_with("Merged with adjacent charge blocks")
            && let Some((gap, max_gap)) = extract_gap_info(reason)
        {
            return Self::EepromMergedGap {
                gap_filled: gap,
                max_gap,
            };
        }

        if reason.starts_with("Extended")
            && reason.contains("sequence for EEPROM protection")
            && let Some((mode, min_blocks)) = extract_extended_info(reason)
        {
            return Self::EepromExtendedSequence { mode, min_blocks };
        }

        // Fallback to custom reason
        Self::Custom {
            reason: reason.to_string(),
        }
    }

    /// Convert back to human-readable string for display
    pub fn to_display_string(&self) -> String {
        match self {
            Self::SelfUseNormal { price } => {
                format!("Self-Use - Normal operation ({:.2} CZK/kWh)", price)
            }
            Self::WinterAdaptiveStandard => "Winter-Adaptive - Standard operation".to_string(),
            Self::WinterAdaptiveCharge {
                price,
                avg_price,
                expected_profit,
            } => format!(
                "Winter-Adaptive - Charging for horizon/target ({:.2} CZK/kWh) (avg: {:.2}) (expected profit: {:.2} CZK)",
                price, avg_price, expected_profit
            ),
            Self::WinterAdaptiveUrgentCharge {
                price,
                avg_price,
                expected_profit,
            } => format!(
                "Winter-Adaptive - Urgent charge for today ({:.2} CZK/kWh) (avg: {:.2}) (expected profit: {:.2} CZK)",
                price, avg_price, expected_profit
            ),
            Self::WinterAdaptiveDischarge {
                price,
                expected_profit,
            } => format!(
                "Winter-Adaptive - Discharging during expensive block ({:.2} CZK/kWh) (expected profit: {:.2} CZK)",
                price, expected_profit
            ),
            Self::WinterAdaptiveHold {
                current_price,
                threshold_price,
                expected_profit,
            } => format!(
                "Winter-Adaptive - Holding charge during cheap block ({:.2} < {:.2}) (expected profit: {:.2} CZK)",
                current_price, threshold_price, expected_profit
            ),
            Self::WinterAdaptivePreserve {
                tomorrow_avg,
                current_price,
                expected_profit,
            } => format!(
                "Winter-Adaptive - Preserving for tomorrow (avg {:.2} > {:.2}) (expected profit: {:.2} CZK)",
                tomorrow_avg, current_price, expected_profit
            ),
            Self::WinterPeakDischarge { price } => {
                format!("Winter-Peak-Discharge - Peak price ({:.2} CZK/kWh)", price)
            }
            Self::TimeAwareCharge { price } => {
                format!("Time-Aware Charge - Cheapest block ({:.2} CZK/kWh)", price)
            }
            Self::EepromMergedGap {
                gap_filled,
                max_gap,
            } => format!(
                "Merged with adjacent charge blocks (gap {}/{} filled for EEPROM protection)",
                gap_filled, max_gap
            ),
            Self::EepromExtendedSequence { mode, min_blocks } => format!(
                "Extended {} sequence for EEPROM protection (min {} blocks)",
                mode, min_blocks
            ),
            Self::Custom { reason } => reason.clone(),
        }
    }
}

// Helper functions to extract values from legacy strings
fn extract_price_from_reason(reason: &str) -> Option<f32> {
    // Extract price like "(3.22 CZK/kWh)"
    if let Some(start) = reason.find('(')
        && let Some(end) = reason[start..].find(' ')
        && let Ok(price) = reason[start + 1..start + end].parse::<f32>()
    {
        return Some(price);
    }
    None
}

fn extract_charge_info(reason: &str) -> Option<(f32, f32, f32)> {
    // Extract: price (avg: X) (expected profit: Y CZK)
    let price = extract_price_from_reason(reason)?;

    let avg = if let Some(avg_start) = reason.find("(avg: ") {
        if let Some(avg_end) = reason[avg_start + 6..].find(')') {
            reason[avg_start + 6..avg_start + 6 + avg_end]
                .parse::<f32>()
                .ok()?
        } else {
            return None;
        }
    } else {
        return None;
    };

    let profit = if let Some(profit_start) = reason.find("(expected profit: ") {
        if let Some(profit_end) = reason[profit_start + 18..].find(' ') {
            reason[profit_start + 18..profit_start + 18 + profit_end]
                .parse::<f32>()
                .ok()?
        } else {
            return None;
        }
    } else {
        return None;
    };

    Some((price, avg, profit))
}

fn extract_discharge_info(reason: &str) -> Option<(f32, f32)> {
    let price = extract_price_from_reason(reason)?;

    let profit = if let Some(profit_start) = reason.find("(expected profit: ") {
        if let Some(profit_end) = reason[profit_start + 18..].find(' ') {
            reason[profit_start + 18..profit_start + 18 + profit_end]
                .parse::<f32>()
                .ok()?
        } else {
            return None;
        }
    } else {
        return None;
    };

    Some((price, profit))
}

fn extract_hold_info(reason: &str) -> Option<(f32, f32, f32)> {
    // Extract: (2.498 < 3.123) (expected profit: 0.00 CZK)
    if let Some(start) = reason.find('(')
        && let Some(less_than) = reason[start..].find(" < ")
    {
        let current_start = start + 1;
        let current_end = start + less_than;
        let current = reason[current_start..current_end].parse::<f32>().ok()?;

        let threshold_start = start + less_than + 3;
        if let Some(end_paren) = reason[threshold_start..].find(')') {
            let threshold = reason[threshold_start..threshold_start + end_paren]
                .parse::<f32>()
                .ok()?;

            let profit = if let Some(profit_start) = reason.find("(expected profit: ") {
                if let Some(profit_end) = reason[profit_start + 18..].find(' ') {
                    reason[profit_start + 18..profit_start + 18 + profit_end]
                        .parse::<f32>()
                        .ok()?
                } else {
                    return None;
                }
            } else {
                return None;
            };

            return Some((current, threshold, profit));
        }
    }
    None
}

fn extract_preserve_info(reason: &str) -> Option<(f32, f32, f32)> {
    // Extract: (avg 4.27 > 2.90) (expected profit: 0.00 CZK)
    if let Some(avg_start) = reason.find("(avg ")
        && let Some(greater_than) = reason[avg_start + 5..].find(" > ")
    {
        let tomorrow_avg = reason[avg_start + 5..avg_start + 5 + greater_than]
            .parse::<f32>()
            .ok()?;

        let current_start = avg_start + 5 + greater_than + 3;
        if let Some(end_paren) = reason[current_start..].find(')') {
            let current = reason[current_start..current_start + end_paren]
                .parse::<f32>()
                .ok()?;

            let profit = if let Some(profit_start) = reason.find("(expected profit: ") {
                if let Some(profit_end) = reason[profit_start + 18..].find(' ') {
                    reason[profit_start + 18..profit_start + 18 + profit_end]
                        .parse::<f32>()
                        .ok()?
                } else {
                    return None;
                }
            } else {
                return None;
            };

            return Some((tomorrow_avg, current, profit));
        }
    }
    None
}

fn extract_gap_info(reason: &str) -> Option<(u8, u8)> {
    // Extract: (gap 1/2 filled for EEPROM protection)
    if let Some(gap_start) = reason.find("(gap ")
        && let Some(slash_pos) = reason[gap_start + 5..].find('/')
    {
        let gap_filled = reason[gap_start + 5..gap_start + 5 + slash_pos]
            .parse::<u8>()
            .ok()?;

        let max_start = gap_start + 5 + slash_pos + 1;
        if let Some(space_pos) = reason[max_start..].find(' ') {
            let max_gap = reason[max_start..max_start + space_pos]
                .parse::<u8>()
                .ok()?;
            return Some((gap_filled, max_gap));
        }
    }
    None
}

fn extract_extended_info(reason: &str) -> Option<(String, u8)> {
    // Extract: Extended charge sequence for EEPROM protection (min 3 blocks)
    if let Some(extended_start) = reason.find("Extended ")
        && let Some(sequence_pos) = reason[extended_start + 9..].find(" sequence")
    {
        let mode = reason[extended_start + 9..extended_start + 9 + sequence_pos].to_string();

        if let Some(min_start) = reason.find("(min ")
            && let Some(blocks_pos) = reason[min_start + 5..].find(" blocks")
        {
            let min_blocks = reason[min_start + 5..min_start + 5 + blocks_pos]
                .parse::<u8>()
                .ok()?;
            return Some((mode, min_blocks));
        }
    }
    None
}

/// Individual price block (for Web API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBlockData {
    pub timestamp: DateTime<Utc>,
    pub price: f32,
    pub block_type: String,           // "charge", "discharge", "self-use"
    pub target_soc: Option<f32>,      // Target SOC for charge/discharge blocks
    pub strategy: Option<String>,     // Strategy that chose this mode
    pub expected_profit: Option<f32>, // Expected profit for this block (CZK)
    pub reason: Option<String>,       // Detailed reason for the decision
    // Note: Debug info omitted for now to avoid circular dependency with strategy crate
    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub debug_info: Option<crate::strategy::BlockDebugInfo>,
    pub is_historical: bool, // True if block is in the past (shows regenerated schedule, not actual history)
}

/// Compact version of PriceBlockData for space-efficient exports
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPriceBlockData {
    /// Unix timestamp (seconds since epoch) - much shorter than ISO 8601
    #[serde(rename = "ts")]
    pub timestamp_unix: i64,

    /// Price rounded to 2 decimal places
    #[serde(rename = "p")]
    pub price: f32,

    /// Operation type: "c" = charge, "d" = discharge, "s" = self-use
    #[serde(rename = "op")]
    pub operation_type: String,

    /// Target SOC for charge/discharge blocks (rounded to 1 decimal)
    #[serde(rename = "soc", skip_serializing_if = "Option::is_none")]
    pub target_soc: Option<f32>,

    /// Strategy name abbreviated: "WA" = Winter-Adaptive, "SU" = Self-Use, etc.
    #[serde(rename = "st", skip_serializing_if = "Option::is_none")]
    pub strategy_code: Option<String>,

    /// Expected profit rounded to 2 decimal places
    #[serde(rename = "pr", skip_serializing_if = "Option::is_none")]
    pub expected_profit: Option<f32>,

    /// Compact decision reason
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    pub reason: Option<DecisionReason>,

    /// Historical flag
    #[serde(rename = "h")]
    pub is_historical: bool,
}

impl CompactPriceBlockData {
    /// Convert from full PriceBlockData
    pub fn from_price_block_data(block: &PriceBlockData) -> Self {
        Self {
            timestamp_unix: block.timestamp.timestamp(),
            price: (block.price * 100.0).round() / 100.0, // Round to 2 decimals
            operation_type: match block.block_type.as_str() {
                "charge" => "c".to_string(),
                "discharge" => "d".to_string(),
                "self-use" => "s".to_string(),
                other => other.chars().take(1).collect(), // First character for unknown types
            },
            target_soc: block.target_soc.map(|soc| (soc * 10.0).round() / 10.0), // Round to 1 decimal
            strategy_code: block.strategy.as_ref().map(|s| strategy_to_code(s)),
            expected_profit: block.expected_profit.map(|p| (p * 100.0).round() / 100.0),
            reason: block
                .reason
                .as_ref()
                .map(|r| DecisionReason::from_string(r)),
            is_historical: block.is_historical,
        }
    }

    /// Convert back to full PriceBlockData for compatibility
    pub fn to_price_block_data(&self) -> PriceBlockData {
        PriceBlockData {
            timestamp: DateTime::from_timestamp(self.timestamp_unix, 0).unwrap_or_else(Utc::now),
            price: self.price,
            block_type: match self.operation_type.as_str() {
                "c" => "charge".to_string(),
                "d" => "discharge".to_string(),
                "s" => "self-use".to_string(),
                other => other.to_string(),
            },
            target_soc: self.target_soc,
            strategy: self.strategy_code.as_ref().map(|c| code_to_strategy(c)),
            expected_profit: self.expected_profit,
            reason: self.reason.as_ref().map(|r| r.to_display_string()),
            is_historical: self.is_historical,
        }
    }
}

/// Convert strategy name to compact code
fn strategy_to_code(strategy: &str) -> String {
    match strategy {
        "Winter-Adaptive" => "WA".to_string(),
        "Self-Use" => "SU".to_string(),
        "Time-Aware Charge" => "TAC".to_string(),
        "Winter-Peak-Discharge" => "WPD".to_string(),
        "Price-Arbitrage" => "PA".to_string(),
        other => other.chars().filter(|c| c.is_uppercase()).take(3).collect(), // Take up to 3 uppercase letters
    }
}

/// Convert compact code back to strategy name
fn code_to_strategy(code: &str) -> String {
    match code {
        "WA" => "Winter-Adaptive".to_string(),
        "SU" => "Self-Use".to_string(),
        "TAC" => "Time-Aware Charge".to_string(),
        "WPD" => "Winter-Peak-Discharge".to_string(),
        "PA" => "Price-Arbitrage".to_string(),
        other => other.to_string(),
    }
}
