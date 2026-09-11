//! Append-only JSON-lines audit trail: one line per authorized request or rejection.

use crate::config::AuditConfig;
use crate::proto::Identity;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    /// The machine whose desktop this entry is about (the daemon's node name).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Set by `rdc audit-view` on entries it received over the network: the node that sent them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub peer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub method: String,
    pub path: String,
    /// What was asked for, e.g. `input.click 640,400 Left x1`. Never contains typed text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// `ok`, `denied` (auth or capability) or `error`.
    pub outcome: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

/// Replace control characters (terminal escapes, newlines) so a hostile chord or window title
/// can neither break the JSON-lines framing nor drive the terminal of whoever reads the log.
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\t' { char::REPLACEMENT_CHARACTER } else { c }).take(512).collect()
}

impl Entry {
    pub fn new(peer: &str, id: Option<&Identity>, method: &str, path: &str) -> Self {
        Self {
            ts: now_rfc3339(),
            host: String::new(),
            via: None,
            peer: peer.to_string(),
            login: id.and_then(|i| i.login.clone()),
            node: id.map(|i| i.node.clone()),
            tags: id.map(|i| i.tags.clone()).unwrap_or_default(),
            method: method.to_string(),
            path: sanitize(path),
            action: None,
            outcome: "ok".into(),
            status: 200,
            detail: None,
            ms: None,
        }
    }

    pub fn action(mut self, a: impl AsRef<str>) -> Self {
        self.action = Some(sanitize(a.as_ref()));
        self
    }

    pub fn denied(mut self, status: u16, why: impl AsRef<str>) -> Self {
        self.outcome = "denied".into();
        self.status = status;
        self.detail = Some(sanitize(why.as_ref()));
        self
    }

    pub fn error(mut self, status: u16, why: impl AsRef<str>) -> Self {
        self.outcome = "error".into();
        self.status = status;
        self.detail = Some(sanitize(why.as_ref()));
        self
    }

    pub fn took(mut self, started: std::time::Instant) -> Self {
        self.ms = Some(started.elapsed().as_millis() as u64);
        self
    }

    /// Re-sanitize every string field. Used for entries that arrived over the network, whose
    /// sender may not be an honest `rdc serve`.
    pub fn sanitized(mut self) -> Self {
        for f in [&mut self.ts, &mut self.host, &mut self.peer, &mut self.method, &mut self.path, &mut self.outcome] {
            *f = sanitize(f);
        }
        for v in
            [&mut self.via, &mut self.login, &mut self.node, &mut self.action, &mut self.detail].into_iter().flatten()
        {
            *v = sanitize(v);
        }
        self.tags = self.tags.iter().take(32).map(|t| sanitize(t)).collect();
        self
    }
}

struct Sink {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    keep: u32,
    written: u64,
}

pub struct Audit {
    sink: Option<Mutex<Sink>>,
    path: Option<PathBuf>,
    /// Stamped into entries that do not name a host yet.
    host: String,
    stream: Option<super::stream::Streamer>,
}

impl Audit {
    pub fn disabled() -> Self {
        Self { sink: None, path: None, host: String::new(), stream: None }
    }

    /// Open the audit file named by `cfg`. `host` is this machine's node name; it is written into
    /// every entry so a viewer collecting several machines can tell them apart.
    pub fn open(cfg: &AuditConfig, host: &str) -> anyhow::Result<Self> {
        if !cfg.enabled {
            return Ok(Self { host: host.to_string(), ..Self::disabled() });
        }
        let path = cfg.resolved_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = open_private(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            sink: Some(Mutex::new(Sink {
                path: path.clone(),
                file,
                max_bytes: cfg.max_size_mb.max(1) * 1024 * 1024,
                keep: cfg.keep,
                written,
            })),
            path: Some(path),
            host: host.to_string(),
            stream: None,
        })
    }

    /// Also send every entry, as JSON lines, to an HTTP endpoint. Best effort: the file stays
    /// the record; the stream is batched, retried, and dropped when the endpoint stays down.
    pub fn stream_to(&mut self, url: &str, token: Option<&str>) -> anyhow::Result<()> {
        self.stream = Some(super::stream::Streamer::spawn(url, token)?);
        Ok(())
    }

    /// Flush what the streamer still holds. Call once before exiting.
    pub async fn shutdown(&self) {
        if let Some(s) = &self.stream {
            s.shutdown().await;
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(&self, mut e: Entry) {
        if e.host.is_empty() {
            e.host = self.host.clone();
        }
        let line = match serde_json::to_string(&e) {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!("audit: serialize failed: {err}");
                return;
            }
        };
        if let Some(stream) = &self.stream {
            stream.push(line.clone());
        }
        let Some(sink) = &self.sink else { return };
        let Ok(mut s) = sink.lock() else { return };
        let mut line = line;
        line.push('\n');
        if s.written + line.len() as u64 > s.max_bytes
            && let Err(err) = s.rotate()
        {
            tracing::warn!("audit: rotate failed: {err}");
        }
        if let Err(err) = s.file.write_all(line.as_bytes()) {
            tracing::warn!("audit: write failed: {err}");
        } else {
            s.written += line.len() as u64;
        }
    }
}

