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

//! Peer allowlist for the web listener (port 8099).
//!
//! The listener has no caller authentication. In the add-on, only the Supervisor ingress proxy
//! should reach it, plus loopback (the upgrader's status poll and Tor's forward to `/mobile/`).
//! Standalone installs are not filtered; they bind to loopback by default instead.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tracing::{info, warn};

/// Source address of the Supervisor ingress proxy on the hassio Docker network
const INGRESS_PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 32, 2));

/// Read-only paths that loopback may GET or HEAD: the upgrader's status poll (`calm.rs`) and the
/// health check. Everything else from loopback is refused, except under `/mobile/` (Tor forwards there).
const LOOPBACK_READ_PATHS: &[&str] = &["/api/upgrader/status", "/health"];

/// Upper bound on distinct denied peers that are logged; later denials are not logged
const MAX_LOGGED_PEERS: usize = 64;

/// How the listener decides which peers may call it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessMode {
    /// Running as a Home Assistant add-on (`SUPERVISOR_TOKEN` set)
    AddOn {
        /// Extra peers from `FLUXION_TRUSTED_PEERS`, allowed for every request
        trusted_peers: Vec<IpAddr>,
    },
    /// Running outside the add-on: no peer filtering
    Standalone,
}

impl AccessMode {
    /// Pick the mode from the environment
    #[must_use]
    pub fn from_env() -> Self {
        if std::env::var_os("SUPERVISOR_TOKEN").is_none() {
            return Self::Standalone;
        }
        let (trusted_peers, rejected) =
            parse_trusted_peers(&std::env::var("FLUXION_TRUSTED_PEERS").unwrap_or_default());
        for entry in &rejected {
            warn!(
                "Ignoring FLUXION_TRUSTED_PEERS entry {:?}: not an IP address",
                entry
            );
        }
        info!(
            "Trusted peers: {} from FLUXION_TRUSTED_PEERS",
            trusted_peers.len()
        );
        Self::AddOn { trusted_peers }
    }
}

/// Split a comma-separated list of IP addresses. Returns the addresses and the entries that are
/// not addresses.
#[must_use]
pub fn parse_trusted_peers(raw: &str) -> (Vec<IpAddr>, Vec<String>) {
    let mut peers = Vec::new();
    let mut rejected = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        match entry.parse::<IpAddr>() {
            Ok(ip) => peers.push(ip),
            Err(_) => rejected.push(entry.to_owned()),
        }
    }
    (peers, rejected)
}

/// Whether a request from `peer` may proceed.
///
/// Add-on: the ingress proxy and trusted peers may do anything; loopback may only GET/HEAD the
/// read-only paths in `LOOPBACK_READ_PATHS` and use any method under `/mobile/`. Standalone:
/// everything is allowed.
#[must_use]
pub fn peer_allowed(mode: &AccessMode, peer: IpAddr, method: &Method, path: &str) -> bool {
    match mode {
        AccessMode::Standalone => true,
        AccessMode::AddOn { trusted_peers } => {
            if peer == INGRESS_PEER || trusted_peers.contains(&peer) {
                return true;
            }
            if !peer.is_loopback() {
                return false;
            }
            let read = *method == Method::GET || *method == Method::HEAD;
            (read && LOOPBACK_READ_PATHS.contains(&path)) || path.starts_with("/mobile/")
        }
    }
}

/// Shared state of the peer guard
#[derive(Clone, Debug)]
pub struct PeerGuard {
    mode: AccessMode,
    logged: Arc<Mutex<HashSet<IpAddr>>>,
}

