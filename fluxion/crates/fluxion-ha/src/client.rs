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

use crate::errors::{HaError, HaResult};
use crate::types::{HaEntityState, HaHistoryState, HistoryDataPoint};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

/// Home Assistant REST API client
#[derive(Clone)]
pub struct HomeAssistantClient {
    base_url: String,
    token: String,
    client: Client,
    max_retries: u32,
    retry_delay: Duration,
}

impl HomeAssistantClient {
    /// Create a new HA client with custom configuration
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> HaResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| HaError::ConfigError(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            client,
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
        })
    }

    /// Create HA client using Supervisor API environment variables
    /// This is the standard method for HA addons
    pub fn from_supervisor() -> HaResult<Self> {
        let base_url = "http://supervisor/core";
        let token = std::env::var("SUPERVISOR_TOKEN").map_err(|_| {
            HaError::ConfigError(
                "SUPERVISOR_TOKEN environment variable not set. Are you running as an HA addon?"
                    .to_string(),
            )
        })?;

        info!("Initializing HA client using Supervisor API");
        Self::new(base_url, token)
    }

    /// Create HA client for development/testing with custom URL
    pub fn from_env() -> HaResult<Self> {
        let base_url =
            std::env::var("HA_BASE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());
        let token = std::env::var("HA_TOKEN").map_err(|_| {
            HaError::ConfigError("HA_TOKEN environment variable not set".to_string())
        })?;

        info!("Initializing HA client for development: {}", base_url);
        Self::new(base_url, token)
    }

    /// Create HA client from configuration values
    /// Falls back to environment variables if config values are not set
    pub fn from_config(ha_base_url: Option<String>, ha_token: Option<String>) -> HaResult<Self> {
        // Try config values first, then fall back to env vars
        let base_url = ha_base_url
            .or_else(|| std::env::var("HA_BASE_URL").ok())
            .unwrap_or_else(|| "http://localhost:8123".to_string());

        let token = ha_token
            .or_else(|| std::env::var("HA_TOKEN").ok())
            .ok_or_else(|| {
                HaError::ConfigError(
                    "HA token not found in config or HA_TOKEN environment variable".to_string(),
                )
            })?;

        info!("Initializing HA client from configuration: {}", base_url);
        Self::new(base_url, token)
    }

    /// Get the state of a specific entity
    pub async fn get_state(&self, entity_id: &str) -> HaResult<HaEntityState> {
        let url = format!("{}/api/states/{}", self.base_url, entity_id);
        debug!("🔍 [HA QUERY] Getting state for entity: {}", entity_id);
        debug!("   URL: {}", url);

        let response = self
            .retry_request(|| async { self.client.get(&url).bearer_auth(&self.token).send().await })
            .await?;

        match response.status() {
            StatusCode::OK => {
                let state = response.json::<HaEntityState>().await?;
                debug!("✅ [HA RESULT] Entity: {} = '{}'", entity_id, state.state);
                trace!("   Attributes: {:?}", state.attributes);
                trace!("   Last updated: {}", state.last_updated);
                Ok(state)
            }
            StatusCode::NOT_FOUND => {
                error!("❌ [HA ERROR] Entity not found: {}", entity_id);
                Err(HaError::EntityNotFound(entity_id.to_string()))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                error!(
                    "❌ [HA ERROR] Authentication failed for entity: {}",
                    entity_id
                );
                Err(HaError::AuthenticationFailed)
            }
            status => {
                let error_text = response.text().await.unwrap_or_default();
                error!("❌ [HA ERROR] Status {}: {}", status, error_text);
                Err(HaError::ApiError {
                    status: status.as_u16(),
                    message: error_text,
                })
            }
        }
    }

    /// Get states of multiple entities
    pub async fn get_states(&self, entity_ids: &[String]) -> HaResult<Vec<HaEntityState>> {
        info!(
            "🔍 [HA BATCH] Getting states for {} entities",
            entity_ids.len()
        );
        debug!("   Entities: {:?}", entity_ids);

        let mut states = Vec::new();
        for entity_id in entity_ids {
            match self.get_state(entity_id).await {
                Ok(state) => states.push(state),
                Err(e) => {
                    warn!("⚠️ [HA BATCH] Failed to get state for {}: {}", entity_id, e);
                    // Continue with other entities instead of failing completely
                }
            }
        }

        info!(
            "✅ [HA BATCH] Retrieved {}/{} entity states",
            states.len(),
            entity_ids.len()
        );
        Ok(states)
    }

    /// Call a Home Assistant service
    ///
    /// # Arguments
    /// * `service` - Service name in format "domain.service" (e.g., "select.select_option")
    /// * `data` - JSON data to send with the service call
    ///
    /// # Example
    /// ```no_run
    /// # use fluxion_ha::client::HomeAssistantClient;
    /// # use serde_json::json;
    /// # async fn example() {
    /// # let client = HomeAssistantClient::from_env().unwrap();
    /// client.call_service(
    ///     "select.select_option",
    ///     json!({
    ///         "entity_id": "select.solax_charger_use_mode",
    ///         "option": "Self Use Mode"
    ///     })
    /// ).await.unwrap();
    /// # }
    /// ```
    pub async fn call_service(&self, service: &str, data: Value) -> HaResult<()> {
        let parts: Vec<&str> = service.split('.').collect();
        if parts.len() != 2 {
            error!("❌ [HA ERROR] Invalid service format: {}", service);
            return Err(HaError::ServiceCallFailed {
                service: service.to_string(),
                reason: "Invalid service format, expected 'domain.service'".to_string(),
            });
        }

        let url = format!("{}/api/services/{}/{}", self.base_url, parts[0], parts[1]);
        info!("📞 [HA SERVICE] Calling: {}", service);
        info!(
            "   Data: {}",
            serde_json::to_string_pretty(&data).unwrap_or_else(|_| format!("{:?}", data))
        );
        debug!("   URL: {}", url);

        let response = self
            .retry_request(|| async {
                self.client
                    .post(&url)
                    .bearer_auth(&self.token)
                    .json(&data)
                    .send()
                    .await
            })
            .await?;

        let status = response.status();
        match status {
            StatusCode::OK => {
                info!("✅ [HA SERVICE] Success: {}", service);
                Ok(())
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                error!("❌ [HA SERVICE] Authentication failed for: {}", service);
                Err(HaError::AuthenticationFailed)
            }
            _status => {
                let error_msg = response.text().await.unwrap_or_default();
                error!("❌ [HA SERVICE] Failed: {} (status: {})", service, status);
                error!("   Error: {}", error_msg);
                Err(HaError::ServiceCallFailed {
                    service: service.to_string(),
                    reason: error_msg,
                })
            }
        }
    }

    /// Health check - ping HA API
    pub async fn ping(&self) -> HaResult<bool> {
        let url = format!("{}/api/", self.base_url);
        debug!("Performing health check");

        match self.client.get(&url).bearer_auth(&self.token).send().await {
            Ok(response) => {
                let is_ok = response.status().is_success();
                if is_ok {
                    debug!("Health check passed");
                } else {
                    warn!("Health check failed: status {}", response.status());
                }
                Ok(is_ok)
            }
            Err(e) => {
                warn!("Health check failed: {}", e);
                Ok(false) // Don't error on health check failure
            }
        }
    }

    /// Get all states (for debugging/discovery)
    pub async fn get_all_states(&self) -> HaResult<Vec<HaEntityState>> {
        let url = format!("{}/api/states", self.base_url);
        debug!("Fetching all entity states");

        let response = self
            .retry_request(|| async { self.client.get(&url).bearer_auth(&self.token).send().await })
            .await?;

        match response.status() {
            StatusCode::OK => Ok(response.json::<Vec<HaEntityState>>().await?),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(HaError::AuthenticationFailed),
            status => Err(HaError::ApiError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            }),
        }
    }

    /// Get Home Assistant configuration (including timezone)
    pub async fn get_config(&self) -> HaResult<Value> {
        let url = format!("{}/api/config", self.base_url);
        debug!("Fetching Home Assistant configuration");

        let response = self
            .retry_request(|| async { self.client.get(&url).bearer_auth(&self.token).send().await })
            .await?;

        match response.status() {
            StatusCode::OK => {
                let config = response.json::<Value>().await?;
                debug!("✅ Retrieved HA configuration");
                Ok(config)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(HaError::AuthenticationFailed),
            status => Err(HaError::ApiError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            }),
        }
    }

    /// Get Home Assistant timezone
    pub async fn get_timezone(&self) -> HaResult<String> {
        let config = self.get_config().await?;

        config
            .get("time_zone")
            .and_then(|tz| tz.as_str())
            .map(|tz| {
                info!("🌍 Home Assistant timezone: {}", tz);
                tz.to_string()
            })
            .ok_or_else(|| HaError::ConfigError("Timezone not found in HA config".to_string()))
    }

    /// Get historical data for a sensor entity
    ///
    /// # Arguments
    /// * `entity_id` - Entity ID to fetch history for (e.g., "sensor.solax_battery_capacity")
    /// * `start_time` - Start of the time range
    /// * `end_time` - Optional end of the time range (defaults to now)
    ///
    /// # Returns
    /// Vector of historical data points with parsed numeric values and timestamps
    pub async fn get_history(
        &self,
        entity_id: &str,
        start_time: DateTime<Utc>,
        end_time: Option<DateTime<Utc>>,
    ) -> HaResult<Vec<HistoryDataPoint>> {
        let end = end_time.unwrap_or_else(Utc::now);

        // HA history API expects ISO 8601 timestamps
        // Format: /api/history/period/{start}?filter_entity_id={entity}&end_time={end}
        let start_str = start_time.to_rfc3339();
        let end_str = end.to_rfc3339();

        // URL-encode the end_time parameter since it contains special characters
        let end_encoded = urlencoding::encode(&end_str);

        let url = format!(
            "{}/api/history/period/{}?filter_entity_id={}&end_time={}",
            self.base_url, start_str, entity_id, end_encoded
        );

        debug!("📊 [HA HISTORY] Fetching history for: {}", entity_id);
        debug!("   Time range: {} to {}", start_str, end_str);
        debug!("   URL: {}", url);

        let response = self
            .retry_request(|| async { self.client.get(&url).bearer_auth(&self.token).send().await })
            .await?;

        match response.status() {
            StatusCode::OK => {
                // HA returns an array of arrays, where each inner array is the history for one entity
                let history: Vec<Vec<HaHistoryState>> = response.json().await?;

                if history.is_empty() {
                    debug!("⚠️ [HA HISTORY] No history data returned for {}", entity_id);
                    return Ok(Vec::new());
                }

                // Take the first array (should be the only one since we filtered by entity_id)
                let entity_history = &history[0];

                // Parse into data points
                let mut data_points = Vec::new();
                for state in entity_history {
                    // Try to parse the state as a float
                    if let Ok(value) = state.state.parse::<f32>() {
                        // Parse the timestamp
                        if let Ok(timestamp) = DateTime::parse_from_rfc3339(&state.last_updated) {
                            data_points.push(HistoryDataPoint {
                                timestamp: timestamp.with_timezone(&Utc),
                                value,
                            });
                        } else {
                            trace!("Could not parse timestamp: {}", state.last_updated);
                        }
                    } else {
                        trace!("Skipping non-numeric state: {}", state.state);
                    }
                }

                info!(
                    "✅ [HA HISTORY] Retrieved {} data points for {}",
                    data_points.len(),
                    entity_id
                );
                Ok(data_points)
            }
            StatusCode::NOT_FOUND => {
                error!("❌ [HA HISTORY] Entity not found: {}", entity_id);
                Err(HaError::EntityNotFound(entity_id.to_string()))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                error!("❌ [HA HISTORY] Authentication failed for: {}", entity_id);
                Err(HaError::AuthenticationFailed)
            }
            status => {
                let error_text = response.text().await.unwrap_or_default();
                error!("❌ [HA HISTORY] Status {}: {}", status, error_text);
                Err(HaError::ApiError {
                    status: status.as_u16(),
                    message: error_text,
                })
            }
        }
    }

    /// Retry a request with exponential backoff
    async fn retry_request<F, Fut>(&self, mut request_fn: F) -> HaResult<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let mut attempts = 0;
        let mut delay = self.retry_delay;

        loop {
            attempts += 1;
            match request_fn().await {
                Ok(response) => return Ok(response),
                Err(e) if attempts >= self.max_retries => {
                    error!("Request failed after {} attempts: {}", attempts, e);
                    return Err(HaError::HttpError(e));
                }
                Err(e) => {
                    warn!(
                        "Request failed (attempt {}/{}): {}. Retrying in {:?}",
                        attempts, self.max_retries, e, delay
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2; // Exponential backoff
                }
            }
        }
    }

    /// Set custom retry configuration
    pub fn with_retry_config(mut self, max_retries: u32, retry_delay: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_delay = retry_delay;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    #[tokio::test]
    async fn test_get_state_success() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/api/states/sensor.test_entity")
            .match_header("authorization", "Bearer test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "entity_id": "sensor.test_entity",
                    "state": "42.5",
                    "attributes": {},
                    "last_changed": "2025-10-02T10:00:00Z",
                    "last_updated": "2025-10-02T10:00:00Z"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token").unwrap();
        let state = client.get_state("sensor.test_entity").await.unwrap();

        assert_eq!(state.entity_id, "sensor.test_entity");
        assert_eq!(state.state, "42.5");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_state_not_found() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/api/states/sensor.nonexistent")
            .match_header("authorization", "Bearer test_token")
            .with_status(404)
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token").unwrap();
        let result = client.get_state("sensor.nonexistent").await;

        assert!(matches!(result, Err(HaError::EntityNotFound(_))));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_call_service_success() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/services/select/select_option")
            .match_header("authorization", "Bearer test_token")
            .match_body(Matcher::Json(json!({
                "entity_id": "select.test",
                "option": "value"
            })))
            .with_status(200)
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token").unwrap();
        let result = client
            .call_service(
                "select.select_option",
                json!({"entity_id": "select.test", "option": "value"}),
            )
            .await;

        assert!(result.is_ok());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_call_service_invalid_format() {
        let client = HomeAssistantClient::new("http://localhost", "token").unwrap();
        let result = client.call_service("invalid", json!({})).await;

        assert!(matches!(result, Err(HaError::ServiceCallFailed { .. })));
    }

    #[tokio::test]
    async fn test_ping_success() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/api/")
            .match_header("authorization", "Bearer test_token")
            .with_status(200)
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token").unwrap();
        let result = client.ping().await.unwrap();

        assert!(result);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_logic() {
        let mut server = Server::new_async().await;

        // Mock will fail twice then succeed - mockito handles multiple responses
        let mock = server
            .mock("GET", "/api/states/sensor.test")
            .with_status(200)
            .with_body(
                json!({
                    "entity_id": "sensor.test",
                    "state": "ok",
                    "attributes": {},
                    "last_changed": "2025-10-02T10:00:00Z",
                    "last_updated": "2025-10-02T10:00:00Z"
                })
                .to_string(),
            )
            .expect_at_least(1)
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token")
            .unwrap()
            .with_retry_config(3, Duration::from_millis(10));

        let result = client.get_state("sensor.test").await;
        assert!(result.is_ok());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_multiple_states() {
        let mut server = Server::new_async().await;

        let mock1 = server
            .mock("GET", "/api/states/sensor.test1")
            .with_status(200)
            .with_body(
                json!({
                    "entity_id": "sensor.test1",
                    "state": "42",
                    "attributes": {},
                    "last_changed": "2025-10-02T10:00:00Z",
                    "last_updated": "2025-10-02T10:00:00Z"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let mock2 = server
            .mock("GET", "/api/states/sensor.test2")
            .with_status(200)
            .with_body(
                json!({
                    "entity_id": "sensor.test2",
                    "state": "43",
                    "attributes": {},
                    "last_changed": "2025-10-02T10:00:00Z",
                    "last_updated": "2025-10-02T10:00:00Z"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let client = HomeAssistantClient::new(server.url(), "test_token").unwrap();
        let states = client
            .get_states(&["sensor.test1".to_string(), "sensor.test2".to_string()])
            .await
            .unwrap();

        assert_eq!(states.len(), 2);
        assert_eq!(states[0].state, "42");
        assert_eq!(states[1].state, "43");
        mock1.assert_async().await;
        mock2.assert_async().await;
    }
}
