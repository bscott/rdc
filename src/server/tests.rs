//! Router-level tests: the real axum stack with a fake desktop and a fake identity table.
//! These cover the security gate end to end without a network, a display or tailscaled.

use super::audit::{Audit, Entry, tail};
use super::auth::{Allowlist, Auth, HostAllow, Identify};
use super::{AppState, routes};
use crate::config::{AuditConfig, ResolvedGrant};
use crate::desktop::Desktop;
use crate::proto::*;
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// Records what the daemon was asked to do.
#[derive(Default)]
struct FakeDesktop {
    actions: Mutex<Vec<String>>,
}

#[async_trait]
impl Desktop for FakeDesktop {
    async fn displays(&self) -> Result<Vec<Display>> {
        Ok(vec![Display {
            id: 1,
            name: "fake".into(),
            rect: Rect { x: 0, y: 0, w: 1440, h: 960 },
            scale: 2.0,
            primary: true,
        }])
    }
    async fn screenshot(&self, req: ScreenshotReq) -> Result<Screenshot> {
        Ok(Screenshot {
            format: req.format,
            width: 720,
            height: 480,
            rect: Rect { x: 0, y: 0, w: 1440, h: 960 },
            data: vec![1, 2, 3],
        })
    }
    async fn windows(&self) -> Result<Vec<Window>> {
        Ok(vec![])
    }
    async fn focus(&self, target: WindowTarget) -> Result<()> {
        self.actions.lock().unwrap().push(format!("focus {target:?}"));
        Ok(())
    }
    async fn input(&self, action: InputAction) -> Result<()> {
        self.actions.lock().unwrap().push(Action::Input(action).describe());
        Ok(())
    }
    async fn clipboard_get(&self) -> Result<String> {
        Ok("clip".into())
    }
    async fn clipboard_set(&self, text: String) -> Result<()> {
        self.actions.lock().unwrap().push(format!("clipboard.set {text}"));
        Ok(())
    }
}

/// IP → identity table standing in for tailscaled.
struct FakeTailnet(HashMap<IpAddr, Identity>);

#[async_trait]
impl Identify for FakeTailnet {
    async fn whois(&self, ip: IpAddr) -> std::result::Result<Identity, RdcError> {
        self.0.get(&ip).cloned().ok_or_else(|| RdcError::Unauthorized(format!("{ip} is not a tailnet peer")))
    }
}

fn ident(login: Option<&str>, node: &str, tags: &[&str]) -> Identity {
    Identity {
        login: login.map(String::from),
        node: node.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ip: String::new(),
        caps: vec![],
    }
}

const ALICE: &str = "100.64.0.10"; // full control
const MONITOR: &str = "100.64.0.20"; // view only, tagged
const STRANGER: &str = "100.64.0.30"; // on the tailnet, not allowed
const OUTSIDE: &str = "203.0.113.5"; // not a Tailscale address

struct Harness {
    app: Router,
    desktop: Arc<FakeDesktop>,
    audit_path: PathBuf,
}

fn harness(name: &str) -> Harness {
    let mut table = HashMap::new();
    table.insert(ALICE.parse().unwrap(), ident(Some("alice@example.com"), "alice-laptop", &[]));
    table.insert(MONITOR.parse().unwrap(), ident(None, "monitor-1", &["tag:monitor"]));
    table.insert(STRANGER.parse().unwrap(), ident(Some("mallory@example.com"), "mallory-pc", &[]));
    let view: BTreeSet<Capability> = [Capability::View].into_iter().collect();
    let grants = vec![
        ResolvedGrant::full(vec!["alice@example.com".into()]),
        ResolvedGrant { who: vec!["tag:monitor".into()], caps: view },
    ];
    let hosts = HostAllow::new(vec!["100.64.0.1".into(), "studio-mac.example.ts.net".into()]);
    let auth = Auth::new(Arc::new(FakeTailnet(table)), Allowlist::new(grants), hosts, false);
    let dir = std::env::temp_dir().join(format!("rdc-router-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let audit_path = dir.join("audit.jsonl");
    let audit =
        Audit::open(&AuditConfig { enabled: true, path: Some(audit_path.clone()), max_size_mb: 1, keep: 1 }).unwrap();
    let desktop = Arc::new(FakeDesktop::default());
    let state = AppState { desktop: desktop.clone(), auth: Arc::new(auth), audit: Arc::new(audit) };
    Harness { app: routes::router(state), desktop, audit_path }
}

async fn call(
    h: &Harness,
    peer: &str,
    host: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header("host", host);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let mut req = req.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 40000)));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn audit_lines(h: &Harness) -> Vec<Entry> {
    tail(&h.audit_path, 100).unwrap_or_default()
}

const CLICK: &str = r#"{"kind":"input","type":"click","x":10,"y":10}"#;

#[tokio::test]
async fn wrong_host_is_421_before_identity() {
    let h = harness("host");
    let (st, body) = call(&h, ALICE, "attacker.example.com:7770", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::MISDIRECTED_REQUEST, "{body}");
    let (st, _) = call(&h, ALICE, "studio-mac.example.ts.net:7770", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&h, ALICE, "[fd7a:115c:a1e0::1]:7770", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::MISDIRECTED_REQUEST, "unlisted IPv6 literal must not pass");
    let denied: Vec<_> = audit_lines(&h).into_iter().filter(|e| e.status == 421).collect();
    assert_eq!(denied.len(), 2);
    assert_eq!(denied[0].outcome, "denied");
    assert!(denied[0].login.is_none(), "no identity is resolved before the host check");
}

