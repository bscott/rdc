# Working on rdc

This file is for contributors and for coding agents that help them. It says what rdc is, what
it must never become, and the checks every change has to pass. `CLAUDE.md` points here.

## What rdc is

One Rust binary. `rdc serve` runs on a machine and lets identified callers on the same Tailscale
tailnet take screenshots and drive mouse, keyboard, window focus and clipboard. `rdc mcp` and the
CLI are clients. The MCP layer converts pixels in the last screenshot to desktop points.

## Design principles

1. **One seam.** New capabilities go into the `Desktop` trait (`src/desktop/mod.rs`) first, then
   the wire API (`src/proto.rs`, `src/server/routes.rs`), then the CLI and MCP tools. All three
   surfaces stay equivalent; nothing is reachable from one that isn't from the others.
2. **Coordinates are logical desktop points on the wire.** Screenshots carry the rect they cover.
   Platform-specific conversion happens only in `src/desktop/local/input.rs` and the
   platform module. Never let a backend's native units leak into `proto`.
3. **Platform code lives behind `cfg(target_os)` in its own module.** The common path must
   compile and pass clippy on Linux, macOS and Windows. Run
   `cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings` and the
   `aarch64-apple-darwin` equivalent from Linux before pushing.
4. **Fail closed.** Config typos are errors. Missing identity is a rejection. Unknown capability
   names are errors. Prefer returning `RdcError` to panicking anywhere network input can reach.
5. **No shell, no file transfer, no new listeners.** rdc is a screen-and-input surface. SSH
   exists for everything else.
6. **Documentation is part of the change.** Update `docs/`, `skills/rdc/SKILL.md` when it
   affects agents, and add a line under Unreleased in `CHANGELOG.md`.

## Security principles

1. **Identity comes from `tailscaled`, never from the request.** The peer IP is taken from the
   socket; `whois` resolves it. Headers, bodies and query strings carry no identity.
2. **Tagged devices are their tags.** Tailscale reports the creating user for tagged nodes; rdc
   discards that login. Do not reintroduce it.
3. **The Host header must name this machine.** This blocks DNS rebinding from a browser on an
   allowed node. Keep the check before authorization.
4. **Capabilities gate every desktop route.** `view`, `input`, `clipboard`. Adding a route means
   choosing its capability and adding an audit call.
5. **Everything is audited.** One JSON line per request or rejection with identity, an action
   summary, outcome and status. Never log typed text or clipboard contents; record their length.
   Sanitize any value that came from a client before it reaches the log or a terminal.
6. **Validate before side effects.** Coordinates inside the display union, bounded scroll,
   supported keys, both ends of a drag checked before the button goes down. Release what you
   pressed even on error.
7. **Bind only Tailscale addresses.** `--dev-loopback` is the single exception and stays
   loopback-only and unauthenticated by name.
8. **Least privilege for helpers.** Anything that stops or kills processes must match the exact
   binary path, command line and user, and must never touch the current process or other rdc
   clients.
9. **Secrets never enter the tree.** No tokens, keys or hostnames of real people in tests or
   docs. Use `studio-mac`, `alice@example.com`.

## Tests are the bar

- New logic ships with unit tests; every bug fix ships with a regression test that fails
  without the fix.
- Anything touching authentication, capabilities, routes or the audit log gets a case in
  `src/server/tests.rs`, which drives the real router with a fake desktop and identity table.
- Platform-specific code is described in the PR: which OS, what you ran.
- Never weaken or delete a test to get green. If a test is wrong, say why in the PR.

## Release flow

Pull requests target the current `release/<version>` branch. `main` holds released code only,
is protected, and only changes through an approved pull request from a release branch. Tags
are cut from `main` after that merge.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo clippy --target aarch64-apple-darwin --all-targets -- -D warnings
cargo test
```

Say which platforms you actually ran on. Sign off commits (`git commit -s`). See
`CONTRIBUTING.md` for layout and process, `SECURITY.md` for the threat model and reporting.

## For coding agents specifically

- Read `docs/architecture.md` before changing anything under `src/server` or `src/desktop`.
- Do not weaken a check to make a test pass. If a security check blocks a legitimate use, raise
  it in the PR description instead.
- Do not run `rdc serve` on the developer's machine without being asked; it exposes their desktop
  to the allowlist. Use `--target local` or `--dev-loopback` for testing.
- Keep prose in docs plain: state facts, commands and numbers; no metaphors or summarising quips.
