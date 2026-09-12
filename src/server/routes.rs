use super::audit::Entry;
use super::{AppState, auth};
use crate::desktop::remote::{H_RECT, H_SIZE};
use crate::proto::*;
use axum::{
    Json, Router,
    extract::{Extension, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::time::Instant;

fn status_of(e: &RdcError) -> u16 {
    auth::error_response(e).status().as_u16()
}

/// What a handler is doing, for the audit line.
struct Call<'a> {
    method: &'a str,
    path: &'a str,
    cap: Capability,
    action: Option<String>,
    /// Record the focused window *before* the body runs, i.e. the context the action was aimed
    /// at rather than whatever it left behind.
    with_window: bool,
}

/// Run a capability-gated handler body and write the audit line for it. `finish` gets the
/// successful result so a handler can attach result-derived facts (the screenshot hash).
async fn audited<T, F>(
    s: &AppState,
    id: &Identity,
    call: Call<'_>,
    f: F,
    finish: impl FnOnce(&T, Entry) -> Entry,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let started = Instant::now();
    let mut entry = Entry::new(&id.ip, Some(id), call.method, call.path);
    if let Some(a) = call.action {
        entry = entry.action(a);
    }
    if let Err(e) = auth::require(id, call.cap) {
        tracing::warn!(who = id.label(), error = %e, "forbidden");
        s.audit.record(entry.denied(status_of(&e), e.message()).took(started));
        return Err(e);
    }
    if call.with_window && s.audit.wants_window_titles() {
        // Best effort: a failed lookup must never block or fail the request. This is the
        // platform's direct focused-window query, not a full enumeration; see Desktop.
        let t0 = Instant::now();
        let w = s.desktop.focused_window().await.ok().flatten();
        tracing::debug!(us = t0.elapsed().as_micros(), found = w.is_some(), "audit focused-window lookup");
        entry = entry.window(w.as_ref().map(super::audit::describe_window));
    }
    let r = f.await;
    match &r {
        Ok(v) => s.audit.record(finish(v, entry).took(started)),
        Err(e) => s.audit.record(entry.error(status_of(e), e.message()).took(started)),
    }
    r
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/state", get(state_h))
        .route("/v1/screenshot", get(screenshot_h))
        .route("/v1/act", post(act_h))
        .route("/v1/clipboard", get(clipboard_h))
        .route("/v1/whoami", get(whoami_h))
        .route("/health", get(health_h))
        .fallback(not_found_h)
        .layer(middleware::from_fn_with_state(state.clone(), auth::middleware))
        .with_state(state)
}

struct Api<T>(Result<T>);

impl<T: serde::Serialize> IntoResponse for Api<T> {
    fn into_response(self) -> Response {
        match self.0 {
            Ok(v) => Json(v).into_response(),
            Err(e) => auth::error_response(&e),
        }
    }
}

async fn state_h(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Api<crate::proto::State> {
    let call = Call { method: "GET", path: "/v1/state", cap: Capability::View, action: None, with_window: false };
    Api(audited(&s, &id, call, s.desktop.state(), |_, e| e).await)
}

#[derive(Deserialize)]
struct ShotQuery {
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    max: Option<u32>,
}

async fn screenshot_h(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<ShotQuery>,
) -> Response {
    let parsed = (|| -> Result<(DisplayTarget, ImageFormat)> {
        let display = match q.display.as_deref() {
            None => DisplayTarget::All,
            Some(d) => d.parse().map_err(RdcError::BadRequest)?,
        };
        let format = match q.format.as_deref() {
            None | Some("png") => ImageFormat::Png,
            Some("jpg") | Some("jpeg") => ImageFormat::Jpeg,
            Some(o) => return Err(RdcError::BadRequest(format!("bad format {o:?}"))),
        };
        Ok((display, format))
    })();
    let (display, format) = match parsed {
        Ok(v) => v,
        Err(e) => {
            // Validation failures are audited too; the request was authorized, so it counts.
            s.audit.record(
                Entry::new(&id.ip, Some(&id), "GET", "/v1/screenshot")
                    .action(format!("screenshot {:?} {:?}", q.display, q.format))
                    .error(status_of(&e), e.message()),
            );
            return auth::error_response(&e);
        }
    };
    let req = ScreenshotReq { display, format, max_long_edge: q.max };
    let action = format!("screenshot {display} {} max={:?}", format.ext(), q.max);
    let call =
        Call { method: "GET", path: "/v1/screenshot", cap: Capability::View, action: Some(action), with_window: true };
    match audited(&s, &id, call, s.desktop.screenshot(req), |shot, e| e.screenshot_hash(&shot.data)).await {
        Ok(shot) => {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(shot.format.mime()));
            let r = shot.rect;
            h.insert(H_RECT, HeaderValue::from_str(&format!("{},{},{},{}", r.x, r.y, r.w, r.h)).unwrap());
            h.insert(H_SIZE, HeaderValue::from_str(&format!("{},{}", shot.width, shot.height)).unwrap());
            (StatusCode::OK, h, shot.data).into_response()
        }
        Err(e) => auth::error_response(&e),
    }
}

async fn act_h(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    body: std::result::Result<Json<Action>, JsonRejection>,
) -> Api<serde_json::Value> {
    let Json(a) = match body {
        Ok(j) => j,
        Err(rej) => {
            // A body that fails to parse never reaches the desktop, but it was an authorized
            // request and belongs in the audit trail.
            let e = RdcError::BadRequest(format!("invalid action body: {}", rej.body_text()));
            s.audit.record(
                Entry::new(&id.ip, Some(&id), "POST", "/v1/act").action("act (unparsed)").error(400, e.message()),
            );
            return Api(Err(e));
        }
    };
    let cap = match &a {
        Action::Input(_) | Action::Focus(_) => Capability::Input,
        Action::ClipboardSet { .. } => Capability::Clipboard,
    };
    let desc = a.describe();
    let call = Call { method: "POST", path: "/v1/act", cap, action: Some(desc), with_window: true };
    Api(audited(&s, &id, call, s.desktop.act(a), |_, e| e).await.map(|_| serde_json::json!({ "ok": true })))
}

async fn clipboard_h(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Api<serde_json::Value> {
    let call = Call {
        method: "GET",
        path: "/v1/clipboard",
        cap: Capability::Clipboard,
        action: Some("clipboard.get".into()),
        with_window: true,
    };
    Api(audited(&s, &id, call, s.desktop.clipboard_get(), |_, e| e)
        .await
        .map(|text| serde_json::json!({ "text": text })))
}

async fn whoami_h(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Json<Identity> {
    s.audit.record(Entry::new(&id.ip, Some(&id), "GET", "/v1/whoami").action("whoami"));
    Json(id)
}

async fn health_h(State(s): State<AppState>, Extension(id): Extension<Identity>) -> &'static str {
    s.audit.record(Entry::new(&id.ip, Some(&id), "GET", "/health").action("health"));
    "ok"
}

/// Unknown paths reach here only after the auth middleware, so the caller is known.
async fn not_found_h(State(s): State<AppState>, Extension(id): Extension<Identity>, uri: Uri) -> Response {
    let e = RdcError::NotFound(format!("no route {}", uri.path()));
    s.audit.record(Entry::new(&id.ip, Some(&id), "-", uri.path()).error(404, e.message()));
    auth::error_response(&e)
}
