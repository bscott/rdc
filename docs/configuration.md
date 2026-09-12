# Configuration

rdc reads one TOML file. If it is missing, defaults apply and everything can be given on the
command line instead.

| Platform | Path |
|---|---|
| Linux | `~/.config/rdc/config.toml` |
| macOS | `~/Library/Application Support/rdc/config.toml` |
| Windows | `%APPDATA%\rdc\config.toml` |

`rdc doctor` prints the path it is using. Unknown keys anywhere in the file are errors, so a
typo such as `caps` instead of `can` stops the daemon from starting rather than silently granting
more than intended.

## Full example

```toml
[serve]
port = 7770
# Optional. Default: this machine's Tailscale IPv4. Must be a Tailscale address.
# bind = "100.101.102.103"

# Who may control this machine, and how much. Empty = daemon refuses to start.
# A plain string grants everything; an inline table limits it to some capabilities.
allow = [
  "you@example.com",                                        # full control
  { who = "monitor-bot", can = "view" },                    # screenshots only
  { who = ["tag:ops", "bob@example.com"], can = ["view", "clipboard"] },
]

# Optional. Extra names clients may use in the URL besides this node's Tailscale IPs,
# MagicDNS name and hostname (e.g. a CNAME you point at it). Keep this above any
# [[serve.grant]] block; TOML would otherwise attach it to the grant.
# hosts = ["desk.internal.example"]

# The same thing as a block, if you prefer one grant per section.
[[serve.grant]]
who = "tag:family"
can = "all"

[serve.audit]
enabled = true                 # default
# path = "/var/log/rdc/audit.jsonl"   # default: rdc/audit.jsonl in the platform state dir
max_size_mb = 50               # rotate above this size
keep = 5                       # keep audit.jsonl.1 … .5

# Names you can pass to `--target` on the client side.
[targets.studio-mac]
url = "http://studio-mac.example-tailnet.ts.net:7770"

[targets.workshop-pc]
url = "http://100.64.10.20:7770"
```

## `[serve]`

| Key | Default | Meaning |
|---|---|---|
| `port` | `7770` | TCP port for the daemon |
| `bind` | Tailscale IPv4 | Address to listen on. Anything that isn't a Tailscale address (100.64.0.0/10 or fd7a:115c:a1e0::/48) is rejected at startup. |
| `allow` | `[]` | Grants: plain identity strings (full control) or `{ who, can }` tables, see below |
| `grant` | `[]` | `[[serve.grant]]` blocks, same shape as the table form of `allow` |
| `audit` | enabled | Audit log settings, see below |
| `hosts` | `[]` | Extra accepted `Host` header names; the node's own IPs, MagicDNS name and hostname are always accepted |

Command-line equivalents: `rdc serve --port 7771 --bind 100.x.y.z --allow a@b --allow tag:ops=view`.
`--allow` flags are **added** to the config grants; `who=cap,cap` limits capabilities, a bare
identity grants all.

### Grants and capabilities

Each grant names one or more identities (`who`) and what they may do (`can`):

| Capability | Allows |
|---|---|
| `view` | `displays`, `windows`, `screenshot`, `whoami` |
| `input` | mouse, keyboard, `focus` |
| `clipboard` | reading and writing the clipboard |
| `all` | everything (the default when `can` is omitted, and what a plain string grants) |

`who` and `can` each take one value or a list. When several grants match the same caller, their
capabilities are combined. A caller that lacks a capability gets `403 forbidden` with a message
naming the missing one, and the attempt is written to the audit log. `whoami` and `/health` need
a valid identity but no particular capability.

### The Tailscale side: let the traffic through

rdc's grants decide what an identity may do. Your **Tailscale access policy** decides whether that
identity's packets reach port 7770 at all. Both have to agree. If the policy blocks the
connection, the client sees a timeout, not a 403, and nothing appears in rdc's audit log because
nothing arrived.

Tailscale's default policy allows everything, so a new tailnet needs no change. If you have
tightened it, add a rule. The cleanest pattern is to tag the machines that run `rdc serve` (for
example `tag:rdc-host`) and open the port from the people and tags you name in rdc's grants.

