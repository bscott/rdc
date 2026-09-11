//! Forward audit entries, as JSON lines, to an HTTP endpoint.
//!
//! Lines are batched (up to `MAX_BATCH` or `FLUSH_AFTER`, whichever comes first) into one
//! `POST` with `Content-Type: application/x-ndjson`. A failed post is retried with backoff
//! while new lines queue behind it; when the queue is full, new lines are dropped and counted.
//! The local file stays the record of truth. This is telemetry, not the audit log.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const QUEUE: usize = 4096;
const MAX_BATCH: usize = 200;
const FLUSH_AFTER: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct Streamer {
    tx: Mutex<Option<mpsc::Sender<String>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
}

impl Streamer {
    pub fn spawn(url: &str, token: Option<&str>) -> anyhow::Result<Self> {
        let url: reqwest::Url = url.parse().map_err(|e| anyhow::anyhow!("audit stream url {url:?}: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            anyhow::bail!("audit stream url must be http or https, got {url}");
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("rdc/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()?;
        let (tx, rx) = mpsc::channel(QUEUE);
        let token = token.map(str::to_owned);
        let task = tokio::spawn(pump(client, url, token, rx));
        Ok(Self { tx: Mutex::new(Some(tx)), task: Mutex::new(Some(task)), dropped: AtomicU64::new(0) })
    }

    /// Queue one JSON line. Never blocks the request path.
    pub fn push(&self, line: String) {
        let Ok(tx) = self.tx.lock() else { return };
        let Some(tx) = tx.as_ref() else { return };
        if tx.try_send(line).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(1000) {
                tracing::warn!("audit stream: endpoint not keeping up, {n} entries dropped so far (file is intact)");
            }
        }
    }

    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Close the queue and give the pump a moment to post what it still holds.
    pub async fn shutdown(&self) {
        let tx = self.tx.lock().ok().and_then(|mut t| t.take());
        drop(tx);
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
        }
    }
}

async fn pump(client: reqwest::Client, url: reqwest::Url, token: Option<String>, mut rx: mpsc::Receiver<String>) {
    let mut pending: Vec<String> = Vec::new();
    let mut closed = false;
    while !closed {
        // Wait for the first line, then gather a batch for a short while.
        match rx.recv().await {
            Some(l) => pending.push(l),
            None => break,
        }
        let deadline = Instant::now() + FLUSH_AFTER;
        while pending.len() < MAX_BATCH {
            match tokio::time::timeout_at(deadline.into(), rx.recv()).await {
                Ok(Some(l)) => pending.push(l),
                Ok(None) => {
                    closed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        let mut backoff = Duration::from_secs(1);
        loop {
            match post(&client, &url, token.as_deref(), &pending).await {
                Ok(()) => {
                    pending.clear();
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        "audit stream: {e}; retrying in {}s ({} lines held)",
                        backoff.as_secs(),
                        pending.len()
                    );
                    if closed {
                        // Shutting down: one retry is all the grace we give.
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
    if !pending.is_empty() {
        let _ = post(&client, &url, token.as_deref(), &pending).await;
    }
}

async fn post(
    client: &reqwest::Client,
    url: &reqwest::Url,
    token: Option<&str>,
    lines: &[String],
) -> anyhow::Result<()> {
    let mut body = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    let mut req = client.post(url.clone()).header(reqwest::header::CONTENT_TYPE, "application/x-ndjson").body(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| anyhow::anyhow!("post to {}: {e}", redact(url)))?;
    if !resp.status().is_success() {
        anyhow::bail!("{} answered {}", redact(url), resp.status());
    }
    Ok(())
}

/// The URL without any userinfo or query, for log lines.
fn redact(u: &reqwest::Url) -> String {
    let mut u = u.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_query(None);
    u.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Received {
        bodies: Mutex<Vec<(HeaderMap, String)>>,
        fail_first: AtomicUsize,
    }

    async fn sink(State(r): State<Arc<Received>>, headers: HeaderMap, body: Bytes) -> axum::http::StatusCode {
        if r.fail_first.load(Ordering::Relaxed) > 0 {
            r.fail_first.fetch_sub(1, Ordering::Relaxed);
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
        }
        r.bodies.lock().unwrap().push((headers, String::from_utf8_lossy(&body).into_owned()));
        axum::http::StatusCode::ACCEPTED
    }

    async fn endpoint(fail_first: usize) -> (String, Arc<Received>) {
        let r = Arc::new(Received { fail_first: AtomicUsize::new(fail_first), ..Default::default() });
        let app = Router::new().route("/ingest", post(sink)).with_state(r.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, r)
    }

    #[tokio::test]
    async fn batches_lines_as_ndjson_with_token() {
        let (url, r) = endpoint(0).await;
        let s = Streamer::spawn(&url, Some("s3cret")).unwrap();
        for i in 0..5 {
            s.push(format!(r#"{{"n":{i}}}"#));
        }
        s.shutdown().await;
        let bodies = r.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "five quick lines make one batch");
        let (h, body) = &bodies[0];
        assert_eq!(h.get("content-type").unwrap(), "application/x-ndjson");
        assert_eq!(h.get("authorization").unwrap(), "Bearer s3cret");
        assert!(h.get("user-agent").unwrap().to_str().unwrap().starts_with("rdc/"));
        assert_eq!(body.lines().count(), 5);
        assert!(body.ends_with('\n'));
        assert_eq!(s.dropped(), 0);
    }

    #[tokio::test]
    async fn retries_a_failed_batch_without_losing_or_duplicating() {
        let (url, r) = endpoint(1).await;
        let s = Streamer::spawn(&url, None).unwrap();
        s.push(r#"{"n":1}"#.into());
        // First post fails, the pump backs off 1 s and posts again.
        let deadline = Instant::now() + Duration::from_secs(5);
        while r.bodies.lock().unwrap().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        s.shutdown().await;
        let bodies = r.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "delivered exactly once after the retry");
        assert_eq!(bodies[0].1, "{\"n\":1}\n");
        assert!(bodies[0].0.get("authorization").is_none());
    }

    #[test]
    fn rejects_non_http_urls() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(Streamer::spawn("file:///tmp/x", None).is_err());
            assert!(Streamer::spawn("not a url", None).is_err());
        });
    }

    #[test]
    fn redact_strips_secrets() {
        let u: reqwest::Url = "https://user:pw@collector.example/ingest?token=abc".parse().unwrap();
        assert_eq!(redact(&u), "https://collector.example/ingest");
    }
}
