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

/// An authenticated websocket session.
pub struct HaWsSession {
    socket: Socket,
    next_id: u64,
}

impl HaWsSession {
    /// Connect and authenticate: `auth_required` → `auth` → `auth_ok`.
    pub async fn connect(url: &str, token: &str) -> HaResult<Self> {
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
}
