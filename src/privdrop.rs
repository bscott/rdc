//! Refuse to serve a desktop with root privileges, or drop to a dedicated account first.
//!
//! rdc never needs root: the port is unprivileged and everything else it does happens inside a
//! desktop session. If someone starts it as root anyway (a system unit, `sudo` out of habit, a
//! binary that someone made setuid), the choice is: exit, or become the account named by
//! `[serve].user` for the rest of the process's life.
//!
//! Both the real and the effective uid matter. A setuid-root binary launched by an ordinary
//! user has `uid != 0` but `euid == 0` and can do everything root can; the reverse (root that
//! has dropped only its effective uid) can regain it with `seteuid`. Either one being 0 counts.
//!
//! [`plan`] decides and touches nothing, so every case is unit-tested without root; [`apply`]
//! does the irreversible part. Neither writes to the environment: mutating `HOME` in a process
//! that has already started its async runtime is undefined behaviour in Rust, and the variable
//! would be wrong anyway (it still describes whoever invoked us). Paths that depend on the
//! account come from its passwd entry instead — see [`Account::state_dir`].
//!
//! On Windows there is no uid; the scheduled task's run level is the equivalent control
//! (`rdc service install`, see docs/setup-windows.md).

use anyhow::Result;
use std::path::PathBuf;

/// Who this process is, as far as this module cares. The ids are `None` on platforms that do
/// not have them, which is why [`Current::is_root`] is false there rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub uid: Option<u32>,
    pub euid: Option<u32>,
    pub name: String,
}

impl Current {
    /// Root if *either* id is 0: real uid 0 is root outright, effective uid 0 has all of root's
    /// authority, and a process holding one can generally recover the other.
    pub fn is_root(&self) -> bool {
        self.uid == Some(0) || self.euid == Some(0)
    }

    /// For `doctor`: `alice (uid 1000)`, `alice (uid 1000, euid 0)` when they differ, or just
    /// `alice` where the platform has no uids.
    pub fn describe(&self) -> String {
        match (self.uid, self.euid) {
            (Some(u), Some(e)) if u != e => format!("{} (uid {u}, euid {e})", self.name),
            (Some(u), _) => format!("{} (uid {u})", self.name),
            (None, Some(e)) => format!("{} (euid {e})", self.name),
            (None, None) => self.name.clone(),
        }
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
    /// Where this account's rdc state (the audit log) belongs, computed from its home directory
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
    /// Switch to this account (only ever chosen when we have root privileges).
    DropTo(Account),
}

/// Decide, without touching the process. `target` is `[serve].user` / `--user`; `lookup`
/// resolves a name to a passwd entry, injected so the whole table is testable.
pub fn plan_with(current: &Current, target: Option<&str>, lookup: impl Fn(&str) -> Result<Account>) -> Result<Plan> {
    let target = target.map(str::trim).filter(|t| !t.is_empty());
    // No uids on this platform: there is nothing to drop from or to.
    if current.uid.is_none() && current.euid.is_none() {
        return match target {
            None => Ok(Plan::Keep),
            Some(t) => anyhow::bail!(
                "[serve].user = {t:?} is not supported on this platform: dropping privileges needs POSIX user ids. \
                 On Windows, control the daemon's privileges with the scheduled task's run level instead — \
                 `rdc service install` registers it at standard integrity (see docs/setup-windows.md)."
            ),
        };
    }
    match (current.is_root(), target) {
        (false, None) => Ok(Plan::Keep),
        (false, Some(t)) if t == current.name => Ok(Plan::Keep),
        (false, Some(t)) => anyhow::bail!(
            "[serve].user = {t:?} but rdc is running as {} and cannot switch users without root privileges; \
             start it as {t} directly, or remove the setting",
            current.name
        ),
        (true, None) => anyhow::bail!(
            "rdc serve refuses to run with root privileges ({}): it does not need them, and a remote-control \
             surface should not hold them. Run it as the desktop user, or set [serve].user (or --user) to the \
             account it should drop to after binding the port.",
            current.describe()
        ),
        (true, Some(t)) => {
            let acct = lookup(t)?;
            if acct.uid == 0 || acct.gid == 0 {
                anyhow::bail!(
                    "[serve].user = {t:?} resolves to uid {} gid {}; rdc refuses to serve a desktop with root \
                     privileges, so the target account must be an ordinary one",
                    acct.uid,
                    acct.gid
                );
            }
            Ok(Plan::DropTo(acct))
        }
    }
}

/// [`plan_with`] using the real passwd database.
pub fn plan(current: &Current, target: Option<&str>) -> Result<Plan> {
    plan_with(current, target, lookup)
}

/// Exit early if this process has root privileges, where no drop target can apply.
pub fn refuse_root() -> Result<()> {
    plan_with(&current(), None, lookup).map(|_| ())
}

#[cfg(unix)]
pub fn current() -> Current {
    // SAFETY: getuid and geteuid take no arguments and cannot fail.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    // Name the real user: that is who invoked us, and it is what `doctor` should show.
    let name = unix::name_of(uid).unwrap_or_else(|| uid.to_string());
    Current { uid: Some(uid), euid: Some(euid), name }
}

#[cfg(not(unix))]
pub fn current() -> Current {
    Current { uid: None, euid: None, name: std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()) }
}

