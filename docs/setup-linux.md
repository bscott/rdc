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

`rdc serve` refuses to run with root privileges. If you need to start it from a root context
anyway — a system unit, a provisioning script — `[serve].user` (or `--user`) names an account to
become instead. The daemon binds the listener, switches to that account with
`initgroups`/`setgid`/`setuid`, checks that regaining root is now impossible, and only then
opens the audit log and accepts the first request. A failed drop is fatal; it never falls back
to serving as root.

### The config file stays owned by root

This is the part that makes the model worth having. rdc reads its config from the config
directory of the user it is *started* as, so under `sudo` that is `/root/.config/rdc/config.toml`,
and it refuses to start from a config file that anyone but that user can write (see
[File permissions](configuration.md#file-permissions)). Keep the file root-owned: the account
the daemon drops to then **cannot edit its own allowlist**, which is exactly what you want from
an account whose whole job is to be remotely driven.

Two consequences follow, and both have bitten people:

- Do **not** use `sudo -E`. It keeps your `HOME`, so rdc reads *your* config file, sees it is
  owned by uid 1000 while the process is uid 0, and refuses to start.
- Set `[serve.audit].path` explicitly. The daemon writes the log as the dropped account, so by
  default it lands in that account's state directory — while `rdc audit`, run by you, looks in
  *yours* and finds nothing. Naming the path in the root-owned config makes `sudo rdc audit`
  read the same setting and find the file. Otherwise you need `rdc audit --path …` every time.

### Whether the account can see anything

The drop is only half the story; the display server decides the rest.

| Session | Can a separate account capture and drive it? |
|---|---|
| X11 | Yes, given `DISPLAY` and permission to the X server (`xhost +SI:localuser:rdc`, or an `XAUTHORITY` file that account may read). This is the setup where a dedicated user is meaningful. |
| Wayland | No. The compositor's socket in `$XDG_RUNTIME_DIR` belongs to the logged-in user and the portal talks only to that session. Run rdc as the desktop user; the root refusal still protects you. |
| macOS | No (see [macOS setup](setup-macos.md)). Same advice as Wayland. |

### Worked example (X11)

```sh
# 1. The account. No login, no password, its own group.
sudo useradd --system --create-home --shell /usr/sbin/nologin rdc

# 2. Somewhere it can write the audit log.
sudo install -d -o rdc -g rdc -m 750 /var/log/rdc

# 3. Root's config: the allowlist the rdc account cannot edit.
sudo install -d -m 700 /root/.config/rdc
sudo tee /root/.config/rdc/config.toml >/dev/null <<'TOML'
[serve]
allow = ["you@example.com"]
user = "rdc"

[serve.audit]
path = "/var/log/rdc/audit.jsonl"
TOML
sudo chmod 600 /root/.config/rdc/config.toml

# 4. From the desktop session, let that account reach the X server.
#    Once per session; add it to your session startup to make it stick.
xhost +SI:localuser:rdc

# 5. Start it. Plain sudo, not sudo -E, and pass the display explicitly.
sudo DISPLAY=:0 rdc serve

# 6. Read the log as root, which reads the same config and so the same path.
sudo rdc audit -n 20
```

`rdc doctor` shows the process user, and the daemon logs the drop (`dropped privileges to rdc
(uid …, gid …)`) as its first action after binding.

A `--log-file`, if you use one, is opened before the drop and handed to the target account
afterwards, so rotation keeps working.

This whole path has been exercised as root on a headless Linux box, but not yet on a real X11
desktop; please report what you find.

## Persistence is opt-in

Nothing rdc does installs itself anywhere. `rdc serve` runs until you stop it and leaves no unit,
LaunchAgent, scheduled task or login item behind. Only `rdc service install`, run explicitly,
creates the per-user service, and `rdc service uninstall` removes it completely.

## Tailscale

rdc uses the LocalAPI socket at `/var/run/tailscale/tailscaled.sock`. If your user can't read it,
`rdc doctor` shows the failure and rdc falls back to the `tailscale` CLI. On most distributions
the socket is world-connectable for read-only calls like `whois` and `status`.
