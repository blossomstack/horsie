# Crate Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take the horsie workspace from 12 crates to 10 by merging two pairs, and make the set of crates published to crates.io exactly the dependency closure of the installable binaries.

**Architecture:** Two pure refactors with no behaviour change — `horsie-runtime-client` + `horsie-runtime-vendor` become `horsie-runtime-host`, and `horsie-workflow` becomes `crates/server/src/agent_loop/`. Then a shell guard, wired into CI, that recomputes the publish surface from `cargo metadata` and fails if it drifts from the binaries' closure. The guard is written before the flags are fixed, so it goes red then green.

**Tech Stack:** Rust 2024 edition, cargo workspaces, `cargo metadata` / `cargo tree`, jq, bash, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-09-crate-consolidation-design.md`

## Global Constraints

- **Rust toolchain is pinned to 1.96.0** (`RUST_TOOLCHAIN` in `.github/workflows/ci.yml` and `publish.yml`). Do not bump it.
- **All workspace crates stay at version `0.1.6`.** `publish.yml`'s `version-guard` job compares the git tag against *every* workspace package, including `publish = false` ones. A new crate must be created at `0.1.6`.
- **Workspace lints are deny-level:** `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. Test code opts out per-file via `#![cfg_attr(test, allow(clippy::unwrap_used, ...))]`. Moved files keep whatever opt-out header they already have.
- **A published crate's path dependencies must carry `version = "0.1.6"`.** Path-only dev-dependencies are stripped by cargo on publish and need no version.
- **No behaviour changes.** Both merges are file moves plus import rewrites. If a test needs its assertions changed, something has gone wrong — stop and re-read the spec.
- **Do not touch `crates/models/fluorite/*.fl`.** Two of them are named `executor.fl` and `runtime_vendor.fl` after the old crate layout, which invites a "tidy-up" rename. Resist it: those names are wire-schema filenames, not crate names, and editing a `.fl` requires regenerating two separate generated type trees. CI's `ts-types` job fails on any drift. Renaming them is a separate change.
- **Never add `Co-Authored-By` trailers or AI attribution** to commits.
- **Work in the worktree** `.claude/worktrees/crate-consolidation` on branch `crate-consolidation`.

## Verification cost

Full-workspace runs are slow. While iterating inside a task use `cargo build -p <crate>` / `cargo test -p <crate>`. Run the full `cargo test --workspace` once at the end of each task, and never twice in the same command.

---

## File Structure

**New:**
- `crates/runtime-host/` — the merged host side of the runtime wire (Task 1)
- `crates/server/src/agent_loop/` — the former `horsie-workflow` (Task 2)
- `scripts/check-publish-surface.sh` — the publish-surface guard (Task 3)

**Deleted:**
- `crates/runtime-client/`, `crates/runtime-vendor/` (Task 1)
- `crates/workflow/` (Task 2)

**Modified:** `crates/{cli,runtime,server,tests}/Cargo.toml`, ~30 `.rs` files across `server`, `tests`, `cli`, `runtime`, plus `crates/actor/Cargo.toml` and `.github/workflows/ci.yml`.

---

### Task 1: Merge `runtime-client` + `runtime-vendor` into `horsie-runtime-host`

The vendor half already depends on the client half. Merging them adds no dependency edge for any consumer, and the two crates' module names and exported symbols are disjoint — verified against both `lib.rs` files. This is a file move plus a mechanical rename.

**Files:**
- Create: `crates/runtime-host/Cargo.toml`, `crates/runtime-host/src/lib.rs`
- Move: all of `crates/runtime-client/src/` and `crates/runtime-vendor/src/` into `crates/runtime-host/src/`; `crates/runtime-vendor/tests/vendor_conformance.rs` into `crates/runtime-host/tests/`
- Delete: `crates/runtime-client/`, `crates/runtime-vendor/`
- Modify: `crates/cli/Cargo.toml`, `crates/runtime/Cargo.toml`, `crates/server/Cargo.toml`, `crates/tests/Cargo.toml`, `crates/workflow/Cargo.toml`
- Modify (imports): the 24 `.rs` files listed in Step 4

