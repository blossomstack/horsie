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

### Where hooks run — revised

An earlier draft of this spec put tool hooks server-side: a `HookedToolbox`
decorator, a manifest fetched at session start, and server-evaluated matchers
round-tripping a `RunHook` message. That was wrong, and the revision is worth
recording because the reasoning generalises.

Every runtime vendor's responsibility ends at materialising `plugins_dir`
(`runtime/src/main.rs` — velos fetches bundles, local passes `--plugins-dir`,
both converge on one directory). After that the runtime is the *only* component
that can see plugin files. Running a hook anywhere else means shipping the
decision away from the data.

So tool hooks run **inside the runtime, inline with the tool call they guard**:

1. **`agentcore` is untouched.** The runtime already receives a `ToolCall`; the
   hook wraps `tools::dispatch` rather than the `Toolbox` trait.
2. **No extra round-trip.** A server-side hook cost a WS hop per tool call,
   which is why the earlier draft needed a manifest to skip it. Inline, there is
   nothing to skip.
3. **No version negotiation.** The manifest doubled as a protocol-version probe.
   A runtime that predates hooks simply runs none — correct degradation, nothing
   to negotiate.
4. **Cancellation is free.** A `RunHook` round-trip was outside the `CancelCall`
   path, so a 30s hook ignored a user pressing Stop. Inline, a hook is
   interrupted by the same cancel that interrupts the tool.

Most of the control protocol then needs no new wire types at all: a denial is
`ToolResult::Err`, which the agent loop already turns into an `is_error` tool
result the model reads; `updatedInput` is applied before dispatch; and
`updatedToolOutput` is simply what the runtime returns.

Turn and session events cannot move: the runtime has no idea a turn ended. Those
stay server-initiated, as `SessionStart` already is.

### Visibility

Hooks change what the agent does, so what they did must be auditable rather than
invisible. Every hook that runs — including one that changed nothing, because
"a guard ran and allowed this" is part of the trail — produces a `HookRecord`
riding back on `ToolCallResponse`:

```
plugin, event, tool, duration_ms,
blocked, reason, failed,
input_before / input_after,      // the diff when a hook rewrote the call
output_before / output_after,
additional_context, system_message
```

Records reach the server out of band, through a `HookSink` on `RuntimeClient`,
rather than through `invoke`'s return type: the tools neither know nor care that
hooks ran, and the records are for the *user*, not the model. The session
journals one `SessionDomainEvent::HookRan` per record. Before/after payloads are
clamped so a large file write cannot bloat the journal.

`HookRan` deliberately folds to nothing in `apply_event`: it records what the
agent did, not what the session *is*.

## Goals

1. Every hook event horsie can meaningfully implement, implemented.
2. Any event it cannot implement fails loudly, naming the event — never a silent
   no-op.
3. Hooks published for Claude Code work unmodified, including their matchers.
4. A session with no hooks pays no per-tool-call cost.

## Non-goals

