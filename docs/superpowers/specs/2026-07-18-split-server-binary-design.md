# Split the session server into its own binary

Date: 2026-07-18
Branch: `split-server-binary` (worktree, off `origin/main@093f274`)

## Context

`horsie serve` is a subcommand bundled inside the `horsie` CLI binary (package
`horsie`, in `cli/`), which also handles `daemon`/`job`/`plugin`/`validate`.
`docker/server.Dockerfile` builds the whole CLI binary and runs it as
`horsie serve ...` — the production server image ships daemon/job/plugin code
that is never invoked.

`horsie-server` (the `server` crate) is already a separate **library** crate;
the CLI just wraps it. The wrapper (`cli/src/serve.rs`) does real bootstrap
work beyond calling into the library: resolving deployment config
(`HorsieConfig`), and — until this change — resolving a default sandbox
capability spec (`cli/src/capabilities.rs`) for sessions that omit one.

## Goal

Give the session server its own binary, fully independent of the `horsie` CLI
binary — its own `cargo build -p horsie-server` output, its own Docker image
build step, no dependency on the `cli` crate in either direction.

## Non-goals

- Rewriting `horsie-server`'s internals (`AppState`, vendors, session actor).
- User-facing documentation (tracked separately — this spec covers the binary
  split only).
- Removing `horsie-server`'s `[[lib]]` target — `tests/tests/session_server_e2e.rs`
  and `executor/tests/integration_test.rs` both depend on it as a library.
  Tracked as a follow-up: [blossomstack/horsie#9](https://github.com/blossomstack/horsie/issues/9).

## Architecture

`horsie-server` becomes **lib + bin**, mirroring how `horsie-runtime` already
works in this workspace (one package, `src/lib.rs` for the reusable API,
`src/main.rs`/`src/bin/*` for the executable). Concretely:

- New `server/src/bin/horsie-server/main.rs` (Cargo auto-discovers
  `src/bin/<name>/main.rs` as a bin target — no `[[bin]]` table needed).
  Owns clap arg parsing (`--config`, `--addr` default `127.0.0.1:3789`,
  `--web`) and the orchestration currently in `cli/src/serve.rs` (open the
  settings DB, build `GithubService`/`McpService`/`PluginService`, spawn
  `SessionSupervisor`, build `AppState`, bind + `axum::serve`).
- New `server/src/bin/horsie-server/config.rs`: a slim `BootConfig` struct —
  **only** the fields the server actually reads:
  - `storage { state_dir, data_dir, plugins_dir }`
  - `runtime { hook_path }` (no `bin` — that was only for spawning the old
    server-side-sandboxed local vendor, which PR #8 already replaced with the
    user-launched `LocalDaemonVendor`)
  - `local_runtime_listen: Option<String>`
  - `database { url }`

  Drops `providers`/`models`/`hackamore`/`velos`/`default_vendor` — those stay
  CLI/job-daemon-only (`cli/src/config.rs`, untouched). The XDG-based default
  resolution (`state_dir`/`data_dir`/`plugins_dir` defaults, `--config`-else-
  user-config-else-empty precedence) is duplicated verbatim from
  `cli/src/config.rs`'s equivalent free functions — same behavior, trimmed
  struct.
- A small bin-local error enum for config-loading failures (not the shared
  `server::error::ServerError`, which covers runtime/executor errors).
- `server/src/lib.rs` and its `[[lib]]` target are unchanged.

### Capability resolution: dropped, not duplicated

`AppState`'s own doc comment already states the intent: `caps_finalize`
"finalizes a request-supplied capability spec (path expansion, plugin grants,
platform seatbelt rules) — **injected by the host binary**, which owns the
capability-resolution helpers." Nothing currently enforces this spec via nono
— the old server-spawned local vendor (the one consumer that did) was removed
in PR #8; the current vendors (`LocalDaemonVendor`, user-launched; `VelosVendor`,
container-isolated) don't apply it. The per-session `caps.json` file is still
written for structural reasons (`RuntimeSpec.capabilities_file`, "durable
source of truth a stopped runtime is revived against"), but nothing reads it
back for enforcement today.

So the new binary supplies the same trivial values the test `AppState`
constructor already uses:

```rust
caps_finalize: Arc::new(|caps| caps),   // identity — request-supplied specs pass through unchanged
default_caps: CapabilitySpec {          // fixed minimal default (== the existing test `block_caps()`)
    network: NetworkPolicy::Block(BlockNetwork {}),
    grants: vec![],
    unsafe_seatbelt_rules: None,
},
```

No `capabilities.rs`, no per-OS default JSON files, no `sandbox` section in
`BootConfig`, no `~`/`$HOME` expansion, no plugin-grant injection get
duplicated into the new binary. This is a pure removal, confined to what the
new binary computes — it doesn't touch `SessionSpec`/`RuntimeSpec`, the wire
protocol, or the web UI. A client can still send an explicit `capabilities`
override in a create request; it just passes through unmodified instead of
being merged with grants/seatbelt rules that nothing enforces anyway.

## `cli` crate changes

- Remove the `Serve` subcommand from `cli/src/main.rs`.
- Delete `cli/src/serve.rs`.
- Drop the `horsie-server` dependency from `cli/Cargo.toml`.

## Docker (`docker/server.Dockerfile`)

- Stage 2 (build): `cargo build --release --locked -p horsie-server` instead
  of `-p horsie`; copy `target/release/horsie-server` instead of `horsie`.
- Stage 3 (runtime): `ENTRYPOINT ["horsie-server"]`; default
  `CMD ["--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]`
  (drop the leading `serve` verb — the binary itself is the server now).
- Image name unchanged: `ghcr.io/blossomstack/horsie` (ops and the
  image-update workflow already key off this name; renaming it is out of
  scope).

## `ops` repo changes

Edited directly in the `ops` primary checkout (its own IaC file, no worktree):

- `ops/stacks/horsie/docker-compose.yml`: `command:` drops the leading
  `- serve` list item, keeps `--config`/`--addr`/`--web` as-is.

## Makefile

- Add `build-server` / `install-server` / `uninstall-server` targets,
  mirroring `build-cli` (`cargo build -p horsie-server`, install/remove
  `horsie-server` from `$(BINDIR)`).
- Fix a pre-existing bug found while touching this file: `build-cli` runs
  `cargo build -p cli -p runtime`, but those are directory names, not package
  names (`cargo pkgid cli` / `cargo pkgid runtime` both fail — confirmed
  empirically). The real package names are `horsie` and `horsie-runtime`.
  Fix to `cargo build -p horsie -p horsie-runtime`.
- Update the top-of-file comment and `help` target text (three binaries now:
  `horsie`, `horsie-runtime`, `horsie-server`).

## CI / release

- `.github/workflows/ci.yml`: no change (`cargo test --workspace
  --all-features` already covers the new bin target).
- `.github/workflows/docker.yml`: no change (keys off Dockerfile paths, not
  cargo package names).
- `.github/workflows/publish.yml` (`release-binaries` job): add
  `horsie-server` alongside `horsie`/`horsie-runtime` in the build command and
  the release tarball, for parity with bare-metal self-hosters.

## Verification plan

- `cargo build -p horsie-server` and `cargo build -p horsie` both succeed
  independently (no cross-dependency between them).
- `make check` (fmt + clippy + tests) passes for the whole workspace.
- `docker build -f docker/server.Dockerfile -t horsie-server-test .` succeeds
  locally; run the resulting container and confirm `GET /api/health` responds
  and the web UI serves at `/`.
- `docker compose -f ops/stacks/horsie/docker-compose.yml config` validates
  the edited compose file (syntax/structure only — not a live deploy to the
  homelab).
