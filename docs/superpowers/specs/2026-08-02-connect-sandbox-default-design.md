# Sandbox-by-default for `horsie connect` — design

**Date:** 2026-08-02
**Status:** approved (design), pending implementation plan

> **Revision note (same day):** the first draft of this spec assumed the CLI
> still owned the capability-resolution pipeline (`capabilities.rs`,
> `builtin_default()`, `sandbox.capabilities_file` config). That pipeline was
> deleted by #114 ("remove workflow commands and daemon dependencies") hours
> before implementation started. This revision is written against the post-#114
> tree. The approved decisions are unchanged: sandbox default-on, `--no-sandbox`
> replaces `--sandbox`, fail-closed startup probe, `RuntimeVendor` library
> default untouched.

## Problem

A plain `horsie connect` spawns `horsie-runtime` children **unsandboxed**: the
server always sends a per-session `sandbox_capabilities` spec
(`server/src/runtime_vendor/link.rs::runtime_spec`), but the vendor agent only
honors it when started with the opt-in `--sandbox` flag.

## Current-state facts that shape the design

- The server authors session capabilities but its fallback `default_caps`
  (`server/src/bin/horsie-server/main.rs`) is a deliberately minimal
  placeholder — `network: Block`, **zero grants** — justified by a comment
  that "no vendor enforces the per-session capability spec today". The moment
  `connect` enforces by default, that placeholder confines runtimes to nothing
  (no workspace access, no system reads) and breaks every session that didn't
  supply explicit capabilities. **The server default must become real as part
  of this change.**
- The runtime skips `Dir`/`File` grants whose paths don't exist on the host
  (`runtime/src/sandbox.rs::build_capability_set`) and ignores Seatbelt rules
  on Linux, so a single cross-platform default spec is safe to send to any
  vendor.
- The vendor agent already merges machine-local plugin-library grants into the
  server spec when writing the per-runtime caps file
  (`runtime-vendor/src/vendor.rs::write_caps_file` →
  `horsie_support::plugin::grants::plugin_library_grants`). No work needed.
- The runtime has a `probe` subcommand: `horsie-runtime probe --workspace
  probe=<dir> --sandbox-caps <file>` exits 0 (sandbox applied), 3
  (unsupported), in milliseconds — no endpoint, no connect-retry budget.
- The deleted CLI default specs survive as unreferenced files at
  `cli/src/capabilities/default.{linux,macos}.json` — the source material for
  the new server default.

## Decisions (from brainstorming)

- Sandbox becomes the **default** for `horsie connect`.
- Opt out with `--no-sandbox`. The `--sandbox` flag is removed outright (not
  kept as a no-op).
- On a host where nono cannot apply, `connect` **probes at startup and refuses
  to start** with an error pointing at `--no-sandbox` (fail closed).
- The `RuntimeVendor` library default (`sandbox: false`) is unchanged — connect
  passes an explicit value; no other consumer's behavior changes.

## Server: a real default capability spec

New `server/src/default_capabilities.rs` + embedded
`server/src/default_capabilities.json`:

- The JSON is the **union** of the old `default.linux.json` and
  `default.macos.json` grant lists (runtime skips paths absent on the host),
  plus the macOS Seatbelt rule `(allow mach-lookup (global-name
  "com.apple.SecurityServer"))` in `unsafe_seatbelt_rules` (ignored on Linux).
  Network stays `Block`; `WorkingDir` is granted `ReadWrite`.
- `pub fn default_capabilities() -> Result<CapabilitySpec, String>` parses the
  embedded JSON (workspace lints deny `unwrap`/`expect` in production code).
- `horsie-server/main.rs` builds `AppState.default_caps` from it (startup
  error → fatal, fail loud) and its "no vendor enforces" comment is rewritten:
  connect now enforces by default.
- `cli/src/capabilities/` (dead since #114) is deleted; the stale pointer in
  `runtime/Cargo.toml:39` (`see cli/src/capabilities.rs`) is updated to the
  server module.

Session requests that supply explicit capabilities are untouched
(`caps_finalize` pass-through, provisioning network-allow override in
`http/handlers.rs`).

## CLI surface (`cli/src/main.rs`)

- Remove `sandbox: bool` from `Command::Connect`.
- Add `no_sandbox: bool` as `--no-sandbox`:
  *"Do not sandbox the runtimes this agent spawns (the server's capability
  spec is ignored)."*
- The `Command::Connect` match arm passes `sandbox: !no_sandbox` into
  `connect::run`; the `sandbox: bool` parameter of `connect::run` is unchanged.

## Startup probe (`cli/src/connect.rs`)

When `sandbox == true`, after `create_dir_all(state_dir)` and before binding
the runtime socket:

1. **Probe spec** — constructed in code (no config, no embedded JSON):
   `CapabilitySpec { network: Block, grants: [WorkingDir ReadWrite],
   unsafe_seatbelt_rules: None }`. The probe only proves nono can apply a spec
   on this host; it does not pre-validate server specs.
2. **Write** it to `<state_dir>/probe-capabilities.json` (overwrite each
   start).
3. **Probe**: run
   `runtime_bin probe --workspace probe=<state_dir> --sandbox-caps <file>`
   synchronously and classify the exit status:
   - **0** → proceed with startup.
   - **3 / other non-zero / signal / spawn failure** → return
     `CliError::Validation`:
     *"nono sandbox is not supported on this host; re-run with `--no-sandbox`
     to spawn unsandboxed runtimes."*
   - Classification is a small pure function over `Option<i32>` for unit
     tests.

The probed binary is the same `runtime_bin` the agent spawns per session, so
this is a true probe of the production path.

## Failure modes

- Probe caps file unwritable → `CliError::Io`, startup aborts (fail closed).
- `--no-sandbox` skips the probe entirely; behavior is identical to today's
  plain `connect`.
- A *server-sent* spec the runtime rejects at session time is unchanged from
  today's `--sandbox` behavior (the runtime child exits, the session errors).

## Behavior changes to call out

- Sandboxed runtimes get a **scrubbed environment**
  (`runtime-vendor/src/env_scrub.rs`); unsandboxed runtimes inherit the full
  ambient env. Default-on means ambient env inheritance goes away unless
  `--no-sandbox`.
- Network inside the sandbox follows the server-sent spec (default: blocked;
  LLM calls are server-side, so model access is unaffected).

## Docs / comments

- `cli/src/main.rs` connect help text.
- `docs/guide/getting-started.md:48` and `docs/guide/runtime-vendors.md:48`
  (`--sandbox` mentions, "off by default" wording).
- `runtime-vendor/src/vendor.rs` doc comments (`sandbox` field,
  `with_sandbox`) updated: connect sandboxes by default, `--no-sandbox` opts
  out; the library default stays off.

## Testing

- `connect.rs` unit tests: probe classification over `Option<i32>` (0 → ok;
  3 / other / None → refusal error naming `--no-sandbox`); probe skipped when
  `sandbox == false`.
- `server/src/default_capabilities.rs` unit tests: embedded JSON parses;
  grants `WorkingDir` read-write; network is `Block`; the SecurityServer
  Seatbelt rule is present.
- `cli/tests/connect_e2e.rs`: the two long-lived `horsie connect` spawns get
  `--no-sandbox` (they exercise the vendor chain, not the sandbox, and must
  not become host-dependent on nono support). The `--background` refusal test
  exits before the probe and needs no change.
- Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`.
