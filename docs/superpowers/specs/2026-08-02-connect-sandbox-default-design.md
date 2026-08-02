# Sandbox-by-default for `horsie connect` — design

**Date:** 2026-08-02
**Status:** approved (design), pending implementation plan

## Problem

A plain `horsie connect` spawns `horsie-runtime` children **unsandboxed**: the
server always sends a per-session `sandbox_capabilities` spec
(`server/src/runtime_vendor/link.rs::runtime_spec`), but the vendor agent only
honors it when started with the opt-in `--sandbox` flag. velos runtimes and
`connect --sandbox` are confined by nono; the default `connect` path is not.

## Decision (from brainstorming)

- Sandbox becomes the **default** for `horsie connect`.
- Opt out with `--no-sandbox`. The `--sandbox` flag is removed outright (not
  kept as a no-op).
- On a host where nono cannot apply, `connect` **probes at startup and refuses
  to start** with an error pointing at `--no-sandbox` (fail closed).
- The `RuntimeVendor` library default (`sandbox: false`) is unchanged — connect
  passes an explicit value; no other consumer's behavior changes.

## CLI surface (`cli/src/main.rs`)

- Remove `sandbox: bool` from `Command::Connect`.
- Add `no_sandbox: bool` as `--no-sandbox`:
  *"Do not sandbox the runtimes this agent spawns (the server's capability
  spec is ignored)."*
- The `Command::Connect` match arm passes `sandbox: !no_sandbox` into
  `connect::run`; the `sandbox: bool` parameter of `connect::run` is unchanged.

## Startup probe (`cli/src/connect.rs`)

When `sandbox == true`, before binding the runtime socket:

1. **Resolve the probe spec** (same pipeline the daemon uses, so the probe
   matches what runtimes actually face):
   - `sandbox.capabilities_file` from the resolved `HorsieConfig` if set
     (loaded from disk), else `capabilities::builtin_default()`;
   - then apply `resolve_user_paths`, `with_default_seatbelt_rules`, and
     `with_plugin_grants`.
   - `connect::run` gains one parameter, `capabilities_file: Option<PathBuf>`,
     passed from the caller's already-resolved `cfg.sandbox.capabilities_file`,
     keeping spec resolution inside the CLI crate (which owns
     `capabilities.rs`).
2. **Write it** to `<state_dir>/probe-capabilities.json` (the state dir is
   already created by `run`; overwrite on each start — no tempdir lifecycle).
3. **Probe**: spawn
   `runtime_bin probe --workspace probe=<state_dir> --sandbox-caps <file>`
   synchronously and classify the exit status (the runtime's `probe`
   subcommand contract: 0 = sandbox applied, 3 = unsupported, anything else =
   cannot prove confinement):
   - **0** → proceed with startup.
   - **3 / other non-zero / signal / spawn failure** → return
     `CliError::Validation`:
     *"nono sandbox is not supported on this host; re-run with `--no-sandbox`
     to spawn unsandboxed runtimes."*

The probed binary is the same `runtime_bin` the agent spawns per session, so
this is a true probe of the production path and returns in milliseconds (no
endpoint, no connect-retry budget).

## Failure modes

- Probe caps file unwritable/unreadable → `CliError::Io`, startup aborts
  (fail closed).
- `--no-sandbox` skips the probe entirely; behavior is identical to today's
  plain `connect`.
- A *server-sent* spec the runtime rejects at session time is unchanged from
  today's `--sandbox` behavior (the runtime child exits, the session errors);
  the probe does not attempt to validate arbitrary future server specs.

## Behavior changes to call out

- Sandboxed runtimes get a **scrubbed environment**
  (`runtime-vendor/src/env_scrub.rs`); unsandboxed runtimes inherit the full
  ambient env. Default-on means ambient env inheritance goes away unless
  `--no-sandbox`.
- Network inside the sandbox follows the server-sent spec (LLM calls are
  server-side, so this does not affect model access).

## Docs / comments

- `cli/src/main.rs` help text for connect.
- `docs/guide/getting-started.md` and `docs/guide/runtime-vendors.md`
  (`--sandbox` mentions, "off by default" wording).
- `runtime-vendor/src/vendor.rs` doc comment ("`horsie connect --sandbox`
  turns it on") updated to describe the new default.

## Testing

- Unit tests in `connect.rs` alongside source:
  - probe classification as a small pure function over `Option<i32>` exit
    status (0 → ok; 3 / other / None → refusal error);
  - probe skipped when `sandbox == false`.
- Existing `cli/tests/sandbox_e2e.rs` probe harness already uses the same
  `probe` contract; `cli/tests/connect_e2e.rs` continues to pass.
- Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`.