impl Sink {
    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let rotated = |n: u32| PathBuf::from(format!("{}.{n}", self.path.display()));
        if self.keep == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            let _ = std::fs::remove_file(rotated(self.keep));
            for n in (1..self.keep).rev() {
                let _ = std::fs::rename(rotated(n), rotated(n + 1));
            }
            std::fs::rename(&self.path, rotated(1))?;
        }
        self.file = open_private(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// Open (or create) the audit file for appending, readable by the owner only.
fn open_private(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        // `mode` only applies at creation; tighten an existing file too.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// Read the last `n` entries of an audit file (skipping unparseable lines).
pub fn tail(path: &Path, n: usize) -> anyhow::Result<Vec<Entry>> {
    let f = File::open(path)?;
    let lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
    Ok(lines
        .iter()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect::<Vec<Entry>>()
        .into_iter()
        .rev()
        .collect())
}

fn now_rfc3339() -> String {
    let d = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let (secs, millis) = (d.as_secs() as i64, d.subsec_millis());
    // Civil-from-days (Howard Hinnant), avoids pulling in a date crate.
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("rdc-audit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        let cfg =
            AuditConfig { enabled: true, path: Some(path.clone()), max_size_mb: 1, keep: 2, ..Default::default() };
        let a = Audit::open(&cfg, "studio-mac").unwrap();
        // Force a tiny cap so rotation triggers.
        a.sink.as_ref().unwrap().lock().unwrap().max_bytes = 400;
        for i in 0..6 {
            a.record(Entry::new("100.64.0.1", None, "POST", "/v1/act").action(format!("input.key f{i}")));
        }
        assert!(path.exists());
        assert!(dir.join("audit.jsonl.1").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(dir.join("audit.jsonl.1")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!dir.join("audit.jsonl.3").exists());
        let last = tail(&path, 10).unwrap();
        assert!(!last.is_empty());
        assert!(last.last().unwrap().action.as_deref().unwrap().starts_with("input.key"));
        assert_eq!(last[0].host, "studio-mac");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_lines_without_host_still_parse() {
        let line = r#"{"ts":"2026-09-01T00:00:00.000Z","peer":"100.64.0.2","method":"GET","path":"/v1/state","outcome":"ok","status":200}"#;
        let e: Entry = serde_json::from_str(line).unwrap();
        assert!(e.host.is_empty() && e.via.is_none());
    }

    #[test]
    fn sanitized_cleans_every_field() {
        let mut e = Entry::new("100.64.0.1", None, "GET", "/v1/state");
        e.host = "mac\u{1b}[2J".into();
        e.via = Some("x\ny".into());
        e.tags = vec!["tag:\u{7}a".into()];
        let e = e.sanitized();
        assert_eq!(e.host, "mac\u{fffd}[2J");
        assert_eq!(e.via.as_deref(), Some("x\u{fffd}y"));
        assert_eq!(e.tags, vec!["tag:\u{fffd}a".to_string()]);
    }

    #[test]
    fn control_characters_are_neutralised() {
        let e = Entry::new("100.64.0.1", None, "POST", "/v1/act").action("input.key \u{1b}]52;c;evil\u{7}");
        let a = e.action.unwrap();
        assert!(!a.contains('\u{1b}') && !a.contains('\u{7}'));
        assert!(a.starts_with("input.key "));
    }

    #[test]
    fn timestamp_shape() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 24, "{t}");
        assert!(t.ends_with('Z') && &t[4..5] == "-" && &t[10..11] == "T");
    }
}
