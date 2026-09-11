//! `rdc audit-view`: a small web server that collects audit entries from `rdc serve`
//! instances (and local audit files) and shows them live in a browser.
//!
//! Same gate as the daemon: it listens on the Tailscale IP only, checks the `Host` header, and
//! identifies every caller through tailscaled `whois` against an allowlist. Daemons post JSON
//! lines to `/v1/ingest`; browsers load `/` and follow `/v1/events` (server-sent events).

mod follow;
#[cfg(test)]
mod tests;

use crate::config::{AuditConfig, ResolvedGrant};
use crate::proto::{Identity, RdcError};
use crate::server::audit::{Audit, Entry, sanitize};
use crate::server::auth::{self, Allowlist, Auth, Gate, HostAllow};
use crate::server::plan_listen;
use crate::tailscale::Tailscale;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Entries kept in memory for the page's initial load.
const RING: usize = 5000;
/// Largest accepted `/v1/ingest` body.
const MAX_INGEST: usize = 8 << 20;
/// Longest single line accepted from a sender.
const MAX_LINE: usize = 8192;

pub struct ViewOpts {
    pub bind: Option<IpAddr>,
    pub port: u16,
    pub grants: Vec<ResolvedGrant>,
    pub hosts: Vec<String>,
    pub dev_loopback: bool,
    /// Where received entries are kept, or `None` for memory only.
    pub store: Option<AuditConfig>,
    pub follow: Vec<PathBuf>,
}

/// Everything the viewer has seen: a bounded ring for newcomers, a broadcast for live pages,
/// and the store file.
pub struct Log {
    ring: Mutex<VecDeque<Entry>>,
    tx: broadcast::Sender<Arc<str>>,
    store: Audit,
    node: String,
}

impl Log {
    pub fn new(store: Audit, node: &str) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { ring: Mutex::new(VecDeque::with_capacity(RING)), tx, store, node: node.to_string() }
    }

    pub fn push(&self, e: Entry) {
        let mut e = e.sanitized();
        if e.host.is_empty() {
            e.host = self.node.clone();
        }
        self.store.record(e.clone());
        let line = match serde_json::to_string(&e) {
            Ok(l) => l,
            Err(_) => return,
        };
        if let Ok(mut r) = self.ring.lock() {
            if r.len() == RING {
                r.pop_front();
            }
            r.push_back(e);
        }
        let _ = self.tx.send(line.into());
    }

    pub fn recent(&self, n: usize, host: Option<&str>) -> Vec<Entry> {
        let Ok(r) = self.ring.lock() else { return vec![] };
        r.iter()
            .rev()
            .filter(|e| host.is_none_or(|h| e.host == h))
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }
}

#[derive(Clone)]
pub struct ViewerState {
    pub auth: Arc<Auth>,
    pub log: Arc<Log>,
}

impl Gate for ViewerState {
    fn auth(&self) -> &Auth {
        &self.auth
    }
    fn record(&self, e: Entry) {
        self.log.push(e);
    }
}

pub async fn run(ts: Tailscale, opts: ViewOpts) -> Result<()> {
    let listen = plan_listen(&ts, opts.bind, opts.port, opts.dev_loopback, opts.hosts).await?;
    if opts.grants.is_empty() && !opts.dev_loopback {
        anyhow::bail!(
            "allowlist is empty: set [audit_view].allow in config or pass --allow; nobody could send or view"
        );
    }
    for g in &opts.grants {
        tracing::info!("allow {}", g.who.join(", "));
    }
    let store = match &opts.store {
        Some(cfg) => {
            let a = Audit::open(cfg, &listen.node).context("opening the store file")?;
            if let Some(p) = a.path() {
                tracing::info!("storing received entries in {}", p.display());
            }
            a
        }
        None => {
            tracing::info!("not storing entries (memory only)");
            Audit::disabled()
        }
    };
    let log = Arc::new(Log::new(store, &listen.node));
    for path in opts.follow {
        tracing::info!("following {}", path.display());
        tokio::spawn(follow::follow(path, log.clone()));
    }
    let auth = Auth::new(Arc::new(ts), Allowlist::new(opts.grants), HostAllow::new(listen.hosts), opts.dev_loopback);
    if opts.dev_loopback {
        tracing::warn!("--dev-loopback: requests from 127.0.0.1 are NOT authenticated");
    }
    let state = ViewerState { auth: Arc::new(auth), log };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen.addr).await.with_context(|| format!("bind {}", listen.addr))?;
    tracing::info!("audit viewer at http://{}/  (daemons stream to http://{}/v1/ingest)", listen.addr, listen.addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}

pub fn router(state: ViewerState) -> Router {
    Router::new()
        .route("/", get(index_h))
        .route("/v1/ingest", post(ingest_h).layer(DefaultBodyLimit::max(MAX_INGEST)))
        .route("/v1/recent", get(recent_h))
        .route("/v1/events", get(events_h))
        .route("/health", get(|| async { Json(serde_json::json!({"ok": true})) }))
        .fallback(|| async { auth::error_response(&RdcError::NotFound("no such route".into())) })
        .layer(middleware::from_fn_with_state(state.clone(), auth::middleware::<ViewerState>))
        .with_state(state)
}

async fn index_h() -> Response {
    let mut resp = Html(include_str!("index.html")).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; \
             img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Serialize)]
struct Ingested {
    accepted: usize,
    rejected: usize,
}

async fn ingest_h(State(s): State<ViewerState>, Extension(id): Extension<Identity>, body: Bytes) -> Response {
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(_) => {
            s.log.push(Entry::new(&id.ip, Some(&id), "POST", "/v1/ingest").error(400, "body is not UTF-8"));
            return auth::error_response(&RdcError::BadRequest("body is not UTF-8".into()));
        }
    };
    let (mut accepted, mut rejected) = (0usize, 0usize);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_LINE {
            rejected += 1;
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(mut e) => {
                // Provenance is ours to assert, not the sender's.
                e.via = Some(id.node.clone());
                if e.host.is_empty() {
                    e.host = id.node.clone();
                }
                s.log.push(e);
                accepted += 1;
            }
            Err(_) => rejected += 1,
        }
    }
    if rejected > 0 {
        let why = format!("{rejected} of {} lines were not audit entries", accepted + rejected);
        tracing::warn!(from = %id.node, "{why}");
        s.log.push(Entry::new(&id.ip, Some(&id), "POST", "/v1/ingest").action("ingest").error(400, sanitize(&why)));
        return (StatusCode::BAD_REQUEST, Json(Ingested { accepted, rejected })).into_response();
    }
    Json(Ingested { accepted, rejected }).into_response()
}

#[derive(Deserialize)]
struct RecentQuery {
    n: Option<usize>,
    host: Option<String>,
}

async fn recent_h(State(s): State<ViewerState>, Query(q): Query<RecentQuery>) -> Json<Vec<Entry>> {
    Json(s.log.recent(q.n.unwrap_or(500).min(RING), q.host.as_deref()))
}

async fn events_h(State(s): State<ViewerState>) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let rx = s.log.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(line) => Some((Ok(Event::default().event("entry").data(line.as_ref())), rx)),
            // The page fell behind; tell it to reload the recent window rather than miss rows.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                Some((Ok(Event::default().event("lagged").data(n.to_string())), rx))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
