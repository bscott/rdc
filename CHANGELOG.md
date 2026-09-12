# Changelog

Entries under **Unreleased** go into the next release's notes; `scripts/release-notes.sh`
builds the GitHub release body from the matching `## <version>` section.

## Unreleased

### Added
- **`rdc serve` refuses to run as root.** It never needs root and a remote-control surface
  should not hold it; started as root it exits with a message. `rdc doctor` reports the process
  user. Unix only.

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
