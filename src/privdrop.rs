//! Refuse to serve a desktop with root privileges.
//!
//! rdc never needs root: the port is unprivileged and everything else it does happens inside a
//! desktop session. If someone starts it as root anyway (a system unit, `sudo` out of habit, a
//! binary that someone made setuid), the remote-control surface would run with full privileges
//! for no benefit, so the daemon exits instead.
//!
//! Both the real and the effective uid matter. A setuid-root binary launched by an ordinary
//! user has `uid != 0` but `euid == 0` and can do everything root can; the reverse (root that
//! has dropped only its effective uid) can regain it with `seteuid`. Either one being 0 counts.
//!
//! On Windows there is no uid; the scheduled task's run level is the equivalent control
//! (`rdc service install`, see docs/setup-windows.md).

use anyhow::Result;

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

/// Decide, without touching the process. Pure so every case is testable.
pub fn check(current: &Current) -> Result<()> {
    if current.is_root() {
        anyhow::bail!(
            "rdc serve refuses to run with root privileges ({}): it does not need them, and a remote-control \
             surface should not hold them. Run it as the desktop user, for example with `rdc service install` \
             from that account.",
            current.describe()
        );
    }
    Ok(())
}

/// Exit early if this process has root privileges.
pub fn refuse_root() -> Result<()> {
    check(&current())
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
mod unix {
    use std::ffi::CStr;

    pub fn name_of(uid: libc::uid_t) -> Option<String> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes only into `pwd` and `buf`, both sized and owned here;
        // `out` is left NULL when there is no such user, which we check before dereferencing.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out) };
        if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        // SAFETY: pw_name points into `buf`, which outlives this read, and is NUL-terminated.
        Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(uid: Option<u32>, euid: Option<u32>) -> Current {
        Current { uid, euid, name: "alice".into() }
    }

    #[test]
    fn ordinary_user_passes() {
        assert!(check(&who(Some(1000), Some(1000))).is_ok());
    }

    #[test]
    fn real_root_is_refused() {
        let e = check(&who(Some(0), Some(0))).unwrap_err().to_string();
        assert!(e.contains("refuses to run with root privileges"), "{e}");
    }

    #[test]
    fn setuid_root_is_refused() {
        // A setuid-root binary run by an ordinary user: real uid 1000, effective uid 0.
        let c = who(Some(1000), Some(0));
        assert!(c.is_root());
        assert!(check(&c).is_err());
        assert_eq!(c.describe(), "alice (uid 1000, euid 0)");
    }

    #[test]
    fn root_with_dropped_euid_is_refused() {
        // Real uid 0 keeps the right to seteuid(0) back, so this is still root.
        let c = who(Some(0), Some(1000));
        assert!(c.is_root());
        assert!(check(&c).is_err());
    }

    #[test]
    fn platform_without_uids_is_not_root_and_prints_only_a_name() {
        let c = who(None, None);
        assert!(!c.is_root());
        assert!(check(&c).is_ok());
        assert_eq!(c.describe(), "alice");
    }

    #[test]
    fn describe_collapses_equal_ids() {
        assert_eq!(who(Some(1000), Some(1000)).describe(), "alice (uid 1000)");
    }

    #[cfg(unix)]
    #[test]
    fn resolves_uid_zero_to_root() {
        assert_eq!(unix::name_of(0).as_deref(), Some("root"));
    }
}
