//! Refuse to serve a desktop as root.
//!
//! rdc never needs root: the port is unprivileged and everything else it does happens inside a
//! desktop session. If someone starts it as root anyway (a system unit, `sudo` out of habit),
//! the remote-control surface would run with full privileges for no benefit, so the daemon
//! exits instead. On Windows there is no root; the scheduled task's run level is the
//! equivalent control (`rdc service install`, see docs/setup-windows.md).

use anyhow::Result;

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

/// Decide, without touching the process. Pure so it can be tested.
pub fn check(current: &Current) -> Result<()> {
    if current.is_root() {
        anyhow::bail!(
            "rdc serve refuses to run as root: it does not need it, and a remote-control surface should not \
             hold it. Run it as the desktop user (for example via `rdc service install` from that account)."
        );
    }
    Ok(())
}

/// Exit early if this process is root.
pub fn refuse_root() -> Result<()> {
    check(&current())
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
mod unix {
    use std::ffi::CStr;

    pub fn name_of(uid: libc::uid_t) -> Option<String> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes into buffers we own and sized; `out` is checked for NULL.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out) };
        if rc != 0 || out.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_user_passes() {
        assert!(check(&Current { uid: 1000, name: "alice".into() }).is_ok());
    }

    #[test]
    fn root_is_refused() {
        let e = check(&Current { uid: 0, name: "root".into() }).unwrap_err().to_string();
        assert!(e.contains("refuses to run as root"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn resolves_uid_zero_to_root() {
        assert_eq!(unix::name_of(0).as_deref(), Some("root"));
    }
}
