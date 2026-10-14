# Changelog

Entries under **Unreleased** go into the next release's notes; `scripts/release-notes.sh`
builds the GitHub release body from the matching `## <version>` section.

## Unreleased

## 0.5.0 — 2026-10-07

### Changed
- **Breaking: `rdc serve` refuses to run with root privileges** (thanks @Mrigbozurike). It never
  needs them and a remote-control surface should not hold them; started with a real or
  effective uid of 0 (`sudo rdc serve`, a system-level unit, a container whose process is root,
  a setuid-root binary) it exits with a message. Run it as the desktop user instead. `rdc doctor`
  reports the process user and flags root. Unix only.

### Added
- **Audit lines record the focused window and the screenshot hash.** Screenshot, input, focus
  and clipboard entries carry `window` (`app: title` of the window that had focus when the
  request arrived, sanitised and truncated like every other field), and screenshot entries carry
  `screenshot_sha256` of the bytes returned, so an action can be tied to what was on screen and
  a saved image to the line that produced it. The lookup is a direct focused-window query per
  platform — `GetForegroundWindow` on Windows, `hyprctl activewindow` on Hyprland,
  `NSWorkspace`'s `activeApplication` on macOS — and reads the title and application of that one
  window, rather than enumerating the desktop. It runs off the request thread under one timeout
  for every backend; `rdc doctor` reports how long it takes. Titles are recorded by default and
  can carry document names, URLs and mail subjects, in the local log and in any audit stream;
  `[serve.audit].window_titles = false` omits them (thanks @Mrigbozurike).
- **The systemd `--user` unit is sandboxed.** `rdc service install` writes `NoNewPrivileges`,
  a `@system-service` syscall filter minus `@privileged`/`@resources`, `RestrictAddressFamilies`,
  `RestrictNamespaces`/`RestrictRealtime`/`RestrictSUIDSGID`/`LockPersonality` and `UMask=0077`
  into the unit, and the directives that need user namespaces (`PrivateUsers`, an empty
  capability set, the `Protect*` family, `ProtectSystem=strict`, `ProtectHome=read-only`,
  `PrivateTmp` with the X11 socket bound back) into a drop-in written only when a real
  `systemd-run --user` probe starts a transient unit carrying them, since those would otherwise
  stop the unit from starting at all. Paths are quoted and `%` escaped, so a binary or audit
  directory containing a space or a percent sign is written correctly. Installing restarts a
  running daemon rather than leaving the old one in place, and the unit is `Type=exec`, so
  install can wait past `RestartSec` and confirm `NRestarts` is 0 instead of mistaking a restart
  loop for success (thanks @Mrigbozurike).

## 0.4.0 — 2026-09-11

### Added
- **Audit streaming.** `[serve.audit] stream = "http://…/v1/ingest"` (or `rdc serve
  --audit-stream URL`) POSTs every audit entry, batched as `application/x-ndjson`, to an HTTP
  endpoint, with retry and backoff; the local file stays the record. `stream_token` adds a
  bearer token for third-party collectors.
- **`rdc audit-view`**, a live audit viewer: receives streams from any number of daemons (and
  follows local files with `--follow`), stores them, and serves a page that updates as entries
  arrive, with text, outcome and host filters. Same gate as the daemon: Tailscale bind, Host
  check, `whois` identity, allowlist. Configured under `[audit_view]`.
