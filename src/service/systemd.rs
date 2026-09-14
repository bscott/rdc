//! Linux: a systemd --user unit that runs `rdc serve` inside the graphical session.
//!
//! The unit is sandboxed. The daemon needs very little from the machine beyond the desktop
//! session it is controlling: read access to its config file, write access to its state
//! directory (audit log), the session's runtime directory (Wayland, D-Bus, portal sockets),
//! the tailscaled socket, and one TCP listener. Everything else is fenced off so a bug in rdc
//! or in one of its capture/input libraries cannot become a foothold on the rest of the
//! account or the system.
//!
//! The directives are split across two files, and the split is not cosmetic. In a `--user`
//! service, everything that needs a private mount or user namespace — `ProtectSystem`,
//! `ProtectHome`, `PrivateTmp`, and also `ProtectKernelTunables`, `ProtectKernelModules`,
//! `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname` and
//! `CapabilityBoundingSet`, which imply `PrivateUsers=` (see systemd.exec(5)) — fails to start
//! at all when unprivileged user namespaces are unavailable. Those live in a drop-in that is
//! written only when a real probe says namespaces work. The base unit keeps the directives
//! that are pure seccomp/prctl and always apply.

use super::{LABEL, Op, service_binary};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn unit_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("systemd/user"))
}

fn unit_name() -> String {
    format!("{LABEL}.service")
}

/// `<unit dir>/dev.rdc.daemon.service.d/10-sandbox.conf`, relative to a unit directory.
fn dropin_path(dir: &Path) -> PathBuf {
    dir.join(format!("{LABEL}.service.d")).join("10-sandbox.conf")
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("systemctl").arg("--user").args(args).output()?)
}

/// The base unit: only directives that need no namespace of any kind, so this file is always
/// written and always starts.
pub(crate) fn unit_text(bin: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=rdc remote desktop control daemon\n\
         Documentation=https://github.com/bscott/rdc\n\
         After=graphical-session.target tailscaled.service\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         ExecStart={bin} serve\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         Environment=RDC_LOG=info\n\
         \n\
         # --- least privilege (seccomp and prctl only; see the drop-in for the rest) -------\n\
         # Nothing rdc execs (hyprctl, tailscale) may gain privileges.\n\
         NoNewPrivileges=yes\n\
         # Files it creates (audit log, rotated copies) are private to your user.\n\
         UMask=0077\n\
         # Sockets: Wayland/D-Bus/portal/tailscaled (unix) and the HTTP listener (inet).\n\
         RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6\n\
         # No new namespaces, no realtime scheduling, no personality changes, native ABI only.\n\
         RestrictNamespaces=yes\n\
         RestrictRealtime=yes\n\
         RestrictSUIDSGID=yes\n\
         LockPersonality=yes\n\
         SystemCallArchitectures=native\n\
         # The standard service allow-list, minus privileged ops and resource-limit tweaks.\n\
         SystemCallFilter=@system-service\n\
         SystemCallFilter=~@privileged @resources\n\
         SystemCallErrorNumber=EPERM\n\
         # No IPC removal on stop: in a --user unit that would delete every SysV/POSIX IPC\n\
         # object the desktop user owns and can break the session (Xwayland MIT-SHM, PipeWire).\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        bin = bin.display(),
    )
}