impl PeerGuard {
    #[must_use]
    pub fn new(mode: AccessMode) -> Self {
        Self {
            mode,
            logged: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

/// Middleware: reject requests from peers that `peer_allowed` does not admit
pub async fn enforce(
    State(guard): State<PeerGuard>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if peer_allowed(
        &guard.mode,
        peer.ip(),
        request.method(),
        request.uri().path(),
    ) {
        return next.run(request).await;
    }

    {
        let mut logged = guard.logged.lock();
        if logged.len() < MAX_LOGGED_PEERS && logged.insert(peer.ip()) {
            warn!(
                "Refused {:?} {:?} from peer {}: not the ingress proxy, loopback or a trusted peer",
                request.method().as_str(),
                request.uri().path(),
                peer.ip()
            );
        }
    }
    (StatusCode::FORBIDDEN, "forbidden\n").into_response()
}

/// Address the listener binds to. The add-on listens on all interfaces because ingress reaches it
/// over the Docker network. Standalone binds to loopback unless `FLUXION_BIND_ADDR` overrides it.
///
/// # Errors
/// Returns an error when `FLUXION_BIND_ADDR` is not an IP address.
pub fn bind_ip(
    mode: &AccessMode,
    bind_addr: Option<&str>,
) -> Result<IpAddr, std::net::AddrParseError> {
    match (mode, bind_addr.map(str::trim).filter(|s| !s.is_empty())) {
        // bind-reviewed: add-on only; reached via Supervisor ingress (172.30.32.2), peers filtered by peer_guard
        (AccessMode::AddOn { .. }, _) => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        (AccessMode::Standalone, Some(addr)) => addr.parse(),
        (AccessMode::Standalone, None) => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::ConnectInfo,
        http::{Request as HttpRequest, StatusCode},
        middleware,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn addon() -> AccessMode {
        AccessMode::AddOn {
            trusted_peers: vec!["10.9.8.7".parse().unwrap()],
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn trusted_peer_list_keeps_addresses_and_reports_the_rest() {
        let (peers, rejected) = parse_trusted_peers(" 10.0.0.5, bad ,::1,,192.168.1.0/24 ");
        assert_eq!(peers, vec![ip("10.0.0.5"), ip("::1")]);
        assert_eq!(
            rejected,
            vec!["bad".to_owned(), "192.168.1.0/24".to_owned()]
        );
        assert_eq!(parse_trusted_peers(""), (Vec::new(), Vec::new()));
    }

    #[test]
    fn addon_ingress_may_do_anything() {
        let ingress = ip("172.30.32.2");
        assert!(peer_allowed(
            &addon(),
            ingress,
            &Method::POST,
            "/api/config/update"
        ));
        assert!(peer_allowed(
            &addon(),
            ingress,
            &Method::PUT,
            "/api/user-control/enabled"
        ));
        assert!(peer_allowed(
            &addon(),
            ingress,
            &Method::GET,
            "/api/backtest/day/2026-10-01"
        ));
    }

    #[test]
    fn addon_loopback_reads_only_upgrader_status_and_health() {
        let lo = ip("127.0.0.1");
        assert!(peer_allowed(
            &addon(),
            lo,
            &Method::GET,
            "/api/upgrader/status"
        ));
        assert!(peer_allowed(&addon(), lo, &Method::HEAD, "/health"));
        assert!(peer_allowed(
            &addon(),
            ip("::1"),
            &Method::GET,
            "/api/upgrader/status"
        ));
        assert!(!peer_allowed(&addon(), lo, &Method::GET, "/api/config"));
        assert!(!peer_allowed(
            &addon(),
            lo,
            &Method::GET,
            "/api/backtest/day/2026-10-01"
        ));
        assert!(!peer_allowed(
            &addon(),
            ip("::1"),
            &Method::GET,
            "/api/config"
        ));
        assert!(!peer_allowed(
            &addon(),
            lo,
            &Method::POST,
            "/api/upgrader/status"
        ));
        assert!(!peer_allowed(
            &addon(),
            lo,
            &Method::POST,
            "/api/config/update"
        ));
        assert!(!peer_allowed(
            &addon(),
            lo,
            &Method::PUT,
            "/api/user-control/enabled"
        ));
        assert!(!peer_allowed(
            &addon(),
            lo,
            &Method::DELETE,
            "/api/plugins/x"
        ));
        assert!(!peer_allowed(
            &addon(),
            ip("::1"),
            &Method::POST,
            "/api/config/update"
        ));
    }

    #[test]
    fn addon_loopback_may_use_any_method_under_mobile() {
        let lo = ip("127.0.0.1");
        assert!(peer_allowed(
            &addon(),
            lo,
            &Method::POST,
            "/mobile/api/control"
        ));
        assert!(peer_allowed(
            &addon(),
            lo,
            &Method::GET,
            "/mobile/api/state"
        ));
        assert!(peer_allowed(
            &addon(),
            ip("::1"),
            &Method::POST,
            "/mobile/api/control"
        ));
    }

    #[test]
    fn addon_other_peers_are_denied() {
        let other = ip("172.30.33.5");
        assert!(!peer_allowed(&addon(), other, &Method::GET, "/api/config"));
        assert!(!peer_allowed(
            &addon(),
            other,
            &Method::POST,
            "/api/config/update"
        ));
        assert!(!peer_allowed(
            &addon(),
            other,
            &Method::POST,
            "/mobile/api/control"
        ));
    }

    #[test]
    fn addon_trusted_peers_may_do_anything() {
        let trusted = ip("10.9.8.7");
        assert!(peer_allowed(
            &addon(),
            trusted,
            &Method::POST,
            "/api/config/update"
        ));
        assert!(peer_allowed(
            &addon(),
            trusted,
            &Method::DELETE,
            "/api/plugins/x"
        ));
    }

    #[test]
    fn standalone_allows_everything() {
        let standalone = AccessMode::Standalone;
        assert!(peer_allowed(
            &standalone,
            ip("172.30.33.5"),
            &Method::POST,
            "/api/config/update"
        ));
        assert!(peer_allowed(
            &standalone,
            ip("203.0.113.9"),
            &Method::DELETE,
            "/api/plugins/x"
        ));
    }

    #[test]
    fn bind_addr_is_all_interfaces_in_addon_and_loopback_standalone() {
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(bind_ip(&addon(), None).unwrap(), any);
        assert_eq!(bind_ip(&addon(), Some("127.0.0.1")).unwrap(), any);
        assert_eq!(
            bind_ip(&AccessMode::Standalone, None).unwrap(),
            ip("127.0.0.1")
        );
        assert_eq!(
            bind_ip(&AccessMode::Standalone, Some("  ")).unwrap(),
            ip("127.0.0.1")
        );
        assert_eq!(
            bind_ip(&AccessMode::Standalone, Some(&any.to_string())).unwrap(),
            any
        );
        assert!(bind_ip(&AccessMode::Standalone, Some("not-an-ip")).is_err());
    }

    async fn ok() -> &'static str {
        "ok"
    }

    /// Stub router with the guard applied, as start_web_server applies it to the full app
    fn guarded(mode: AccessMode) -> Router {
        Router::new()
            .route("/api/config/update", post(ok))
            .route("/mobile/api/control", post(ok))
            .route("/api/config", get(ok))
            .layer(middleware::from_fn_with_state(
                PeerGuard::new(mode),
                enforce,
            ))
    }

    async fn status_from(app: Router, peer: &str, method: &str, path: &str) -> StatusCode {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip(peer), 40000)));
        app.oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn guard_refuses_denied_peer_on_config_update_and_admits_ingress() {
        let denied = status_from(
            guarded(addon()),
            "172.30.33.5",
            "POST",
            "/api/config/update",
        )
        .await;
        assert_eq!(denied, StatusCode::FORBIDDEN);

        let ingress = status_from(
            guarded(addon()),
            "172.30.32.2",
            "POST",
            "/api/config/update",
        )
        .await;
        assert_eq!(ingress, StatusCode::OK);

        let loopback_post =
            status_from(guarded(addon()), "127.0.0.1", "POST", "/api/config/update").await;
        assert_eq!(loopback_post, StatusCode::FORBIDDEN);

        let loopback_get = status_from(guarded(addon()), "127.0.0.1", "GET", "/api/config").await;
        assert_eq!(loopback_get, StatusCode::FORBIDDEN);

        let loopback_mobile =
            status_from(guarded(addon()), "127.0.0.1", "POST", "/mobile/api/control").await;
        assert_eq!(loopback_mobile, StatusCode::OK);

        let standalone = status_from(
            guarded(AccessMode::Standalone),
            "172.30.33.5",
            "POST",
            "/api/config/update",
        )
        .await;
        assert_eq!(standalone, StatusCode::OK);
    }
}