- Audit entries carry `host` (the daemon's node name, the first label of its MagicDNS name);
  the viewer adds `via` (the sending node) and never trusts the sender for it. Old lines without
  `host` still parse.

### Changed
- Release flow: pull requests target a `release/<version>` branch, which is tested on real
  machines before a reviewed merge into `main`. `main` is protected. CONTRIBUTING and AGENTS
  spell out the testing bar: unit tests for new logic, a regression test per bug fix, and a
  router test for anything touching authentication, capabilities, routes or the audit log.
- Router tests (`src/server/tests.rs`) drive the real axum stack with a fake desktop and a
  fake identity table: Host check, unknown and non-Tailscale peers, per-capability denials,
  screenshot headers, and audit completeness and sanitisation.

### Fixed
- Audit completeness that 0.3.0's notes claimed but did not ship: `whoami`, `/health`, unknown
  routes, malformed action bodies and screenshot parameter errors are now written to the audit
  log. The 0.3.0 release only fixed the middleware's recorded status.

### Changed
- **Windows: the scheduled task no longer runs elevated by default** (thanks @Mrigbozurike). `rdc service install`
  now registers the logon task at `LeastPrivilege` run level, so the daemon holds only the
  user's standard token and the install itself no longer needs an Administrator shell. Input
  aimed at elevated windows is dropped by UIPI in this mode; pass `rdc service install
  --elevated` (from an elevated PowerShell) to get the previous `HighestAvailable` behaviour.
  Existing installs keep their run level until reinstalled.
- Windows setup docs scope the firewall rule to the tailnet (`100.64.0.0/10`, Tailscale interface).
- Docs: the configuration guide now shows the Tailscale access-policy rule (`grants` and `acls`
  forms) that must accompany rdc's grants, and the troubleshooting table distinguishes a
  policy timeout from an rdc 403.

## 0.3.0 — 2026-09-10

### Added
- **Config file permission check (Unix)** (thanks @Mrigbozurike). `rdc serve` and `rdc service install` refuse to run
  when `config.toml` or its directory is owned by another user or writable by group/others,
  because editing the allowlist is equivalent to desktop access. `rdc doctor` reports the check
  as `config perms`; `RDC_INSECURE_CONFIG=1` downgrades the refusal to a warning.
- **Windows support, verified on Windows 11.** `rdc service install` creates an elevated Task
  Scheduler logon task that runs the daemon in the interactive session with a log file;
  `service uninstall` stops the running daemon. Window `focus` implemented (foreground-thread
  attach with an Alt-tap fallback). Absolute pointer moves use `SendInput` over the whole
  virtual desktop so secondary monitors are addressable (untested on real hardware yet).
- `--log-file` / `RDC_LOG_FILE` to append logs to a file instead of stderr.

### Fixed
- Windows: the service path no longer carries the `\\?\` verbatim prefix.

## 0.2.1 — 2026-09-09

### Changed
- **License is now GPL-3.0-or-later** (was AGPL-3.0-or-later). rdc is a program you run on
  your own machines, not a hosted service, so the AGPL network clause added friction without
  protecting anything; plain GPL keeps the requirement that distributed modifications are
  published. There are no external contributions to date, so no consent was needed.
- GitHub releases carry the changelog section for the version as their notes.
- CONTRIBUTING asks for DCO sign-off (`git commit -s`).

## 0.2.0 — 2026-09-09

### Added
- **Capabilities.** Grants can limit an identity to `view`, `input` and/or `clipboard`.
  Config accepts plain strings (full control), inline tables
  `{ who = "...", can = [...] }` inside `allow`, or `[[serve.grant]]` blocks; the CLI accepts
  `--allow who=view,clipboard`. Missing capability → `403 forbidden`. `whoami` reports `caps`.
- **Audit log.** One JSON line per authorized request or rejection (host, identity and
  capability denials included) with identity, action summary, outcome and duration. Mode 0600,
  size-rotated, configurable under `[serve.audit]`. New `rdc audit` command to read it.
- `rdc doctor` prints the configured grants and the audit log path.

### Changed
- `--allow` and `[serve].allow` entries are now grants; existing plain-string configs behave
  exactly as before (full control).

## 0.1.0 — 2026-09-08

First public release. Remote desktop control for AI agents over Tailscale: screenshot, mouse,
keyboard, window focus and clipboard as MCP tools and a CLI. Tailscale whois identity with an
allowlist, Host-header check, input validation. Verified on Linux (Hyprland) and macOS (Apple
silicon); Windows compiles but is untested.
