# Linux setup

Verified on Arch Linux with Hyprland (Wayland). Other Wayland compositors and X11 compile and
should work but are untested; please report what you find.

## Wayland

### How rdc talks to the compositor

| Need | Mechanism | Works on |
|---|---|---|
| Screen capture | `org.freedesktop.portal.Screenshot` via xdg-desktop-portal, falling back to `wlr-screencopy` | GNOME, KDE, Hyprland, sway, river… |
| Mouse and keyboard | `wlr-virtual-pointer` + `zwp-virtual-keyboard` protocols | wlroots compositors (Hyprland, sway, river, labwc…) |
| Window list and focus | `hyprctl` when `HYPRLAND_INSTANCE_SIGNATURE` is set | Hyprland only |
| Clipboard | `wlr-data-control` | wlroots compositors and KDE |

GNOME and KDE do not implement the wlr virtual input protocols, so on those desktops **rdc can
take screenshots but cannot move the mouse or type yet**. The input library rdc uses (enigo) has
paths through the RemoteDesktop portal and libei, but rdc does not enable them; wiring and
testing them is tracked as an issue. `rdc doctor` reports the session type and whether input
initialised.

rdc pins exactly one input backend per session (Wayland when `WAYLAND_DISPLAY` is set, X11
otherwise) so events are not delivered twice to Xwayland applications.

Absolute pointer positioning under Wayland is expressed as a fraction of the first output's
mode, which rdc maps from logical desktop coordinates, so clicks land correctly on scaled
displays (verified at 2× on Hyprland).

### Packages

Runtime: a portal backend for your compositor (`xdg-desktop-portal-hyprland`,
`xdg-desktop-portal-gnome`, `xdg-desktop-portal-kde`, or `xdg-desktop-portal-wlr`), PipeWire, and
`tailscaled` running. Build dependencies are listed in [Install](install.md#build-dependencies).

## X11

Capture and input go through xcb/x11rb. Window focus uses the generic xcap window list (no
`_NET_ACTIVE_WINDOW` focus yet, so `focus` is unsupported on X11 for now).

## Running

Put the allowlist in `~/.config/rdc/config.toml` first (see [Configuration](configuration.md));
the service reads it from there.

```sh
rdc doctor
rdc serve                                  # foreground test
rdc service install                        # systemd --user unit dev.rdc.daemon.service
journalctl --user -u dev.rdc.daemon -f
```

The unit is wanted by `graphical-session.target`, so it starts with your desktop session and
restarts on failure. It runs the binary from the path where you invoked `service install`; put
`rdc` somewhere permanent first (`~/.local/bin` or `/usr/local/bin`).

Config lives at `~/.config/rdc/config.toml`; see [Configuration](configuration.md).

### What the unit is allowed to do

`rdc service install` writes a sandboxed unit. The daemon runs as your user inside your session
(it has to, to see the screen and inject input), but systemd fences off everything it does not
need. The directives are split across two files because in a `--user` service they are not all
equally available:

**Always applied**, in `dev.rdc.daemon.service` — these are pure seccomp and prctl settings and
need no namespace:

| Restriction | Effect |
|---|---|
| `NoNewPrivileges`, `RestrictSUIDSGID` | neither rdc nor anything it runs (`hyprctl`, `tailscale`) can gain privileges |
| `SystemCallFilter=@system-service` minus `@privileged @resources`, native ABI only | privileged and resource-limit syscalls return `EPERM` |
| `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` | unix sockets (Wayland, D-Bus, portal, tailscaled) and the HTTP listener; nothing else |
| `RestrictNamespaces`, `RestrictRealtime`, `LockPersonality` | no new namespaces, no realtime scheduling, no personality changes |
| `UMask=0077` | audit log and rotated copies are private to your user |

**Applied when unprivileged user namespaces work**, in
`dev.rdc.daemon.service.d/10-sandbox.conf`:

| Restriction | Effect |
|---|---|
| `PrivateUsers=yes` | every uid but yours maps to `nobody` inside the service |
| empty `CapabilityBoundingSet`/`AmbientCapabilities` | rdc can never hold a capability |
| `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname` | no kernel tunables, modules, logs, cgroups, clock or hostname |
| `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp` | the filesystem is read-only except `$XDG_RUNTIME_DIR`, `~/.local/state/rdc` and the audit log directory |
| `BindReadOnlyPaths="-/tmp/.X11-unix"` | puts the X11 socket back, which `PrivateTmp` would otherwise hide |

Every directive in the second table implies `PrivateUsers=` in a `--user` service (see
`systemd.exec(5)`), so where unprivileged user namespaces are unavailable the unit would refuse
to start at all. `rdc service install` therefore asks systemd itself rather than guessing: it
runs a throwaway transient unit,

```sh
systemd-run --user --wait --quiet -p PrivateUsers=yes -p ProtectSystem=strict -p PrivateTmp=yes /bin/true
```

and writes the drop-in only if that starts. Reading `/proc/sys` is not enough (Ubuntu 23.10+
restricts namespaces through an AppArmor profile the old knobs do not reflect) and neither is
`unshare -Ur`, which can succeed on hosts where systemd's own mount setup still fails. Install
says so when it skips the drop-in, and removes a stale one if an upgrade takes namespaces away.

Paths in the generated files are quoted and any `%` is written `%%`, so a binary or audit
directory containing a space is one path rather than two, and a `%` is not read as a systemd
specifier. A path containing a quote, a backslash or a newline is refused at install time
rather than written out ambiguously.

Installing over a running daemon **restarts** it, so the new settings take effect immediately
instead of at the next login. The unit is `Type=exec`, which means systemd calls it active only
once the binary has actually been reached; install then waits past `RestartSec` and checks that
`NRestarts` is still 0, so a daemon that starts and dies in a loop is reported rather than
mistaken for a healthy one.

`RemoveIPC` is deliberately not set: in a `--user` unit it would delete every shared-memory
object owned by your account when the unit stops, which can take Xwayland (MIT-SHM) and
PipeWire down with it.

Check the result with `systemd-analyze security --user dev.rdc.daemon`. If your desktop needs
something the sandbox blocks (the journal will show `EPERM` or a mount error), override it with
`systemctl --user edit dev.rdc.daemon` rather than editing the generated files, so the change
survives the next `rdc service install`.

## Persistence is opt-in

Nothing rdc does installs itself anywhere. `rdc serve` runs until you stop it and leaves no unit,
LaunchAgent, scheduled task or login item behind. Only `rdc service install`, run explicitly,
creates the per-user service, and `rdc service uninstall` removes it completely.

## Tailscale

rdc uses the LocalAPI socket at `/var/run/tailscale/tailscaled.sock`. If your user can't read it,
`rdc doctor` shows the failure and rdc falls back to the `tailscale` CLI. On most distributions
the socket is world-connectable for read-only calls like `whois` and `status`.
