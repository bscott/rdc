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
    /// Focused window when the request arrived, as `app: title`, so the log shows what the
    /// action was aimed at. Omitted when `[serve.audit].window_titles = false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// SHA-256 (hex) of the image bytes returned by a screenshot request, so a saved screenshot
    /// can be matched to the exact audit line that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_sha256: Option<String>,
}

/// Longest window title kept in the log; titles can carry document names, URLs and mail
/// subjects, and the log is not the place for a whole one.
const MAX_TITLE: usize = 160;

/// `app: title` for one window, truncated to [`MAX_TITLE`] characters.
pub fn describe_window(w: &crate::proto::Window) -> String {
    let mut s = if w.app.is_empty() { w.title.clone() } else { format!("{}: {}", w.app, w.title) };
    if s.chars().count() > MAX_TITLE {
        s = s.chars().take(MAX_TITLE - 1).collect::<String>() + "…";
    }
    s
}

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
            window: None,
            screenshot_sha256: None,
        }
    }

    /// Window titles come from arbitrary applications: sanitize them like any other string
    /// that lands in the log.
    pub fn window(mut self, w: Option<String>) -> Self {
        self.window = w.map(|w| sanitize(&w));
        self
    }

    pub fn screenshot_hash(mut self, bytes: &[u8]) -> Self {
        self.screenshot_sha256 = Some(sha256_hex(bytes));
        self
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
    window_titles: bool,
}

impl Audit {
    pub fn disabled() -> Self {
        Self { sink: None, path: None, window_titles: false }
    }

    /// Whether handlers should look up the focused window for the log.
    pub fn wants_window_titles(&self) -> bool {
        self.sink.is_some() && self.window_titles
    }

    pub fn open(cfg: &AuditConfig) -> anyhow::Result<Self> {
        if !cfg.enabled {
            return Ok(Self::disabled());
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
            window_titles: cfg.window_titles,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(&self, e: Entry) {
        let Some(sink) = &self.sink else { return };
        let Ok(mut s) = sink.lock() else { return };
        let mut line = match serde_json::to_string(&e) {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!("audit: serialize failed: {err}");
                return;
            }
        };
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
    fn sha256_matches_known_vector() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn window_is_described_and_truncated() {
        use crate::proto::{Rect, Window};
        let mk = |app: &str, title: &str| Window {
            id: 1,
            pid: 1,
            app: app.into(),
            title: title.into(),
            rect: Rect { x: 0, y: 0, w: 1, h: 1 },
            focused: true,
            minimized: false,
        };
        assert_eq!(describe_window(&mk("Firefox", "Inbox")), "Firefox: Inbox");
        assert_eq!(describe_window(&mk("", "Untitled")), "Untitled");
        let long = "x".repeat(500);
        let d = describe_window(&mk("Firefox", &long));
        assert_eq!(d.chars().count(), MAX_TITLE);
        assert!(d.ends_with('…'));
    }

    #[test]
    fn window_titles_are_sanitized() {
        let e = Entry::new("100.64.0.1", None, "POST", "/v1/act").window(Some("Term: \u{1b}]0;evil\u{7}".into()));
        let w = e.window.unwrap();
        assert!(!w.contains('\u{1b}') && !w.contains('\u{7}'), "{w:?}");
    }

    #[test]
    fn entry_serializes_new_fields_only_when_set() {
        let e = Entry::new("100.64.0.1", None, "GET", "/v1/screenshot");
        let j = serde_json::to_string(&e).unwrap();
        assert!(!j.contains("window") && !j.contains("screenshot_sha256"));
        let e = e.window(Some("Terminal: ~".into())).screenshot_hash(b"abc");
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("\"window\":\"Terminal: ~\""));
        assert!(j.contains("\"screenshot_sha256\":\"ba7816bf"));
        // Old logs without the fields still parse.
        let old: Entry = serde_json::from_str(
            r#"{"ts":"t","peer":"p","method":"GET","path":"/v1/state","outcome":"ok","status":200}"#,
        )
        .unwrap();
        assert_eq!(old.window, None);
    }

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("rdc-audit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        let cfg = AuditConfig { enabled: true, path: Some(path.clone()), max_size_mb: 1, keep: 2, window_titles: true };
        let a = Audit::open(&cfg).unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
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