#[tokio::test]
async fn non_tailscale_and_unknown_peers_are_refused() {
    let h = harness("peers");
    let (st, body) = call(&h, OUTSIDE, "100.64.0.1", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("not a Tailscale address"));
    let (st, body) = call(&h, "100.64.0.99", "100.64.0.1", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let (st, body) = call(&h, STRANGER, "100.64.0.1", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("not in the allowlist"));
    let (st, _) = call(&h, "127.0.0.1", "100.64.0.1", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "loopback is refused without --dev-loopback");
    assert_eq!(audit_lines(&h).iter().filter(|e| e.outcome == "denied").count(), 4);
    assert!(h.desktop.actions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn capabilities_gate_each_route() {
    let h = harness("caps");
    // view-only, tagged monitor
    let (st, _) = call(&h, MONITOR, "100.64.0.1", "GET", "/v1/state", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&h, MONITOR, "100.64.0.1", "GET", "/v1/screenshot", None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(&h, MONITOR, "100.64.0.1", "POST", "/v1/act", Some(CLICK)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("may not use `input`"));
    let (st, _) = call(&h, MONITOR, "100.64.0.1", "GET", "/v1/clipboard", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, body) = call(&h, MONITOR, "100.64.0.1", "GET", "/v1/whoami", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("\"caps\":[\"view\"]"), "{body}");
    assert!(h.desktop.actions.lock().unwrap().is_empty(), "a denied action never reaches the desktop");
    // full control
    let (st, _) = call(&h, ALICE, "100.64.0.1", "POST", "/v1/act", Some(CLICK)).await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(&h, ALICE, "100.64.0.1", "GET", "/v1/clipboard", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("clip"));
    let actions = h.desktop.actions.lock().unwrap().clone();
    assert_eq!(actions, vec!["input.click 10,10 Left x1".to_string()]);
    let forbidden: Vec<_> = audit_lines(&h).into_iter().filter(|e| e.status == 403).collect();
    assert_eq!(forbidden.len(), 2);
    assert_eq!(forbidden[0].node.as_deref(), Some("monitor-1"));
    assert!(forbidden[0].login.is_none(), "tagged devices carry no login");
}

#[tokio::test]
async fn screenshot_returns_geometry_headers() {
    let h = harness("shot");
    let mut req = Request::builder()
        .method("GET")
        .uri("/v1/screenshot?display=primary&format=jpg&max=800")
        .header("host", "100.64.0.1")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ALICE.parse().unwrap(), 1)));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-rdc-rect").unwrap(), "0,0,1440,960");
    assert_eq!(resp.headers().get("x-rdc-size").unwrap(), "720,480");
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/jpeg");
    let (st, body) = call(&h, ALICE, "100.64.0.1", "GET", "/v1/screenshot?display=zzz", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn everything_authorized_is_audited() {
    let h = harness("audit");
    call(&h, ALICE, "100.64.0.1", "GET", "/health", None).await;
    call(&h, ALICE, "100.64.0.1", "GET", "/v1/whoami", None).await;
    let (st, _) = call(&h, ALICE, "100.64.0.1", "GET", "/v1/nowhere", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&h, ALICE, "100.64.0.1", "POST", "/v1/act", Some("{not json")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&h, ALICE, "100.64.0.1", "GET", "/v1/screenshot?format=bmp", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(
        &h,
        ALICE,
        "100.64.0.1",
        "POST",
        "/v1/act",
        Some(r#"{"kind":"input","type":"type","text":"secret password"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let lines = audit_lines(&h);
    let actions: Vec<String> = lines.iter().map(|e| e.action.clone().unwrap_or_default()).collect();
    assert_eq!(lines.len(), 6, "{actions:?}");
    assert_eq!(lines[0].action.as_deref(), Some("health"));
    assert_eq!(lines[1].action.as_deref(), Some("whoami"));
    assert_eq!((lines[2].status, lines[2].outcome.as_str()), (404, "error"));
    assert_eq!((lines[3].status, lines[3].action.as_deref()), (400, Some("act (unparsed)")));
    assert_eq!(lines[4].status, 400);
    assert_eq!(lines[5].action.as_deref(), Some("input.type 15 chars"), "typed text is never logged");
    let raw = std::fs::read_to_string(&h.audit_path).unwrap();
    assert!(!raw.contains("secret password"));
}

#[tokio::test]
async fn hostile_values_cannot_break_the_audit_file() {
    // The fake desktop accepts any chord, so the hostile string reaches the audit log as the
    // action description. It must come out as one line with no terminal control sequences.
    let h = harness("hostile");
    let chord = "\u{1b}]52;c;ZXZpbA==\u{7}\nfake line";
    let body = serde_json::json!({"kind":"input","type":"key","chord": chord}).to_string();
    let (st, _) = call(&h, ALICE, "100.64.0.1", "POST", "/v1/act", Some(&body)).await;
    assert_eq!(st, StatusCode::OK);
    let raw = std::fs::read_to_string(&h.audit_path).unwrap();
    assert_eq!(raw.lines().count(), 1, "one request, one line");
    assert!(!raw.contains('\u{1b}') && !raw.contains('\u{7}'), "{raw}");
    let entries = audit_lines(&h);
    assert!(entries[0].action.as_deref().unwrap().contains("fake line"));
}
