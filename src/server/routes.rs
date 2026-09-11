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

/// Run a capability-gated handler body and write the audit line for it.
async fn audited<T, F>(
    s: &AppState,
    id: &Identity,
    method: &str,
    path: &str,
    cap: Capability,
    action: Option<String>,
    f: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let started = Instant::now();
    let mut entry = Entry::new(&id.ip, Some(id), method, path);
    if let Some(a) = action {
        entry = entry.action(a);
    }
    if let Err(e) = auth::require(id, cap) {
        tracing::warn!(who = id.label(), error = %e, "forbidden");
        s.audit.record(entry.denied(status_of(&e), e.message()).took(started));
        return Err(e);
    }
    let r = f.await;
    match &r {
        Ok(_) => s.audit.record(entry.took(started)),
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
        .layer(middleware::from_fn_with_state(state.clone(), auth::middleware::<AppState>))
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
    Api(audited(&s, &id, "GET", "/v1/state", Capability::View, None, s.desktop.state()).await)
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
    let action = match q.max {
        Some(m) => format!("screenshot {display} {} max={m}", format.ext()),
        None => format!("screenshot {display} {}", format.ext()),
    };
    match audited(&s, &id, "GET", "/v1/screenshot", Capability::View, Some(action), s.desktop.screenshot(req)).await {
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
    Api(audited(&s, &id, "POST", "/v1/act", cap, Some(desc), s.desktop.act(a))
        .await
        .map(|_| serde_json::json!({ "ok": true })))
}

async fn clipboard_h(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Api<serde_json::Value> {
    Api(audited(
        &s,
        &id,
        "GET",
        "/v1/clipboard",
        Capability::Clipboard,
        Some("clipboard.get".into()),
        s.desktop.clipboard_get(),
    )
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
