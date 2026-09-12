//! Refuse to serve a desktop as root, or drop to a dedicated user right after the listener is
//! bound.
//!
//! rdc never needs root: the port is unprivileged and everything else it does happens inside a
//! desktop session. If someone starts it as root anyway (a system unit, `sudo` out of habit),
//! the choice is: exit, or become the configured `[serve].user` for the rest of the process's
//! life. The decision is made by [`plan`], which is pure so it can be tested; [`apply`] does
//! the irreversible part and never touches the process environment (mutating `HOME` inside a
//! multi-threaded runtime is undefined behaviour); paths that depend on the account are derived
//! from its passwd entry instead, see [`Account::state_dir`].
//!
//! What a dedicated user can actually see is platform-bound. On X11 a separate account can
//! capture and drive the session once it has `DISPLAY` and an `XAUTHORITY` it may read. On
//! Wayland and macOS the compositor / WindowServer only talk to the logged-in user, so the
//! only thing the drop buys there is that the daemon never *serves* as root. The docs say so.

use anyhow::Result;
use std::path::PathBuf;

/// Who we are now, as far as this module cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub uid: u32,
    pub name: String,
}

impl Current {
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

/// A resolved passwd entry for the account to drop to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl Account {
    /// Where this account's rdc state (audit log) lives, computed from its home directory
    /// rather than from `HOME`/`XDG_STATE_HOME`, which still describe the invoking user.
    pub fn state_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/rdc")
        } else {
            self.home.join(".local/state/rdc")
        }
    }
}

/// What to do before accepting the first request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Already an ordinary user; nothing to do.
    Keep,
    /// Switch to this account (only ever chosen when running as root).
    DropTo(Account),
}

/// Decide, without touching the process. `target` is `[serve].user` / `--user`; `lookup`
/// resolves a name to a passwd entry (injected so the decision table is testable).
pub fn plan_with(current: &Current, target: Option<&str>, lookup: impl Fn(&str) -> Result<Account>) -> Result<Plan> {
    let target = target.map(str::trim).filter(|t| !t.is_empty());
    match (current.is_root(), target) {
        (false, None) => Ok(Plan::Keep),
        (false, Some(t)) if t == current.name => Ok(Plan::Keep),
        (false, Some(t)) => anyhow::bail!(
            "[serve].user = {t:?} but rdc is running as {} and cannot switch users without root; \
             start it as {t} directly, or remove the setting",
            current.name
        ),
        (true, None) => anyhow::bail!(
            "rdc serve refuses to run as root: it does not need it, and a remote-control surface should not \
             hold it. Run it as the desktop user, or set [serve].user (or --user) to the account it should \
             drop to after binding the port."
        ),
        (true, Some(t)) => {
            let acct = lookup(t)?;
            if acct.uid == 0 {
                anyhow::bail!("[serve].user = {t:?} is uid 0; rdc refuses to serve a desktop as root");
            }
            Ok(Plan::DropTo(acct))
        }
    }
}

/// [`plan_with`] using the real passwd database.
pub fn plan(current: &Current, target: Option<&str>) -> Result<Plan> {
    plan_with(current, target, lookup)
}

/// Exit early if this process is root (used where no drop target can apply).
pub fn refuse_root() -> Result<()> {
    plan_with(&current(), None, lookup).map(|_| ())
}

#[cfg(unix)]
pub fn current() -> Current {
    let uid = unsafe { libc::getuid() };
    let name = unix::name_of(uid).unwrap_or_else(|| uid.to_string());
    Current { uid, name }
}

#[cfg(not(unix))]
pub fn current() -> Current {
    Current { uid: 1000, name: std::env::var("USERNAME").unwrap_or_else(|_| "user".into()) }
}

#[cfg(unix)]
pub fn lookup(name: &str) -> Result<Account> {
    unix::lookup(name)
}

#[cfg(not(unix))]
pub fn lookup(name: &str) -> Result<Account> {
    anyhow::bail!("[serve].user = {name:?} is only supported on Linux and macOS")
}

