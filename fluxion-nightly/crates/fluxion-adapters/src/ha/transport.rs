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

//! Where the Home Assistant access token may travel. Shared by the REST client and the websocket
//! session, so the two cannot drift apart.

use crate::ha::errors::{HaError, HaResult};
use reqwest::Url;
use std::net::IpAddr;

/// Refuses a Home Assistant URL that would send the access token unencrypted, or to a host the
/// URL does not name (scan #12, #31, N4, N5). Used by `HomeAssistantClient::new` and by the
/// websocket session before any network attempt.
pub fn ensure_token_transport(url: &str) -> HaResult<()> {
    let parsed = Url::parse(url).ok();
    if parsed.as_ref().is_some_and(plaintext_token_allowed) {
        return Ok(());
    }
    let host = parsed
        .as_ref()
        .and_then(Url::host_str)
        .unwrap_or("an unparseable address");
    Err(HaError::ConfigError(format!(
        "refusing Home Assistant URL for {host}: the access token would cross the network unencrypted or to another host; use an https:// Home Assistant URL with no user or password in it"
    )))
}

/// Whether the access token may be sent to this URL. `https` and `wss` always pass. `http` and
/// `ws` pass only to a plaintext host the LAN, the Supervisor or the overlay keeps private (see
/// `plaintext_host_allowed`). A URL with userinfo is refused for every scheme.
pub fn plaintext_token_allowed(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" | "wss" => true,
        "http" | "ws" => url.host_str().is_some_and(plaintext_host_allowed),
        _ => false,
    }
}

/// Plaintext stays allowed for loopback, RFC1918 and link-local addresses, `.local` and
/// single-label names (the Supervisor's `supervisor`).
fn plaintext_host_allowed(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_refused_for_every_scheme() {
        for url in [
            "ws://ha.local:8123@evil.com/api/websocket",
            "ws://user:pw@192.168.1.5:8123/api/websocket",
            "http://user@192.168.1.5:8123",
            "https://user:pw@ha.example.com",
            "wss://user:pw@ha.example.com/api/websocket",
            // password without a user name: only the password half of the check refuses these
            "http://:pw@192.168.1.5:8123/",
            "ws://:pw@host/api/websocket",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert!(!plaintext_token_allowed(&parsed), "{url}");
            assert!(ensure_token_transport(url).is_err(), "{url}");
        }
    }

    #[test]
    fn unknown_schemes_and_garbage_are_refused() {
        assert!(ensure_token_transport("ftp://192.168.1.5/").is_err());
        assert!(ensure_token_transport("not a url").is_err());
    }

    #[test]
    fn encrypted_urls_pass() {
        assert!(ensure_token_transport("https://ha.example.com").is_ok());
        assert!(ensure_token_transport("wss://ha.example.com/api/websocket").is_ok());
    }
}
