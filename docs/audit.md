# Audit log and live viewer

Every request an `rdc serve` daemon handles, and every one it refuses, becomes one JSON line in
its audit file. This page covers reading that file, streaming it off the machine, and watching
several machines at once in a browser.

## The file

Default location: `rdc/audit.jsonl` in the platform state directory (`~/.local/state/rdc/` on
Linux, `~/Library/Application Support/rdc/` on macOS, `%LOCALAPPDATA%\rdc\` on Windows). It is
created with owner-only permissions and rotated at `max_size_mb`, keeping `keep` old files.

```bash
rdc audit              # last 50 entries as a table
rdc audit -n 200 --json
```

One entry:

```json
{"ts":"2026-09-11T18:02:12.430Z","host":"studio-mac","peer":"100.64.0.11",
 "login":"brian@example.com","node":"laptop","method":"POST","path":"/v1/act",
 "action":"input.click 640,400 Left x1","outcome":"ok","status":200,"ms":8}
```

| Field | Meaning |
|---|---|
| `ts` | UTC time, RFC 3339 |
| `host` | the machine whose desktop this is about (the daemon's node name) |
| `peer` | source IP of the request |
| `login`, `node`, `tags` | the caller's Tailscale identity, when one was established. Tagged devices have no `login`. |
| `method`, `path` | the HTTP request |
| `action` | what was asked for. Typed text is recorded as a character count, never as content. |
| `outcome` | `ok`, `denied` (bad host, unknown peer, not allowed, missing capability) or `error` |
| `status` | HTTP status returned |
| `detail` | why it was denied or failed |
| `ms` | how long the request took |
| `via` | added by the viewer: the node that sent this entry over the network |

Strings from the network (window titles, key chords, host names) have control characters
replaced before they are written, so a hostile value cannot break the line framing or drive the
terminal of whoever reads the log.

## Streaming entries off the machine

A daemon can also POST its entries, as they happen, to an HTTP endpoint:

```toml
[serve.audit]
stream = "http://laptop.example-tailnet.ts.net:7771/v1/ingest"
# stream_token = "..."   # sent as `Authorization: Bearer`, for third-party collectors
```

or `rdc serve --audit-stream http://...`.

Entries are batched (a few hundred lines or half a second, whichever comes first) into one
request with `Content-Type: application/x-ndjson`: one JSON object per line, newline
terminated. A failed request is retried with backoff up to 30 seconds while new entries queue
behind it. If the endpoint stays down and the queue fills, new entries are dropped for the stream
and a warning is logged. The local file is never affected. Treat the stream as a live feed and
the file as the record.

Any collector that accepts newline-delimited JSON over HTTP works: an `rdc audit-view` instance,
Vector's `http_server` source, a small script, or a log service's ingest URL with
`stream_token`. Keep the endpoint on your tailnet or behind TLS; the stream carries who did what
on your machines.

## The live viewer

`rdc audit-view` is a small web server that receives streams from your daemons and shows them in
one page, newest first, updating as entries arrive.

```bash
rdc audit-view --allow brian@example.com
# audit viewer at http://100.64.0.11:7771/  (daemons stream to http://100.64.0.11:7771/v1/ingest)
```

Then point each daemon at it with `[serve.audit] stream = ".../v1/ingest"` and open the URL in a
browser on an allowed machine.

The viewer applies the same rules as the daemon. It listens only on a Tailscale address (or
loopback with `--dev-loopback`), refuses requests whose `Host` header is not one of its own
names, and identifies every caller, daemon or browser, through `tailscaled whois`. Both the
daemons that send and the people who view must be in its allowlist. Capabilities in the
allowlist are ignored here; membership is what counts. Entries the viewer itself refuses show up
in the page too, with the viewer's own host name.

The viewer sets `via` on every entry it receives to the sending node's name and never trusts
the sender for it. An entry that arrives without a `host` is attributed to its sender.

Options:

| Flag | Config | Meaning |
|---|---|---|
| `--port` | `[audit_view].port` | default `7771` |
| `--bind` | `[audit_view].bind` | default: this node's Tailscale IPv4 |
| `--allow` | `[audit_view].allow` | who may send and view; same shapes as `[serve].allow` |
| `--store PATH` | `[audit_view].store` | where received entries are kept; default `audit-view.jsonl` in the state dir, rotated like the daemon's file |
| `--no-store` | | keep entries in memory only |
| `--follow PATH` | `[audit_view].follow` | also follow a local audit file, for the daemon on the same machine |
| `--dev-loopback` | | bind 127.0.0.1 without authentication, for testing |

The page has a text filter, an outcome filter, a per-host filter, and a pause box that holds new
rows until you uncheck it. It is plain HTML and JavaScript served with a strict content security
policy; log content is only ever inserted as text.

### Endpoints

| Route | Used by | Purpose |
|---|---|---|
| `POST /v1/ingest` | daemons | body is `application/x-ndjson`, up to 8 MiB; returns `{accepted, rejected}` and `400` if any line was not an entry |
| `GET /v1/recent?n=500&host=` | the page | the most recent entries held in memory (up to 5000) |
| `GET /v1/events` | the page | server-sent events, one `entry` event per line |
| `GET /` | you | the page |

### Tailscale policy

Daemons connect to the viewer's port, so the policy must allow the daemon machines to reach
`<viewer>:7771`, and your own devices too for the page. With the tagging pattern from
[Grants and Tailscale policy](grants-and-acls.md):

```jsonc
{"action": "accept", "src": ["tag:rdc-host", "autogroup:member"], "dst": ["tag:rdc-viewer:7771"]}
```