#[cfg(unix)]
pub fn lookup(name: &str) -> Result<Account> {
    unix::lookup(name)
}

#[cfg(not(unix))]
pub fn lookup(name: &str) -> Result<Account> {
    anyhow::bail!("cannot look up user {name:?}: this platform has no passwd database")
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
        Plan::DropTo(acct) => anyhow::bail!("dropping to {} is not supported on this platform", acct.name),
    }
}

/// Give `path` to the account we are about to become, while we still can. Used for files the
/// daemon opened as root (a `--log-file`): the open descriptor keeps working across the drop,
/// but the file on disk would stay root-owned, so rotation or a later run as that account
/// would fail.
#[cfg(unix)]
pub fn chown_to(path: &std::path::Path, acct: &Account) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::chown(c.as_ptr(), acct.uid, acct.gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn chown_to(_path: &std::path::Path, _acct: &Account) -> std::io::Result<()> {
    Ok(())
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
        // SAFETY: getpwnam_r writes only into `pwd` and `buf`, both sized and owned here;
        // `out` is left NULL when there is no such user, which we check before reading `pwd`.
        let rc = unsafe {
            libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out)
        };
        if rc != 0 {
            anyhow::bail!("getpwnam({name}): {}", std::io::Error::from_raw_os_error(rc));
        }
        if out.is_null() {
            anyhow::bail!("no such user: {name}");
        }
        // A passwd entry may legitimately carry a NULL home directory (some NSS backends do);
        // treat it as "no home" rather than dereferencing it.
        let home = if pwd.pw_dir.is_null() {
            PathBuf::new()
        } else {
            // SAFETY: pw_dir points into `buf`, which outlives this read, and is NUL-terminated.
            PathBuf::from(unsafe { CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned())
        };
        Ok(Account { name: name.to_string(), uid: pwd.pw_uid, gid: pwd.pw_gid, home })
    }

    pub fn name_of(uid: libc::uid_t) -> Option<String> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: as in `lookup`.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out) };
        if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        // SAFETY: pw_name points into `buf` and is NUL-terminated.
        Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
    }

    fn errno(what: &str) -> anyhow::Error {
        anyhow::anyhow!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn drop_to(acct: &Account) -> Result<()> {
        if acct.uid == 0 || acct.gid == 0 {
            anyhow::bail!("{} is uid {} gid {}; refusing", acct.name, acct.uid, acct.gid);
        }
        let cuser = CString::new(acct.name.as_str())?;
        // Order matters: supplementary groups while still root, then gid, then uid last.
        // glibc and libSystem apply the id change to every thread of the process, so this is
        // safe after the async runtime has started its workers.
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
        Current { uid: Some(1000), euid: Some(1000), name: "alice".into() }
    }
    fn root() -> Current {
        Current { uid: Some(0), euid: Some(0), name: "root".into() }
    }
    fn no_uids() -> Current {
        Current { uid: None, euid: None, name: "USER".into() }
    }
    fn fake(name: &str) -> Result<Account> {
        Ok(match name {
            "rdc" => Account { name: "rdc".into(), uid: 999, gid: 999, home: "/home/rdc".into() },
            "root" => Account { name: "root".into(), uid: 0, gid: 0, home: "/root".into() },
            "wheelie" => Account { name: "wheelie".into(), uid: 999, gid: 0, home: "/home/wheelie".into() },
            _ => anyhow::bail!("no such user: {name}"),
        })
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
    fn root_must_name_a_target() {
        let e = plan_with(&root(), None, fake).unwrap_err().to_string();
        assert!(e.contains("refuses to run with root privileges"), "{e}");
        assert!(matches!(plan_with(&root(), Some("rdc"), fake).unwrap(), Plan::DropTo(a) if a.uid == 999));
    }

    #[test]
    fn setuid_root_can_still_drop() {
        // Real uid 1000, effective 0: root privileges, so a drop is both possible and required.
        let c = Current { uid: Some(1000), euid: Some(0), name: "alice".into() };
        assert!(c.is_root());
        assert!(plan_with(&c, None, fake).is_err());
        assert!(matches!(plan_with(&c, Some("rdc"), fake).unwrap(), Plan::DropTo(a) if a.uid == 999));
    }

    #[test]
    fn a_privileged_target_is_refused() {
        for (who, why) in [("root", "uid 0"), ("wheelie", "gid 0")] {
            let e = plan_with(&root(), Some(who), fake).unwrap_err().to_string();
            assert!(e.contains("uid 0") || e.contains("gid 0"), "{why}: {e}");
        }
        assert!(plan_with(&root(), Some("nobody-here"), fake).is_err());
    }

    #[test]
    fn platforms_without_uids_say_so() {
        assert_eq!(plan_with(&no_uids(), None, fake).unwrap(), Plan::Keep);
        let e = plan_with(&no_uids(), Some("rdc"), fake).unwrap_err().to_string();
        assert!(e.contains("not supported on this platform"), "{e}");
        assert!(!e.contains("without root"), "the Unix wording would be misleading here: {e}");
    }

    #[test]
    fn state_dir_follows_the_target_home_not_the_environment() {
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
