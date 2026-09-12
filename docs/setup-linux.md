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
(it has to, to see the screen and inject input) but systemd fences off everything it does not
need:

| Restriction | Effect |
|---|---|
| `NoNewPrivileges`, empty `CapabilityBoundingSet`, `RestrictSUIDSGID` | neither rdc nor anything it runs (`hyprctl`, `tailscale`) can gain privileges |
| `SystemCallFilter=@system-service` minus `@privileged @resources`, native ABI only | privileged and resource-limit syscalls return `EPERM` |
| `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` | unix sockets (Wayland, D-Bus, portal, tailscaled) and the HTTP listener; nothing else |
| `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname` | no kernel tunables, modules, logs, cgroups, clock or hostname |
| `UMask=0077` | audit log and rotations are private to your user |
| `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp` (drop-in) | the filesystem is read-only except `$XDG_RUNTIME_DIR`, `~/.local/state/rdc` and the audit log directory |

`RemoveIPC` is deliberately left out: in a `--user` unit it would delete every shared-memory
object owned by your account when the unit stops, which can take Xwayland (MIT-SHM) and PipeWire
down with it.

The filesystem rules live in a separate drop-in, `~/.config/systemd/user/dev.rdc.daemon.service.d/10-sandbox-mounts.conf`,
because in a `--user` service they need unprivileged user namespaces. The installer checks the
kernel knobs and skips the drop-in when they are disabled; everything else still applies.

Check the result with `systemd-analyze security --user dev.rdc.daemon`. If your desktop needs
something the sandbox blocks (the journal will show `EPERM` or a mount error), loosen it with
`systemctl --user edit dev.rdc.daemon` rather than editing the generated files, so the override
survives the next `service install`.

## Tailscale

rdc uses the LocalAPI socket at `/var/run/tailscale/tailscaled.sock`. If your user can't read it,
`rdc doctor` shows the failure and rdc falls back to the `tailscale` CLI. On most distributions
the socket is world-connectable for read-only calls like `whois` and `status`.
