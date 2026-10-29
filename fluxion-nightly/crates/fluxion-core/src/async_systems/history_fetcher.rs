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
use chrono::{DateTime, Utc};
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

/// While the history has no hourly profile or the last read failed, the fetcher retries this often
/// (brief H38). Without it a failed startup fetch would wait for the next midnight.
const HISTORY_RETRY_INTERVAL: chrono::Duration = chrono::Duration::minutes(10);

/// After `HISTORY_FAST_RETRY_ATTEMPTS` failing retries in a row the fetcher retries this slowly.
const HISTORY_SLOW_RETRY_INTERVAL: chrono::Duration = chrono::Duration::minutes(60);
const HISTORY_FAST_RETRY_ATTEMPTS: u32 = 6;

/// Retry interval after `attempts` consecutive failing retries (brief H38 round 3).
pub fn retry_interval(attempts: u32) -> chrono::Duration {
    if attempts < HISTORY_FAST_RETRY_ATTEMPTS {
        HISTORY_RETRY_INTERVAL
    } else {
        HISTORY_SLOW_RETRY_INTERVAL
    }
}

/// What a fetch produced: whether it gave an hourly profile, and whether a Home Assistant read failed.
#[derive(Clone, Copy)]
struct FetchStatus {
    has_profile: bool,
    failed: bool,
}

/// Whether the fetcher fetches again now (brief H38): without a profile, or after a failed read,
/// once `interval` has passed since the last attempt. Otherwise only the midnight refresh fetches.
pub fn should_refetch(
    now: DateTime<Utc>,
    last_attempt: DateTime<Utc>,
    has_profile: bool,
    last_failed: bool,
    interval: chrono::Duration,
) -> bool {
    (!has_profile || last_failed) && now - last_attempt >= interval
}

