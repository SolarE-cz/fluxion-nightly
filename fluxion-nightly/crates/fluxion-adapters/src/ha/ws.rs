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

//! Minimal one-shot Home Assistant websocket session.
//!
//! The REST API cannot create helpers (`input_select`, `input_number`, ...); the websocket API can.
//! A session authenticates once and then makes request/response calls with incrementing ids.

use crate::ha::errors::{HaError, HaResult};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

/// Timeout for the handshake and for each call.
const WS_TIMEOUT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Websocket URL for a REST base URL.
///
/// Supervisor (`http://supervisor/core`) → `ws://supervisor/core/websocket`;
/// anything else → `ws://host:port/api/websocket`. `None` for https or non-http bases.
pub fn websocket_url(base_url: &str) -> Option<String> {
    let rest = base_url.trim_end_matches('/').strip_prefix("http://")?;
    let ws = format!("ws://{rest}");
    Some(if ws.ends_with("/core") {
        format!("{ws}/websocket")
    } else {
        format!("{ws}/api/websocket")
    })
}

/// Refuses a plaintext `ws://` URL that would carry the access token to a public host (scan
/// #12, #31). Plaintext stays allowed for loopback, RFC1918 and link-local addresses, `.local`
/// and single-label names (the Supervisor's `supervisor`). `wss://` URLs pass unchecked.
pub fn ensure_token_transport(url: &str) -> HaResult<()> {
    let Some(rest) = url.strip_prefix("ws://") else {
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    if plaintext_host_allowed(host) {
        Ok(())
    } else {
        Err(HaError::ConfigError(format!(
            "refusing plaintext websocket to {host}: the access token would cross the network unencrypted; use an https:// Home Assistant URL"
        )))
    }
}

fn plaintext_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            // 100.64.0.0/10: the NetBird and Tailscale overlay, already encrypted (scan #12, #31)
            let [first, second, ..] = ip.octets();
            let overlay = first == 100 && (second & 0xc0) == 0x40;
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || overlay
        }
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => {
            host == "localhost"
                || host.ends_with(".local")
                || host.ends_with(".netbird.cloud")
                || host.ends_with(".ts.net")
                || !host.contains('.')
        }
    }
}

/// An authenticated websocket session.
pub struct HaWsSession {
    socket: Socket,
    next_id: u64,
}

impl HaWsSession {
    /// Connect and authenticate: `auth_required` → `auth` → `auth_ok`.
    pub async fn connect(url: &str, token: &str) -> HaResult<Self> {
        ensure_token_transport(url)?;
        tokio::time::timeout(WS_TIMEOUT, Self::handshake(url, token))
            .await
            .map_err(|_| HaError::Timeout)?
    }

    async fn handshake(url: &str, token: &str) -> HaResult<Self> {
        let (mut socket, _) = connect_async(url)
            .await
            .map_err(|e| HaError::InvalidResponse(format!("websocket connect: {e}")))?;

        let hello = next_json(&mut socket).await?;
        if message_type(&hello) != "auth_required" {
            return Err(HaError::InvalidResponse(format!(
                "expected auth_required, got {hello}"
            )));
        }

        send_json(&mut socket, json!({"type": "auth", "access_token": token})).await?;

        let reply = next_json(&mut socket).await?;
        match message_type(&reply) {
            "auth_ok" => Ok(Self { socket, next_id: 1 }),
            "auth_invalid" => Err(HaError::AuthenticationFailed),
            other => Err(HaError::InvalidResponse(format!(
                "unexpected auth reply: {other}"
            ))),
        }
    }

    /// Send one command and return its `result`. Non-`success` replies become errors.
    pub async fn call(&mut self, mut msg: Value) -> HaResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let kind = message_type(&msg).to_string();
        msg["id"] = json!(id);

        let reply = tokio::time::timeout(WS_TIMEOUT, async {
            send_json(&mut self.socket, msg).await?;
            loop {
                let reply = next_json(&mut self.socket).await?;
                if message_type(&reply) == "result" && reply["id"].as_u64() == Some(id) {
                    return Ok::<Value, HaError>(reply);
                }
            }
        })
        .await
        .map_err(|_| HaError::Timeout)??;

