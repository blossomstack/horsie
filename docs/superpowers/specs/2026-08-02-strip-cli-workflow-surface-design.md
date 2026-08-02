# Strip the workflow surface from the `cli` crate

Date: 2026-08-02

## Goal

The `horsie` CLI is drifting toward two different jobs: the local daemon
(`horsie validate` / `horsie job` / `horsie daemon`, workflow files) and the
session-server client (`horsie plugin` / `horsie session` / `horsie connect`).
This change drops the local-workflow half from the CLI crate, so `horsie` is
only a session-server client, and removes every workflow-related Cargo
dependency from it. The `horsie-workflow` / `horsie-supervisor` / daemon crates
stay in the workspace — the server and `tests` crate still need them. Only the
CLI's dependency on them is removed.

## Scope

### 1. Commands (main.rs)

Remove:

- `Command::Validate` (and `do_validate`, `load_workflow`)
- `Command::Job` (and `JobAction`, plus `build_submit`, `load_hackamore_policy`,
  `print_job_status`, `active_label`, `humanize`, `now_ms`, and the three
  hackamore unit tests)
- `Command::Daemon` (and `DaemonAction`, plus `spawn_background_daemon`,
  `resolve_state_dir`)

Keep: `Command::Plugin`, `Command::Session`, `Command::Connect`, and
`resolve_plugins_dir`.

Update the clap `about` string — it currently says "Run agent workflows in a
nono-sandboxed runtime, supervised by a local daemon" — to describe the CLI's
remaining role (session-server client: runtime vendor, session tail, plugin
library).

### 2. Modules (lib.rs + src tree)

Delete:

- `src/validate.rs`
- `src/client.rs` (daemon client: submit/list/status/logs/stop/resume/remove)
- `src/capabilities.rs` (sandbox capability file parsing; only used by the
  daemon path and `sandbox_e2e.rs`)
- `src/daemon/` (mod.rs + protocol.rs — the whole daemon)

`lib.rs` keeps only `config`, `connect`, `error`, `plugins`, `session`.

Move `default_runtime_bin()` (currently `daemon/mod.rs:34`; only consumer is
`connect`) into `connect.rs` as `pub fn default_runtime_bin() -> PathBuf`;
main.rs calls `connect::default_runtime_bin()`.

### 3. Config trim (config.rs)

Keep only the fields `connect` / `plugin` / `session` read:

- `storage` (state_dir, data_dir, plugins_dir)
- `runtime` (bin, hook_path)

Delete the fields `providers`, `models`, `sandbox`, `hackamore`, `velos`,
`default_vendor`, `local_runtime_listen`, `database`; the types
`ProviderConfig`, `ModelConfig`, `SandboxConfig`, `HackamoreConfig`,
`VelosVendorConfig`, `DatabaseConfig`; the functions `build_registry`,
`build_registry_from`, and the velos default helpers.

Keep `HorsieConfig::load` / `resolve` / `resolve_path`, the storage-dir
defaults (`default_state_dir`, `default_data_dir`, `default_plugins_dir`,
`storage_dir_from`, `user_config_path`), and `Secret` is no longer needed
(velos/provider keys were its only consumers) — remove its import.

Old config files still parse: serde ignores unknown JSON fields, so a
`config.json` written for the daemon (with providers/models/sandbox/hackamore)
deserializes fine and the extra fields are dropped.

Remove the config unit tests that covered deleted fields (hackamore, velos,
default_vendor, local_runtime_listen, registry building, sandbox
capabilities_file, providers/models parsing). Keep the storage/runtime/XDG
tests.

### 4. Cargo dependencies (cli/Cargo.toml)

Remove:

- `horsie-workflow`, `horsie-supervisor`, `horsie-actor`, `horsie-agentcore`,
  `horsie-anthropic`, `horsie-openai`, `horsie-runtime-client`
- `eval` (workflow conditions), `uuid` (unused after removal), `async-trait`
  (unused after removal), `tracing` (daemon-only)
- dev-dep `horsie-mock-llm` (sandbox_e2e only)

Keep:

- `horsie-models` (session), `horsie-runtime-vendor` (connect)
- `clap`, `serde`, `serde_json`, `tokio`, `tokio-util`, `futures-util`,
  `reqwest`, `reqwest-eventsource`, `thiserror`
- dev-deps `tempfile`, `tokio-tungstenite`

### 5. Tests

Delete `cli/tests/sandbox_e2e.rs` (supervisor/daemon harness; the only
production use of `horsie_workflow` in the CLI crate).

Keep `cli/tests/connect_e2e.rs`; fix its doc comment, which references
`cli/src/daemon/mod.rs`'s `default_runtime_bin` → `connect.rs`.

### 6. Docs

Update `docs/guide/README.md:10` ("it does not cover the separate `horsie` CLI
(`horsie job`/`horsie daemon` and workflow files)") to describe the CLI's
actual remaining role. No other docs reference the removed commands.

## Out of scope

- Removing the `horsie-workflow`, `horsie-supervisor`, or daemon-related crates
  from the workspace — the server and `tests` crate still depend on them.
- Changing the session server, runtime-vendor, or runtime crates.

## Verification

- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo fmt --check` (stable toolchain)
- `cargo test --workspace`
- `cargo build -p horsie` plus `horsie --help` smoke test to confirm the
  remaining command surface (`plugin`, `session`, `connect`).