/// Carry out `plan`. After `DropTo` returns, the process is that account irrevocably: real,
/// effective and saved ids are set and supplementary groups replaced. The environment is left
/// alone; callers derive account-specific paths from the [`Account`].
pub fn apply(p: &Plan) -> Result<()> {
    match p {
        Plan::Keep => Ok(()),
        #[cfg(unix)]
        Plan::DropTo(acct) => unix::drop_to(acct),
        #[cfg(not(unix))]
        Plan::DropTo(acct) => anyhow::bail!("dropping to {} is only supported on Linux and macOS", acct.name),
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::Context;
    use std::ffi::{CStr, CString};

    pub fn lookup(name: &str) -> Result<Account> {
        let cname = CString::new(name).context("user name contains a NUL byte")?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwnam_r writes into buffers we own and sized; `out` is checked for NULL.
        let rc = unsafe {
            libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out)
        };
        if rc != 0 {
            anyhow::bail!("getpwnam({name}): {}", std::io::Error::from_raw_os_error(rc));
        }
        if out.is_null() {
            anyhow::bail!("no such user: {name}");
        }
        let home = unsafe { CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned();
        Ok(Account { name: name.to_string(), uid: pwd.pw_uid, gid: pwd.pw_gid, home: PathBuf::from(home) })
    }

    pub fn name_of(uid: libc::uid_t) -> Option<String> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: as above.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out) };
        if rc != 0 || out.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
    }

    fn errno(what: &str) -> anyhow::Error {
        anyhow::anyhow!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn drop_to(acct: &Account) -> Result<()> {
        if acct.uid == 0 {
            anyhow::bail!("{} is uid 0; refusing", acct.name);
        }
        let cuser = CString::new(acct.name.as_str())?;
        // Order matters: groups while still root, then gid, then uid last. glibc/libSystem
        // apply the id change to every thread of the process, so it is safe to do this after
        // the async runtime has started its workers.
        // SAFETY: plain libc calls with values from a passwd entry we just resolved.
        unsafe {
            if libc::initgroups(cuser.as_ptr(), acct.gid as _) != 0 {
                return Err(errno("initgroups"));
            }
            if libc::setgid(acct.gid) != 0 {
                return Err(errno("setgid"));
            }
            if libc::setuid(acct.uid) != 0 {
                return Err(errno("setuid"));
            }
            // Prove it stuck: regaining root must now be impossible.
            if libc::setuid(0) == 0 || libc::geteuid() != acct.uid || libc::getegid() != acct.gid {
                anyhow::bail!("privilege drop to {} did not take; refusing to continue", acct.name);
            }
        }
        tracing::info!("dropped privileges to {} (uid {}, gid {})", acct.name, acct.uid, acct.gid);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Current {
        Current { uid: 1000, name: "alice".into() }
    }
    fn root() -> Current {
        Current { uid: 0, name: "root".into() }
    }
    fn fake(name: &str) -> Result<Account> {
        match name {
            "rdc" => Ok(Account { name: "rdc".into(), uid: 999, gid: 999, home: "/home/rdc".into() }),
            "root" => Ok(Account { name: "root".into(), uid: 0, gid: 0, home: "/root".into() }),
            _ => anyhow::bail!("no such user: {name}"),
        }
    }

    #[test]
    fn ordinary_user_keeps_going() {
        assert_eq!(plan_with(&me(), None, fake).unwrap(), Plan::Keep);
        assert_eq!(plan_with(&me(), Some("alice"), fake).unwrap(), Plan::Keep);
        assert_eq!(plan_with(&me(), Some("  "), fake).unwrap(), Plan::Keep);
    }

    #[test]
    fn ordinary_user_cannot_switch() {
        let e = plan_with(&me(), Some("rdc"), fake).unwrap_err().to_string();
        assert!(e.contains("cannot switch users without root"), "{e}");
    }

    #[test]
    fn root_must_name_a_real_non_root_target() {
        let e = plan_with(&root(), None, fake).unwrap_err().to_string();
        assert!(e.contains("refuses to run as root"), "{e}");
        assert!(matches!(plan_with(&root(), Some("rdc"), fake).unwrap(), Plan::DropTo(a) if a.uid == 999));
        assert!(plan_with(&root(), Some("root"), fake).unwrap_err().to_string().contains("uid 0"));
        assert!(plan_with(&root(), Some("nobody-here"), fake).is_err());
    }

    #[test]
    fn state_dir_follows_the_target_home() {
        let a = Account { name: "rdc".into(), uid: 999, gid: 999, home: "/home/rdc".into() };
        let d = a.state_dir();
        assert!(d.starts_with("/home/rdc"), "{d:?}");
        assert!(d.ends_with("rdc"));
    }

    #[cfg(unix)]
    #[test]
    fn lookup_resolves_root_and_rejects_unknown() {
        assert_eq!(unix::lookup("root").unwrap().uid, 0);
        assert!(unix::lookup("rdc-no-such-user-xyz").is_err());
        assert_eq!(unix::name_of(0).as_deref(), Some("root"));
    }
}