**Interfaces:**
- Produces: crate `horsie-runtime-host` exporting the union of the two old crates' public items, unchanged in name and signature. `RuntimeClient`, `RuntimeTransport`, `TransportError`, `HookSink`, `RuntimeCallError`, `add_runtime_tools`, `inbound_call_id`, `outbound_call_id`, `tools` (pub module), `testkit` (pub module, feature-gated) from the client half; `ConnectedRuntimeRegistry`, `SANDBOX_ENV_ALLOWLIST`, `scrubbed_env`, `CredentialError`, `ExecutorError`, `RuntimeError`, `IssuedTokens`, `handle_runtime_connection`, `serve_runtime_connections`, `ProcessRuntimeProvider`, `SandboxPolicy`, `HealthStatus`, `RuntimeHandle`, `RuntimeProvider`, `Backoff`, `AcceptedStream`, `RuntimeEndpoint`, `RuntimeListenerServer`, `runtime_vendor` (pub module), `SocketRuntimeTransport`, `UnixSocketRuntimeTransport`, `AgentExit`, `BundleDelivery`, `CredentialProvider`, `FixedWorkspaces`, `ProviderFactory`, `RuntimeVendorClient`, `WorkspaceResolver`, `no_credential` from the vendor half.
- Produces: feature `test-util`, forwarding to `horsie-agentcore/test-util`, gating `testkit`.

- [ ] **Step 1: Move the files with `git mv` so history follows**

The vendor half is the larger of the two, so move it first and rename the directory, then move the client half in on top. No filename collides — checked against both trees.

```bash
cd .claude/worktrees/crate-consolidation
git mv crates/runtime-vendor crates/runtime-host
git mv crates/runtime-client/src/client.rs      crates/runtime-host/src/client.rs
git mv crates/runtime-client/src/testkit.rs     crates/runtime-host/src/testkit.rs
git mv crates/runtime-client/src/transport.rs   crates/runtime-host/src/transport.rs
git mv crates/runtime-client/src/tools          crates/runtime-host/src/tools
git rm -q crates/runtime-client/src/lib.rs crates/runtime-client/Cargo.toml
rmdir crates/runtime-client/src crates/runtime-client 2>/dev/null || true
```

- [ ] **Step 2: Write the merged `crates/runtime-host/Cargo.toml`**

Dependencies are the union of the two. `tokio` takes the vendor half's `process`/`net` features; `tokio-util` and `rand` come from the vendor half; `agentcore` from the client half. The self-referential dev-dependency replaces the old `horsie-runtime-client` dev-dependency, and is the same pattern `horsie-server` already uses to enable its own `test-util` for integration tests.

```toml
[package]
name = "horsie-runtime-host"
license = "MIT OR Apache-2.0"
repository = "https://github.com/blossomstack/horsie"
description = "The host side of the horsie runtime wire: dialling into a runtime, and supplying one"
version = "0.1.6"
edition = "2024"

[features]
# Exposes `testkit` (MockTransport and its fault modes). Pulls in agentcore's
# testkit for `Script`.
test-util = ["horsie-agentcore/test-util"]

[dependencies]
horsie-models = { version = "0.1.6", path = "../models" }
horsie-agentcore = { version = "0.1.6", path = "../agentcore" }
horsie-support = { version = "0.1.6", path = "../support" }
async-trait       = { workspace = true }
thiserror         = { workspace = true }
tokio             = { workspace = true, features = ["process", "net"] }
tokio-tungstenite = { workspace = true }
tokio-util        = { workspace = true }
futures-util      = { workspace = true }
serde_json        = { workspace = true }
uuid              = { workspace = true }
rand              = { workspace = true }

[dev-dependencies]
# Enables this crate's own `test-util` for the integration tests in tests/.
horsie-runtime-host = { path = ".", features = ["test-util"] }
horsie-server = { path = "../server" }
tempfile = "3"

[lints]
workspace = true
```

- [ ] **Step 3: Write the merged `crates/runtime-host/src/lib.rs`**

The module list is the union in alphabetical order; the `pub use` list is the concatenation of the two old ones, re-sorted. The doc comment on `pub mod runtime_vendor` is carried over verbatim — it documents a live migration, not a style choice.