        if reply["success"].as_bool() == Some(true) {
            Ok(reply["result"].clone())
        } else {
            Err(HaError::ServiceCallFailed {
                service: kind,
                reason: reply["error"].to_string(),
            })
        }
    }
}

fn message_type(value: &Value) -> &str {
    value["type"].as_str().unwrap_or_default()
}

async fn send_json(socket: &mut Socket, value: Value) -> HaResult<()> {
    socket
        .send(Message::Text(value.to_string()))
        .await
        .map_err(|e| HaError::InvalidResponse(format!("websocket send: {e}")))
}

/// Next text frame as JSON; control and binary frames are skipped.
async fn next_json(socket: &mut Socket) -> HaResult<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Close(_))) | None => {
                return Err(HaError::InvalidResponse("websocket closed".to_string()));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                return Err(HaError::InvalidResponse(format!("websocket read: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_supervisor_core() {
        assert_eq!(
            websocket_url("http://supervisor/core").as_deref(),
            Some("ws://supervisor/core/websocket")
        );
        assert_eq!(
            websocket_url("http://supervisor/core/").as_deref(),
            Some("ws://supervisor/core/websocket")
        );
    }

    #[test]
    fn websocket_url_host_and_port() {
        assert_eq!(
            websocket_url("http://host:8123").as_deref(),
            Some("ws://host:8123/api/websocket")
        );
    }

    #[test]
    fn websocket_url_skips_https() {
        assert_eq!(websocket_url("https://ha.example.com"), None);
    }

    /// Scan #12, #31: plaintext only where the LAN, the Supervisor and the overlay keep it
    #[test]
    fn plaintext_token_policy_table() {
        let cases = [
            ("ws://supervisor/core/websocket", true),
            ("ws://homeassistant.local:8123/api/websocket", true),
            ("ws://ha:8123/api/websocket", true),
            ("ws://127.0.0.1:8123/api/websocket", true),
            ("ws://localhost:8123/api/websocket", true),
            ("ws://[::1]:8123/api/websocket", true),
            ("ws://10.0.0.5:8123/api/websocket", true),
            ("ws://172.20.1.9:8123/api/websocket", true),
            ("ws://192.168.1.10:8123/api/websocket", true),
            ("ws://169.254.10.2:8123/api/websocket", true),
            ("ws://[fd00::5]:8123/api/websocket", true),
            ("ws://[fe80::5]:8123/api/websocket", true),
            // NetBird / Tailscale overlay, 100.64.0.0/10
            ("ws://100.64.0.1:8123/api/websocket", true),
            ("ws://100.98.32.20:8123/api/websocket", true),
            ("ws://100.127.255.255:8123/api/websocket", true),
            ("ws://peer.netbird.cloud:8123/api/websocket", true),
            ("ws://ha.tailnet.ts.net:8123/api/websocket", true),
            ("ws://HA.TS.NET:8123/api/websocket", true),
            ("ws://ha.example.com:8123/api/websocket", false),
            ("ws://203.0.113.7:8123/api/websocket", false),
            ("ws://[2001:db8::7]:8123/api/websocket", false),
            ("ws://100.63.255.255:8123/api/websocket", false),
            ("ws://100.128.0.1:8123/api/websocket", false),
            ("ws://notts.net:8123/api/websocket", false),
        ];
        for (url, allowed) in cases {
            let result = ensure_token_transport(url);
            assert_eq!(result.is_ok(), allowed, "{url}: {result:?}");
            if let Err(err) = result {
                assert!(
                    err.to_string()
                        .contains("use an https:// Home Assistant URL"),
                    "{url}: {err}"
                );
            }
        }
    }

    #[test]
    fn encrypted_and_non_websocket_urls_are_not_checked() {
        assert!(ensure_token_transport("wss://ha.example.com/api/websocket").is_ok());
        assert!(ensure_token_transport("https://ha.example.com").is_ok());
    }
}
