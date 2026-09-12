//! Linux: a systemd --user unit that runs `rdc serve` inside the graphical session.
//!
//! The unit is written with systemd's sandboxing directives turned on. The daemon needs very
//! little from the machine beyond the desktop session it is controlling: read access to its
//! config file, write access to its state directory (audit log), the session's runtime
//! directory (Wayland, D-Bus, portal sockets), the tailscaled socket, and one TCP listener.
//! Everything else is fenced off so a bug in rdc or in one of its capture/input libraries
//! cannot turn into a foothold on the rest of the account or the system.

use super::{LABEL, Op, service_binary};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn unit_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("systemd/user"))
}

fn unit_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(format!("{LABEL}.service")))
}

/// Drop-in holding the directives that need a private mount namespace.
fn mount_dropin_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(format!("{LABEL}.service.d")).join("10-sandbox-mounts.conf"))
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("systemctl").arg("--user").args(args).output()?)
}

/// The base unit. Every directive here works in a `--user` service without any special kernel
/// support, so this file is always written as-is.
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
         # --- least privilege -------------------------------------------------------\n\
         # The daemon runs as you, inside your session, and needs nothing more than that.\n\
         # Nothing it execs (hyprctl, tailscale) may gain privileges either.\n\
         NoNewPrivileges=yes\n\
         # Files it creates (audit log, rotated logs) are private to your user.\n\
         UMask=0077\n\
         # It never holds capabilities; say so explicitly so a future setuid helper can't add any.\n\
         CapabilityBoundingSet=\n\
         AmbientCapabilities=\n\
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
         # Kernel and host surfaces the daemon has no business touching.\n\
         ProtectKernelTunables=yes\n\
         ProtectKernelModules=yes\n\
         ProtectKernelLogs=yes\n\
         ProtectControlGroups=yes\n\
         ProtectClock=yes\n\
         ProtectHostname=yes\n\
         # No IPC removal on stop: in a --user unit that would delete every SysV/POSIX IPC\n\
         # object the desktop user owns and can break the session (Xwayland MIT-SHM, PipeWire).\n\
         # Directives that need a private mount namespace live in\n\
         # {LABEL}.service.d/10-sandbox-mounts.conf, written only when the kernel allows it.\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        bin = bin.display(),
    )
}

/// Filesystem isolation. In a `--user` service these directives require unprivileged user
/// namespaces; on kernels or distributions that disable them the unit would fail to start with
/// "Failed to set up mount namespacing", so they go in a separate drop-in that is only
/// installed when the check in [`userns_available`] passes.
pub(crate) fn mount_dropin_text(rw: &[PathBuf]) -> String {
    let mut s = String::from(
        "# Filesystem sandbox for rdc. Remove this file (or `systemctl --user edit` the unit)\n\
         # if your desktop needs something not listed here.\n\
         [Service]\n\
         # / is read-only, /home /root /run/user are read-only, /tmp and /dev/shm are private.\n\
         ProtectSystem=strict\n\
         ProtectHome=read-only\n\
         PrivateTmp=yes\n\
         # Only our own state dir (audit log) and the session runtime dir are writable.\n\
         # A leading '-' means: don't fail if the path does not exist yet.\n\
         ReadWritePaths=-%t\n",
    );
    for p in rw {
        s.push_str(&format!("ReadWritePaths=-{}\n", p.display()));
    }
    s
}

/// True when this user may create user namespaces, which systemd needs to build a private
/// mount namespace for a `--user` service.
fn userns_available() -> bool {
    // Debian/Ubuntu kernel knob: 0 disables unprivileged user namespaces entirely.
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        && v.trim() == "0"
    {
        return false;
    }
    // Upstream knob: a limit of 0 has the same effect.
    if let Ok(v) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
        && v.trim() == "0"
    {
        return false;
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

pub fn run(op: Op) -> Result<()> {
    let unit = unit_path()?;
    let dropin = mount_dropin_path()?;
    let name = format!("{LABEL}.service");
    match op {
        Op::Install { .. } => {
            let bin = service_binary()?;
            std::fs::create_dir_all(unit.parent().unwrap())?;
            std::fs::write(&unit, unit_text(&bin)).with_context(|| format!("writing {}", unit.display()))?;

            let rw = writable_dirs();
            for d in &rw {
                // Create them now so ReadWritePaths has something to bind and the first audit
                // write does not need to mkdir through a read-only home.
                let _ = std::fs::create_dir_all(d);
            }
            let sandboxed_fs = userns_available();
            if sandboxed_fs {
                std::fs::create_dir_all(dropin.parent().unwrap())?;
                std::fs::write(&dropin, mount_dropin_text(&rw))
                    .with_context(|| format!("writing {}", dropin.display()))?;
            } else if dropin.exists() {
                std::fs::remove_file(&dropin)?;
            }

            systemctl(&["daemon-reload"])?;
            let out = systemctl(&["enable", "--now", &name])?;
            if !out.status.success() {
                anyhow::bail!("systemctl enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            println!("installed {} → {}", unit.display(), bin.display());
            if sandboxed_fs {
                println!("filesystem sandbox: {}", dropin.display());
            } else {
                println!(
                    "filesystem sandbox: skipped (unprivileged user namespaces are disabled on this kernel);\n\
                     the syscall, capability and address-family restrictions in the unit still apply"
                );
            }
            println!(
                "logs: journalctl --user -u {name} -f\n\
                 sandbox score: systemd-analyze security --user {name}"
            );
            Ok(())
        }
        Op::Uninstall => {
            let _ = systemctl(&["disable", "--now", &name]);
            if unit.exists() {
                std::fs::remove_file(&unit)?;
            }
            if dropin.exists() {
                std::fs::remove_file(&dropin)?;
                let _ = std::fs::remove_dir(dropin.parent().unwrap());
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

    #[test]
    fn unit_has_the_hardening_directives() {
        let u = unit_text(Path::new("/usr/local/bin/rdc"));
        assert!(u.contains("ExecStart=/usr/local/bin/rdc serve\n"));
        for d in [
            "NoNewPrivileges=yes",
            "CapabilityBoundingSet=\n",
            "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
            "SystemCallFilter=@system-service",
            "SystemCallFilter=~@privileged @resources",
            "ProtectKernelTunables=yes",
            "UMask=0077",
        ] {
            assert!(u.contains(d), "missing {d}");
        }
        // Would tear down the desktop user's shared memory (Xwayland, PipeWire) on stop.
        assert!(!u.contains("RemoveIPC=yes"));
        // Mount-namespace directives must not be in the base unit: they live in the drop-in.
        assert!(!u.contains("ProtectSystem="));
        assert!(!u.contains("ProtectHome="));
    }

    #[test]
    fn dropin_lists_writable_paths() {
        let d = mount_dropin_text(&[PathBuf::from("/home/a/.local/state/rdc"), PathBuf::from("/var/log/rdc")]);
        assert!(d.contains("ProtectSystem=strict"));
        assert!(d.contains("ReadWritePaths=-%t\n"));
        assert!(d.contains("ReadWritePaths=-/home/a/.local/state/rdc\n"));
        assert!(d.contains("ReadWritePaths=-/var/log/rdc\n"));
    }
}