```rust
//! The host side of the runtime wire.
//!
//! Two halves of one protocol: dialling into a runtime ([`RuntimeClient`],
//! [`RuntimeTransport`]) and supplying one ([`RuntimeVendorClient`], the
//! listener, the credential provider). The sandboxed child process at the far
//! end of that wire is `horsie-runtime`.

mod baseline;
mod client;
mod connected_registry;
mod env_scrub;
mod error;
mod issued_tokens;
mod listener;
mod process_provider;
mod provider;
mod reconnect;
mod runtime_listener;
/// The vendor contract. A public module rather than a root re-export while the
/// old `provider::RuntimeHandle` still exists: two traits of that name would be
/// a genuine ambiguity for a reader, and the old one is deleted as each vendor
/// is ported onto this one.
pub mod runtime_vendor;
mod socket_transport;
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
pub mod tools;
mod transport;
mod vendor;

pub use client::{HookSink, RuntimeCallError, RuntimeClient};
pub use connected_registry::ConnectedRuntimeRegistry;
pub use env_scrub::{SANDBOX_ENV_ALLOWLIST, scrubbed_env};
pub use error::{CredentialError, ExecutorError, RuntimeError};
pub use issued_tokens::IssuedTokens;
pub use listener::{handle_runtime_connection, serve_runtime_connections};
pub use process_provider::{ProcessRuntimeProvider, SandboxPolicy};
pub use provider::{HealthStatus, RuntimeHandle, RuntimeProvider};
pub use reconnect::Backoff;
pub use runtime_listener::{AcceptedStream, RuntimeEndpoint, RuntimeListenerServer};
pub use runtime_vendor::{
    RuntimeEvent, RuntimeHandleImpl, RuntimeHandleTransport, RuntimeProgress, RuntimeProgressSink,
    RuntimeVendorError,
};
pub use socket_transport::{SocketRuntimeTransport, UnixSocketRuntimeTransport};
#[cfg(any(test, feature = "test-util"))]
pub use testkit::{BlockHandle, MockTransport, TransportOutcome, TransportProbe};
pub use tools::add_runtime_tools;
pub use transport::{RuntimeTransport, TransportError, inbound_call_id, outbound_call_id};
pub use vendor::{
    AgentExit, BundleDelivery, CredentialProvider, FixedWorkspaces, ProviderFactory,
    RuntimeVendorClient, WorkspaceResolver, no_credential,
};
```

- [ ] **Step 4: Rewrite the imports**

Four files that moved *into* the crate referenced the client half as an external crate and must now use `crate::`:

```bash
cd .claude/worktrees/crate-consolidation
sed -i '' 's/horsie_runtime_client::/crate::/g' \
  crates/runtime-host/src/connected_registry.rs \
  crates/runtime-host/src/runtime_vendor.rs \
  crates/runtime-host/src/socket_transport.rs \
  crates/runtime-host/src/vendor.rs
```

Everything else keeps naming the crate externally, under its new name. `tests/vendor_conformance.rs` is an integration test, so it stays external even though it lives in the crate:

```bash
sed -i '' 's/horsie_runtime_client/horsie_runtime_host/g; s/horsie_runtime_vendor/horsie_runtime_host/g' \
  crates/runtime-host/tests/vendor_conformance.rs \
  crates/cli/src/connect.rs \
  crates/runtime/tests/provision_steps.rs \
  crates/server/src/runtime_manager.rs \
  crates/server/src/users.rs \
  crates/server/src/http/runtime_connect.rs \
  crates/server/src/runtime_vendor/config.rs \
  crates/server/src/runtime_vendor/fake.rs \
  crates/server/src/runtime_vendor/fly.rs \
  crates/server/src/runtime_vendor/mod.rs \
  crates/server/src/runtime_vendor/transport.rs \
  crates/server/src/runtime_vendor/velos.rs \
  crates/server/src/runtime_vendor/websocket.rs \
  crates/server/src/sessions/session_actor/context.rs \
  crates/server/src/sessions/session_actor/hooks.rs \
  crates/tests/tests/vendor_reconnect_e2e.rs \
  crates/workflow/src/context.rs \
  crates/workflow/src/mcp_toolbox.rs \
  crates/workflow/src/workspace.rs \
  crates/workflow/tests/workspace_context.rs
```

Then confirm nothing was missed anywhere in the tree, including comments and docs:

```bash
grep -rn "runtime_client\|runtime-client\|runtime_vendor::\|horsie-runtime-vendor" \
  crates/ docs/src scripts/ Makefile .github/ 2>/dev/null | grep -v "^crates/server/src/runtime_vendor/" | grep -v target
```

