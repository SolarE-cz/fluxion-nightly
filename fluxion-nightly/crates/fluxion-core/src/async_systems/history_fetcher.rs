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

use anyhow::Result;
use bevy_ecs::prelude::*;
use futures_timer::Delay;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::{
    async_tasks::*,
    components::*,
    resources::{ConsumptionHistoryConfig, ConsumptionHistoryDataSourceResource},
};

/// Days of house-load power history used for the hourly load profile
const HOUSE_LOAD_HISTORY_DAYS: i64 = 7;

/// Spawns the consumption history fetcher worker task
pub fn spawn_history_fetcher_worker(
    commands: &mut Commands,
    history_source: &ConsumptionHistoryDataSourceResource,
    history_config: &ConsumptionHistoryConfig,
) {
    info!("📊 Setting up consumption history fetcher...");

    let (history_tx, history_rx) = crossbeam_channel::bounded(10);
    let history_source_clone = history_source.0.clone();
    let history_config = history_config.clone();

    tokio::spawn(async move {
        info!("📊 Consumption history fetcher started");

        // Fetch immediately on startup
        debug!("Fetching initial consumption history from HA...");
        if let Err(e) =
            fetch_consumption_history(&history_source_clone, &history_config, &history_tx).await
        {
            error!("❌ Failed to fetch initial consumption history: {e}");
        }

        // Then continue with daily polling (fetch at midnight)
        loop {
            // Sleep until next midnight + 5 minutes (to ensure daily sensors have reset)
            let now = chrono::Local::now();
            let tomorrow_midnight = (now + chrono::Duration::days(1))
                .date_naive()
                .and_hms_opt(0, 5, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .unwrap();
            let sleep_duration = (tomorrow_midnight - now)
                .to_std()
                .unwrap_or(Duration::from_secs(3600));

            info!(
                "💤 Consumption history fetcher: sleeping until {} ({} seconds)",
                tomorrow_midnight.format("%Y-%m-%d %H:%M:%S"),
                sleep_duration.as_secs()
            );
            Delay::new(sleep_duration).await;

            debug!("Fetching consumption history from HA (daily update)...");
            if let Err(e) =
                fetch_consumption_history(&history_source_clone, &history_config, &history_tx).await
            {
                error!("❌ Failed to fetch consumption history: {e}");
            }
        }
    });

    commands.spawn((
        ConsumptionHistoryFetcher {
            source_name: "ha_consumption".to_string(),
            fetch_interval_hours: 24,
        },
        ConsumptionHistoryChannel {
            receiver: history_rx,
        },
    ));

    info!("✅ Consumption history fetcher entity created");
}

/// System that polls the consumption history channel and updates the ConsumptionHistory resource
pub fn poll_consumption_history_channel(
    history_channel: Query<&ConsumptionHistoryChannel>,
    mut consumption_history: ResMut<ConsumptionHistory>,
) {
    let Ok(channel) = history_channel.single() else {
        return; // No consumption history fetcher entity yet
    };

    // NON-BLOCKING: try to receive from channel
    while let Ok(update) = channel.receiver.try_recv() {
        // The hourly profile comes from house-load power and stands on its own: a site whose
        // consumption entity is missing has no daily summaries but still has a profile.
        if update.daily_summaries.is_empty() && update.hourly_profile.is_none() {
            debug!("⚠️ Received empty consumption history");
            continue;
        }

        info!(
            "📊 Updating consumption history: {} daily summaries, hourly profile: {}",
            update.daily_summaries.len(),
            if update.hourly_profile.is_some() {
                "yes"
            } else {
                "no"
            }
        );

        // Add all summaries to the history
        for summary in update.daily_summaries {
            consumption_history.add_summary(summary);
        }

        // Set the hourly consumption profile if available
        if let Some(profile) = update.hourly_profile {
            consumption_history.set_hourly_profile(profile);
        }

        info!(
            "✅ Consumption history updated: {} days available",
            consumption_history.summaries().len()
        );
    }
}

/// Helper function to fetch consumption history from Home Assistant
async fn fetch_consumption_history(
    source: &Arc<dyn crate::traits::ConsumptionHistoryDataSource>,
    config: &ConsumptionHistoryConfig,
    tx: &crossbeam_channel::Sender<ConsumptionHistoryUpdate>,
) -> Result<()> {
    info!(
        "📊 Fetching consumption history for last {} days",
        config.ema_days
    );

    // Calculate date range
    let now = chrono::Utc::now();
    let start_time = now - chrono::Duration::days(config.ema_days as i64);

    // Fetch consumption history
    info!(
        "   Fetching consumption from: {}",
        config.consumption_entity
    );
    let consumption_history = source
        .get_history(&config.consumption_entity, start_time, Some(now))
        .await
        .unwrap_or_else(|e| {
            error!("❌ Failed to fetch consumption history: {e}");
            Vec::new()
        });

    // Fetch solar production history
    info!(
        "   Fetching solar production from: {}",
        config.solar_production_entity
    );
    let solar_history = source
        .get_history(&config.solar_production_entity, start_time, Some(now))
        .await
        .unwrap_or_else(|e| {
            error!("❌ Failed to fetch solar history: {e}");
            Vec::new()
        });

    debug!(
        "   Retrieved {} consumption points and {} solar points",
        consumption_history.len(),
        solar_history.len()
    );

    // Aggregate into daily summaries
    let summaries =
        crate::components::aggregate_daily_consumption(&consumption_history, &solar_history);

    // Compute hourly consumption profile: house-load power when configured and it has enough days
    let consumption_profile = crate::components::aggregate_hourly_consumption(&consumption_history);
    let house_load_profile = match &config.house_load_power_entity {
        Some(entity) => {
            info!("   Fetching house load from: {}", entity);
            match source
                .get_history_with(
                    entity,
                    now - chrono::Duration::days(HOUSE_LOAD_HISTORY_DAYS),
                    Some(now),
                    true,
                )
                .await
            {
                Ok(power_history) => {
                    let profile = crate::components::aggregate_hourly_house_load(&power_history);
                    if profile.as_ref().is_none_or(|p| {
                        p.days_averaged
                            < crate::components::consumption_history::MIN_HOUSE_LOAD_DAYS
                    }) {
                        warn!(
                            "House load history for {} has too little data for a profile ({} points), using the import profile instead",
                            entity,
                            power_history.len()
                        );
                    }
                    profile
                }
                Err(e) => {
                    warn!(
                        "House load history for {} could not be fetched, using the import profile instead: {e}",
                        entity
                    );
                    None
                }
            }
        }
        None => None,
    };
    let hourly_profile =
        crate::components::pick_hourly_profile(house_load_profile, consumption_profile);

    info!(
        "✅ Aggregated {} daily summaries, hourly profile: {}",
        summaries.len(),
        if hourly_profile.is_some() {
            "computed"
        } else {
            "none"
        }
    );
    if let Some(profile) = &hourly_profile {
        info!(
            "📊 Hourly load profile ({} days) mean kWh/h {:?} p75 {:?}",
            profile.days_averaged, profile.hourly_avg_kwh, profile.hourly_p75_kwh
        );
    }

    // Send to channel
    let update = ConsumptionHistoryUpdate {
        daily_summaries: summaries,
        hourly_profile,
    };
    if let Err(e) = tx.send(update) {
        error!("❌ Failed to send history update to channel: {e}");
    }

    Ok(())
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use bevy_ecs::system::RunSystemOnce;

    fn profile() -> HourlyConsumptionProfile {
        HourlyConsumptionProfile {
            hourly_avg_kwh: [2.0; 24],
            days_averaged: 8,
            hourly_p75_kwh: None,
        }
    }

    fn poll(update: ConsumptionHistoryUpdate) -> ConsumptionHistory {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(update).unwrap();
        let mut world = World::new();
        world.insert_resource(ConsumptionHistory::new(7));
        world.spawn(ConsumptionHistoryChannel { receiver: rx });
        world
            .run_system_once(poll_consumption_history_channel)
            .unwrap();
        world.remove_resource::<ConsumptionHistory>().unwrap()
    }

    #[test]
    fn profile_is_kept_without_daily_summaries() {
        // A site whose consumption entity is missing has no daily summaries; its house-load profile
        // must still reach the planner.
        let history = poll(ConsumptionHistoryUpdate {
            daily_summaries: Vec::new(),
            hourly_profile: Some(profile()),
        });
        assert!(history.summaries().is_empty());
        assert_eq!(history.hourly_profile().unwrap().hourly_avg_kwh[3], 2.0);
    }

    #[test]
    fn empty_update_changes_nothing() {
        let history = poll(ConsumptionHistoryUpdate {
            daily_summaries: Vec::new(),
            hourly_profile: None,
        });
        assert!(history.hourly_profile().is_none());
    }
}
