//! Identify the peer behind each connection with tailscaled whois and check the allowlist.

use super::{AppState, is_tailscale_ip};
use crate::config::ResolvedGrant;
use crate::proto::{ApiError, Capability, Identity, RdcError};
use crate::tailscale::Tailscale;
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Allowlist {
    grants: Vec<ResolvedGrant>,
}

impl Allowlist {
    pub fn new(grants: Vec<ResolvedGrant>) -> Self {
        let grants = grants
            .into_iter()
            .map(|g| ResolvedGrant {
                who: g.who.into_iter().map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).collect(),
                caps: g.caps,
            })
            .filter(|g| !g.who.is_empty())
            .collect();
        Self { grants }
    }

    /// Everyone listed gets full control.
    #[cfg(test)]
    pub fn simple(entries: Vec<String>) -> Self {
        Self::new(entries.into_iter().map(|e| ResolvedGrant::full(vec![e])).collect())
    }

    /// The union of capabilities from every grant that matches this identity, or `None`.
    pub fn permits(&self, id: &Identity) -> Option<BTreeSet<Capability>> {
        let node = id.node.to_lowercase();
        let login = id.login.as_deref().map(str::to_lowercase);
        let mut caps = BTreeSet::new();
        let mut matched = false;
        for g in &self.grants {
            let hit = g.who.iter().any(|e| {
                e == "*"
                    || login.as_deref() == Some(e.as_str())
                    || node == *e
                    || id.tags.iter().any(|t| t.to_lowercase() == *e)
            });
            if hit {
                matched = true;
                caps.extend(g.caps.iter().copied());
            }
        }
        matched.then_some(caps)
    }
}

/// Reject a request whose identity lacks `cap`.
pub fn require(id: &Identity, cap: Capability) -> Result<(), RdcError> {
    if id.has(cap) {
        Ok(())
    } else {
        Err(RdcError::Forbidden(format!("{} may not use `{cap}` on this machine", id.label())))
    }
}

/// Host names (without port) that requests may be addressed to.
#[derive(Debug, Clone)]
pub struct HostAllow {
    names: HashSet<String>,
}

impl HostAllow {
    pub fn new(names: Vec<String>) -> Self {
        Self { names: names.into_iter().map(|n| normalize_host(&n)).filter(|n| !n.is_empty()).collect() }
    }

    /// `host` is the raw `Host` header value or URI authority, possibly with a port.
    pub fn permits(&self, host: &str) -> bool {
        self.names.contains(&normalize_host(host))
    }
}

/// Lower-case, strip a trailing dot, the port, and IPv6 brackets.
fn normalize_host(raw: &str) -> String {
    let h = raw.trim().to_lowercase();
    let h = h.trim_end_matches('.');
    let h = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if h.matches(':').count() == 1 {
        h.split(':').next().unwrap_or("")
    } else {
        h
    };
    h.trim_end_matches('.').to_string()
}

/// Where identities come from. `Tailscale` in production; tests substitute a table.
#[async_trait]
pub trait Identify: Send + Sync {
    async fn whois(&self, ip: IpAddr) -> Result<Identity, RdcError>;
}

#[async_trait]
impl Identify for Tailscale {
    async fn whois(&self, ip: IpAddr) -> Result<Identity, RdcError> {
        Tailscale::whois(self, ip).await
    }
}

pub struct Auth {
    ts: Arc<dyn Identify>,
    allow: Allowlist,
    hosts: HostAllow,
    dev_loopback: bool,
    cache: Mutex<HashMap<IpAddr, (Instant, Identity)>>,
}

const CACHE_TTL: Duration = Duration::from_secs(30);

impl Auth {
    pub fn new(ts: Arc<dyn Identify>, allow: Allowlist, hosts: HostAllow, dev_loopback: bool) -> Self {
        Self { ts, allow, hosts, dev_loopback, cache: Mutex::new(HashMap::new()) }
    }

    /// Refuse requests whose `Host` names something we are not. This is the one line of
    /// defence against a browser on an allowed machine being pointed at us via DNS rebinding.
    pub fn check_host(&self, host: Option<&str>) -> Result<(), RdcError> {
        match host {
            Some(h) if self.hosts.permits(h) => Ok(()),
            Some(h) => Err(RdcError::Unauthorized(format!("request addressed to unexpected host {h:?}"))),
            None => Err(RdcError::Unauthorized("request has no Host header".into())),
        }
    }

    pub async fn identify(&self, ip: IpAddr) -> Result<Identity, RdcError> {
        if ip.is_loopback() {
            if self.dev_loopback {
                return Ok(Identity {
                    login: Some("dev@loopback".into()),
                    node: "localhost".into(),
                    tags: vec![],
                    ip: ip.to_string(),
                    caps: Capability::ALL.to_vec(),
                });
            }
            return Err(RdcError::Unauthorized(
                "loopback connections are not accepted (use --dev-loopback while testing)".into(),
            ));
        }
        if !is_tailscale_ip(ip) {
            return Err(RdcError::Unauthorized(format!("{ip} is not a Tailscale address")));
        }
        if let Some((at, id)) = self.cache.lock().await.get(&ip)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(id.clone());
        }
        let id = self.ts.whois(ip).await?;
        self.cache.lock().await.insert(ip, (Instant::now(), id.clone()));
        Ok(id)
    }

    pub async fn authorize(&self, ip: IpAddr) -> Result<Identity, RdcError> {
        let mut id = self.identify(ip).await?;
        if self.dev_loopback && ip.is_loopback() {
            return Ok(id);
        }
        match self.allow.permits(&id) {
            Some(caps) => {
                id.caps = caps.into_iter().collect();
                Ok(id)
            }
            None => Err(RdcError::Unauthorized(format!(
                "{} ({}) is not in the allowlist",
                id.login.as_deref().unwrap_or("no-login"),
                id.node
            ))),
        }
    }
}

