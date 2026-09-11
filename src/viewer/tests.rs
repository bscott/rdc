//! Viewer router tests: the real stack with a fake identity table.

use super::{Log, ViewerState, router};
use crate::config::{AuditConfig, ResolvedGrant};
use crate::proto::{Identity, RdcError};
use crate::server::audit::{Audit, Entry};
use crate::server::auth::{Allowlist, Auth, HostAllow, Identify};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

struct Table(HashMap<IpAddr, Identity>);

#[async_trait]
impl Identify for Table {
    async fn whois(&self, ip: IpAddr) -> Result<Identity, RdcError> {
        self.0.get(&ip).cloned().ok_or_else(|| RdcError::Unauthorized(format!("{ip} unknown")))
    }
}

const MAC: &str = "100.64.0.10"; // a daemon that is allowed to send
const ADMIN: &str = "100.64.0.11"; // the person viewing
const STRANGER: &str = "100.64.0.30";

fn ident(login: Option<&str>, node: &str) -> Identity {
    Identity { login: login.map(String::from), node: node.into(), tags: vec![], ip: String::new(), caps: vec![] }
}

struct H {
    app: Router,
    log: Arc<Log>,
    store: PathBuf,
}

fn harness(name: &str) -> H {
    let mut t = HashMap::new();
    t.insert(MAC.parse().unwrap(), ident(Some("brian@example.com"), "studio-mac"));
    t.insert(ADMIN.parse().unwrap(), ident(Some("brian@example.com"), "laptop"));
    t.insert(STRANGER.parse().unwrap(), ident(Some("mallory@example.com"), "mallory-pc"));
    let auth = Auth::new(
        Arc::new(Table(t)),
        Allowlist::new(vec![ResolvedGrant::full(vec!["brian@example.com".into()])]),
        HostAllow::new(vec!["100.64.0.1".into()]),
        false,
    );
    let dir = std::env::temp_dir().join(format!("rdc-viewer-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = dir.join("audit-view.jsonl");
    let audit = Audit::open(&AuditConfig { path: Some(store.clone()), ..Default::default() }, "laptop").unwrap();
    let log = Arc::new(Log::new(audit, "laptop"));
    let state = ViewerState { auth: Arc::new(auth), log: log.clone() };
    H { app: router(state), log, store }
}

async fn call(h: &H, peer: &str, method: &str, path: &str, body: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header("host", "100.64.0.1");
    if body.is_some() {
        req = req.header("content-type", "application/x-ndjson");
    }
    let mut req = req.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 5000)));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn line(host: &str, action: &str) -> String {
    let mut e = Entry::new("100.64.0.99", None, "POST", "/v1/act").action(action);
    e.host = host.into();
    serde_json::to_string(&e).unwrap()
}

#[tokio::test]
async fn ingest_stamps_provenance_and_serves_recent() {
    let h = harness("ingest");
    let mut rx = h.log.subscribe();
    let body = format!("{}\n{}\n\n", line("studio-mac", "input.click 1,1 Left x1"), line("", "screenshot all png"));
    let (st, resp) = call(&h, MAC, "POST", "/v1/ingest", Some(&body)).await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    assert_eq!(resp, r#"{"accepted":2,"rejected":0}"#);
    let (st, recent) = call(&h, ADMIN, "GET", "/v1/recent?n=10", None).await;
    assert_eq!(st, StatusCode::OK);
    let got: Vec<Entry> = serde_json::from_str(&recent).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].host, "studio-mac");
    assert_eq!(got[0].via.as_deref(), Some("studio-mac"), "via is set by the viewer, not the sender");
    assert_eq!(got[1].host, "studio-mac", "an entry without a host is attributed to its sender");
    // live subscribers saw both lines
    let first: Entry = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(first.action.as_deref(), Some("input.click 1,1 Left x1"));
    assert!(rx.try_recv().is_ok());
    // and the store file has them
    let stored = std::fs::read_to_string(&h.store).unwrap();
    assert_eq!(stored.lines().count(), 2);
    // filter by host
    let (_, only) = call(&h, ADMIN, "GET", "/v1/recent?host=nowhere", None).await;
    assert_eq!(only, "[]");
}

