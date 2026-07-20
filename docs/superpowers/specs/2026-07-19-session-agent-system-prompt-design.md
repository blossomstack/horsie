# Session agent system prompt (closes #13)

## Context

Issue #13 asked to feed the runtime-reported `RuntimeReady.workdir` into the
session agent's system prompt. Investigation found:

- The agent's working directory is **already** in the prompt, via the
  `# Workspaces` section `compose_system_prompt` builds from the workspace
  scan (`workflow/src/workspace.rs:253`, `:270-295`). That path comes from
  the runtime's own `WorkspaceRegistry` — the same source `RuntimeReady.workdir`
  was derived from (`runtime/src/scan.rs:51` vs `runtime/src/main.rs:153`) —
  but it is per-session and vendor-uniform, whereas `RuntimeReady.workdir` is
  per-vendor (stored on `LocalDaemonVendor` only) and never captured at all on
  the velos path (`http/runtime_connect.rs:54` only installs the capture hook
  for `?register=local`).
- `LocalDaemonVendor::workdir()` has exactly one reader: an assertion in its
  own test (`server/src/vendor/local.rs:383`). It is unreachable through
  `Arc<dyn RuntimeVendor>` since it's not on the trait.
- The actual gap is that **no session ever gets a system prompt unless the
  user supplies one**. With no user prompt, no `AGENTS.md`/`CLAUDE.md`, and no
  plugins, `compose_system_prompt` returns `None` — the model receives no
  system block at all: no role, no tool-usage guidance, no behavioral norms.
  The web UI (`NewSessionModal.tsx:334`) and docs (`docs/guide/sessions.md:31`)
  both describe an "override the default system prompt" that doesn't exist.

## Decision

1. Author the baseline system prompt the UI already promises. Wire it in
   unconditionally so every session agent gets role, tool-usage, and
   environment guidance regardless of user input or scan success.
2. Remove the per-session system prompt override entirely (wire, storage,
   handler, UI, docs) — the baseline is not overridable. This is separate
   from the workflow agent's per-agent role prompt (`models/fluorite/workflow.fl:18`),
   which stays untouched.
3. Retire the now-fully-dead `RuntimeReady.workdir` plumbing rather than
   wiring it up per the issue's literal ask — it would duplicate information
   the workspace scan already puts in the prompt, through a mechanism with
   structural gaps (per-vendor not per-session, velos-blind). Remove
   `workdir` from `RuntimeReady` on the wire itself, not just its consumers.
4. Defer exposing a working directory on `SessionDetail` — no UI consumer
   exists today; add it when one does.

## Changes

### Baseline prompt

New file `server/src/sessions/system_prompt.md`, pulled in via
`include_str!` as a `SESSION_AGENT_PROMPT` constant. Lives under `sessions/`
because it is specific to session agents; workflow agents keep their own
per-agent role prompts via `AgentDef.system_prompt`.

`session_actor.rs:385` changes from

```rust
params.system_prompt =
    compose_system_prompt(def.system_prompt.as_deref(), &ws, shared.as_ref());
```

to passing `Some(SESSION_AGENT_PROMPT)` instead of `def.system_prompt.as_deref()`.
`compose_system_prompt` itself is unchanged — the workflow path
(`workflow_actor.rs:306`) keeps passing its own per-agent prompt through the
same function.

Net effect: a failed workspace scan now degrades to "no path/workspace info"
rather than "no system prompt at all."

### Remove per-session `system_prompt`

- `models/fluorite/session.fl:23` — drop `system_prompt` from
  `AgentSettings`. (`models/fluorite/workflow.fl:18` is a different struct
  and is untouched.)
- `server/src/sessions/spec.rs:34` — drop from storage `AgentSettings`. Old
  journal rows carrying the field still deserialize (extra field ignored),
  matching the precedent already set for `WorkspaceDef` (`spec.rs:46-49`) —
  no migration needed.
- `server/src/http/handlers.rs:54` — drop the wire→storage mapping.
- `server/src/sessions/session_actor.rs:334` — drop from `AgentRunDef`
  construction.
- `clients/web/src/components/NewSessionModal.tsx` — remove the `systemPrompt`
  state (`:33`), the field in the create payload (`:121`), and the Advanced
  form field (`:334`).
- `docs/guide/sessions.md:31` — remove the "System prompt" bullet from the
  Advanced list.
- Regenerate TS client types (`session.fl` change flows through
  `fluorite_codegen` / the TS generator) so `AgentSettings` no longer carries
  `systemPrompt` anywhere in `clients/*/src/generated`.

### Retire `RuntimeReady.workdir`

Removed from the wire, not just unused by consumers:

- `models/fluorite/runtime.fl:107` — `struct RuntimeReady { runtime_id: String }`.
- `runtime/src/main.rs` — drop the `workdir` computation (`:153-157`) and its
  threading through `run_loop` (`:165`, `:173`, signature `:188`) and the
  `Ready` construction (`:235`).
- `executor/src/executor.rs` — `Handshake::Ready` drops its second field
  (`:286`, `:301`, `:310`); `ConnectHook` narrows from `Fn(String, String)`
  to `Fn(String)` (`:365` and the type itself); rename the test at `:691`
  (`on_registered_hook_fires_with_id_and_workdir_once_per_registration` →
  drop "_and_workdir").
- `server/src/vendor/local.rs` — delete the `workdir: RwLock<String>` field,
  `workdir()`, `set_workdir()`, the write at connect (`:174`), and the test
  assertion (`:383`). The hook itself stays; it still registers the vendor
  under its label on connect, just without a workdir argument.
- Test doubles constructing `RuntimeReady` — `tests/tests/session_server_e2e.rs:304`,
  `server/src/vendor/velos.rs:641`, `server/src/vendor/local.rs:290`,
  `executor/src/executor.rs:627` — drop the `workdir` field from each
  literal.

**Compatibility note:** no type in the codebase uses
`#[serde(deny_unknown_fields)]`, so an old daemon binary sending a stray
`workdir` to a new server deserializes fine (field ignored). The only
breaking direction is a new daemon (no `workdir` in its `Ready` message)
against an old server that still expects the field — this only affects the
`local` vendor, where a user runs their own long-lived daemon binary that
could lag a server upgrade. Given this is a pre-1.0 internal protocol, take
the break directly and note it in the PR description rather than staging a
two-release deprecation.

## Testing

- Existing `compose_system_prompt` unit tests (`workflow/src/workspace.rs`
  tests module) continue to exercise the function; update/add a case
  confirming the session path now always includes a prompt.
- `server/src/vendor/local.rs` — remove the `workdir()` assertion; the
  remaining connect-registers-vendor test still covers hook wiring.
- `executor/src/executor.rs` — update the renamed hook-fires test to assert
  only `runtime_id`.
- `tests/tests/session_server_e2e.rs` — update the `RuntimeReady` literal;
  confirm no assertion in that suite depends on `workdir`.
- Web: `NewSessionModal` — remove/update any test coverage of the system
  prompt field if present.
- Full pre-PR gate per `CLAUDE.md`: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`, plus web typecheck/build.
