// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Recorder history for the backtest page (brief H28): `HistoryReader` over the HA REST API.
//! Read-only (GET `/api/history/period`), 60 s timeout per request, never on the scheduling loop:
//! the web handler awaits it on the web server's runtime.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fluxion_core::backtest::{HistoryError, HistoryReader, HistoryState};

use crate::ha::client::HomeAssistantClient;
use crate::ha::errors::HaError;
use crate::ha::types::HaHistoryState;

/// Backtest history from Home Assistant.
#[derive(Clone)]
pub struct HaBacktestHistory {
    client: Arc<HomeAssistantClient>,
}

impl std::fmt::Debug for HaBacktestHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No URL or token
        f.debug_struct("HaBacktestHistory").finish_non_exhaustive()
    }
}

impl HaBacktestHistory {
    pub fn new(client: Arc<HomeAssistantClient>) -> Self {
        Self { client }
    }
}

/// The error kinds the page may show; URL, entity and token stay in the log.
fn history_error(err: &HaError) -> HistoryError {
    match err {
        HaError::HttpError(e) if e.is_timeout() => HistoryError::Timeout,
        HaError::HttpError(e) if e.is_decode() => HistoryError::InvalidResponse,
        HaError::HttpError(_) => HistoryError::Unreachable,
        HaError::Timeout => HistoryError::Timeout,
        HaError::AuthenticationFailed => HistoryError::Unauthorized,
        HaError::ApiError { status, .. } => HistoryError::Status(*status),
        HaError::EntityNotFound(_) => HistoryError::Status(404),
        HaError::InvalidResponse(_)
        | HaError::JsonError(_)
        | HaError::ServiceCallFailed { .. }
        | HaError::ConfigError(_) => HistoryError::InvalidResponse,
    }
}

/// HA states to backtest states. The time is `last_updated` when present (attribute changes),
/// else `last_changed`; a state with an unreadable time is dropped.
fn to_states(states: Vec<HaHistoryState>) -> Vec<HistoryState> {
    states
        .into_iter()
        .filter_map(|s| {
            let raw_time = s.last_updated.as_deref().unwrap_or(&s.last_changed);
            let at = DateTime::parse_from_rfc3339(raw_time)
                .ok()?
                .with_timezone(&Utc);
            Some(HistoryState::from_raw(at, &s.state, s.attributes))
        })
        .collect()
}

#[async_trait]
impl HistoryReader for HaBacktestHistory {
    async fn history(
        &self,
        entity_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        with_attributes: bool,
    ) -> Result<Vec<HistoryState>, HistoryError> {
        match self
            .client
            .get_history_states(entity_id, start, Some(end), !with_attributes)
            .await
        {
            Ok(states) => {
                let mut states = to_states(states);
                states.sort_by_key(|s| s.at);
                Ok(states)
            }
            Err(err) => {
                tracing::warn!("Backtest history of {entity_id} failed: {err}");
                Err(history_error(&err))
            }
        }
    }

    /// Same choice as the live HDO fetcher: `select_hdo_entity` over one `/api/states` read
    /// (the configured entity when it exists, else the HDO sensor found by content).
    async fn resolve_hdo_entity(&self, configured: &str) -> Result<Option<String>, HistoryError> {
        let states = self.client.get_all_states().await.map_err(|err| {
            tracing::warn!("Backtest HDO discovery failed: {err}");
            history_error(&err)
        })?;
        let chosen = crate::ha::plugin::select_hdo_entity(&states, configured)
            .map(|state| state.entity_id.clone());
        match &chosen {
            Some(entity) if entity != configured => tracing::info!(
                "Backtest HDO entity for '{configured}' resolved to '{entity}' by content"
            ),
            None => tracing::warn!("Backtest: no HDO entity found for '{configured}'"),
            Some(_) => {}
        }
        Ok(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[tokio::test]
    async fn reads_raw_states_with_gaps_and_minimal_params() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", Matcher::Regex(r"^/api/history/period/".to_owned()))
            .match_query(Matcher::Regex(
                r"filter_entity_id=sensor\.solax_house_load&end_time=[^&]+&minimal_response&no_attributes$"
                    .to_owned(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[[{"entity_id":"sensor.solax_house_load","state":"512","attributes":{},"last_changed":"2026-10-14T10:00:00+00:00","last_updated":"2026-10-14T10:00:00+00:00"},{"state":"unavailable","last_changed":"2026-10-14T10:05:00+00:00"},{"state":"700","last_changed":"2026-10-14T10:10:00+00:00"}]]"#)
            .create_async()
            .await;
        let reader = HaBacktestHistory::new(Arc::new(
            HomeAssistantClient::new(server.url(), "t").unwrap(),
        ));
        let start: DateTime<Utc> = "2026-10-14T10:00:00Z".parse().unwrap();
        let states = reader
            .history(
                "sensor.solax_house_load",
                start,
                start + chrono::Duration::hours(1),
                false,
            )
            .await
            .unwrap();
        assert_eq!(states.len(), 3);
        assert_eq!(states[0].state.as_deref(), Some("512"));
        assert_eq!(states[1].state, None);
        assert_eq!(states[2].state.as_deref(), Some("700"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn hdo_entity_is_discovered_like_live() {
        let mut server = Server::new_async().await;
        // The configured entity does not exist; a renamed schedule sensor does
        server
            .mock("GET", "/api/states")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[
                {"entity_id":"sensor.solax_house_load","state":"500","attributes":{},"last_changed":"2026-10-14T10:00:00+00:00","last_updated":"2026-10-14T10:00:00+00:00"},
                {"entity_id":"sensor.hdo_schedule_renamed","state":"on","attributes":{"schedule":[{"start":"2026-10-14T00:00:00+02:00","end":"2026-10-14T06:00:00+02:00","tariff":"NT","value":1}]},"last_changed":"2026-10-14T10:00:00+00:00","last_updated":"2026-10-14T10:00:00+00:00"}
            ]"#)
            .create_async()
            .await;
        let reader = HaBacktestHistory::new(Arc::new(
            HomeAssistantClient::new(server.url(), "t").unwrap(),
        ));
        assert_eq!(
            reader
                .resolve_hdo_entity("sensor.cez_hdo_raw_data")
                .await
                .unwrap(),
            Some("sensor.hdo_schedule_renamed".to_owned())
        );
        // The configured entity wins when it exists
        assert_eq!(
            reader
                .resolve_hdo_entity("sensor.solax_house_load")
                .await
                .unwrap(),
            Some("sensor.solax_house_load".to_owned())
        );
    }

    #[tokio::test]
    async fn errors_carry_no_url_or_token() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", Matcher::Regex(r"^/api/history/period/".to_owned()))
            .with_status(401)
            .create_async()
            .await;
        let reader = HaBacktestHistory::new(Arc::new(
            HomeAssistantClient::new(server.url(), "secret-token").unwrap(),
        ));
        let start: DateTime<Utc> = "2026-10-14T10:00:00Z".parse().unwrap();
        let err = reader
            .history("sensor.x", start, start, true)
            .await
            .unwrap_err();
        assert_eq!(err, HistoryError::Unauthorized);
        let text = err.to_string();
        assert!(
            !text.contains("secret-token") && !text.contains("http"),
            "{text}"
        );
    }
}