/// Next daily refresh: 00:05 local on the day after `now`, when the daily sensors have reset.
fn next_history_refresh(now: DateTime<chrono::Local>) -> DateTime<chrono::Local> {
    (now + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(0, 5, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .unwrap()
}

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
        let mut last_attempt = Utc::now();
        let mut status =
            fetch_and_report(&history_source_clone, &history_config, &history_tx).await;
        // Failing retries in a row since the last clean fetch
        let mut retry_attempts: u32 = 0;

        // Then continue with daily polling (fetch at midnight), retrying while there is no profile
        // or the last read failed
        let mut next_refresh = next_history_refresh(chrono::Local::now());
        loop {
            // Sleep until the next midnight refresh (+5 minutes, to ensure daily sensors have
            // reset), or until the retry is due
            let mut sleep_duration = (next_refresh - chrono::Local::now())
                .to_std()
                .unwrap_or(Duration::from_secs(3600));
            if !status.has_profile || status.failed {
                let retry_in = (last_attempt + retry_interval(retry_attempts)) - Utc::now();
                sleep_duration = sleep_duration.min(retry_in.to_std().unwrap_or(Duration::ZERO));
            }

            info!(
                "💤 Consumption history fetcher: sleeping until {} ({} seconds)",
                (chrono::Local::now()
                    + chrono::Duration::from_std(sleep_duration).unwrap_or_default())
                .format("%Y-%m-%d %H:%M:%S"),
                sleep_duration.as_secs()
            );
            Delay::new(sleep_duration).await;

            let refresh_due = chrono::Local::now() >= next_refresh;
            if refresh_due {
                next_refresh = next_history_refresh(chrono::Local::now());
            }
            let retry_due = should_refetch(
                Utc::now(),
                last_attempt,
                status.has_profile,
                status.failed,
                retry_interval(retry_attempts),
            );
            if refresh_due || retry_due {
                if refresh_due {
                    debug!("Fetching consumption history from HA (daily update)...");
                } else {
                    let message = format!(
                        "Retrying consumption history fetch (attempt {}, no house-load profile yet, or the last read failed)",
                        retry_attempts + 1
                    );
                    if retry_attempts == 0 {
                        info!("📊 {message}");
                    } else {
                        debug!("{message}");
                    }
                    retry_attempts = retry_attempts.saturating_add(1);
                }
                last_attempt = Utc::now();
                status =
                    fetch_and_report(&history_source_clone, &history_config, &history_tx).await;
                if status.has_profile && !status.failed {
                    retry_attempts = 0;
                }
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
        // Any result, even an empty one, ends the first schedule's wait for the load profile
        if !consumption_history.fetch_received() {
            consumption_history.mark_fetch_received();
        }
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

/// Runs one fetch and logs its error; a failed fetch counts as a failed read without a profile.
async fn fetch_and_report(
    source: &Arc<dyn crate::traits::ConsumptionHistoryDataSource>,
    config: &ConsumptionHistoryConfig,
    tx: &crossbeam_channel::Sender<ConsumptionHistoryUpdate>,
) -> FetchStatus {
    match fetch_consumption_history(source, config, tx).await {
        Ok(status) => status,
        Err(e) => {
            error!("❌ Failed to fetch consumption history: {e}");
            FetchStatus {
                has_profile: false,
                failed: true,
            }
        }
    }
}

/// Helper function to fetch consumption history from Home Assistant
async fn fetch_consumption_history(
    source: &Arc<dyn crate::traits::ConsumptionHistoryDataSource>,
    config: &ConsumptionHistoryConfig,
    tx: &crossbeam_channel::Sender<ConsumptionHistoryUpdate>,
) -> Result<FetchStatus> {
    let mut read_failed = false;
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
            read_failed = true;
            Vec::new()
        });

    // Fetch solar production history
    info!(
        "   Fetching solar production from: {}",
        config.solar_production_entity
    );
    // Solar plays no part in the hourly profile, so its read error does not make the fetch failed
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
                    read_failed = true;
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
    let status = FetchStatus {
        has_profile: hourly_profile.is_some(),
        failed: read_failed,
    };
    let update = ConsumptionHistoryUpdate {
        daily_summaries: summaries,
        hourly_profile,
    };
    if let Err(e) = tx.send(update) {
        error!("❌ Failed to send history update to channel: {e}");
    }

    Ok(status)
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
        // ...except that a fetch result arrived: the first schedule stops waiting (H31)
        assert!(history.fetch_received());
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use async_trait::async_trait;

    fn ten_minutes() -> chrono::Duration {
        chrono::Duration::minutes(10)
    }

    #[test]
    fn empty_or_failed_fetch_is_retried_within_ten_minutes() {
        let t0 = Utc::now();
        // No profile (empty fetch) or a failed read: the retry is due after 10 min, not before
        for (has_profile, failed) in [(false, false), (false, true), (true, true)] {
            assert!(!should_refetch(
                t0 + chrono::Duration::minutes(9),
                t0,
                has_profile,
                failed,
                retry_interval(0)
            ));
            assert!(should_refetch(
                t0 + chrono::Duration::minutes(10),
                t0,
                has_profile,
                failed,
                retry_interval(0)
            ));
        }
    }

    #[test]
    fn clean_profile_waits_for_the_midnight_refresh() {
        let t0 = Utc::now();
        assert!(!should_refetch(
            t0 + chrono::Duration::hours(2),
            t0,
            true,
            false,
            retry_interval(0)
        ));
    }

    #[test]
    fn retries_are_ten_minutes_then_hourly_after_six_failing_attempts() {
        assert_eq!(retry_interval(0), ten_minutes());
        assert_eq!(retry_interval(5), ten_minutes());
        assert_eq!(retry_interval(6), chrono::Duration::minutes(60));
        let t0 = Utc::now();
        // Fast: due at 10 min
        assert!(should_refetch(
            t0 + chrono::Duration::minutes(10),
            t0,
            false,
            false,
            retry_interval(5)
        ));
        // Slow: not due at 10 or 30 min, due at 60 min
        assert!(!should_refetch(
            t0 + chrono::Duration::minutes(30),
            t0,
            false,
            false,
            retry_interval(6)
        ));
        assert!(should_refetch(
            t0 + chrono::Duration::minutes(60),
            t0,
            false,
            false,
            retry_interval(6)
        ));
    }

    /// Source whose reads of the listed entities fail; the others return no points.
    struct ScriptedSource {
        failing: Vec<&'static str>,
    }

    #[async_trait]
    impl crate::traits::ConsumptionHistoryDataSource for ScriptedSource {
        async fn get_history(
            &self,
            entity_id: &str,
            _start_time: chrono::DateTime<Utc>,
            _end_time: Option<chrono::DateTime<Utc>>,
        ) -> anyhow::Result<Vec<crate::traits::HistoryDataPoint>> {
            if self.failing.contains(&entity_id) {
                anyhow::bail!("HA read failed for {entity_id}");
            }
            Ok(Vec::new())
        }

        async fn health_check(&self) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    /// Runs one fetch with the given house-load entity (None: not configured) and failing entities.
    fn fetch_status(house_load: Option<&str>, failing: Vec<&'static str>) -> FetchStatus {
        let config = ConsumptionHistoryConfig {
            consumption_entity: "sensor.import".to_string(),
            solar_production_entity: "sensor.solar".to_string(),
            house_load_power_entity: house_load.map(str::to_string),
            ..Default::default()
        };
        let source: Arc<dyn crate::traits::ConsumptionHistoryDataSource> =
            Arc::new(ScriptedSource { failing });
        let (tx, _rx) = crossbeam_channel::bounded(10);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(fetch_consumption_history(&source, &config, &tx))
            .unwrap()
    }

    #[test]
    fn solar_read_error_is_not_a_failed_read() {
        let status = fetch_status(None, vec!["sensor.solar"]);
        assert!(!status.failed);
    }

    #[test]
    fn import_or_house_load_read_error_is_a_failed_read() {
        assert!(fetch_status(None, vec!["sensor.import"]).failed);
        assert!(fetch_status(Some("sensor.house_load"), vec!["sensor.house_load"]).failed);
    }
}
