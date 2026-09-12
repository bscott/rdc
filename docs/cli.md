# CLI reference

```
rdc [-t TARGET] [--log FILTER] <command>
```

`-t/--target` selects the machine for every client command (default `local`; see
[Configuration](configuration.md#resolving---target)). `--log` sets the tracing filter
(`info` by default); logs go to stderr, or to a file with `--log-file PATH` (`RDC_LOG_FILE`).

## Daemon side

### `rdc serve`

Run the daemon on the machine to be controlled. Binds the Tailscale IPv4 on port 7770 unless
told otherwise, verifies every caller through `tailscaled whois`, and refuses to start with an
empty allowlist.

| Flag | Meaning |
|---|---|
| `--allow WHO[=CAPS]` | identity to permit, optionally limited: `--allow tag:ops=view,clipboard` (repeatable, added to config) |
| `--port N` | listen port |
| `--bind IP` | listen address (must be a Tailscale address) |
| `--dev-loopback` | bind 127.0.0.1 and skip authentication for loopback. **Testing only.** |
| `--user NAME` | Unix: if started as root, drop to this account after binding (default `[serve].user`). rdc refuses to serve as root otherwise. |

### `rdc service install [--elevated] | uninstall | status`

Manage a per-user background service that runs `rdc serve`: a LaunchAgent on macOS
(`dev.rdc.daemon`), a `systemd --user` unit on Linux, a Task Scheduler logon task on Windows. On
Windows the task runs at standard integrity; `--elevated` requests the highest run level so the
daemon can drive elevated windows (see [Windows setup](setup-windows.md#how-it-has-to-run)). The service runs the binary at the path `rdc service install` was invoked from, so
install from the final location (on macOS, from inside `rdc.app`).

### `rdc audit [-n N] [--json] [--path FILE]`

Show the last N entries (default 50) of this machine's audit log as a table, or as raw JSON
lines with `--json`. The file is `[serve.audit].path` or the platform default. See
[Configuration](configuration.md#audit-log).

### `rdc doctor [--request-permissions]`

Prints what rdc can see on this machine: platform and session type, `tailscaled` connectivity and
this node's identity, permission state, displays with geometry, a timed test screenshot, window
count, whether input initialises, the configured grants, and the audit log path. Exits non-zero if something needed for serving is missing.
`--request-permissions` (macOS) triggers the Screen Recording and Accessibility prompts.

## Client side

All coordinates are **logical desktop points** in the virtual desktop that spans every monitor.
`rdc displays` shows each monitor's rectangle in that space.

| Command | What it does |
|---|---|
| `rdc displays` | list displays as JSON: id, name, rect, scale, primary |
| `rdc windows` | list windows as JSON: id, pid, app, title, rect, focused, minimized |
| `rdc shot [--display all\|primary\|ID] [--max PX] [--jpeg] [-o FILE]` | screenshot. `--max` limits the longer edge. Prints the output path; the desktop rect it covers goes to stderr. |
| `rdc move X Y` | move the pointer |
| `rdc click X Y [--button left\|right\|middle] [--double]` | click |
| `rdc drag X1 Y1 X2 Y2 [--button …]` | press, move in steps, release |
| `rdc scroll [--dx N] [--dy N] [--at X Y]` | wheel steps; positive `dy` scrolls down, positive `dx` right |
| `rdc type TEXT` | type literal text (unicode ok) |
| `rdc key CHORD` | press a chord, see below |
| `rdc focus --id ID \| --app SUBSTR \| --title SUBSTR` | bring a window forward |
| `rdc clip [TEXT]` | read the clipboard, or set it |
| `rdc whoami` | ask the daemon how it identifies you (remote targets only) |

### Mapping a screenshot pixel to a click

A screenshot of size `W×H` that covers desktop rect `(x, y, w, h)` maps pixel `(px, py)` to
`(x + px·w/W, y + py·h/H)`. On a 2× display, a full-resolution screenshot is twice the size of
the logical desktop, so halve pixel coordinates. `rdc mcp` does this automatically; the CLI does
not.

### Key chords

Tokens joined by `+`; all but the last are modifiers.

| Modifiers | `cmd` `command` `super` `win` `meta` (all = the platform's Meta key), `ctrl` `control`, `alt` `option`, `shift` |
|---|---|
| Named keys | `enter`/`return`, `esc`/`escape`, `tab`, `space`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up` `down` `left` `right`, `capslock`, `f1`…`f24` |
| Punctuation names | `plus` `minus` `comma` `period` `slash` `backslash` `semicolon` `quote` `grave` `equal` `bracketleft` `bracketright` |
| Anything else | a single character, typed with the modifiers held |

Examples: `cmd+shift+4`, `ctrl+c`, `alt+f4`, `enter`, `ctrl+plus`, `+`.

## MCP server

### `rdc mcp [--target T] [--max PX]`

Serve MCP over stdio for an agent. `--max` sets the longest screenshot edge sent to the model
(default 1568). See [MCP tools](mcp.md).
