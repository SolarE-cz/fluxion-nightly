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

//! Utility functions for FluxION

/// Calculate Exponential Moving Average (EMA)
///
/// The EMA gives more weight to recent values while still considering historical data.
/// Uses the standard formula: EMA_t = α * value_t + (1 - α) * EMA_{t-1}
/// where α = 2 / (period + 1)
///
/// # Arguments
/// * `values` - Historical values, ordered from oldest to newest
/// * `period` - Number of periods to consider (e.g., 7 for 7-day EMA)
///
/// # Returns
/// The calculated EMA value, or None if values is empty
///
/// # Examples
/// ```
/// use fluxion_core::calculate_ema;
/// let consumption_history = vec![20.0, 22.0, 21.0, 23.0, 24.0, 22.0, 23.0];
/// let ema = calculate_ema(&consumption_history, 7);
/// assert!(ema.is_some());
/// ```
pub fn calculate_ema(values: &[f32], period: usize) -> Option<f32> {
    if values.is_empty() || period == 0 {
        return None;
    }

    // Calculate smoothing factor (alpha)
    // α = 2 / (period + 1)
    let alpha = 2.0 / (period + 1) as f32;

    // Initialize EMA with the first value
    let mut ema = values[0];

    // Calculate EMA for each subsequent value
    for &value in values.iter().skip(1) {
        ema = alpha * value + (1.0 - alpha) * ema;
    }

    Some(ema)
}

/// Calculate Simple Moving Average (SMA)
///
/// Simple average of the last N values.
///
/// # Arguments
/// * `values` - Historical values
/// * `period` - Number of periods to consider
///
/// # Returns
/// The calculated SMA value, or None if values is empty
pub fn calculate_sma(values: &[f32], period: usize) -> Option<f32> {
    if values.is_empty() || period == 0 {
        return None;
    }

    let window_size = period.min(values.len());
    let window = &values[values.len().saturating_sub(window_size)..];

    Some(window.iter().sum::<f32>() / window.len() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_empty_values() {
        assert_eq!(calculate_ema(&[], 7), None);
    }

    #[test]
    fn test_ema_zero_period() {
        assert_eq!(calculate_ema(&[1.0, 2.0, 3.0], 0), None);
    }

    #[test]
    fn test_ema_single_value() {
        assert_eq!(calculate_ema(&[10.0], 7), Some(10.0));
    }

    #[test]
    fn test_ema_calculation() {
        let values = vec![20.0, 22.0, 24.0, 21.0, 20.0, 23.0, 22.0];
        let ema = calculate_ema(&values, 7).unwrap();

        // EMA should be close to the values but weighted towards recent ones
        assert!(ema > 20.0 && ema < 24.0);
    }

    #[test]
    fn test_ema_consistent_values() {
        let values = vec![25.0; 10];
        let ema = calculate_ema(&values, 7).unwrap();

        // EMA of consistent values should equal the value
        assert!((ema - 25.0).abs() < 0.001);
    }

    #[test]
    fn test_sma_calculation() {
        let values = vec![10.0, 20.0, 30.0, 40.0];
        let sma = calculate_sma(&values, 4).unwrap();
        assert_eq!(sma, 25.0);
    }

    #[test]
    fn test_sma_window_larger_than_data() {
        let values = vec![10.0, 20.0];
        let sma = calculate_sma(&values, 5).unwrap();
        assert_eq!(sma, 15.0); // Should use all available data
    }
}
