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

//! Arti Tor client management.
//!
//! Embeds the Arti Rust Tor client for HTTP requests over Tor hidden services.
//! On first launch, Arti bootstraps (~10-30s). Subsequent launches use cached
//! consensus data (~2-5s).
//!
//! The Rust backend uses Arti directly for HTTP requests — no SOCKS5 proxy.
//! The WebView loads cached UI from local storage; all Tor communication
//! happens in the Rust layer with data passed to the WebView via Tauri IPC.

use std::path::PathBuf;

use arti_client::TorClient as ArtiClient;
use arti_client::config::TorClientConfigBuilder;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tor_rtcompat::PreferredRuntime;

/// Manages the embedded Arti Tor client.
pub struct TorClient {
    state_dir: PathBuf,
    onion_address: Option<String>,
    client_auth_key: Option<[u8; 32]>,
    inner: Option<ArtiClient<PreferredRuntime>>,
    bootstrap_status: BootstrapState,
}

/// Bootstrap progress state, exposed to UI.
#[derive(Debug, Clone)]
pub enum BootstrapState {
    NotStarted,
    Bootstrapping,
    Ready,
    Failed(String),
}

impl TorClient {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            onion_address: None,
            client_auth_key: None,
            inner: None,
            bootstrap_status: BootstrapState::NotStarted,
        }
    }

    /// Configure the connection target.
    pub fn configure(&mut self, onion_address: String, client_auth_key: [u8; 32]) {
        self.onion_address = Some(onion_address);
        self.client_auth_key = Some(client_auth_key);
    }

    /// Bootstrap the Tor client (async, may take 2-30 seconds).
    pub async fn bootstrap(&mut self) -> Result<(), String> {
        if self.onion_address.is_none() || self.client_auth_key.is_none() {
            return Err("Not configured — scan QR code first".to_owned());
        }

        // Create state directory for Tor consensus cache
        let state_dir = self.state_dir.clone();
        let cache_dir = self.state_dir.join("cache");
        std::fs::create_dir_all(&state_dir)
            .map_err(|e| format!("Failed to create Tor state dir: {e}"))?;
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create Tor cache dir: {e}"))?;

        self.bootstrap_status = BootstrapState::Bootstrapping;

        // Build configuration with custom directories
        let config = TorClientConfigBuilder::from_directories(state_dir, cache_dir)
            .build()
            .map_err(|e| {
                let msg = format!("Failed to build Tor config: {e}");
                self.bootstrap_status = BootstrapState::Failed(msg.clone());
                msg
            })?;

        // Create and bootstrap the Tor client
        tracing::info!("Bootstrapping Arti Tor client...");
        let client = ArtiClient::create_bootstrapped(config).await.map_err(|e| {
            let msg = format!("Tor bootstrap failed: {e}");
            self.bootstrap_status = BootstrapState::Failed(msg.clone());
            msg
        })?;

        tracing::info!("Arti Tor client bootstrapped successfully");
        self.inner = Some(client);
        self.bootstrap_status = BootstrapState::Ready;
        Ok(())
    }

    /// Check if the client is bootstrapped and ready.
    pub fn is_ready(&self) -> bool {
        matches!(self.bootstrap_status, BootstrapState::Ready)
    }

    /// Get current bootstrap status.
    pub fn bootstrap_status(&self) -> &BootstrapState {
        &self.bootstrap_status
    }

    /// Perform an HTTP GET request over Tor to the configured .onion address.
    pub async fn get(&self, path: &str) -> Result<String, String> {
        let client = self.inner.as_ref().ok_or("Tor client not bootstrapped")?;
        let onion = self
            .onion_address
            .as_ref()
            .ok_or("No onion address configured")?;

        let stream = client
            .connect((onion.as_str(), 80u16))
            .await
            .map_err(|e| format!("Tor connection failed: {e}"))?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| format!("HTTP handshake failed: {e}"))?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("HTTP connection task error: {e}");
            }
        });

        let req = hyper::Request::get(path)
            .header("Host", onion.as_str())
            .body(Empty::<Bytes>::new())
            .map_err(|e| format!("Failed to build request: {e}"))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("HTTP GET failed: {e}"))?;

        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("Failed to read response body: {e}"))?
            .to_bytes();

        String::from_utf8(body.to_vec()).map_err(|e| format!("Invalid UTF-8 response: {e}"))
    }

    /// Perform an HTTP POST request over Tor.
    pub async fn post(&self, path: &str, body: &str) -> Result<String, String> {
        let client = self.inner.as_ref().ok_or("Tor client not bootstrapped")?;
        let onion = self
            .onion_address
            .as_ref()
            .ok_or("No onion address configured")?;

        let stream = client
            .connect((onion.as_str(), 80u16))
            .await
            .map_err(|e| format!("Tor connection failed: {e}"))?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| format!("HTTP handshake failed: {e}"))?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("HTTP connection task error: {e}");
            }
        });

        let req = hyper::Request::post(path)
            .header("Host", onion.as_str())
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body.to_owned())))
            .map_err(|e| format!("Failed to build request: {e}"))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("HTTP POST failed: {e}"))?;

        let resp_body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("Failed to read response body: {e}"))?
            .to_bytes();

        String::from_utf8(resp_body.to_vec()).map_err(|e| format!("Invalid UTF-8 response: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_client() {
        let client = TorClient::new(PathBuf::from("/tmp/test-tor"));
        assert!(!client.is_ready());
        assert!(matches!(
            client.bootstrap_status(),
            BootstrapState::NotStarted
        ));
    }

    #[test]
    fn test_configure() {
        let mut client = TorClient::new(PathBuf::from("/tmp/test-tor"));
        client.configure("test.onion".to_owned(), [0u8; 32]);
        assert!(client.onion_address.is_some());
        assert!(client.client_auth_key.is_some());
    }

    #[test]
    fn test_bootstrap_status_transitions() {
        let client = TorClient::new(PathBuf::from("/tmp/test-tor"));
        assert!(matches!(
            client.bootstrap_status(),
            BootstrapState::NotStarted
        ));
    }

    #[test]
    fn test_not_ready_without_bootstrap() {
        let mut client = TorClient::new(PathBuf::from("/tmp/test-tor"));
        client.configure("test.onion".to_owned(), [0u8; 32]);
        assert!(!client.is_ready());
    }
}
