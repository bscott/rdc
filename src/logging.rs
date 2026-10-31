//! Where log lines go, and — when privileges are about to be dropped — when the file is opened.
//!
//! `--log-file` is normally opened at startup, which is fine when rdc runs as the user that
//! will keep running it. It is not fine when `rdc serve` is about to drop privileges: the
//! documented setup has the *target* account owning the log directory, so a root process
//! opening a path there creates a root-owned file, and follows any symlink that account has
//! put in its place. Either way root has touched a file chosen by a less privileged account.
//!
//! So when a drop is coming, the path is remembered and nothing is opened. Lines written in the
//! meantime go to stderr, and [`open_deferred`] opens the file once the process is the target
//! account — at which point the open is subject to that account's own permissions, and a
//! symlink to somewhere it cannot write simply fails.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

enum Target {
    Stderr,
    File(std::fs::File),
}

/// The process's log destination, swappable once.
pub struct Sink {
    target: Mutex<Target>,
    /// `Some` while an open is still owed, i.e. the drop has not happened yet.
    pending: Mutex<Option<PathBuf>>,
}

static SINK: OnceLock<&'static Sink> = OnceLock::new();

/// Open with the same restrictions used everywhere rdc creates a file: owner-only, append.
fn open_private(path: &Path) -> io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

impl Sink {
    /// Open the file now.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self { target: Mutex::new(Target::File(open_private(path)?)), pending: Mutex::new(None) })
    }

    /// Log to stderr for now, and open `path` later, from [`open_deferred`].
    pub fn deferred(path: &Path) -> Self {
        Self { target: Mutex::new(Target::Stderr), pending: Mutex::new(Some(path.to_path_buf())) }
    }

    /// Open a deferred path, if one is still owed. Returns what was opened.
    pub fn open_deferred(&self) -> io::Result<Option<PathBuf>> {
        let Some(path) = self.pending.lock().unwrap().take() else {
            return Ok(None);
        };
        let file = open_private(&path)?;
        *self.target.lock().unwrap() = Target::File(file);
        Ok(Some(path))
    }
}

impl Write for &Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut *self.target.lock().unwrap() {
            Target::Stderr => io::stderr().write(buf),
            Target::File(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut *self.target.lock().unwrap() {
            Target::Stderr => io::stderr().flush(),
            Target::File(f) => f.flush(),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for &'static Sink {
    type Writer = &'static Sink;
    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

/// Make `sink` the process's log destination. Called once, before the subscriber is built.
pub fn install(sink: Sink) -> &'static Sink {
    let leaked: &'static Sink = Box::leak(Box::new(sink));
    let _ = SINK.set(leaked);
    leaked
}

/// Open the deferred log file, now that privileges have been dropped. `Ok(None)` when there was
/// nothing to open, which is the usual case.
pub fn open_deferred() -> io::Result<Option<PathBuf>> {
    match SINK.get() {
        Some(s) => s.open_deferred(),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rdc-logging-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_deferred_log_file_is_not_opened_until_privileges_are_dropped() {
        let dir = tmp("deferred");
        let path = dir.join("serve.log");
        let sink = Sink::deferred(&path);

        writeln!(&sink, "while still root").unwrap();
        assert!(!path.exists(), "the file must not be created before the drop");

        assert_eq!(sink.open_deferred().unwrap().as_deref(), Some(path.as_path()));
        writeln!(&sink, "after the drop").unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("after the drop"), "{body:?}");
        assert!(!body.contains("while still root"), "early lines belong on stderr, not in the file");
        // Opening again is a no-op: the debt is settled.
        assert_eq!(sink.open_deferred().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_undeferred_log_file_is_opened_straight_away() {
        let dir = tmp("immediate");
        let path = dir.join("serve.log");
        let sink = Sink::open(&path).unwrap();
        assert!(path.exists());
        writeln!(&sink, "first line").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("first line"));
        assert_eq!(sink.open_deferred().unwrap(), None, "nothing was deferred");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("mode");
        let path = dir.join("serve.log");
        let sink = Sink::deferred(&path);
        sink.open_deferred().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
