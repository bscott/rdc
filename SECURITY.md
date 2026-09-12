# Security

## What rdc is

`rdc serve` gives whoever it trusts full control of a logged-in desktop session: they can see the
screen, move the mouse, type, press shortcuts, focus windows and read or write the clipboard.
Treat it exactly like handing someone the keyboard. There is deliberately no shell tool, but a
desktop session is more than enough to open one.

## Threat model

**Trusted:** your Tailscale tailnet, your tailnet identity provider, the machine running
`rdc serve`, and every identity in `[serve].allow`.

**How access is decided.** The daemon binds only to the machine's Tailscale IP (it refuses any
other non-loopback address). For every request it takes the peer IP from the TCP connection,
asks the local `tailscaled` who that IP is (`whois`), and compares the login name, node name and
tags against the allowlist. Results are cached for 30 seconds. There are no passwords, tokens or
TLS: the tailnet's WireGuard layer provides encryption and the identity.

**Consequences.**

- Anyone whose login, node or tag matches a grant gets that grant's capabilities: `view`
  (screenshots, windows), `input` (mouse, keyboard, focus), `clipboard`, or `all`. A plain
  identity string grants all three. `"*"` allows the entire tailnet. Tags match every node
  carrying them. Tagged devices are identified by their tags and node name only; the login of
  the user who created them carries no authority.
- If an allowed identity is compromised (stolen device, leaked auth key, shared tailnet), the
  attacker has your desktop. Tailscale ACLs are your second layer: restrict which nodes may reach
  the rdc port at all.
- `whois` is only as accurate as `tailscaled`. If the local daemon is unavailable, rdc falls back
  to the `tailscale` CLI; if neither works, every request is rejected.
- The config file is the allowlist. On Unix the daemon refuses to start if `config.toml` or its
  directory is owned by someone else or writable by group/others, since editing it is
  equivalent to desktop access; `RDC_INSECURE_CONFIG=1` overrides with a warning.
- The daemon does not run as root: started as root, `rdc serve` exits. It never needs root (the
  port is unprivileged, everything else happens inside the desktop session), and a remote-control
  surface should not hold it. On Windows, `rdc service install` creates the logon task at
  standard integrity unless `--elevated` is passed.
- Nothing persists unless you ask: only `rdc service install` creates a unit, LaunchAgent or
  scheduled task; `rdc serve` alone leaves nothing behind.
- `--dev-loopback` binds 127.0.0.1 and disables authentication for loopback connections. It is
  for local development and must never be used on a shared machine or forwarded.
- MCP clients talk to `rdc mcp` over stdio on the operator's machine. The operator's agent
  inherits the operator's tailnet identity; anything the agent does is done as you.
- The macOS build needs Screen Recording and Accessibility permissions. Grant them only to a
  signed bundle you built or verified; see `scripts/macos`.

**Audit.** Every request and rejection is appended to a JSON-lines audit log (mode 0600, size
rotated) with the caller's identity, an action summary, the outcome and timing. Typed text is
never logged, only its length. Read it with `rdc audit`.

**Not yet implemented** (tracked as issues): rate limiting, and a pause when a human is
physically using the input devices.

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's private vulnerability
reporting on this repository ("Report a vulnerability" under the Security tab). You should get a
response within a week. Fixes will be released as a new tagged version with a note in the
release description.

## Supported versions

Only the latest tagged release receives fixes.