/// Everything that needs user or mount namespaces. Written only when [`userns_available`]
/// confirms the kernel allows them, because without them systemd refuses to start the unit.
pub(crate) fn dropin_text(rw: &[PathBuf]) -> String {
    let mut s = String::from(
        "# Namespace sandbox for rdc, written by `rdc service install` only when unprivileged\n\
         # user namespaces work on this kernel. Every directive here needs them: the Protect*\n\
         # ones below and CapabilityBoundingSet imply PrivateUsers= in a --user service.\n\
         #\n\
         # If your desktop needs something this blocks, override it with\n\
         # `systemctl --user edit dev.rdc.daemon` rather than editing this file, so the change\n\
         # survives the next `rdc service install`.\n\
         [Service]\n\
         PrivateUsers=yes\n\
         # rdc never holds capabilities; say so explicitly.\n\
         CapabilityBoundingSet=\n\
         AmbientCapabilities=\n\
         # Kernel and host surfaces the daemon has no business touching.\n\
         ProtectKernelTunables=yes\n\
         ProtectKernelModules=yes\n\
         ProtectKernelLogs=yes\n\
         ProtectControlGroups=yes\n\
         ProtectClock=yes\n\
         ProtectHostname=yes\n\
         # / is read-only, /home and /root are read-only, /tmp and /dev/shm are private.\n\
         ProtectSystem=strict\n\
         ProtectHome=read-only\n\
         PrivateTmp=yes\n\
         # PrivateTmp hides the X11 socket directory, which an X11 session needs to reach the\n\
         # display. A leading '-' skips it on Wayland-only systems where it does not exist.\n\
         BindReadOnlyPaths=-/tmp/.X11-unix\n\
         # Writable: the session runtime dir (%t) and rdc's own state.\n\
         ReadWritePaths=-%t\n",
    );
    for p in rw {
        s.push_str(&format!("ReadWritePaths=-{}\n", p.display()));
    }
    s
}

/// Can this user actually create a user namespace?
///
/// Reading sysctls is not enough: Ubuntu 23.10+ restricts unprivileged user namespaces through
/// an AppArmor profile (`kernel.apparmor_restrict_unprivileged_userns`) that the older knobs do
/// not reflect, and containers add their own seccomp filters. So try it for real. If `unshare`
/// is not installed, fall back to the sysctl heuristic rather than guessing.
fn userns_available() -> bool {
    match Command::new("unshare").args(["-Ur", "true"]).output() {
        Ok(out) => out.status.success(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => sysctls_allow_userns(),
        Err(_) => false,
    }
}

fn sysctls_allow_userns() -> bool {
    for knob in ["/proc/sys/kernel/unprivileged_userns_clone", "/proc/sys/user/max_user_namespaces"] {
        if let Ok(v) = std::fs::read_to_string(knob)
            && v.trim() == "0"
        {
            return false;
        }
    }
    true
}

/// Directories the daemon must be able to write: its state dir and, if the audit log was
/// pointed somewhere else in config, that directory too.
fn writable_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::config::state_dir()];
    if let Ok(cfg) = crate::config::load()
        && let Some(parent) = cfg.serve.audit.resolved_path().parent()
        && !dirs.iter().any(|d| d == parent)
    {
        dirs.push(parent.to_path_buf());
    }
    dirs
}

/// Write the unit, and the drop-in when `sandbox` is true (removing a stale one when it is
/// not). Returns the paths written. Split out from [`run`] so tests can drive it on a temp
/// directory without touching systemd.
pub(crate) fn write_unit_files(dir: &Path, bin: &Path, rw: &[PathBuf], sandbox: bool) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let unit = dir.join(unit_name());
    std::fs::write(&unit, unit_text(bin)).with_context(|| format!("writing {}", unit.display()))?;
    let mut written = vec![unit];
    let dropin = dropin_path(dir);
    if sandbox {
        std::fs::create_dir_all(dropin.parent().unwrap())?;
        std::fs::write(&dropin, dropin_text(rw)).with_context(|| format!("writing {}", dropin.display()))?;
        written.push(dropin);
    } else {
        // A kernel that used to allow namespaces may not any more (a distribution upgrade
        // tightening AppArmor, say). Leaving the old drop-in would wedge the unit.
        remove_unit_files(dir, false)?;
    }
    Ok(written)
}

