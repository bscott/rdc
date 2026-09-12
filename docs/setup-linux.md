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

## Running as a dedicated user

`rdc serve` refuses to run as root. If you want to start it from a root context anyway (a system
unit, a provisioning script), `[serve].user` (or `--user`) names an account to become. The
daemon binds the listener, switches to that account with `initgroups`/`setgid`/`setuid`, checks
that regaining root is impossible, and only then opens the audit log and accepts the first
request. A failed drop is fatal; the daemon never falls back to serving as root. The process
environment is not modified: the audit log goes to that account's state directory
(`~account/.local/state/rdc`, or `~account/Library/Application Support/rdc` on macOS) unless
`[serve.audit].path` says otherwise, and the account needs a writable home for that.

Whether that account can *see* anything depends on the display server:

| Session | Can a separate account capture and drive it? |
|---|---|
| X11 | Yes, given `DISPLAY` and an `XAUTHORITY` file it may read (or `xhost +SI:localuser:rdc`). This is the setup where a dedicated `rdc` user is meaningful. |
| Wayland | No. The compositor's socket in `$XDG_RUNTIME_DIR` belongs to the logged-in user and the portal talks only to that session. Run rdc as the desktop user; the root refusal still protects you. |
| macOS | No (see [macOS setup](setup-macos.md)). Same advice as Wayland. |

Minimal X11 example:

```sh
sudo useradd --system --create-home --shell /usr/sbin/nologin rdc
sudo install -o rdc -g rdc -m 700 -d /home/rdc/.config/rdc
printf '[serve]\nallow = ["you@example.com"]\nuser = "rdc"\n' \
  | sudo install -o rdc -g rdc -m 600 /dev/stdin /home/rdc/.config/rdc/config.toml
# From the desktop session, let that account reach the X server:
xhost +SI:localuser:rdc
# Then, from a root shell or system unit with DISPLAY and XAUTHORITY set:
sudo -E rdc serve --user rdc
```

This path has not yet been exercised on a real X11 desktop; please report what you find.

## Persistence is opt-in

Nothing rdc does installs itself anywhere. `rdc serve` runs until you stop it and leaves no unit,
LaunchAgent, scheduled task or login item behind. Only `rdc service install`, run explicitly,
creates the per-user service, and `rdc service uninstall` removes it completely.

## Tailscale

rdc uses the LocalAPI socket at `/var/run/tailscale/tailscaled.sock`. If your user can't read it,
`rdc doctor` shows the failure and rdc falls back to the `tailscale` CLI. On most distributions
the socket is world-connectable for read-only calls like `whois` and `status`.