pub fn error_response(e: &RdcError) -> Response {
    let status = match e {
        RdcError::NotFound(_) => StatusCode::NOT_FOUND,
        RdcError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        RdcError::Permission(_) => StatusCode::SERVICE_UNAVAILABLE,
        RdcError::Unauthorized(_) | RdcError::Forbidden(_) => StatusCode::FORBIDDEN,
        RdcError::BadRequest(_) => StatusCode::BAD_REQUEST,
        RdcError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = ApiError { code: e.code().into(), message: e.message().to_string() };
    (status, axum::Json(body)).into_response()
}

pub async fn middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.to_string()));
    let (method, path) = (req.method().to_string(), req.uri().path().to_string());
    let peer_ip = peer.ip().to_string();
    if let Err(e) = state.auth.check_host(host.as_deref()) {
        tracing::warn!(peer = %peer_ip, error = %e, "rejected");
        state.audit.record(super::audit::Entry::new(&peer_ip, None, &method, &path).denied(421, e.message()));
        return (
            StatusCode::MISDIRECTED_REQUEST,
            axum::Json(ApiError { code: e.code().into(), message: e.message().to_string() }),
        )
            .into_response();
    }
    match state.auth.authorize(peer.ip()).await {
        Ok(id) => {
            tracing::info!(peer = %peer_ip, who = id.label(), method = %method, path = %path, "request");
            req.extensions_mut().insert(id);
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!(peer = %peer_ip, error = %e, "rejected");
            let resp = error_response(&e);
            let entry = super::audit::Entry::new(&peer_ip, None, &method, &path);
            state.audit.record(match e {
                RdcError::Unauthorized(_) | RdcError::Forbidden(_) => entry.denied(resp.status().as_u16(), e.message()),
                _ => entry.error(resp.status().as_u16(), e.message()),
            });
            resp
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(login: Option<&str>, node: &str, tags: &[&str]) -> Identity {
        Identity {
            login: login.map(String::from),
            node: node.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            ip: "100.1.1.1".into(),
            caps: vec![],
        }
    }

    #[test]
    fn allowlist_matches_login_node_tag_and_star() {
        let a = Allowlist::simple(vec!["Alice@github".into(), "studio-mac".into(), "tag:family".into()]);
        assert!(a.permits(&id(Some("alice@github"), "x", &[])).is_some());
        assert!(a.permits(&id(None, "Studio-Mac", &[])).is_some());
        assert!(a.permits(&id(None, "gaming-pc", &["tag:family"])).is_some());
        assert!(a.permits(&id(Some("someone@github"), "other", &["tag:server"])).is_none());
        assert!(Allowlist::simple(vec!["*".into()]).permits(&id(None, "anyone", &[])).is_some());
        assert!(Allowlist::simple(vec![]).permits(&id(Some("a@b"), "n", &[])).is_none());
        // A tagged node never carries a login (see tailscale::identity), so only its tag or
        // node name can match.
        assert!(a.permits(&id(None, "alice-server", &["tag:server"])).is_none());
    }

    #[test]
    fn capabilities_union_across_grants() {
        let view: BTreeSet<Capability> = [Capability::View].into_iter().collect();
        let clip: BTreeSet<Capability> = [Capability::Clipboard].into_iter().collect();
        let a = Allowlist::new(vec![
            ResolvedGrant { who: vec!["monitor-bot".into()], caps: view.clone() },
            ResolvedGrant { who: vec!["tag:ops".into()], caps: clip.clone() },
            ResolvedGrant::full(vec!["alice@example.com".into()]),
        ]);
        assert_eq!(a.permits(&id(None, "monitor-bot", &[])), Some(view.clone()));
        // Matches two grants: capabilities are unioned.
        let both = a.permits(&id(None, "monitor-bot", &["tag:ops"])).unwrap();
        assert_eq!(both, view.union(&clip).copied().collect());
        assert_eq!(a.permits(&id(Some("alice@example.com"), "l", &[])).unwrap().len(), 3);
        let mut who = id(None, "monitor-bot", &[]);
        who.caps = a.permits(&who).unwrap().into_iter().collect();
        assert!(require(&who, Capability::View).is_ok());
        assert!(matches!(require(&who, Capability::Input), Err(RdcError::Forbidden(_))));
    }

    #[test]
    fn host_allow_normalizes() {
        let h =
            HostAllow::new(vec!["Studio-Mac.example.ts.net.".into(), "100.64.0.5".into(), "fd7a:115c:a1e0::1".into()]);
        assert!(h.permits("studio-mac.example.ts.net:7770"));
        assert!(h.permits("STUDIO-MAC.EXAMPLE.TS.NET"));
        assert!(h.permits("100.64.0.5:7770"));
        assert!(h.permits("[fd7a:115c:a1e0::1]:7770"));
        assert!(!h.permits("attacker.example.com:7770"));
        assert!(!h.permits("100.64.0.6"));
        assert!(!h.permits(""));
    }

    #[test]
    fn tailscale_ranges() {
        assert!(is_tailscale_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_ip("100.127.255.255".parse().unwrap()));
        assert!(!is_tailscale_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_ip("10.0.2.113".parse().unwrap()));
        assert!(is_tailscale_ip("fd7a:115c:a1e0::1".parse().unwrap()));
    }
}