Expected: no hits for `runtime-client`/`runtime_client`. Hits naming `server/src/runtime_vendor` (the server's own module, unrelated) and `horsie_runtime_host::runtime_vendor::` (the public module) are correct and stay.

- [ ] **Step 5: Point the four consumer manifests at the new crate**

`crates/cli/Cargo.toml` — replace the `horsie-runtime-vendor` line:

```toml
horsie-runtime-host = { version = "0.1.6", path = "../runtime-host" }
```

`crates/runtime/Cargo.toml` — in `[dev-dependencies]`, replace the `horsie-runtime-vendor` line:

```toml
horsie-runtime-host = { path = "../runtime-host" }
```

`crates/server/Cargo.toml` — the feature and both dependency lines collapse to one each:

```toml
[features]
# Exposes `vendor::mock::MockVendor` to external test crates. Off by default, so
# a production build of horsie-server never contains a mock runtime vendor.
test-util = ["horsie-runtime-host/test-util"]
```

In `[dependencies]`, delete the `horsie-runtime-vendor` and `horsie-runtime-client` lines and add:

```toml
horsie-runtime-host    = { path = "../runtime-host" }
```

In `[dev-dependencies]`, replace the `horsie-runtime-client` line:

```toml
horsie-runtime-host = { path = "../runtime-host", features = ["test-util"] }
```

`crates/tests/Cargo.toml` — in `[dev-dependencies]`, delete the `horsie-runtime-vendor` line and replace the `horsie-runtime-client` line:

```toml
horsie-runtime-host = { path = "../runtime-host", features = ["test-util"] }
```

`crates/workflow/Cargo.toml` — replace the `horsie-runtime-client` line in `[dependencies]`:

```toml
horsie-runtime-host = { version = "0.1.6", path = "../runtime-host" }
```

and in `[dev-dependencies]`:

```toml
horsie-runtime-host = { path = "../runtime-host", features = ["test-util"] }
```

- [ ] **Step 6: Build the new crate alone**

```bash
cargo build -p horsie-runtime-host --all-features
```

Expected: PASS. If it fails on an unresolved `crate::` path, a file from the vendor half referenced something in the client half that Step 4's `sed` missed — grep that file for `horsie_runtime`.

- [ ] **Step 7: Format, then lint**

`cargo fmt` before clippy — clippy reports formatting-adjacent lints that disappear once the moved files are reformatted, and chasing them first wastes a cycle.

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [ ] **Step 8: Run the full workspace suite**

```bash
TMPDIR=/tmp cargo test --workspace --all-features
```

`TMPDIR=/tmp` because the default macOS `$TMPDIR` is long enough to overflow `sockaddr_un.sun_path`, and this crate binds Unix sockets.

Expected: PASS, same test count as before the move.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "refactor: merge runtime-client and runtime-vendor into horsie-runtime-host"
```

---

### Task 2: Merge `horsie-workflow` into `crates/server/src/agent_loop/`

The crate's package description ("Agent workflow graphs for horsie") is wrong — its own module doc says "The agent loop on top of the event-sourced `actor` runtime", and that is what it is. It lands as `agent_loop`, **not** `workflow`: `server/src/workflows/` (the graph feature) and `server/src/sessions/workflow/` already exist, and a third `workflow` naming a fourth unrelated thing would actively mislead.

**Files:**
- Create: `crates/server/src/agent_loop/mod.rs`
- Move: the nine modules of `crates/workflow/src/` into `crates/server/src/agent_loop/`; `crates/workflow/tests/workspace_context.rs` into `crates/server/tests/`
- Delete: `crates/workflow/`
- Modify: `crates/server/src/lib.rs`, `crates/server/Cargo.toml`, `crates/tests/Cargo.toml`, `crates/support/src/plugin/skills.rs`
- Modify (imports): the 20 `server/src` files and `crates/tests/tests/agent_recovery_e2e.rs`

**Interfaces:**
- Consumes: `horsie-runtime-host` from Task 1 — `agent_loop` files already say `horsie_runtime_host::` after Task 1 Step 4 and need no further change.
- Produces: `horsie_server::agent_loop::*`, re-exporting exactly what `horsie_workflow` exported: `AgentActor`, `AgentCommand`, `AgentDomainEvent`, `AgentObserver`, `AgentParams`, `AgentState`, `AgentStateView`, `AgentUsageSnapshot`, `ReadOutcome`, `ReplayWindow`, `UsageTotal`, `hook_entry`, `hook_entry_id`, `Cursor`, `LogPage`, `REPLAY_CAP`, `page_after`, `page_before`, `replay_window`, `AgentOutcome`, `AgentOutcomeSink`, `AgentRunDef`, `AgentRuntimeContext`, `AskedQuestion`, `CONCLUDE_TOOL`, `ContextError`, `ContextProvider`, `Contexts`, `DefaultToolboxFactory`, `FixedContextProvider`, `INSPECT_WORKSPACE_TOOL`, `SKILL_TOOL`, `StartTurn`, `ToolboxFactory`, `TurnPreparation`, `conclude_tool_spec`, `start_blocked`, `translate`, `ABANDONED_ASK_RESULT`, `AnswerError`, `AskAnswer`, `Incoming`, `MERGE_SEPARATOR`, `Turn`, `answered_turn`, `queued_turn`, `CompositeToolbox`, `McpToolbox`, `PluginMcpToolbox`, `TASK_LIST_TOOL`, `TaskListAction`, `TaskListState`, `TaskRecord`, `TaskStatus`, `task_list_tool_spec`, `CancelSelector`, `TimerId`, `TimerKind`, `TimerRecord`, `TimerView`, `timer_tool_specs`, `AgentCatalog`, `CatalogAgent`, `SharedContext`, `SharedScan`, `Skill`, `SkillSet`, `WorkspaceContext`, `compose_system_prompt`, `scan as scan_workspace`.

- [ ] **Step 1: Move the files**

```bash
cd .claude/worktrees/crate-consolidation
mkdir -p crates/server/src/agent_loop
for f in agent_actor agent_log context hook_translation inbox mcp_toolbox task_list timers workspace; do
  git mv "crates/workflow/src/$f.rs" "crates/server/src/agent_loop/$f.rs"
done
git mv crates/workflow/src/lib.rs crates/server/src/agent_loop/mod.rs
git mv crates/workflow/tests/workspace_context.rs crates/server/tests/workspace_context.rs
git rm -q crates/workflow/Cargo.toml
rmdir crates/workflow/src crates/workflow/tests crates/workflow 2>/dev/null || true
```

- [ ] **Step 2: Turn the old `lib.rs` into a module**

`crates/server/src/agent_loop/mod.rs` keeps its entire body — the nine `mod` lines and every `pub use` — unchanged. Only the header doc comment changes, because `//!` on a crate root and `//!` on a module read the same but the first line should now say what the module is rather than what the crate was. Replace the existing header block with:

```rust
//! The agent loop, on top of the event-sourced `actor` runtime.
//!
//! An [`AgentActor`] runs one agent: it calls the provider, executes tools
//! through a [`Toolbox`](horsie_agentcore::Toolbox), and reports a terminal
//! [`AgentOutcome`] to whoever spawned it. It is event-sourced, so a restarted
//! process recovers an in-flight conversation from the journal.
//!
//! Sequencing several agents — an interactive session's main agent and its
//! subagents, or a workflow run's steps — belongs to the owner that spawns
//! them, not here. That owner is `crate::sessions`; the workflow *graph*
//! feature that schedules runs is `crate::workflows`, which is a different
//! thing despite the adjacent name.
```

- [ ] **Step 3: Declare the module**

In `crates/server/src/lib.rs`, add the declaration in alphabetical position — it becomes the new first line, before `pub mod agents;`. It is `pub` because `integration-tests` reaches these types through `horsie-server`.

```rust
pub mod agent_loop;
pub mod agents;
```

- [ ] **Step 4: Rewrite the imports**

Inside the server, the crate reference becomes a `crate::` path:

```bash
cd .claude/worktrees/crate-consolidation
grep -rl "horsie_workflow" crates/server/src \
  | xargs sed -i '' 's/horsie_workflow::/crate::agent_loop::/g'
```

The moved test file and the external integration test address it through the server crate:

```bash
sed -i '' 's/horsie_workflow::/horsie_server::agent_loop::/g' \
  crates/server/tests/workspace_context.rs \
  crates/tests/tests/agent_recovery_e2e.rs
```

The moved modules referred to each other as siblings via `crate::` when they were their own crate, and they still are siblings — but `crate::` now means the server. There are 77 such paths. Rewrite them to `super::`:

```bash
sed -i '' 's/\bcrate::/super::/g' crates/server/src/agent_loop/*.rs
```

Four of the 77 are not sibling *modules* but root re-exports — `crate::start_blocked`, `crate::queued_turn`, `crate::answered_turn`, and one more from `hook_translation`/`inbox`. These still resolve, because `mod.rs` keeps the `pub use` lines that put them at the module root, and `super::` now names that root. No special handling needed.

Then confirm `mod.rs` was untouched — it contains only `mod` declarations and `pub use` of its own children, all already relative:

```bash
git diff --stat crates/server/src/agent_loop/mod.rs
```

Expected: no change beyond the Step 2 doc-comment edit.

- [ ] **Step 5: Verify no `crate::` path was mangled**

The blanket `super::` rewrite in Step 4 is the one risky edit in this task: a file that legitimately said `crate::something_else` would now be wrong. In the old crate every `crate::` path resolved to a sibling of these nine modules, so every rewrite is correct — but confirm the compiler agrees before going further.

```bash
cargo build -p horsie-server
```

Expected: PASS. An `unresolved import super::X` means a path that was `crate::X` in the old crate is a sibling module — check it exists under `agent_loop/`.

- [ ] **Step 6: Fold the dependencies into `horsie-server`**

`crates/workflow`'s dependencies were `actor`, `agentcore`, `runtime-host`, `support`, `models`, `async-trait`, `thiserror`, `tokio`, `tokio-util`, `serde`, `serde_json`, `tracing`, `uuid`, `futures-util`. **Every one is already in `crates/server/Cargo.toml`.** Verify rather than assume:

```bash
for d in horsie-actor horsie-agentcore horsie-runtime-host horsie-support horsie-models \
         async-trait thiserror tokio tokio-util serde serde_json tracing uuid futures-util; do
  grep -q "^$d " crates/server/Cargo.toml || echo "MISSING: $d"
done
```

Expected: no output. Then delete the `horsie-workflow` line from `[dependencies]` in `crates/server/Cargo.toml`, and the `horsie-workflow` line from `[dev-dependencies]` in `crates/tests/Cargo.toml`.

- [ ] **Step 7: Fix the stale doc reference in `support`**

`crates/support/src/plugin/skills.rs:47` points a reader at "the runtime-side reader in `horsie_workflow`". It is prose only — `support` is a dependency *of* the agent loop, not a consumer — but it now names a crate that does not exist.

```bash
sed -i '' 's/`horsie_workflow`/`horsie_server::agent_loop`/' crates/support/src/plugin/skills.rs
grep -n "agent_loop" crates/support/src/plugin/skills.rs
```

Expected: one hit on line 47.

- [ ] **Step 8: Confirm the crate is gone everywhere**

```bash
grep -rn "horsie_workflow\|horsie-workflow" crates/ docs/src scripts/ Makefile .github/ 2>/dev/null | grep -v target
```

Expected: no hits.

- [ ] **Step 9: Format, lint, and run the full suite**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: PASS.

```bash
TMPDIR=/tmp cargo test --workspace --all-features
```

Expected: PASS, same test count as before the move. `crates/tests` exercises the session routes that `agent_loop` sits underneath, so a `-p horsie-server` run alone would be a false green here.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "refactor: fold horsie-workflow into horsie-server as agent_loop"
```

---

### Task 3: Guard the publish surface, then fix it

The governing rule from the spec: **the published set is exactly the dependency closure of the installable binaries, and nothing else.** Write the check first so it fails against the current flags, then fix the flags.

The roots are `horsie` and `horsie-runtime`, the two binaries `scripts/install.sh` installs. `horsie-server` is deliberately not a root: it ships as a release tarball binary and a container image, never through crates.io.

**Files:**
- Create: `scripts/check-publish-surface.sh`
- Modify: `crates/actor/Cargo.toml`

**Interfaces:**
- Consumes: the 10-crate workspace produced by Tasks 1 and 2.
- Produces: `scripts/check-publish-surface.sh`, exit 0 on match and exit 1 with a diff on drift. Task 4 wires it into CI.

- [ ] **Step 1: Write the guard**

```bash
cat > scripts/check-publish-surface.sh <<'EOF'
#!/usr/bin/env bash
# The published set is exactly the dependency closure of the installable
# binaries — no more, no less.
#
# A crate neither binary can reach has no business on crates.io; a crate one of
# them needs cannot be silently dropped. Without this check, publishability is a
# cargo default rather than a decision, which is how six renamed crates came to
# be stranded on the registry at 0.1.6.
#
# horsie-server is deliberately not a root: it is distributed as a release
# tarball binary and a container image, never through crates.io.
#
# See docs/superpowers/specs/2026-08-09-crate-consolidation-design.md.
set -euo pipefail

ROOTS=(horsie horsie-runtime)

members=$(cargo metadata --format-version 1 --no-deps | jq -r '.packages[].name' | sort)

publishable=$(cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[] | select(.publish == null) | .name' | sort)

closure=$(
  for root in "${ROOTS[@]}"; do
    cargo tree -p "$root" --edges normal --prefix none --format '{p}' | awk '{print $1}'
  done | sort -u
)
# Restrict the closure to workspace members; third-party crates are not ours to publish.
closure=$(comm -12 <(echo "$closure") <(echo "$members"))

if diff_out=$(diff <(echo "$publishable") <(echo "$closure")); then
  echo "publish surface matches the binary closure:"
  echo "$closure" | sed 's/^/  /'
  exit 0
fi

echo "::error::publish surface has drifted from the closure of ${ROOTS[*]}"
echo "  '<' is publishable but unreachable — add 'publish = false' with a reason."
echo "  '>' is reachable but not publishable — remove its 'publish = false'."
echo "$diff_out"
exit 1
EOF
chmod +x scripts/check-publish-surface.sh
```

- [ ] **Step 2: Run it and watch it fail**

```bash
./scripts/check-publish-surface.sh
```

Expected: FAIL, exit 1, with `< horsie-actor` in the diff. `horsie-actor` is publishable but only `horsie-server` consumes it, so it is not in either binary's closure.

If the diff shows anything *other* than `horsie-actor`, stop — Tasks 1 or 2 left something inconsistent, and the guard has just caught it. Investigate before continuing.

- [ ] **Step 3: Make `horsie-actor` private**

Its only non-test consumer is `horsie-server`. That was already nearly true before Task 2 — `horsie-workflow` used exactly one symbol from it, `JournalError`, in two places — and Task 2 turned those two references into server references, so no code change is needed here.

In `crates/actor/Cargo.toml`, add one line directly after `edition = "2024"` (line 7). Leave `repository` and `description` in place — they are still accurate documentation, and cargo ignores them for an unpublished crate.

```toml
publish = false # only horsie-server consumes it, and the server does not ship via crates.io
```

- [ ] **Step 4: Run it and watch it pass**

```bash
./scripts/check-publish-surface.sh
```

Expected: PASS, exit 0, listing exactly six crates: `horsie`, `horsie-agentcore`, `horsie-models`, `horsie-runtime`, `horsie-runtime-host`, `horsie-support`.

- [ ] **Step 5: Confirm every publishable crate actually publishes**

This is the only check that catches a path dependency missing its `version =` key, which is the classic way a workspace refactor breaks publishing without breaking the build. `horsie-runtime-host` is the one at real risk: it is newly publishable and carries a path-only dev-dependency on `horsie-server`, which cargo must strip.

```bash
for c in horsie-models horsie-support horsie-agentcore horsie-runtime-host horsie-runtime horsie; do
  echo "=== $c"
  cargo publish --dry-run -p "$c" --allow-dirty 2>&1 | tail -5
done
```

Expected: each ends in `Packaged N files`. A failure naming `horsie-support` or `horsie-runtime-host` as "not found in registry" is expected and fine for the *dependent* crates — those two do not exist on crates.io yet, and Task 4's note covers it. Any failure about a *missing version field* is a real bug: add `version = "0.1.6"` to that path dependency.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "build: guard the publish surface against the binary closure"
```

---

### Task 4: Wire the guard into CI and record the release pre-flight

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `CONTRIBUTING.md`

**Interfaces:**
- Consumes: `scripts/check-publish-surface.sh` from Task 3.

- [ ] **Step 1: Add the step to the existing `check` job**

Add it to the `check` job rather than creating a new one. The repository requires seven status checks to pass before a PR can merge, and a new job would be an eighth that nobody has registered as required — so it could go red without blocking anything. A step inside `check` is enforced immediately.

In `.github/workflows/ci.yml`, inside the `check` job, add after the `Clippy` step (before `Tests`):

```yaml
      - name: Publish surface
        run: ./scripts/check-publish-surface.sh
```

- [ ] **Step 2: Verify the runner has what the script needs**

The script uses `cargo`, `jq`, `awk`, `comm` and `diff`. `jq` is preinstalled on `ubuntu-latest`; the rest are coreutils. Confirm the script is executable in git, or the runner will fail with permission denied:

```bash
git ls-files -s scripts/check-publish-surface.sh
```

Expected: mode `100755`. If it shows `100644`, run `git update-index --chmod=+x scripts/check-publish-surface.sh`.

- [ ] **Step 3: Add the new script to the shellcheck workflow**

`shellcheck.yml` triggers on `scripts/**` but its only run step is hardcoded to a single file:

```yaml
      - run: shellcheck scripts/install.sh
```

Change it to cover both:

```yaml
      - run: shellcheck scripts/install.sh scripts/check-publish-surface.sh
```

Then run it locally and fix whatever it reports:

```bash
shellcheck scripts/check-publish-surface.sh
```

Expected: clean. `SC2046`/`SC2086` around the `comm` process substitutions are the likely warnings; quote as directed rather than adding a `# shellcheck disable` line.

- [ ] **Step 4: Document the release pre-flight**

`horsie-support` and `horsie-runtime-host` do not exist on crates.io, and trusted publishing can only be configured for a crate that already exists. The first publish of each therefore needs `CARGO_REGISTRY_TOKEN` — which `publish.yml`'s own comment says should have been deleted once OIDC was set up.

Add this to `CONTRIBUTING.md` in the release section (or create a `## Releasing` section at the end if none exists):

```markdown
### Before the first tag after a crate rename

`publish.yml` authenticates by OIDC, but trusted publishing can only be
configured for a crate that already exists on crates.io. A newly named crate
therefore needs the `CARGO_REGISTRY_TOKEN` secret for its first publish only.

`horsie-support` and `horsie-runtime-host` are both in this state as of
v0.1.6. Before tagging:

1. Check whether the `CARGO_REGISTRY_TOKEN` repository secret still exists. If
   not, mint a scoped publish token on crates.io and add it back.
2. Tag and let the publish run create both crates.
3. On crates.io, configure trusted publishing for each new crate: repository
   `blossomstack/horsie`, workflow `publish.yml`.
4. Delete the secret again.

The auth step in `publish.yml` already has `continue-on-error: true` and falls
through to the secret, so no workflow change is needed.
```

- [ ] **Step 5: Final full verification**

Everything at once, one time, before opening the PR:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
./scripts/check-publish-surface.sh
TMPDIR=/tmp cargo test --workspace --all-features
```

Expected: all PASS.

Then the web e2e suite, since `agent_loop` sits under the session routes it drives. There is no `make` target for this — it runs from `clients/web`, and the web client installs with **bun, not npm** (`npm ci` fails in a fresh worktree):

```bash
cd clients/web
bun install --frozen-lockfile
bun run build
TMPDIR=/tmp bun run test:e2e
cd ../..
```

Expected: PASS. `TMPDIR=/tmp` is required — Playwright's global setup dies under the default macOS `$TMPDIR` because the path overflows `sockaddr_un.sun_path`.

- [ ] **Step 6: Confirm the workspace is what the spec says**

```bash
cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[] | "\(if .publish == null then "PUBLISH" else "private" end)  \(.name)"' | sort
```

Expected: exactly ten lines — six `PUBLISH` (`horsie`, `horsie-agentcore`, `horsie-models`, `horsie-runtime`, `horsie-runtime-host`, `horsie-support`) and four `private` (`horsie-actor`, `horsie-llm-providers`, `horsie-server`, `integration-tests`).

- [ ] **Step 7: Commit and open the PR**

```bash
git add -A
git commit -m "ci: enforce the publish surface, and document the release pre-flight"
git push -u origin crate-consolidation
gh pr create --title "Crate consolidation, and a publish surface that matches the install path" --body "$(cat <<'EOF'
Merges two crate pairs and makes the crates.io surface exactly the dependency closure of the installable binaries. Workspace goes from 12 crates to 10; published crates from 9 to 6. No behaviour change — both merges are file moves plus import rewrites.

`horsie-runtime-client` and `horsie-runtime-vendor` were the two halves of the same wire, and the vendor half already depended on the client half. They become `horsie-runtime-host`. No consumer gains a dependency it did not already have.

`horsie-workflow` was misnamed — its own module doc calls it "the agent loop on top of the event-sourced actor runtime", and its only consumer was the server. It becomes `crates/server/src/agent_loop/`, not `workflow`, because `server/src/workflows/` and `server/src/sessions/workflow/` already exist and mean different things. That merge also absorbs the single `JournalError` reference that was keeping `horsie-actor` from being server-private, so `horsie-actor` becomes `publish = false` with no code change.

`scripts/check-publish-surface.sh` recomputes the surface from `cargo metadata` and fails if it drifts. Without it, publishability is a cargo default rather than a decision — which is how six renamed crates came to be stranded on crates.io at 0.1.6.

One operational note before the next tag: `horsie-support` and `horsie-runtime-host` do not exist on crates.io, and trusted publishing can only be configured for a crate that already exists, so each needs `CARGO_REGISTRY_TOKEN` for its first publish. `CONTRIBUTING.md` has the sequence.

Design: `docs/superpowers/specs/2026-08-09-crate-consolidation-design.md`
EOF
)"
```

---

## Self-Review

**Spec coverage.** Every section of the design maps to a task: the `runtime-host` merge and all four verified properties (no module collision, no symbol collision, no new dependency edge, no cycle) to Task 1; the `agent_loop` merge with all four enumerated reference sites — 20 server files, the moved test, the 13 sites in `agent_recovery_e2e.rs`, the `skills.rs:47` doc comment — to Task 2; `horsie-actor` going private for free to Task 3 Step 3; the six-crate publish table to Task 3 Step 4 and Task 4 Step 6; the release pre-flight to Task 4 Step 4; the four verification commands to Task 4 Step 5.

The spec's "what does not change" section is honoured by omission — no task touches the version guard, `release-binaries`, the install script, or the container image, and Task 1 Step 4's grep confirms no build config references the renamed crates.

**Additions beyond the spec.** `scripts/check-publish-surface.sh` and its CI wiring are not in the design document. They make the spec's governing rule executable rather than aspirational, and they are what would have prevented the original drift. Flagging as a deliberate scope addition.

**Three assumptions checked and corrected before publishing this plan.** `make e2e` does not exist — the e2e suite runs from `clients/web` via `bun run test:e2e`, and the web client installs with bun rather than npm. `shellcheck.yml` does not glob `scripts/*.sh`; its run step is hardcoded to `scripts/install.sh` and needs the new script added by name. And the `crate::` rewrite in Task 2 touches 77 paths, four of which are root re-exports rather than sibling modules — they resolve anyway, but the plan now says so rather than leaving the implementer to discover it.

**One risk worth naming.** Task 2 Step 4's blanket `s/\bcrate::/super::/g` across `agent_loop/*.rs` is the only edit in this plan that could silently produce a wrong-but-compiling path. It is safe here because every `crate::` path in the old crate resolved to a sibling of those nine modules, and Step 5 builds immediately to confirm. A reviewer should still read that hunk of the diff rather than skim it.