Agents, commands and `.mcp.json` (Phases 2–4 of #105); a permission system;
context compaction; changing where runtimes are sandboxed.

## What the ecosystem actually does

Sizing this by Claude Code's documented surface alone would overbuild it. Every
harness with hooks has its own vocabulary, and none implements all 31:

| Harness | Events | Naming | Declared in |
| --- | ---: | --- | --- |
| Claude Code | 31 | `PreToolUse` | plugin `hooks/hooks.json` |
| OpenCode | ~28 | `tool.execute.before` | JS/TS plugin code |
| Cursor | ~21 | `preToolUse` | `.cursor/hooks.json` |
| Codex CLI | 11 | `PreToolUse` — identical to Claude's | `.codex/hooks.json` |
| Grok Build | Claude's | Claude-compatible | reads Claude's directly |
| Gemini, Kiro, Pi, Qoder, Rovo Dev, Mistral Vibe, Copilot, Antigravity | none documented | — | — |

Plugin authors do not rely on graceful degradation across these — impeccable
generates four separate hook manifests, one per vocabulary. Codex is the closest
precedent for horsie: it borrows Claude's exact event names and implements 11 of
the 31, which has proven sufficient for a real plugin ecosystem.

Demand is narrower still. Across *every* plugin in the official marketplace,
only six distinct events are declared anywhere:

```
3  Stop              2  PostToolUse
3  SessionStart      1  PreToolUse
2  UserPromptSubmit  1  UserPromptExpansion
```

The supported set below is therefore drawn from measured demand plus the two
subagent events Codex and Cursor both ship, rather than from "the seam exists".

## The event inventory

### A. Supported (8) — this phase

| Event | Seam |
| --- | --- |
| `SessionStart` | already wired; migrates onto the new dispatch |
| `SessionEnd` | `delete_session` (`server/src/http/handlers.rs:406`) |
| `UserPromptSubmit` | session actor, before the agent run |
| `PreToolUse` | `runtime::hooks`, before `tools::dispatch` |
| `PostToolUse` | `runtime::hooks`, after `tools::dispatch` |
| `Stop` | session actor, `AgentOutcome::Concluded` |
| `SubagentStart` / `SubagentStop` | `workflow/src/workflow_actor.rs` `spawn_agent` |

Five of these are the five in-the-wild events horsie can run; the sixth in the
wild, `UserPromptExpansion`, needs slash commands (Phase 3 of #105). The
subagent pair is added because Codex and Cursor both ship it and Phase 2 of #105
makes session subagents real.

### B. Deferred — the seam exists, nothing uses it (7)

`PostToolUseFailure`, `PostToolBatch`, `StopFailure`, `Notification`,
`TaskCreated`, `TaskCompleted`, `CwdChanged`.

horsie could implement each of these today, and each is declared by exactly zero
published plugins. Building them now would be speculative code exercised only by
its own tests. They fail like any other unsupported event, but with an error
that says horsie has not implemented them *yet* and invites an issue — as
opposed to groups C and D, which say horsie has no such concept.

### C. Needs a horsie feature that does not exist (9)

`UserPromptExpansion` (no slash commands — Phase 3 of #105),
`PermissionRequest`, `PermissionDenied` (no permission model), `PreCompact`,
`PostCompact` (no context compaction), `FileChanged` (no file watcher),
`ConfigChange` (no in-session config reload), `DirectoryAdded` (workspaces are
fixed at session start), `Setup` (no init mode).

### D. No horsie concept at all (7)

`MessageDisplay`, `TeammateIdle`, `WorktreeCreate`, `WorktreeRemove`,
`Elicitation`, `ElicitationResult`, `InstructionsLoaded`.

8 + 7 + 9 + 7 = 31. Groups B, C and D all fail; the distinction is only in what
the error tells the user to do about it.

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

**Matchers are evaluated runtime-side**, in the same pass that runs the hook,
because that is where both the plugin declarations and the tool call already
are. `horsie-support` gains a `regex` dependency; `regex` is already in the
workspace tree (`runtime/Cargo.toml`), so this adds an edge rather than a new
crate.

One wrinkle the runtime must bridge: the wire `ToolCall` union is tagged in
PascalCase (`Bash`, `FindAndReplace`), while the LLM, the matcher alias table
and the user all see snake_case (`bash`, `find_and_replace`). `runtime::hooks`
maps the tag to the agent-facing name with an exhaustive match, so adding a tool
fails to compile until it is named.

### The hook reader

`horsie-support::plugin::hooks` gains:

```rust
pub enum HookEvent { SessionStart, PreToolUse, /* … the 8 supported … */ }

pub struct HookDecl {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
}

/// Why an event cannot run, which decides what the error tells the user.
pub enum Unsupported {
    /// Group B: horsie has the seam, no published plugin uses it.
    NotImplemented,
    /// Groups C and D: horsie has no such concept.
    NoConcept,
    /// Not a documented Claude Code event at all.
    Unknown,
}

pub struct PluginHooks {
    pub decls: Vec<HookDecl>,
    /// Event names this build cannot run, verbatim as declared, with the reason.
    pub unsupported: Vec<(String, Unsupported)>,
}

pub fn read(plugin_root: &Path) -> Result<PluginHooks, String>;
```

`HookEvent::parse` recognises all 31 documented names. A name in group A becomes
a variant; anything else lands in `unsupported` with its reason. Collecting
rather than erroring lets the caller report *every* unsupported event at once
instead of one per attempt, and lets the message differ: "horsie does not
implement `PostToolBatch` yet" reads very differently from "horsie has no
worktree concept".

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

`models/fluorite/runtime.fl` gains only `HookRecord` and one field on
`ToolCallResponse`:

```
struct ToolCallResponse {
    call_id: String,
    result: ToolResult,
    /// Every hook that ran for this call, in execution order. Empty for the
    /// overwhelmingly common case of a session with no matching hooks.
    hooks: Vec<HookRecord>,
}
```

No manifest message, no `RunHook` message, and no new outbound variant. A denial
is `ToolResult::Err`; the mutations are applied by the runtime before and after
dispatch. The record exists for the user's audit trail, not to carry the
decision — the decision is already in the result.

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

Unchanged by this phase: hooks run inside the runtime, because that is where the
plugin files and the workspace are.

Since #115, `horsie connect` **sandboxes its runtimes by default** — the flag
inverted from an opt-in `--sandbox` to an opt-out `--no-sandbox`, and the
capability spec is vendor-owned. So hooks normally run under the nono sandbox,
with the plugin library and its clone roots granted read-only (the grants added
in #110). A user who passes `--no-sandbox` runs them unconfined on their own
machine, as does anyone who was relying on the old default.

This phase widens *when* hooks run, not *what* they can reach: the confinement
is whatever the runtime already had. The guide still tells users to install only
plugins they trust, which remains the operative advice for the `--no-sandbox`
path.

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

Two stacked PRs.

**PR1 — dispatch layer and tool events.** The reader with all 31 event names
classified, tool-name aliasing, matcher evaluation, the manifest and `RunHook`
protocol, the runtime-side executor and control-protocol parser, the
the runtime-side hook dispatcher, and the record path that journals what hooks
did.
Wires `PreToolUse` and `PostToolUse` — the two events that need the decorator,
and the two that carry blocking and mutation.

**PR2 — session, turn and subagent events.** `SessionEnd`, `UserPromptSubmit`,
`Stop`, `SubagentStart`, `SubagentStop`, and the migration of `SessionStart`
onto the new dispatch.

## Testing

- `horsie-support` unit tests: every one of the 31 names parses to either a
  supported variant or an unsupported entry carrying its group, with a test that
  asserts the counts (8 supported / 7 deferred / 16 absent) so adding a variant
  cannot silently change the contract.
- A test that each unsupported group produces its own guidance — "not
  implemented yet" for group B versus "no such concept" for C and D.
- Matcher tests using the three real-world matchers, asserting
  `Edit|Write|MultiEdit|NotebookEdit` matches `write_file` and
  `find_and_replace` and does not match `bash`.
- Control-protocol tests over fixture scripts: exit 0 with each JSON shape, exit
  2, non-zero exit, and a timeout — asserting `blocked` versus `failed`.
- A `runtime::hooks` test proving a denying `PreToolUse` hook prevents the tool
  from running at all, and that the denial reaches the model as an error tool
  result naming the plugin.
- A test that a hook rewriting `updatedInput` changes what actually executes,
  and that an input a hook mangles into something undeserializable is ignored
  rather than corrupting the call.
- A test that a failing `PreToolUse` hook denies, and that a failing
  `PostToolUse` hook does not.
- An end-to-end test with a real plugin fixture whose hook rewrites a tool's
  input, proving `updatedInput` takes effect.
- The existing `SessionStart` runtime and server tests must pass unmodified —
  they are the guard that the generalisation preserved today's behaviour.

## Consequences

- A plugin declaring any group B, C or D event becomes uninstallable. Measured
  against the official marketplace today, that rejects exactly one pattern:
  `UserPromptExpansion`, used by `claude-security`. The exposure is therefore
  much smaller than the raw count of unsupported events suggests.
- No other harness hard-fails this way — Cursor, Codex and OpenCode all appear
  to ignore what they do not recognise, which is how impeccable ships one
  payload to fifteen harnesses. horsie is deliberately stricter: it should never
  claim to run a guard it silently drops. The cost is brittleness against Claude
  Code adding events, since a plugin adopting a new one fails until horsie
  learns it. The error names the event, so diagnosis is immediate.
- `PostToolUseFailure` is nearly free to add later — the runtime dispatcher
  already distinguishes `Err` from `Ok` — so if a plugin ever needs it,
  promoting it out of group B is a small change rather than new machinery.
- `PreToolUse` adds a runtime round-trip per matching tool call. Sessions whose
  plugins declare no matching hook are unaffected.
- Hooks can now change what a tool does. A buggy hook can corrupt a tool call in
  a way that is hard to attribute, so `systemMessage` and the reason strings
  should always name the plugin.