Current syntax (`grants`), in the policy file at
[login.tailscale.com/admin/acls](https://login.tailscale.com/admin/acls):

```jsonc
{
  "tagOwners": {
    "tag:rdc-host": ["autogroup:admin"],
  },
  "grants": [
    // people who may control rdc hosts
    { "src": ["alice@example.com", "bob@example.com"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] },
    // a monitoring tag that only screenshots (rdc grant: can = "view")
    { "src": ["tag:monitor"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] },
  ],
}
```

Older syntax (`acls`), if your policy still uses it:

```jsonc
"acls": [
  { "action": "accept", "src": ["alice@example.com", "tag:monitor"], "dst": ["tag:rdc-host:7770"] },
]
```

Then tag the controlled machine (`tailscale up --advertise-tags=tag:rdc-host` or from the admin
console) and write the matching rdc grants:

```toml
[serve]
allow = [
  "alice@example.com",
  "bob@example.com",
  { who = "tag:monitor", can = "view" },
]
```

### Policy rules for the examples on this page

The full example at the top of this page has four grants. This is the policy that lets each of
them through, assuming the controlled machine is tagged `tag:rdc-host`:

| rdc grant (`config.toml`) | Who the policy must allow | Policy `grants` entry |
|---|---|---|
| `"you@example.com"` | that login | `{ "src": ["you@example.com"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] }` |
| `{ who = "monitor-bot", can = "view" }` | a specific device, named by its node name in rdc | policies can't name a node as `src`; give the device a tag (`tag:monitor`) and use `{ "src": ["tag:monitor"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] }`, or list its Tailscale IP under `"hosts"` and use that name as `src` |
| `{ who = ["tag:ops", "bob@example.com"], can = ["view", "clipboard"] }` | the tag and the login | `{ "src": ["tag:ops", "bob@example.com"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] }` |
| `[[serve.grant]] who = "tag:family"` | the tag | `{ "src": ["tag:family"], "dst": ["tag:rdc-host"], "ip": ["tcp:7770"] }` |

Or, as one policy fragment covering all four (plus the tag definitions the rules need):

```jsonc
{
  "tagOwners": {
    "tag:rdc-host": ["autogroup:admin"],
    "tag:monitor":  ["autogroup:admin"],
    "tag:ops":      ["autogroup:admin"],
    "tag:family":   ["autogroup:admin"],
  },
  "grants": [
    { "src": ["you@example.com", "bob@example.com", "tag:ops", "tag:monitor", "tag:family"],
      "dst": ["tag:rdc-host"],
      "ip":  ["tcp:7770"] },
  ],
}
```

Legacy `acls` equivalent of that single rule:

```jsonc
{ "action": "accept",
  "src": ["you@example.com", "bob@example.com", "tag:ops", "tag:monitor", "tag:family"],
  "dst": ["tag:rdc-host:7770"] }
```

The policy only opens the port. What each caller may then do (`view`, `input`, `clipboard`) is
still decided by the rdc grant, so a `tag:monitor` device reaches the daemon but gets `403` on
anything but screenshots.

Notes:

- `src` names in the policy and `who` names in rdc are the same identities: tailnet logins and
  tags. Node names work in rdc but not as a policy `src`; use tags for machines.
- If the rdc host stays a user-owned device instead of a tagged one, use the owner's login as
  `dst` (all of that user's devices), or list the machine under `hosts` in the policy.
- SSH and rdc are separate ports. Opening 7770 does not open 22, and rdc never needs 22.
- Check the network path before blaming rdc: `tailscale ping <host>` from the client, then
  `rdc -t <host> whoami`. A timeout is the policy; a 403 is rdc.

### Allowlist rules

Each request's peer IP is resolved with `tailscaled`'s `whois`. The result has a node name and
either a login name (user-owned devices) or one or more tags (tagged devices). Tailscale still
reports the *creating* user's profile for tagged devices, but rdc ignores it: a tagged device can
only match by tag or node name, never by that user's login. An entry matches when,
case-insensitively:

- it equals the caller's **login name**, e.g. `alice@github`, `alice@example.com`;
- it equals the caller's **node name** (the short device name, without the tailnet suffix);
- it equals one of the caller's **tags**, e.g. `tag:family`;
- it is `*`, which allows everyone on the tailnet who can reach the port.

Results are cached for 30 seconds per IP. Loopback connections are always refused unless the
daemon was started with `--dev-loopback`.

### Audit log

Every authorized request and every rejection is appended as one JSON object per line:

```json
{"ts":"2026-09-09T16:08:55.979Z","peer":"100.64.0.7","login":"alice@example.com","node":"laptop",
 "method":"POST","path":"/v1/act","action":"input.click 100,100 Left x1","outcome":"denied",
 "status":403,"detail":"alice@example.com may not use `input` on this machine","ms":0}
{"ts":"2026-09-09T16:09:02.114Z","peer":"100.64.0.7","login":"alice@example.com","node":"laptop",
 "method":"GET","path":"/v1/screenshot","action":"screenshot all png max=Some(1568)","outcome":"ok",
 "status":200,"ms":312,"window":"Firefox: Inbox — Mail","screenshot_sha256":"9f86d081…"}
```

`outcome` is `ok`, `denied` (host, identity or capability) or `error`. `action` describes the
request without its payload: typed text is recorded only as a character count. Key chords, window
selectors, window titles and error messages are recorded as sent, with control characters
replaced, so a hostile value cannot break the file or the terminal you read it in. Two fields tie
each line to what was actually on screen:

- `window` is the focused window when the request arrived (`app: title`, truncated to 160
  characters), recorded for screenshot, input, focus and clipboard requests. It answers "what
  was that click aimed at?" after the fact. The lookup is a direct focused-window query
  (`GetForegroundWindow` on Windows, `hyprctl activewindow` on Hyprland, the on-screen window
  list on macOS/X11), not a full enumeration; `rdc doctor` prints how long it takes on your
  machine. Titles can carry document names, URLs and mail subjects; set `window_titles = false`
  to leave it out.
- `screenshot_sha256` is the SHA-256 of the image bytes a screenshot request returned, so a
  screenshot an agent saved (or that appears in a transcript) can be matched to the exact audit
  line that produced it. Every MCP action ends with a fresh screenshot, so every tool call an
  agent makes leaves at least one hashed line.

The file and its rotated copies are mode 0600. Read it with `rdc audit` (`-n`, `--json`,
`--path`); the table view shows the window in brackets and the first 12 hex digits of the hash.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | write the log at all |
| `path` | platform state dir | where to write it |
| `max_size_mb` | `50` | rotate above this size |
| `keep` | `5` | rotated files to keep |
| `window_titles` | `true` | record the focused window on each action |

Default location: `~/.local/state/rdc/audit.jsonl` (Linux), `~/Library/Application
Support/rdc/audit.jsonl` (macOS), `%LOCALAPPDATA%\rdc\audit.jsonl` (Windows).

### Host check

Before identity, the daemon checks the request's `Host` header against the names it answers to:
its Tailscale IPs, its MagicDNS name, its short hostname, and anything in `[serve].hosts`. A
request addressed to any other name gets `421 Misdirected Request`. This stops a web page on an
allowed machine from reaching the daemon through DNS rebinding, since the browser would send the
attacker's hostname. Use the Tailscale name or IP in your `[targets]` URLs.

## `[targets]`

Each table under `[targets]` names a machine for the client side. `url` is the daemon's base URL;
use the Tailscale MagicDNS name or the Tailscale IP. Only plain `http://` is needed since the
tailnet is already encrypted.

## Resolving `--target`

`rdc -t VALUE …` and `rdc mcp --target VALUE` accept, in order:

1. `local` — control this machine directly, no daemon involved (default).
2. A name from `[targets]`.
3. A full URL, `http://host:port`.
4. A bare `host` or `host:port`; the port defaults to `[serve].port`.

## File permissions

Whoever can edit `config.toml` can add themselves to `[serve].allow`, so the file is as
sensitive as `~/.ssh/authorized_keys` and rdc treats it the same way. On Linux and macOS,
`rdc serve` and `rdc service install` refuse to start when the config file or its directory is
owned by another user or is writable by group or others; `rdc doctor` reports the same check as
`config perms`. World-*readable* is fine (the allowlist is not a secret), but the recommended
layout is:

```sh
chmod 700 ~/.config/rdc            # macOS: ~/Library/Application\ Support/rdc
chmod 600 ~/.config/rdc/config.toml
```

Set `RDC_INSECURE_CONFIG=1` to turn the refusal into a logged warning if you have a deliberate
reason (a shared dotfiles checkout, say). Windows is not checked: the per-user ACL on `%APPDATA%`
already restricts it to the profile's owner and administrators.

## Environment variables

| Variable | Effect |
|---|---|
| `RDC_TARGET` | default for `--target` |
| `RDC_LOG` | log filter, e.g. `debug`, `rdc=debug,hyper=warn` (tracing syntax) |
| `RDC_INSECURE_CONFIG` | set to `1` to run the daemon even if `config.toml` is writable by others (see [File permissions](#file-permissions)) |
| `RDC_SIGN_IDENTITY` | macOS: code-signing identity name for `scripts/macos/bundle-and-sign.sh` (default `rdc-dev`) |
| `RDC_ALLOW_ADHOC` | macOS: set to `1` to let the bundle script fall back to ad-hoc signing |
