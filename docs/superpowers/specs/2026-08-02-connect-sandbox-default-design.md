# Sandbox-by-default for `horsie connect` — design

**Date:** 2026-08-02
**Status:** approved (design), implemented

> **Revision 2 (same day):** after reviewing revision 1's implementation, the
> user decided the capability document does not belong on the server at all:
> the local vendor owns the workspaces, plugins, and machine resources, and the
> server only relays tool calls. Revision 1's server-side default spec
> (`server/src/default_capabilities.json`) is removed again; the baseline moves
> into the vendor crate, and the server→vendor wire no longer carries
> capability specs. Revision 1 also noted the loss of the CLI's capability
> pipeline in #114.
>
> **Revision 1 (superseded):** server authors a real cross-platform default
> spec; vendor merges machine-local grants. Rationale for dropping it: the
> server knows least about the vendor's machine yet authored its whole
> confinement policy, and the abstraction leaked — the runtime fetches plugin
> bundles over HTTP from the server *inside* the sandbox, which a
> network-blocking server default can't permit because the server doesn't
> model itself as a resource of the vendor's machine.

## Problem

A plain `horsie connect` spawns `horsie-runtime` children **unsandboxed**:
nono confinement was only applied with the opt-in `--sandbox` flag, and the
spec it applied was authored by the server.

## Decisions (final)

- Sandbox becomes the **default** for `horsie connect`; `--no-sandbox` opts
  out. The `--sandbox` flag is removed outright.
- **The vendor owns the capability document.** The server→vendor protocol
  (`runtime_vendor.fl::RuntimeSpec.sandbox_capabilities`) and the session API
  (`session_api.fl` create-session `capabilities`) no longer carry capability
  specs; the server has no `default_caps`/`caps_finalize` and writes no
  per-session `capabilities.json`.
- The vendor's **baseline spec** lives in the `horsie-runtime-vendor` crate
  (`baseline_capabilities.json` + `baseline_capabilities()`), embedded at
  compile time: system toolchain reads (cross-platform union — the runtime
  skips host-absent grants and ignores Seatbelt rules off macOS), `WorkingDir`
  read-write (the runtime resolves it to each runtime's actual workspace
  dirs), device nodes, the macOS SecurityServer TLS rule, and **network
  Allow** — a local vendor's tools (git/cargo) and the runtime's own
  plugin-bundle fetch from the server need egress; filesystem confinement is
  the actual boundary.
- `write_caps_file` (vendor.rs) already merges machine-local plugin-library
  grants into whatever spec it writes; with the baseline as input, the
  per-runtime caps file = baseline + plugin grants.
- Fail-closed **startup probe** (unchanged from revision 1): when sandboxing
  is on, connect runs `horsie-runtime probe` against a minimal spec and
  refuses to start on hosts where nono can't apply, naming `--no-sandbox`.
- The `RuntimeVendor` library default (`sandbox: false`) is unchanged — the
  CLI passes an explicit value; velos (`sandbox` off, container as boundary)
  is unaffected.
- The daemon-era job runner (`supervisor`, `daemon.fl`) has its own
  capability plumbing and is **out of scope** — the server does not use it.

## Removal surface (server side)

- `models/fluorite/session_api.fl`: create-session request drops
  `capabilities`.
- `models/fluorite/runtime_vendor.fl`: `RuntimeSpec` drops
  `sandbox_capabilities`.
- `server/src/http/handlers.rs`: caps resolution (incl. the provisioning
  network-allow override) deleted.
- `server/src/http/mod.rs`: `AppState` drops `default_caps` and
  `caps_finalize`.
- `server/src/runtime_manager.rs` + `server/src/runtime_vendor/{mod,link,fake}.rs`:
  no per-session caps file, no inlining.
- `server/src/sessions/spec.rs`: `SessionSpec` drops `capabilities` (serde
  ignores the unknown field in old persisted rows).
- `server/src/bin/horsie-server/main.rs`: default caps wiring removed;
  `server/src/default_capabilities.{rs,json}` deleted (added in revision 1 of
  this branch).
- TS clients: `make ts-types` + web `generate-types` regenerate; no
  hand-written client code uses the field.

## Vendor side (detail)

- `runtime-vendor/src/baseline.rs`:
  `pub fn baseline_capabilities() -> Result<CapabilitySpec, String>` parsing
  the embedded JSON (no `expect` in production code — workspace lints).
- `vendor.rs::provision`: when `self.sandbox` is on, always write the per-
  runtime caps file from the baseline (merged with plugin grants); when off,
  no file and no `--sandbox-caps`, as today. The revive/`GetRuntime` path
  re-runs `provision`, so a recovered runtime gets a freshly written baseline
  file — no stale server spec to worry about.
- `connect.rs`: probe + `--no-sandbox` as implemented in revision 1.
- `cli/tests/connect_e2e.rs`: wire fixtures drop `sandbox_capabilities`;
  long-lived agent spawns keep `--no-sandbox`.

## Testing

- `baseline.rs` unit tests: parses, WorkingDir RW, network Allow, SecurityServer
  rule, system toolchain read grants.
- `connect.rs` unit tests: probe verdict classification (revision 1, kept).
- `vendor.rs` tests: sandboxed provision writes a caps file containing the
  baseline plus plugin grants.
- Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`, TS typecheck/drift.