#[tokio::test]
async fn a_sender_cannot_forge_its_own_provenance() {
    let h = harness("forge");
    let mut e = Entry::new("1.2.3.4", None, "GET", "/v1/state");
    e.via = Some("laptop".into());
    e.host = "the-admins-box\u{1b}[2J".into();
    let body = serde_json::to_string(&e).unwrap();
    let (st, _) = call(&h, MAC, "POST", "/v1/ingest", Some(&body)).await;
    assert_eq!(st, StatusCode::OK);
    let got = h.log.recent(10, None);
    assert_eq!(got[0].via.as_deref(), Some("studio-mac"));
    assert_eq!(got[0].host, "the-admins-box\u{fffd}[2J", "control characters are neutralised on arrival");
}

#[tokio::test]
async fn garbage_lines_are_counted_and_recorded() {
    let h = harness("garbage");
    let body = format!("{}\nnot json\n{{\"ts\":1}}\n", line("studio-mac", "whoami"));
    let (st, resp) = call(&h, MAC, "POST", "/v1/ingest", Some(&body)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(resp, r#"{"accepted":1,"rejected":2}"#);
    let got = h.log.recent(10, None);
    assert_eq!(got.len(), 2, "the good line plus one entry about the bad ones");
    let meta = got.last().unwrap();
    assert_eq!((meta.outcome.as_str(), meta.status), ("error", 400));
    assert_eq!(meta.host, "laptop", "the viewer's own entries carry the viewer's host");
    assert_eq!(meta.node.as_deref(), Some("studio-mac"));
    let (st, _) = call(&h, MAC, "POST", "/v1/ingest", Some("\u{ff}\u{fe}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn strangers_and_wrong_hosts_are_refused_and_logged() {
    let h = harness("auth");
    let (st, _) = call(&h, STRANGER, "POST", "/v1/ingest", Some(&line("x", "y"))).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = call(&h, STRANGER, "GET", "/", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = call(&h, "203.0.113.9", "GET", "/v1/recent", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let mut req = Request::builder().uri("/").header("host", "evil.example").body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ADMIN.parse().unwrap(), 1)));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
    let got = h.log.recent(10, None);
    assert_eq!(got.len(), 4);
    assert!(got.iter().all(|e| e.outcome == "denied" && e.host == "laptop"));
    assert!(got[0].detail.as_deref().unwrap().contains("mallory-pc"), "{:?}", got[0].detail);
    assert_eq!(got[3].status, 421);
}

#[tokio::test]
async fn page_and_health_for_allowed_viewer() {
    let h = harness("page");
    let mut req = Request::builder().uri("/").header("host", "100.64.0.1").body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ADMIN.parse().unwrap(), 1)));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
    let csp = resp.headers().get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("connect-src 'self'"));
    let html = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned();
    assert!(html.contains("/v1/events") && html.contains("/v1/recent"));
    assert!(!html.contains("innerHTML"), "the page never interprets log content as HTML");
    let (st, body) = call(&h, ADMIN, "GET", "/health", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);
    let (st, _) = call(&h, ADMIN, "GET", "/nope", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn follows_a_local_file_through_rotation() {
    let h = harness("follow");
    let dir = h.store.parent().unwrap().to_path_buf();
    let path = dir.join("local-audit.jsonl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, format!("{}\n", line("", "seeded"))).unwrap();
    let mut rx = h.log.subscribe();
    tokio::spawn(super::follow::follow(path.clone(), h.log.clone()));
    let seeded: Entry = serde_json::from_str(
        &tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(seeded.action.as_deref(), Some("seeded"));
    assert_eq!(seeded.host, "laptop", "local entries without a host get the viewer's");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", line("", "appended")).unwrap();
    }
    let appended: Entry = serde_json::from_str(
        &tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(appended.action.as_deref(), Some("appended"));
    // rotate: a smaller new file is read from the start
    std::fs::write(&path, format!("{}\n", line("", "after-rotate"))).unwrap();
    let rotated: Entry = serde_json::from_str(
        &tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(rotated.action.as_deref(), Some("after-rotate"));
    let _ = std::fs::remove_dir_all(&dir);
}
