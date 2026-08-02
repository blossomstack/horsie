# Plugin hooks beyond SessionStart

## Context

horsie runs exactly one plugin hook event: `SessionStart`. Every other event a
plugin declares in `hooks/hooks.json` is silently ignored. This spec covers
Phase 1 of issue #105 — a real hook dispatch layer, the control protocol that
lets a hook block or amend what the agent does, and a hard failure whenever a
plugin declares an event horsie cannot run.

Phase 0 (#110, #113) is merged: plugin packaging is now read in one place,
`horsie-support::plugin`, and marketplaces are first-class. This phase builds on
that reader.

### What exists today

- `runtime/src/plugins.rs` `session_start_commands()` reads
  `<plugin>/hooks/hooks.json`, takes `hooks.SessionStart[].hooks[]` where
  `type == "command"`, substitutes `${CLAUDE_PLUGIN_ROOT}`, and runs each via
  `sh -c` with a 30s hardcoded timeout and a 50 KB output clamp.
- `extract_context()` understands one output shape:
  `{"hookSpecificOutput":{"additionalContext":…}}`, else raw stdout. It has no
  notion of exit codes, decisions, or blocking.
- `models/fluorite/runtime.fl:97-102` defines
  `SessionStartRequest { call_id }` → `SessionStartResponse { call_id, context }`.
  The server calls it from `session_actor.rs:745` and folds the result into the
  system prompt as `# Session bootstrap`.
- The `matcher` field is parsed by nobody: only `SessionStart` runs, and it has
  no tool matcher.

### The three seams

Issue #105 predicted "a structural change to `agentcore`/`workflow`". It is not
one. Three existing seams carry the whole feature:

1. **`agentcore/src/agent.rs:539`** is the single tool-dispatch site, and it
   calls through the `Toolbox` trait. `DefaultToolboxFactory`
   (`workflow/src/context.rs:252`) already stacks decorators —
   `CompositeToolbox` → `FilteredToolbox` → `AgentToolbox`. A `HookedToolbox`
   slots into that stack and sees a tool's input on the way in and its output on
   the way out, which is exactly what `updatedInput` and `updatedToolOutput`
   need. **No change to the agent loop.**
2. **`server/src/sessions/session_actor.rs`** owns the turn boundary. It calls
   the agent and handles `AgentOutcome::{Concluded, Asked, Failed}`. Turn events
   attach here.
3. **The runtime** is where hook commands must actually execute, because that is
   where the plugin files and the workspace are. The `SessionStart` message is
   already precisely this: a request that runs hooks in the right place and
   returns their output. This phase generalises that one message.

## Goals

1. Every hook event horsie can meaningfully implement, implemented.
2. Any event it cannot implement fails loudly, naming the event — never a silent
   no-op.
3. Hooks published for Claude Code work unmodified, including their matchers.
4. A session with no hooks pays no per-tool-call cost.

## Non-goals

Agents, commands and `.mcp.json` (Phases 2–4 of #105); a permission system;
context compaction; changing where runtimes are sandboxed.

## The event inventory

Claude Code documents **31** hook events. They divide by whether horsie has the
concept, not by whether they are useful.

### A. Implementable on seams that exist (15) — this phase

| Event | Seam |
| --- | --- |
| `SessionStart` | already wired; migrates onto the new dispatch |
| `SessionEnd` | `delete_session` (`server/src/http/handlers.rs:406`) |
| `UserPromptSubmit` | session actor, before the agent run |
| `PreToolUse` | `HookedToolbox`, before `inner.execute` |
| `PostToolUse` | `HookedToolbox`, after a successful `execute` |
| `PostToolUseFailure` | `HookedToolbox`, after `execute` returns `Err` |
| `PostToolBatch` | agent loop already runs a turn's calls as one batch |
| `Stop` | session actor, `AgentOutcome::Concluded` |
| `StopFailure` | session actor, `AgentOutcome::Failed` |
| `SubagentStart` / `SubagentStop` | `workflow/src/workflow_actor.rs` `spawn_agent` |
| `TaskCreated` / `TaskCompleted` | `workflow/src/task_list.rs`, the `task_list` tool |
| `Notification` | session actor progress frames |
| `CwdChanged` | the `set_working_dir` tool |

### B. Needs a horsie feature that does not exist (9) — hard failure

`UserPromptExpansion` (no slash commands — Phase 3 of #105),
`PermissionRequest`, `PermissionDenied` (no permission model), `PreCompact`,
`PostCompact` (no context compaction), `FileChanged` (no file watcher),
`ConfigChange` (no in-session config reload), `DirectoryAdded` (workspaces are
fixed at session start), `Setup` (no init mode).

### C. No horsie concept at all (7) — hard failure

`MessageDisplay`, `TeammateIdle`, `WorktreeCreate`, `WorktreeRemove`,
`Elicitation`, `ElicitationResult`, `InstructionsLoaded`.

## Design

### Tool-name aliasing

Matchers in the wild are Claude's tool names. Surveying the official
marketplace, every matcher that exists is one of:

```
PostToolUse :: Bash
PostToolUse :: Edit|Write|MultiEdit|NotebookEdit
UserPromptExpansion :: ^claude-security:claude-security$
```

horsie's tools are `bash`, `read_file`, `write_file`, `find_and_replace`,
`replace_lines`, `list_files`, `glob`, `grep`, `set_working_dir`, `set_env`.
**None of them match those patterns.** Without aliasing, this phase would ship
and no published plugin's hook would ever fire — including impeccable's, the
plugin that opened this issue. Grok Build solved it the same way.

Each horsie tool therefore carries its Claude aliases, and a matcher is tested
against the horsie name *and* every alias:

| Claude name(s) | horsie tool |
| --- | --- |
| `Bash` | `bash` |
| `Read` | `read_file` |
| `Write` | `write_file` |
| `Edit`, `MultiEdit` | `find_and_replace`, `replace_lines` |
| `Glob` | `glob` |
| `Grep` | `grep` |
| `LS` | `list_files` |

`set_env` and `set_working_dir` have no Claude equivalent and match only their
own names. The map lives in `horsie-support::plugin::hooks` beside the matcher
logic, so there is one place to extend when a tool is added.

A matcher is a regex, consistent with Claude Code — one real matcher in the
wild, `^claude-security:claude-security$`, uses anchors, so simple alternation
splitting is not enough. An empty or absent matcher matches every tool.

**Matchers are evaluated server-side**, in `horsie-support::plugin::hooks`
against the manifest, because that is what lets the server skip the round-trip
when nothing matches. `horsie-support` therefore gains a `regex` dependency;
`regex` is already in the workspace tree (`runtime/Cargo.toml`), so this adds no
new third-party crate, only a new edge to it. Evaluating matchers runtime-side
instead would mean a round-trip on every tool call to discover that no hook
applies, which defeats the manifest.

### The hook reader

`horsie-support::plugin::hooks` gains:

```rust
pub enum HookEvent { SessionStart, PreToolUse, /* … all 15 supported … */ }

pub struct HookDecl {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
}

pub struct PluginHooks {
    pub decls: Vec<HookDecl>,
    /// Event names this build cannot run, verbatim as declared.
    pub unsupported: Vec<String>,
}

pub fn read(plugin_root: &Path) -> Result<PluginHooks, String>;
```

`HookEvent::parse` recognises all 31 documented names. A name in group A becomes
a variant; a name in group B or C, or any unrecognised string, lands in
`unsupported`. Collecting rather than erroring lets the caller report *every*
unsupported event at once instead of one per attempt.

Per-hook `timeout` is honoured (impeccable declares 5s and 30s); absent means the
existing 30s default.

### Failing on unsupported events

Two gates, because either alone leaves a silent-no-op path:

- **`horsie plugin install`** rejects a plugin whose hooks declare an
  unsupported event, naming every one and saying horsie cannot run it. The user
  learns before installing.
- **Session start** re-validates, which catches plugins installed before this
  change and `hooks.json` files that changed on `plugin update`. The session
  fails to start with the same message rather than running with a guard that
  never fires.

### Protocol

`models/fluorite/runtime.fl` gains a manifest request and a general hook call.
`SessionStart` is left in place and is the fallback described below.

```
struct HookDeclWire { event: String, matcher: Option<String> }

struct HookManifestRequest  { call_id: String }
struct HookManifestResponse {
    call_id: String,
    entries: Vec<HookDeclWire>,
    /// Event names the plugins declared that this runtime cannot run.
    unsupported: Vec<String>,
}

struct RunHookRequest {
    call_id: String,
    event: String,
    /// The event payload as JSON, fed to the hook on stdin.
    payload: String,
}

struct HookOutcomeWire {
    /// The hook blocked the action (exit 2, `decision: "block"`, or
    /// `permissionDecision: "deny"`).
    blocked: bool,
    reason: Option<String>,
    additional_context: Option<String>,
    /// Replacement tool input / output, as JSON.
    updated_input: Option<String>,
    updated_tool_output: Option<String>,
    system_message: Option<String>,
    /// `continue: false` — end the turn.
    stop: bool,
    stop_reason: Option<String>,
    /// The hook could not be run to completion (spawn failure, timeout, or a
    /// non-zero exit other than 2). Distinct from `blocked`, because the two
    /// carry different intent and the caller treats them differently.
    failed: bool,
}

struct RunHookResponse { call_id: String, outcome: HookOutcomeWire }
```

### Manifest as optimisation and as negotiation

At session start the server requests the manifest once. It then round-trips to
the runtime only when a declaration's event and matcher match the thing about to
happen. A session whose plugins declare no `PreToolUse` hook pays nothing per
tool call — which is the common case, and the reason this is a manifest rather
than an unconditional call.

The manifest doubles as version negotiation, which matters because
`RuntimeReady { runtime_id }` carries no protocol version and users install the
CLI independently of the server. A runtime that does not answer the manifest —
an older `horsie connect` — is recorded as not supporting hooks, and the server
falls back to calling the existing `SessionStart` message alone. Behaviour is
then exactly what ships today, rather than an error on an unknown message.

### Control protocol

Per event, the runtime runs each matching hook's command via `sh -c` with the
plugin root as cwd, `CLAUDE_PLUGIN_ROOT` set, the hook path prepended to `PATH`,
and the event payload on stdin — the mechanism `run_hook` already uses.

- **exit 0** — stdout is parsed as JSON when it parses, else treated as
  `additionalContext`. Recognised fields: `continue`, `stopReason`,
  `systemMessage`, `decision`/`reason`, and `hookSpecificOutput` with
  `additionalContext`, `permissionDecision`, `permissionDecisionReason`,
  `updatedInput`, `updatedToolOutput`.
- **exit 2** — blocking. stderr is the reason.
- **any other outcome** — non-zero exit, timeout, or spawn failure — sets
  `failed`.

Two deliberate deviations from Claude Code, both decided for this design:

1. **A failed `PreToolUse` hook denies the tool.** Claude Code treats a
   non-blocking error as "continue". horsie fails closed: a guard that cannot
   run is not a guard. This applies only to `PreToolUse` — every other event
   either runs after the fact or has nothing to block.
2. **`permissionDecision: "ask"` and `"defer"` are treated as allow**, and
   logged. horsie has no permission prompt and runs unattended sessions, so
   there is nobody to ask.

These pull in opposite directions — a hook that *crashes* is denied, while a
hook that explicitly *asks for approval* is allowed. That is intentional: a
crash is an outage, whereas `ask` is a considered signal horsie has no mechanism
to act on. It is called out here so it is a documented choice rather than a
discovery.

Several hooks may match one event. They run in stable plugin order; the first
`blocked` wins and stops the chain, and `additionalContext` from all of them is
concatenated. `updatedInput`/`updatedToolOutput` from a later hook overwrite an
earlier one's, so ordering is deterministic and stated rather than incidental.

### Where hooks execute

Unchanged by this phase, and worth stating precisely because it is easy to get
wrong: hooks run inside the runtime. For velos, and for
`horsie connect --sandbox`, that is under the nono sandbox with the plugin
library and its clone roots granted read-only. For a plain `horsie connect`,
runtimes are **unsandboxed by default** — `cli/src/main.rs` documents
`--sandbox` as "Off by default: the machine is already your own", and
`runtime-vendor/src/vendor.rs` defaults `sandbox: false`.

So enabling plugins on the default local vendor means a plugin's hooks run with
the user's own privileges. That is already true of `SessionStart` today, and the
guide already warns to install only plugins you trust. This phase widens *when*
those hooks run, not *what* they can reach.

### Error handling

- Unsupported event at install → install fails, naming every unsupported event.
- Unsupported event at session start → session fails to start, same message.
- Malformed `hooks.json` → the plugin's hooks are an error, not silently empty;
  install and session start both surface it.
- Hook failure on `PreToolUse` → tool denied, reason names the plugin and the
  failure.
- Hook failure on any other event → logged and skipped; the action proceeds.
- Runtime does not support the manifest → hooks disabled for that session,
  `SessionStart` still runs.

## Staging

Three stacked PRs. Every event in group A lands; each PR is independently
reviewable.

**PR1 — dispatch layer and tool events.** The reader with all 31 event names,
tool-name aliasing, matcher evaluation, the manifest and `RunHook` protocol, the
runtime-side executor and control-protocol parser, the `HookedToolbox`
decorator, and the unsupported-event failure at both gates. Wires `PreToolUse`,
`PostToolUse` and `PostToolUseFailure`.

**PR2 — turn events.** `UserPromptSubmit`, `Stop`, `StopFailure`,
`PostToolBatch`, `SessionEnd`, `Notification`, and the migration of
`SessionStart` onto the new dispatch.

**PR3 — subagent, task and cwd events.** `SubagentStart`, `SubagentStop`,
`TaskCreated`, `TaskCompleted`, `CwdChanged`.

## Testing

- `horsie-support` unit tests: every one of the 31 names parses to either a
  supported variant or an `unsupported` entry, with a test that asserts the
  counts (15 / 16) so adding a variant cannot silently change the contract.
- Matcher tests using the three real-world matchers, asserting
  `Edit|Write|MultiEdit|NotebookEdit` matches `write_file` and
  `find_and_replace` and does not match `bash`.
- Control-protocol tests over fixture scripts: exit 0 with each JSON shape, exit
  2, non-zero exit, and a timeout — asserting `blocked` versus `failed`.
- A `HookedToolbox` test proving a denying `PreToolUse` hook prevents the inner
  toolbox from being called at all, and that the denial reaches the model as an
  error tool result.
- A test that a failing `PreToolUse` hook denies, and that a failing
  `PostToolUse` hook does not.
- An end-to-end test with a real plugin fixture whose hook rewrites a tool's
  input, proving `updatedInput` takes effect.
- The existing `SessionStart` runtime and server tests must pass unmodified —
  they are the guard that the generalisation preserved today's behaviour.

## Consequences

- A plugin declaring any group B or C event becomes uninstallable. That is the
  point of the failure rule, but it makes horsie brittle against Claude Code
  adding events: a plugin adopting a new one fails until horsie learns it. The
  error names the event, so the diagnosis is immediate.
- `PreToolUse` adds a runtime round-trip per matching tool call. Sessions whose
  plugins declare no matching hook are unaffected.
- Hooks can now change what a tool does. A buggy hook can corrupt a tool call in
  a way that is hard to attribute, so `systemMessage` and the reason strings
  should always name the plugin.