/// Remove the drop-in (and its directory), and the unit itself when `unit_too`. Returns what
/// was actually removed.
pub(crate) fn remove_unit_files(dir: &Path, unit_too: bool) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let dropin = dropin_path(dir);
    if dropin.exists() {
        std::fs::remove_file(&dropin).with_context(|| format!("removing {}", dropin.display()))?;
        removed.push(dropin.clone());
    }
    if let Some(d) = dropin.parent()
        && d.exists()
    {
        // Only if empty: a user may have added their own drop-in beside ours.
        if std::fs::read_dir(d)?.next().is_none() {
            std::fs::remove_dir(d)?;
            removed.push(d.to_path_buf());
        }
    }
    if unit_too {
        let unit = dir.join(unit_name());
        if unit.exists() {
            std::fs::remove_file(&unit).with_context(|| format!("removing {}", unit.display()))?;
            removed.push(unit);
        }
    }
    Ok(removed)
}

/// `systemctl --user is-active`, polled: a unit that crashes on startup is restarted by
/// `Restart=on-failure`, so it reports `activating` for a while before settling on `failed`.
/// Without this, `enable --now` succeeds and the caller never learns the daemon is looping.
fn wait_until_active(name: &str) -> Result<String> {
    let mut state = String::new();
    for _ in 0..15 {
        let out = systemctl(&["is-active", name])?;
        state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if state == "active" || state == "failed" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(state)
}

pub fn run(op: Op) -> Result<()> {
    let dir = unit_dir()?;
    let name = unit_name();
    match op {
        Op::Install { .. } => {
            let bin = service_binary()?;
            let rw = writable_dirs();
            for d in &rw {
                // Create them now so ReadWritePaths has something to bind and the first audit
                // write does not have to mkdir through a read-only home.
                let _ = std::fs::create_dir_all(d);
            }
            let sandbox = userns_available();
            let written = write_unit_files(&dir, &bin, &rw, sandbox)?;

            systemctl(&["daemon-reload"])?;
            let out = systemctl(&["enable", "--now", &name])?;
            if !out.status.success() {
                anyhow::bail!("systemctl enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            for p in &written {
                println!("wrote {}", p.display());
            }
            if !sandbox {
                println!(
                    "namespace sandbox: skipped — this kernel does not allow unprivileged user namespaces\n\
                     (Ubuntu 23.10+ restricts them via AppArmor). The syscall, capability-escalation and\n\
                     address-family restrictions in the unit still apply."
                );
            }
            let state = wait_until_active(&name)?;
            if state != "active" {
                let status = systemctl(&["status", "--no-pager", "--lines=15", &name])?;
                anyhow::bail!(
                    "{name} did not stay running (systemctl reports {state:?}). The sandbox may be blocking \
                     something this desktop needs; try `systemctl --user edit {name}` to override a directive, \
                     or delete {}.\n\n{}",
                    dropin_path(&dir).display(),
                    String::from_utf8_lossy(&status.stdout).trim()
                );
            }
            println!(
                "{name} is active, running {}\nlogs: journalctl --user -u {name} -f\nsandbox score: systemd-analyze security --user {name}",
                bin.display()
            );
            Ok(())
        }
        Op::Uninstall => {
            let _ = systemctl(&["disable", "--now", &name]);
            for p in remove_unit_files(&dir, true)? {
                println!("removed {}", p.display());
            }
            systemctl(&["daemon-reload"])?;
            println!("removed {name}");
            Ok(())
        }
        Op::Status => {
            let out = systemctl(&["status", "--no-pager", &name])?;
            print!("{}", String::from_utf8_lossy(&out.stdout));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rdc-systemd-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Directives that must never move into the base unit: each one implies `PrivateUsers=` in
    /// a `--user` service, so a kernel without namespaces would refuse to start the unit.
    const NEEDS_NAMESPACES: &[&str] = &[
        "PrivateUsers=",
        "CapabilityBoundingSet=",
        "AmbientCapabilities=",
        "ProtectKernelTunables=",
        "ProtectKernelModules=",
        "ProtectKernelLogs=",
        "ProtectControlGroups=",
        "ProtectClock=",
        "ProtectHostname=",
        "ProtectSystem=",
        "ProtectHome=",
        "PrivateTmp=",
    ];

    #[test]
    fn base_unit_starts_without_namespaces() {
        let u = unit_text(Path::new("/usr/local/bin/rdc"));
        assert!(u.contains("ExecStart=/usr/local/bin/rdc serve\n"));
        for d in ["NoNewPrivileges=yes", "SystemCallFilter=@system-service", "RestrictAddressFamilies=", "UMask=0077"] {
            assert!(u.contains(d), "base unit lost {d}");
        }
        for d in NEEDS_NAMESPACES {
            assert!(!u.contains(d), "{d} needs user namespaces and must live in the drop-in");
        }
        // Would tear down the desktop user's shared memory (Xwayland, PipeWire) on stop.
        assert!(!u.contains("RemoveIPC=yes"));
    }

    #[test]
    fn dropin_carries_the_namespace_directives_and_the_x11_socket() {
        let d = dropin_text(&[PathBuf::from("/home/a/.local/state/rdc")]);
        for x in NEEDS_NAMESPACES {
            assert!(d.contains(x), "drop-in missing {x}");
        }
        // Without this, PrivateTmp hides the X11 socket and an X11 session cannot be reached.
        assert!(d.contains("BindReadOnlyPaths=-/tmp/.X11-unix"));
        assert!(d.contains("ReadWritePaths=-%t\n"));
        assert!(d.contains("ReadWritePaths=-/home/a/.local/state/rdc\n"));
    }

    #[test]
    fn install_without_namespaces_writes_no_dropin() {
        let dir = tmp("no-userns");
        let written = write_unit_files(&dir, Path::new("/usr/local/bin/rdc"), &[], false).unwrap();
        assert_eq!(written.len(), 1, "only the unit should be written");
        assert!(dir.join(unit_name()).exists());
        assert!(!dropin_path(&dir).exists());
        // The unit alone must still be a valid, startable unit.
        let u = std::fs::read_to_string(dir.join(unit_name())).unwrap();
        assert!(u.contains("NoNewPrivileges=yes") && u.contains("[Install]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_removes_a_stale_dropin_when_namespaces_go_away() {
        let dir = tmp("stale");
        write_unit_files(&dir, Path::new("/usr/local/bin/rdc"), &[], true).unwrap();
        assert!(dropin_path(&dir).exists());
        // Same machine, kernel tightened since the last install.
        write_unit_files(&dir, Path::new("/usr/local/bin/rdc"), &[], false).unwrap();
        assert!(!dropin_path(&dir).exists(), "a stale drop-in would wedge the unit");
        assert!(!dropin_path(&dir).parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_removes_the_unit_and_the_dropin() {
        let dir = tmp("uninstall");
        write_unit_files(&dir, Path::new("/usr/local/bin/rdc"), &[], true).unwrap();
        let removed = remove_unit_files(&dir, true).unwrap();
        assert!(!dir.join(unit_name()).exists(), "unit left behind");
        assert!(!dropin_path(&dir).exists(), "drop-in left behind");
        assert!(!dropin_path(&dir).parent().unwrap().exists(), "drop-in directory left behind");
        assert_eq!(removed.len(), 3);
        // Removing twice is not an error: `service uninstall` may be run after a manual cleanup.
        assert!(remove_unit_files(&dir, true).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_keeps_a_dropin_directory_that_has_other_files() {
        let dir = tmp("other-dropin");
        write_unit_files(&dir, Path::new("/usr/local/bin/rdc"), &[], true).unwrap();
        let theirs = dropin_path(&dir).parent().unwrap().join("99-local.conf");
        std::fs::write(&theirs, "[Service]\nEnvironment=RDC_LOG=debug\n").unwrap();
        remove_unit_files(&dir, true).unwrap();
        assert!(theirs.exists(), "someone else's override must not be deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
