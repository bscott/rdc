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

## Persistence is opt-in

Nothing rdc does installs itself anywhere. `rdc serve` runs until you stop it and leaves no unit,
LaunchAgent, scheduled task or login item behind. Only `rdc service install`, run explicitly,
creates the per-user service, and `rdc service uninstall` removes it completely.

## Tailscale

rdc uses the LocalAPI socket at `/var/run/tailscale/tailscaled.sock`. If your user can't read it,
`rdc doctor` shows the failure and rdc falls back to the `tailscale` CLI. On most distributions
the socket is world-connectable for read-only calls like `whois` and `status`.
