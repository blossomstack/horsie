# Hook events library, and horsie's wiring onto it — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every per-event hook fact into one spec-derived library, reshape `HookRecord` into a per-event model where illegal states are unrepresentable, and wire horsie's five real call sites (`PreToolUse`, `PostToolUse`, `SessionStart`, `Stop`, `UserPromptSubmit`) onto it — with `Stop` honouring continuation.

**Architecture:** Three layers. `horsie_support::plugin::hooks` is a complete, spec-derived library: it describes all fifteen events horsie has a seam for (input payload, matcher domain, permitted output fields), turns a hook process's `(exit_code, stdout, stderr)` into one typed `HookOutput`, and folds that output into the event's own fluorite record. The runtime owns process execution and nothing else — it is the only place hooks ever run, tool-inline or server-initiated. The server owns consequences: `PreToolUse` fails closed, `Stop` continues the turn, everything else is recorded.

**Tech Stack:** Rust 2024 (workspace crates `models`, `support`, `runtime`, `runtime-client`, `workflow`, `server`, `cli`), fluorite IDL codegen → Rust + TypeScript, React 19 + Vite + Vitest + Playwright for `clients/web`.

**Spec:** `docs/superpowers/specs/2026-08-04-hook-events-library-design.md`

## Global Constraints

- **Branch:** `feat/hook-events-model`, worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie/.horsie/worktrees/tool-response`. Based on `origin/main` @ `60e4ef1`.
- **No backward compatibility.** Hook records journaled since #140 stop deserializing. This is accepted, twice, explicitly. Do not add `#[serde(default)]` shims, do not keep a legacy variant.
- **Scope of *wiring* is five events:** `PreToolUse`, `PostToolUse`, `SessionStart` (already wired) plus `Stop`, `UserPromptSubmit` (new). The library *models* fifteen. The remaining ten stay `NotImplemented` and refused at install. Do not add call sites for them.
- **CI gates, all of which must pass before the PR is opened:**
  - `cargo fmt --all` then `cargo clippy --locked --all-targets --all-features -- -D warnings`
  - `cargo test --workspace` (never `-p horsie-runtime-client` alone — feature unification breaks `horsie_agentcore::testkit`)
  - `make ts-types` then `git diff --exit-code clients/ts/src/generated`
  - `cd clients/web && bun install && bun run generate-types && bun run build && bunx vitest run`
- **fmt before clippy, always.** clippy caches a failed parse and reports stale errors otherwise.
- **Never pass `-c user.name` / `-c user.email` to git.** The repo identity is already correct.
- **Commit messages:** short subject, no body unless the diff hides context. No `Co-Authored-By`, no tool attribution.
- **Verified toolchain facts** (checked against this tree, do not re-derive):
  - fluorite unions accept payload-less arms: `union U { WithPayload(A), Bare }` → `#[serde(tag="kind", content="value")] enum U { WithPayload(A), Bare }` in Rust and `{ kind: "Bare" }` in TS. The spec's `Ask` / `Defer` / `Ran` unit arms are legal.
  - `models/build.rs` compiles the whole `models/fluorite` directory, so a new `.fl` file needs only a `pub mod` in `models/src/lib.rs`.
  - A new `.fl` file reachable from `agent.fl`'s `use` **must** be added to `generate-types` in **both** `clients/ts/package.json` and `clients/web/package.json`, or TS codegen fails.
  - `horsie-support` already depends on `horsie-models`, so record construction can live in the library.
  - CI runs clippy with `-D warnings`; oversized generated enum variants need `#[allow(clippy::large_enum_variant)]` with a comment (precedent: `models/src/lib.rs:1-9`, `server/src/sessions/mod.rs:56`).
  - Local Playwright on macOS needs `TMPDIR=/tmp/he2e`, a release build of `horsie-server`/`horsie-runtime`/`horsie`, `bun run build`, then `HORSIE_E2E_SKIP_BUILD=1`.

## File Structure

**Created**

| File | Responsibility |
| --- | --- |
| `support/src/plugin/hooks/mod.rs` | Re-exports; `HookDecl`, `PluginHooks`, `read()` (moved verbatim from the flat file) |
| `support/src/plugin/hooks/events.rs` | `HookEvent` (15 arms), `Unsupported`, `OutputField`, the permitted-field table, `is_wired()`, `claude_aliases`, `matcher_selects` |
| `support/src/plugin/hooks/process.rs` | `HookReply` → `HookOutput`. Exit-code semantics, JSON envelope parsing, off-spec field rejection |
| `support/src/plugin/hooks/invoke.rs` | `HookInvocation` — one arm per event, carrying that event's facts. `event()`, `matcher_subjects()`, `payload()`, `record()` |
| `models/fluorite/hooks.fl` | The per-event record model: `HookRecord`, `HookAction`, 15 record structs, their outcome unions, shared payloads |
| `runtime/src/hooks/mod.rs` | `matching()`, `run_one()` — the shared runner |
| `runtime/src/hooks/tool.rs` | `dispatch_with_hooks` — `PreToolUse` / `PostToolUse`, inline with the call |
| `runtime/src/hooks/server.rs` | `run_hooks` — the server-initiated events |
| `clients/web/src/components/HookNoticeRow.tsx` | A standalone (non-tool) hook record as its own transcript row |
| `clients/web/src/lib/hookSummary.ts` | `HookAction` → the one line a person reads. Shared by the tool card and the notice row |

**Modified**

| File | Change |
| --- | --- |
| `support/src/plugin/hooks.rs` | Deleted — becomes the `hooks/` directory |
| `models/fluorite/runtime.fl` | `HookRecord` deleted; `SessionStartRequest`/`Response` replaced by `RunHooksRequest`/`Response` + `ServerHookEvent`; `ToolCallResponse.hooks` retyped |
| `models/fluorite/agent.fl` | `use runtime.HookRecord` → `use hooks.HookRecord` |
| `models/src/lib.rs` | `pub mod hooks` |
| `runtime/src/hooks.rs` | Deleted — becomes the `hooks/` directory |
| `runtime/src/plugins.rs` | `session_start_commands`, `run_hook`, `extract_context`, `run_session_start` deleted; `run_hook_raw` becomes `pub(crate)`-to-`pub(crate)` unchanged |
| `runtime/src/main.rs` | `SessionStart` inbound arm → `RunHooks` |
| `runtime-client/src/{client,transport,testkit}.rs` | `run_session_start` → `run_hooks(event)` returning records |
| `workflow/src/agent_actor.rs` | `hook_entry_id` → `hook:{seq}`; `hook_records_for` → `hook_entry_count`; `AgentCommand::ContinueAfterStop` |
| `workflow/src/context.rs` | `AgentOutcomeSink` unchanged; `SharedContext.bootstrap` derived from records |
| `server/src/sessions/session_actor.rs` | `StopHookSink` decorator, `SessionCommand::StopBlocked`, `run_hooks` bootstrap |
| `server/src/sessions/events.rs` | `agent_frame` hook arm — new entry id |
| `server/src/wire_redact.rs` | New record shape |
| `cli/src/plugins.rs` | Install refusal names unsupported hook events |
| `clients/web/src/hooks/useSessionStream.ts` | `items: TranscriptItem[]` replaces `messages` |
| `clients/web/src/components/{Transcript,ToolCallCard}.tsx` | Notice rows; per-action tool summary |
| `clients/web/src/lib/transcriptSegments.ts` | Operates on `TranscriptItem[]` |
| `clients/{ts,web}/package.json` | `hooks.fl` added to `generate-types` |

**Out of this PR, tracked as follow-ups:** the ten unwired events' call sites; `continue`/`stopReason`; HTTP hooks.

---

## Task 1: The event table

The library's spine. Every per-event fact that today lives in an inline `json!`, a match arm in the dispatcher, or nowhere at all.

**Files:**
- Create: `support/src/plugin/hooks/events.rs`
- Create: `support/src/plugin/hooks/mod.rs`
- Delete: `support/src/plugin/hooks.rs` (content splits between the two above)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  ```rust
  pub enum HookEvent { PreToolUse, PostToolUse, PostToolUseFailure, PostToolBatch,
      SessionStart, SessionEnd, UserPromptSubmit, Stop, StopFailure,
      SubagentStart, SubagentStop, TaskCreated, TaskCompleted, Notification, CwdChanged }
  pub enum Unsupported { NotImplemented, NoConcept, Unknown }
  pub enum OutputField { SystemMessage, Decision, PermissionDecision,
      AdditionalContext, UpdatedInput, UpdatedToolOutput }
  impl HookEvent {
      pub fn parse(name: &str) -> Result<HookEvent, Unsupported>;
      pub fn name(self) -> &'static str;
      pub fn is_wired(self) -> bool;
      pub fn permitted(self) -> &'static [OutputField];
      pub fn permits(self, field: OutputField) -> bool;
      pub fn injects_bare_stdout(self) -> bool;
  }
  pub fn claude_aliases(horsie_tool: &str) -> &'static [&'static str];
  pub fn matcher_selects(matcher: Option<&str>, subjects: &[&str]) -> bool;
  pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool;
  pub struct HookDecl { pub event: HookEvent, pub matcher: Option<String>,
      pub command: String, pub timeout: Option<u64> }
  pub struct PluginHooks { pub decls: Vec<HookDecl>, pub unsupported: Vec<(String, Unsupported)> }
  pub fn read(plugin_root: &Path) -> Result<PluginHooks, String>;
  ```

The classification contract changes shape but not behaviour. Today `parse` returns `Ok` only for the three wired events. Now `parse` returns `Ok` for all fifteen events the library *describes* — that is protocol knowledge, not a horsie capability — and `read()` is what refuses: a decl whose event `!is_wired()` goes to `unsupported` with `NotImplemented`. A plugin declaring `Stop` before Task 9 still gets exactly the message it gets today.

- [ ] **Step 1: Create the module directory and move the unchanged parts**

`git mv support/src/plugin/hooks.rs support/src/plugin/hooks/mod.rs`, then create `support/src/plugin/hooks/events.rs`. Move `HookEvent`, `Unsupported`, `claude_aliases`, `matcher_applies` and their tests into `events.rs`; leave `HookDecl`, `PluginHooks`, `read` and their tests in `mod.rs`. Head `mod.rs` with:

```rust
//! Reading and classifying `<plugin>/hooks/hooks.json`.
//!
//! The library below is spec-derived and horsie-free: it describes every hook
//! event horsie has a seam for, turns a hook process's reply into one typed
//! outcome, and folds that outcome into the event's own record. What a verdict
//! *means* is never decided here — `PreToolUse` fails closed, `Stop` blocking
//! continues a turn, `Notification` cannot block at all. One parser, three
//! consequences, and the consequences belong to the call sites.

mod events;
mod invoke;
mod process;

pub use events::{
    HookEvent, OutputField, Unsupported, claude_aliases, matcher_applies, matcher_selects,
};
pub use invoke::HookInvocation;
pub use process::{HookOutput, HookReply, Permission, Verdict};
```

(`invoke` and `process` are created in Tasks 2 and 3; add those `mod`/`pub use` lines only when each lands, so the tree compiles at every step.)

- [ ] **Step 2: Write the failing tests for the widened event set**

Replace `every_documented_event_is_classified_with_the_expected_counts` in `events.rs` with these, and add the new ones:

```rust
/// The library describes every event horsie has a seam for; `read()` is what
/// refuses the unwired ones. Pinning both halves separately is the point:
/// widening what the library knows must not silently widen what horsie claims.
#[test]
fn all_fifteen_seam_events_are_described_and_the_other_sixteen_are_not() {
    let mut described = 0;
    let mut no_concept = 0;
    for name in ALL_31 {
        match HookEvent::parse(name) {
            Ok(_) => described += 1,
            Err(Unsupported::NoConcept) => no_concept += 1,
            Err(Unsupported::NotImplemented) => {
                panic!("{name}: parse describes or refuses; NotImplemented is read()'s verdict")
            }
            Err(Unsupported::Unknown) => panic!("{name} is documented but classified Unknown"),
        }
    }
    assert_eq!(described, 15, "described set changed");
    assert_eq!(no_concept, 16, "absent set changed");
}

/// Wiring an event is a deliberate act. This is the list the PR moves.
#[test]
fn exactly_five_events_are_wired() {
    let wired: Vec<&str> = ALL_31
        .iter()
        .filter_map(|n| HookEvent::parse(n).ok())
        .filter(|e| e.is_wired())
        .map(|e| e.name())
        .collect();
    assert_eq!(
        wired,
        vec!["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"],
        "wired set changed"
    );
}

/// The field table is the whole point of the library: a hook setting a field
/// its event does not offer must be visibly ignored, never silently obeyed.
#[test]
fn only_pre_tool_use_may_rewrite_an_input() {
    assert!(HookEvent::PreToolUse.permits(OutputField::UpdatedInput));
    for e in [HookEvent::PostToolUse, HookEvent::SessionStart, HookEvent::Stop] {
        assert!(!e.permits(OutputField::UpdatedInput), "{}", e.name());
    }
}

#[test]
fn pre_tool_use_offers_no_additional_context() {
    // The bug on main: records carried it, nothing consumed it, the spec does
    // not offer it, and there is no result yet to attach it to.
    assert!(!HookEvent::PreToolUse.permits(OutputField::AdditionalContext));
    assert!(HookEvent::PostToolUse.permits(OutputField::AdditionalContext));
}

#[test]
fn side_effect_events_permit_no_output_at_all() {
    for e in [
        HookEvent::SessionEnd,
        HookEvent::StopFailure,
        HookEvent::Notification,
        HookEvent::CwdChanged,
    ] {
        assert!(e.permitted().is_empty(), "{} must permit nothing", e.name());
    }
}

#[test]
fn session_start_cannot_block() {
    assert!(!HookEvent::SessionStart.permits(OutputField::Decision));
    assert!(!HookEvent::SessionStart.permits(OutputField::PermissionDecision));
}

/// Only these two read bare stdout as injected context. For every other event
/// non-JSON stdout is debug output and is discarded.
#[test]
fn bare_stdout_is_context_for_exactly_two_events() {
    let injecting: Vec<&str> = ALL_31
        .iter()
        .filter_map(|n| HookEvent::parse(n).ok())
        .filter(|e| e.injects_bare_stdout())
        .map(|e| e.name())
        .collect();
    assert_eq!(injecting, vec!["SessionStart", "UserPromptSubmit"]);
}

/// A matcher's subject is per-event, not always a tool name.
#[test]
fn a_matcher_selects_on_the_events_own_subject() {
    assert!(matcher_selects(Some("startup|resume"), &["startup"]));
    assert!(!matcher_selects(Some("startup|resume"), &["compact"]));
    // CwdChanged has no matcher domain: only an absent matcher selects it.
    assert!(matcher_selects(None, &[]));
    assert!(!matcher_selects(Some("anything"), &[]));
}

/// `read()` keeps its contract: a plugin declaring an unwired event is told so
/// at install, exactly as before, rather than installing to silence.
#[test]
fn read_defers_a_described_but_unwired_event() {
    let dir = TempDir::new().unwrap();
    write_hooks(
        dir.path(),
        r#"{"hooks":{
             "PreToolUse":[{"hooks":[{"type":"command","command":"ok"}]}],
             "PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
    );
    let h = read(dir.path()).unwrap();
    assert_eq!(h.decls.len(), 1, "only the wired event runs");
    assert_eq!(
        h.unsupported,
        vec![("PostToolBatch".to_string(), Unsupported::NotImplemented)]
    );
}
```

Move `write_hooks` and the `read`-facing tests into `mod.rs`'s test module; keep the `HookEvent`/matcher tests in `events.rs`. `ALL_31` is needed by both — put it in `events.rs` as `#[cfg(test)] pub(super) const ALL_31`.

Also update `the_supported_three_are_exactly_these` → delete (replaced by `exactly_five_events_are_wired`), and `the_unwired_events_are_deferred_until_their_call_sites_exist` → assert through `read()` rather than `parse`.

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p horsie-support --lib
```
Expected: compile errors — `OutputField` not found, `is_wired`/`permits`/`permitted`/`injects_bare_stdout`/`matcher_selects` not found, `HookEvent::Stop` not a variant.

- [ ] **Step 4: Write `events.rs`**

```rust
//! Every Claude Code hook event horsie has a seam for, described once.
//!
//! Three facts per event, each of which the rest of the system used to
//! re-derive: what its stdin payload looks like (see `invoke.rs`), what its
//! `matcher` selects on, and which output fields it may set. Claude Code
//! documents 31 events; the sixteen absent here need a horsie subsystem that
//! does not exist, so they are refused rather than modelled.

/// A hook event horsie can describe. Being here is protocol knowledge, not a
/// capability: [`HookEvent::is_wired`] is what says horsie has a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PostToolBatch,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
    Notification,
    CwdChanged,
}

/// Why a declared hook cannot run, which decides what the error tells the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Described by the library; horsie has no call site for it yet.
    NotImplemented,
    /// horsie has no such concept, and no plan for one.
    NoConcept,
    /// Not a documented Claude Code event at all.
    Unknown,
}

/// A field a hook may set on its JSON reply.
///
/// Named rather than free-form because the whole illegal-state problem this
/// library exists to fix was a field recorded on an event that never offered
/// it. A hook may still *emit* anything; what it may *affect* is this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputField {
    /// `systemMessage` — warning text shown to the user, never to the model.
    SystemMessage,
    /// Top-level `decision: "block"` with `reason`.
    Decision,
    /// `hookSpecificOutput.permissionDecision` — `PreToolUse` only.
    PermissionDecision,
    /// `hookSpecificOutput.additionalContext` — injected into the model.
    AdditionalContext,
    /// `hookSpecificOutput.updatedInput` — `PreToolUse` only.
    UpdatedInput,
    /// `hookSpecificOutput.updatedToolOutput` — `PostToolUse` only.
    UpdatedToolOutput,
}

use OutputField::{
    AdditionalContext, Decision, PermissionDecision, SystemMessage, UpdatedInput, UpdatedToolOutput,
};

impl HookEvent {
    /// Classify a documented event name. `Err` carries why horsie cannot
    /// describe it — never `NotImplemented`, which is [`super::read`]'s verdict
    /// about horsie's call sites rather than this table's about the protocol.
    pub fn parse(name: &str) -> Result<HookEvent, Unsupported> {
        match name {
            "PreToolUse" => Ok(HookEvent::PreToolUse),
            "PostToolUse" => Ok(HookEvent::PostToolUse),
            "PostToolUseFailure" => Ok(HookEvent::PostToolUseFailure),
            "PostToolBatch" => Ok(HookEvent::PostToolBatch),
            "SessionStart" => Ok(HookEvent::SessionStart),
            "SessionEnd" => Ok(HookEvent::SessionEnd),
            "UserPromptSubmit" => Ok(HookEvent::UserPromptSubmit),
            "Stop" => Ok(HookEvent::Stop),
            "StopFailure" => Ok(HookEvent::StopFailure),
            "SubagentStart" => Ok(HookEvent::SubagentStart),
            "SubagentStop" => Ok(HookEvent::SubagentStop),
            "TaskCreated" => Ok(HookEvent::TaskCreated),
            "TaskCompleted" => Ok(HookEvent::TaskCompleted),
            "Notification" => Ok(HookEvent::Notification),
            "CwdChanged" => Ok(HookEvent::CwdChanged),

            // No horsie concept: no permission model (horsie runs unattended by
            // design), no context compaction, no worktrees, no file watcher, no
            // slash commands, no agent teams, no MCP elicitation, no display
            // layer. Each would need a subsystem, not a call site.
            "UserPromptExpansion" | "PermissionRequest" | "PermissionDenied" | "PreCompact"
            | "PostCompact" | "FileChanged" | "ConfigChange" | "DirectoryAdded" | "Setup"
            | "MessageDisplay" | "TeammateIdle" | "WorktreeCreate" | "WorktreeRemove"
            | "Elicitation" | "ElicitationResult" | "InstructionsLoaded" => {
                Err(Unsupported::NoConcept)
            }

            _ => Err(Unsupported::Unknown),
        }
    }

    /// The documented name, so a record can be attributed on the wire.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PostToolBatch => "PostToolBatch",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::Notification => "Notification",
            HookEvent::CwdChanged => "CwdChanged",
        }
    }

    /// Whether horsie has a call site that fires this event.
    ///
    /// Exhaustive on purpose: promoting an event is a one-line change here plus
    /// its call site, and adding a variant without deciding fails to compile.
    pub fn is_wired(self) -> bool {
        match self {
            HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::Stop => true,
            HookEvent::PostToolUseFailure
            | HookEvent::PostToolBatch
            | HookEvent::SessionEnd
            | HookEvent::StopFailure
            | HookEvent::SubagentStart
            | HookEvent::SubagentStop
            | HookEvent::TaskCreated
            | HookEvent::TaskCompleted
            | HookEvent::Notification
            | HookEvent::CwdChanged => false,
        }
    }

    /// Which output fields this event may set.
    ///
    /// `PreToolUse` keeps top-level `Decision` alongside `PermissionDecision`:
    /// the docs deprecate the former but still honour it, and published plugins
    /// use it. Refusing it would break them for no gain.
    pub fn permitted(self) -> &'static [OutputField] {
        match self {
            HookEvent::PreToolUse => {
                &[SystemMessage, Decision, PermissionDecision, UpdatedInput]
            }
            HookEvent::PostToolUse => {
                &[SystemMessage, Decision, AdditionalContext, UpdatedToolOutput]
            }
            HookEvent::PostToolUseFailure
            | HookEvent::PostToolBatch
            | HookEvent::UserPromptSubmit
            | HookEvent::Stop
            | HookEvent::SubagentStop => &[SystemMessage, Decision, AdditionalContext],
            // No `decision`: neither can refuse anything, because by the time
            // they run there is nothing left to refuse.
            HookEvent::SessionStart | HookEvent::SubagentStart => {
                &[SystemMessage, AdditionalContext]
            }
            HookEvent::TaskCreated | HookEvent::TaskCompleted => &[SystemMessage],
            // Side-effect only: the docs give these no JSON output at all, not
            // even `systemMessage`, and exit 2 has no special meaning for them.
            HookEvent::SessionEnd
            | HookEvent::StopFailure
            | HookEvent::Notification
            | HookEvent::CwdChanged => &[],
        }
    }

    /// Whether this event may set `field`.
    pub fn permits(self, field: OutputField) -> bool {
        self.permitted().contains(&field)
    }

    /// Whether non-JSON stdout is injected context rather than debug output.
    pub fn injects_bare_stdout(self) -> bool {
        matches!(self, HookEvent::SessionStart | HookEvent::UserPromptSubmit)
    }
}
```

Keep `Unsupported::explain` exactly as it is today (its wording is pinned by a test), and keep `claude_aliases` verbatim. Replace `matcher_applies` with:

```rust
/// Whether a hook's `matcher` selects an occurrence, given that occurrence's
/// matchable names.
///
/// The regex semantics are unchanged — unanchored, absent or empty selects
/// everything, a pattern that fails to compile selects nothing so a broken
/// matcher cannot widen to "all". What generalises is the *subject*: a tool
/// event passes the tool name and its Claude aliases, `SessionStart` passes its
/// `source`, `Notification` its type. An event with no matcher domain passes
/// nothing, so only an absent matcher selects it.
pub fn matcher_selects(matcher: Option<&str>, subjects: &[&str]) -> bool {
    let Some(pattern) = matcher.map(str::trim).filter(|m| !m.is_empty()) else {
        return true;
    };
    let Ok(re) = regex::Regex::new(pattern) else {
        return false;
    };
    subjects.iter().any(|s| re.is_match(s))
}

/// [`matcher_selects`] for a tool event, whose subjects are the horsie tool
/// name plus every Claude name it answers to.
pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool {
    let mut subjects = vec![horsie_tool];
    subjects.extend_from_slice(claude_aliases(horsie_tool));
    matcher_selects(matcher, &subjects)
}
```

- [ ] **Step 5: Update `read()` to refuse unwired events**

In `mod.rs`, the classification arm becomes:

```rust
        let event = match HookEvent::parse(name) {
            // Described, but horsie has nowhere to fire it. Deferred rather
            // than accepted so no hook installs believing it works and then
            // silently never fires.
            Ok(e) if !e.is_wired() => {
                out.unsupported.push((name.clone(), Unsupported::NotImplemented));
                continue;
            }
            Ok(e) => e,
            Err(reason) => {
                out.unsupported.push((name.clone(), reason));
                continue;
            }
        };
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo fmt --all && cargo test -p horsie-support --lib
```
Expected: PASS. Then `cargo clippy -p horsie-support --all-targets -- -D warnings`.

- [ ] **Step 7: Commit**

```bash
git add support/src/plugin/hooks
git commit -m "hooks: describe all fifteen seam events in one table"
```

---

## Task 2: The generic processor

One code path from a hook process's reply to a typed outcome, driven by Task 1's table. This is what stops the next `additionalContext`-on-`PreToolUse`.

**Files:**
- Create: `support/src/plugin/hooks/process.rs`
- Modify: `support/src/plugin/hooks/mod.rs` (add `mod process;` and its `pub use`)

**Interfaces:**
- Consumes: `HookEvent`, `OutputField` from Task 1.
- Produces:
  ```rust
  pub struct HookReply { pub code: Option<i32>, pub stdout: String, pub stderr: String }
  pub enum Verdict { Proceed, Block { reason: Option<String> }, Failed { reason: String } }
  pub enum Permission { Deny { reason: Option<String> }, Ask, Defer }
  pub struct HookOutput {
      pub verdict: Verdict,
      pub permission: Option<Permission>,
      pub system_message: Option<String>,
      pub additional_context: Option<String>,
      pub updated_input: Option<serde_json::Value>,
      pub updated_tool_output: Option<String>,
      pub ignored: Vec<&'static str>,
  }
  pub fn process(event: HookEvent, reply: &HookReply) -> HookOutput;
  ```

- [ ] **Step 1: Write the failing tests**

Create `support/src/plugin/hooks/process.rs` with only its test module first:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn ok(stdout: &str) -> HookReply {
        HookReply { code: Some(0), stdout: stdout.to_string(), stderr: String::new() }
    }

    #[test]
    fn exit_zero_with_no_output_just_proceeds() {
        let out = process(HookEvent::PostToolUse, &ok(""));
        assert!(matches!(out.verdict, Verdict::Proceed));
        assert!(out.system_message.is_none());
        assert!(out.ignored.is_empty());
    }

    /// Exit 2 is a blocking error: stderr is the reason and stdout is ignored
    /// entirely, JSON or not. A hook cannot both refuse and rewrite.
    #[test]
    fn exit_two_blocks_with_stderr_and_discards_stdout() {
        let reply = HookReply {
            code: Some(2),
            stdout: r#"{"hookSpecificOutput":{"additionalContext":"ignored"}}"#.to_string(),
            stderr: "  writes are not allowed\n".to_string(),
        };
        let out = process(HookEvent::PostToolUse, &reply);
        match out.verdict {
            Verdict::Block { reason } => assert_eq!(reason.as_deref(), Some("writes are not allowed")),
            other => panic!("expected a block, got {other:?}"),
        }
        assert!(out.additional_context.is_none());
    }

    /// An event that cannot block treats exit 2 as a plain failure — the same
    /// process reply, a different outcome, decided by the table.
    #[test]
    fn exit_two_is_a_failure_for_an_event_that_cannot_block() {
        let reply = HookReply { code: Some(2), stdout: String::new(), stderr: "boom".into() };
        let out = process(HookEvent::SessionStart, &reply);
        match out.verdict {
            Verdict::Failed { reason } => assert!(reason.contains("boom"), "{reason}"),
            other => panic!("SessionStart cannot block, got {other:?}"),
        }
    }

    #[test]
    fn any_other_exit_is_a_failure_naming_the_first_line_of_stderr() {
        let reply = HookReply {
            code: Some(1),
            stdout: String::new(),
            stderr: "cannot find node\nstack trace line\nanother".into(),
        };
        match process(HookEvent::PreToolUse, &reply).verdict {
            Verdict::Failed { reason } => {
                assert!(reason.contains("cannot find node"), "{reason}");
                assert!(!reason.contains("stack trace"), "one line, not a dump: {reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// `None` is spawn failure or timeout — an outage, never a decision.
    #[test]
    fn a_hook_that_never_ran_is_a_failure() {
        let reply = HookReply { code: None, stdout: String::new(), stderr: "timed out".into() };
        assert!(matches!(
            process(HookEvent::PreToolUse, &reply).verdict,
            Verdict::Failed { .. }
        ));
    }

    #[test]
    fn bare_stdout_is_context_for_session_start() {
        let out = process(HookEvent::SessionStart, &ok("  project conventions  "));
        assert_eq!(out.additional_context.as_deref(), Some("project conventions"));
    }

    /// For every other event non-JSON stdout is debug output. Recording it as
    /// injected context is how `PreToolUse` ended up carrying a field it never
    /// had.
    #[test]
    fn bare_stdout_is_discarded_for_every_other_event() {
        for e in [HookEvent::PreToolUse, HookEvent::PostToolUse, HookEvent::Stop] {
            let out = process(e, &ok("debug noise"));
            assert!(out.additional_context.is_none(), "{}", e.name());
            assert!(matches!(out.verdict, Verdict::Proceed));
        }
    }

    #[test]
    fn a_permitted_field_is_read() {
        let out = process(
            HookEvent::PostToolUse,
            &ok(r#"{"systemMessage":"heads up","hookSpecificOutput":{"additionalContext":"note","updatedToolOutput":"clean"}}"#),
        );
        assert_eq!(out.system_message.as_deref(), Some("heads up"));
        assert_eq!(out.additional_context.as_deref(), Some("note"));
        assert_eq!(out.updated_tool_output.as_deref(), Some("clean"));
        assert!(out.ignored.is_empty());
    }

    /// The library's reason for existing: a field the event does not offer is
    /// dropped *and named*, so the ignoring is visible rather than silent.
    #[test]
    fn a_field_the_event_does_not_permit_is_ignored_and_named() {
        let out = process(
            HookEvent::PreToolUse,
            &ok(r#"{"hookSpecificOutput":{"additionalContext":"nope","updatedInput":{"command":"ls"}}}"#),
        );
        assert!(out.additional_context.is_none(), "PreToolUse offers no context");
        assert!(out.updated_input.is_some(), "but it does offer updatedInput");
        assert_eq!(out.ignored, vec!["additionalContext"]);
    }

    #[test]
    fn side_effect_events_ignore_every_field_including_system_message() {
        let out = process(
            HookEvent::Notification,
            &ok(r#"{"systemMessage":"hi","decision":"block","reason":"no"}"#),
        );
        assert!(out.system_message.is_none());
        assert!(matches!(out.verdict, Verdict::Proceed));
        assert_eq!(out.ignored, vec!["systemMessage", "decision"]);
    }

    #[test]
    fn decision_block_is_a_block_with_its_reason() {
        let out = process(HookEvent::Stop, &ok(r#"{"decision":"block","reason":"tests still failing"}"#));
        match out.verdict {
            Verdict::Block { reason } => assert_eq!(reason.as_deref(), Some("tests still failing")),
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn permission_decisions_are_carried_separately_from_the_verdict() {
        let deny = process(
            HookEvent::PreToolUse,
            &ok(r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"root"}}"#),
        );
        match deny.permission {
            Some(Permission::Deny { reason }) => assert_eq!(reason.as_deref(), Some("root")),
            other => panic!("expected a deny, got {other:?}"),
        }
        // `ask`/`defer` are the hook's word; what horsie does about them is the
        // call site's decision, not this parser's.
        let ask = process(
            HookEvent::PreToolUse,
            &ok(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#),
        );
        assert!(matches!(ask.permission, Some(Permission::Ask)));
        assert!(matches!(ask.verdict, Verdict::Proceed));
    }

    /// Malformed JSON is not a hook failure: the process succeeded. It is
    /// stdout that happens not to parse, which for most events is noise.
    #[test]
    fn unparseable_json_on_exit_zero_is_not_a_failure() {
        let out = process(HookEvent::PostToolUse, &ok("{not json"));
        assert!(matches!(out.verdict, Verdict::Proceed));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test -p horsie-support --lib process
```
Expected: FAIL — `process`, `HookReply`, `HookOutput`, `Verdict`, `Permission` undefined.

- [ ] **Step 3: Write the processor**

```rust
//! Turning one hook process's reply into one typed outcome.
//!
//! Generic by construction: the event decides what a reply *may* say (via
//! [`HookEvent::permitted`]) and this decides what it *did* say. What that then
//! *means* belongs to the call site — `PreToolUse` fails closed, `Stop`
//! blocking continues a turn, `Notification` cannot block at all.

use super::events::{HookEvent, OutputField};
use serde_json::Value;

/// What a hook process produced. `code` is `None` when it could not be run to
/// completion — spawn failure or timeout.
#[derive(Debug, Clone)]
pub struct HookReply {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The hook's top-level answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The hook ran and did not refuse.
    Proceed,
    /// The hook refused, via exit 2 or `decision: "block"`. What refusing means
    /// is per-event and decided elsewhere.
    Block { reason: Option<String> },
    /// The hook could not be run to completion, or exited non-zero in a way
    /// that is not a refusal. An outage, never a decision.
    Failed { reason: String },
}

/// `PreToolUse`'s permission vocabulary, carried separately from the verdict
/// because `ask` and `defer` are neither a refusal nor an outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Deny { reason: Option<String> },
    Ask,
    Defer,
}

/// Everything one hook said, filtered to what its event may say.
#[derive(Debug, Clone, Default)]
pub struct HookOutput {
    pub verdict: Verdict,
    pub permission: Option<Permission>,
    pub system_message: Option<String>,
    pub additional_context: Option<String>,
    pub updated_input: Option<Value>,
    pub updated_tool_output: Option<String>,
    /// Fields the hook set that its event does not offer. Named rather than
    /// dropped so a plugin author can be told why nothing happened.
    pub ignored: Vec<&'static str>,
}

impl Default for Verdict {
    fn default() -> Self {
        Verdict::Proceed
    }
}

/// The first line of `stderr`, or a fallback. A failing hook often dumps a
/// stack trace; the reason is a sentence, not a log.
fn first_line(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| fallback.to_string(), str::to_string)
}

pub fn process(event: HookEvent, reply: &HookReply) -> HookOutput {
    let mut out = HookOutput::default();

    match reply.code {
        Some(0) => {}
        // A blocking error — but only for an event that can block. The same
        // exit code, a different outcome, decided by the table rather than by
        // whoever wrote the call site.
        Some(2) if event.permits(OutputField::Decision) => {
            let reason = reply.stderr.trim();
            out.verdict = Verdict::Block {
                reason: (!reason.is_empty()).then(|| reason.to_string()),
            };
            return out;
        }
        Some(2) => {
            out.verdict = Verdict::Failed {
                reason: first_line(&reply.stderr, "the hook exited 2"),
            };
            return out;
        }
        Some(code) => {
            out.verdict = Verdict::Failed {
                reason: first_line(&reply.stderr, &format!("the hook exited {code}")),
            };
            return out;
        }
        None => {
            out.verdict = Verdict::Failed {
                reason: first_line(&reply.stderr, "the hook could not be run"),
            };
            return out;
        }
    }

    let Ok(json) = serde_json::from_str::<Value>(&reply.stdout) else {
        // Not JSON. For the two events that inject context this *is* the
        // output; for every other it is debug noise the hook printed.
        if event.injects_bare_stdout() {
            let text = reply.stdout.trim();
            if !text.is_empty() {
                out.additional_context = Some(text.to_string());
            }
        }
        return out;
    };

    let str_at = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    // Reads `field` only when the event offers it; otherwise names it as
    // ignored and yields nothing. `out` is a parameter rather than a capture so
    // the closure and the `&mut out` at each call site do not overlap.
    let take = |present: bool, field: OutputField, name: &'static str, out: &mut HookOutput| {
        if !present {
            return false;
        }
        if event.permits(field) {
            return true;
        }
        out.ignored.push(name);
        false
    };

    if take(
        json.get("systemMessage").is_some(),
        OutputField::SystemMessage,
        "systemMessage",
        &mut out,
    ) {
        out.system_message = str_at(json.get("systemMessage"));
    }

    if take(
        json.get("decision").and_then(Value::as_str) == Some("block"),
        OutputField::Decision,
        "decision",
        &mut out,
    ) {
        out.verdict = Verdict::Block {
            reason: str_at(json.get("reason")),
        };
    }

    let Some(hso) = json.get("hookSpecificOutput") else {
        return out;
    };

    if take(
        hso.get("additionalContext").is_some(),
        OutputField::AdditionalContext,
        "additionalContext",
        &mut out,
    ) {
        out.additional_context = str_at(hso.get("additionalContext"));
    }

    if take(
        hso.get("updatedInput").is_some(),
        OutputField::UpdatedInput,
        "updatedInput",
        &mut out,
    ) {
        out.updated_input = hso.get("updatedInput").cloned();
    }

    if take(
        hso.get("updatedToolOutput").is_some(),
        OutputField::UpdatedToolOutput,
        "updatedToolOutput",
        &mut out,
    ) {
        out.updated_tool_output = hso.get("updatedToolOutput").map(|v| {
            v.as_str()
                .map_or_else(|| v.to_string(), str::to_string)
        });
    }

    if take(
        hso.get("permissionDecision").is_some(),
        OutputField::PermissionDecision,
        "permissionDecision",
        &mut out,
    ) {
        out.permission = match hso.get("permissionDecision").and_then(Value::as_str) {
            Some("deny") => Some(Permission::Deny {
                reason: str_at(hso.get("permissionDecisionReason")),
            }),
            Some("ask") => Some(Permission::Ask),
            Some("defer") => Some(Permission::Defer),
            _ => None,
        };
    }

    out
}
```

Then add `mod process;` and `pub use process::{HookOutput, HookReply, Permission, Verdict};` to `mod.rs`.

- [ ] **Step 4: Run to verify it passes**

```bash
cargo fmt --all && cargo test -p horsie-support --lib && cargo clippy -p horsie-support --all-targets -- -D warnings
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/hooks
git commit -m "hooks: one generic processor from reply to outcome"
```

---

## Task 3: The record model and invocations

The fluorite types, plus the one thing that makes the whole design hold: a `HookInvocation` that carries an event *with* the facts that event needs, so a call site cannot fire `Stop` with a tool payload or build a `SessionStartRecord` from a tool call.

**Files:**
- Create: `models/fluorite/hooks.fl`
- Create: `support/src/plugin/hooks/invoke.rs`
- Modify: `models/src/lib.rs`, `clients/ts/package.json`, `clients/web/package.json`
- Modify: `support/src/plugin/hooks/mod.rs`

**Interfaces:**
- Consumes: `HookEvent`, `HookOutput`, `Verdict`, `Permission`.
- Produces: `horsie_models::hooks::{HookRecord, HookAction, ...}` and
  ```rust
  pub enum HookInvocation<'a> { /* one arm per wired event */ }
  impl HookInvocation<'_> {
      pub fn event(&self) -> HookEvent;
      pub fn matcher_subjects(&self) -> Vec<&str>;
      pub fn payload(&self) -> String;
      pub fn record(&self, plugin: &str, duration_ms: u64, out: &HookOutput) -> HookRecord;
  }
  ```

Nothing consumes these yet — the runtime moves onto them in Task 4. This task ends compiling and green with the new types unused, which keeps Task 4's breaking sweep to one commit.

- [ ] **Step 1: Write `models/fluorite/hooks.fl`**

```fluorite
/// What a plugin hook did, modelled per event.
///
/// One struct per event rather than one flat record with optional everything.
/// The duplication between arms is deliberate: each event's handling is checked
/// against its own type, and one event gaining a capability cannot silently
/// widen another. The flat shape this replaces recorded `additionalContext` on
/// an event that never offered it, and set `blocked` when a hook had merely
/// failed.
package hooks;

// --- Shared payloads ---

/// What a tool hook guarded. The join key attaching a record to its call.
struct ToolScope {
    tool: String,
    tool_call_id: String,
}

/// A value a hook replaced. Both halves or neither — never a dangling "before".
struct HookRewrite {
    before: String,
    after: String,
}

/// The hook never ran to completion: spawn failure, timeout, or a non-zero exit
/// that is not a refusal. An outage, never a decision.
struct HookFailed {
    reason: String,
}

/// A `PreToolUse` refusal, via `permissionDecision: "deny"` or exit 2.
struct HookDenied {
    reason: Option<String>,
}

/// Every other event's refusal, via top-level `decision: "block"` or exit 2.
/// For `Stop` this means *blocked from stopping*, which continues the turn.
struct HookBlocked {
    reason: Option<String>,
}

/// The hook ran and may have injected context into the model.
struct ContextInjected {
    additional_context: Option<String>,
}

// --- Tool events ---

/// `PreToolUse` allowed the call, having possibly rewritten its input. Only an
/// allowed call can be rewritten, which is why the rewrite lives here.
struct PreToolUseAllowed {
    input: Option<HookRewrite>,
}

/// The only event that can refuse a call before it runs, and the only one that
/// can rewrite its input. No `additionalContext`: the spec does not offer it
/// here, and there is no result yet to attach it to.
#[type_tag = "outcome"]
union PreToolUseOutcome {
    Allowed(PreToolUseAllowed),
    Denied(HookDenied),
    /// horsie has no permission prompt and runs unattended sessions, so there
    /// is nobody to ask. Recorded and treated as allowed.
    Ask,
    Defer,
    /// Denies the call: `PreToolUse` fails closed, alone among the events.
    Failed(HookFailed),
}

struct PreToolUseRecord {
    call: ToolScope,
    system_message: Option<String>,
    outcome: PreToolUseOutcome,
}

struct PostToolUseRan {
    output: Option<HookRewrite>,
    additional_context: Option<String>,
}

#[type_tag = "outcome"]
union PostToolUseOutcome {
    Ran(PostToolUseRan),
    /// Recorded; the call already ran, so nothing is undone.
    Blocked(HookBlocked),
    Failed(HookFailed),
}

struct PostToolUseRecord {
    call: ToolScope,
    system_message: Option<String>,
    outcome: PostToolUseOutcome,
}

#[type_tag = "outcome"]
union PostToolUseFailureOutcome {
    Ran(ContextInjected),
    Blocked(HookBlocked),
    Failed(HookFailed),
}

struct PostToolUseFailureRecord {
    call: ToolScope,
    system_message: Option<String>,
    outcome: PostToolUseFailureOutcome,
}

#[type_tag = "outcome"]
union PostToolBatchOutcome {
    Ran(ContextInjected),
    Blocked(HookBlocked),
    Failed(HookFailed),
}

/// A whole batch of parallel calls, so it names every call rather than one.
struct PostToolBatchRecord {
    calls: Vec<ToolScope>,
    system_message: Option<String>,
    outcome: PostToolBatchOutcome,
}

// --- Turn and session events ---

/// Cannot block: `SessionStart` has no decision field, because by the time it
/// runs there is nothing to refuse.
#[type_tag = "outcome"]
union SessionStartOutcome {
    Ran(ContextInjected),
    Failed(HookFailed),
}

/// `source` is the matcher domain: startup | resume | clear | compact | fork.
struct SessionStartRecord {
    source: String,
    system_message: Option<String>,
    outcome: SessionStartOutcome,
}

#[type_tag = "outcome"]
union UserPromptSubmitOutcome {
    Ran(ContextInjected),
    /// The prompt is rejected and never reaches the model.
    Blocked(HookBlocked),
    Failed(HookFailed),
}

/// Injects context via raw stdout as well as `additionalContext`.
struct UserPromptSubmitRecord {
    system_message: Option<String>,
    outcome: UserPromptSubmitOutcome,
}

#[type_tag = "outcome"]
union StopOutcome {
    Ran(ContextInjected),
    /// *Blocked from stopping* — the turn continues with `reason` as its input.
    /// This is not a refusal like `PreToolUse`'s; it is the opposite.
    Blocked(HookBlocked),
    /// Recorded, never fatal: `Stop` runs after the fact, so a guard that could
    /// not run cannot deny anything.
    Failed(HookFailed),
    /// The continuation cap ended the turn despite a block. Recorded distinctly
    /// so an unattended session that hit the guard says so rather than looking
    /// like a turn that ended on its own.
    CapReached(HookBlocked),
}

struct StopRecord {
    system_message: Option<String>,
    outcome: StopOutcome,
}

#[type_tag = "outcome"]
union SubagentStartOutcome {
    Ran(ContextInjected),
    Failed(HookFailed),
}

struct SubagentStartRecord {
    agent_type: String,
    system_message: Option<String>,
    outcome: SubagentStartOutcome,
}

#[type_tag = "outcome"]
union SubagentStopOutcome {
    Ran(ContextInjected),
    Blocked(HookBlocked),
    Failed(HookFailed),
}

struct SubagentStopRecord {
    agent_type: String,
    system_message: Option<String>,
    outcome: SubagentStopOutcome,
}

#[type_tag = "outcome"]
union TaskOutcome {
    Ran,
    Failed(HookFailed),
}

struct TaskCreatedRecord {
    task_id: String,
    system_message: Option<String>,
    outcome: TaskOutcome,
}

struct TaskCompletedRecord {
    task_id: String,
    system_message: Option<String>,
    outcome: TaskOutcome,
}

// --- Side-effect-only events ---

/// These support no JSON output at all — not even `systemMessage` — and cannot
/// block: exit 2 has no special meaning for them. They can still fail, and
/// their stderr is still user-facing, which is the whole of what to record.
#[type_tag = "outcome"]
union SideEffectOutcome {
    Ran,
    Failed(HookFailed),
}

/// `reason` is the matcher domain: clear | resume | logout | prompt_input_exit | …
struct SessionEndRecord {
    reason: String,
    outcome: SideEffectOutcome,
}

/// `error` is the matcher domain: rate_limit | overloaded | … | unknown
struct StopFailureRecord {
    error: String,
    outcome: SideEffectOutcome,
}

struct NotificationRecord {
    message: String,
    outcome: SideEffectOutcome,
}

struct CwdChangedRecord {
    cwd: String,
    outcome: SideEffectOutcome,
}

// --- The envelope ---

/// What one hook did, tagged by the event it ran for.
///
/// Named `HookAction` rather than `HookEvent` because
/// `horsie_support::plugin::hooks::HookEvent` already exists as the name
/// classifier that powers install-time refusal. Two jobs, two names.
#[type_tag = "event"]
union HookAction {
    PreToolUse(PreToolUseRecord),
    PostToolUse(PostToolUseRecord),
    PostToolUseFailure(PostToolUseFailureRecord),
    PostToolBatch(PostToolBatchRecord),
    SessionStart(SessionStartRecord),
    SessionEnd(SessionEndRecord),
    UserPromptSubmit(UserPromptSubmitRecord),
    Stop(StopRecord),
    StopFailure(StopFailureRecord),
    SubagentStart(SubagentStartRecord),
    SubagentStop(SubagentStopRecord),
    TaskCreated(TaskCreatedRecord),
    TaskCompleted(TaskCompletedRecord),
    Notification(NotificationRecord),
    CwdChanged(CwdChangedRecord),
}

/// One hook's run, as the transcript records it.
///
/// `plugin` and `duration_ms` are the only universally true facts: every hook
/// that ran was declared by a plugin and took time. Everything else is
/// per-event and lives on the action.
struct HookRecord {
    plugin: String,
    /// Wall-clock, so a hook slowing every tool call is visible.
    duration_ms: u64,
    action: HookAction,
}
```

- [ ] **Step 2: Register the package and regenerate**

`models/src/lib.rs`, after the `github` module (alphabetical among the plain ones):

```rust
/// `large_enum_variant`: `HookAction`'s arms differ by a few optional strings
/// each. Generated types cannot be boxed here, and a record is moved once per
/// hook run — not on a hot path.
#[allow(clippy::doc_markdown, clippy::large_enum_variant)]
pub mod hooks {
    include!(concat!(env!("OUT_DIR"), "/hooks/mod.rs"));
}
```

Add `../../models/fluorite/hooks.fl` to the `-i` list in `generate-types` in **both** `clients/ts/package.json` and `clients/web/package.json`.

```bash
cargo build -p horsie-models
```
Expected: compiles. If fluorite rejects the file, the error names the line.

- [ ] **Step 3: Write the failing tests for `HookInvocation`**

Create `support/src/plugin/hooks/invoke.rs` with only its tests:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::hooks::{HookAction, PreToolUseOutcome, SessionStartOutcome, StopOutcome};
    use serde_json::json;

    fn allowed() -> HookOutput {
        HookOutput::default()
    }

    #[test]
    fn an_invocation_knows_its_own_event() {
        let input = json!({"command": "ls"});
        let i = HookInvocation::PreToolUse { tool: "bash", tool_call_id: "tc1", input: &input };
        assert_eq!(i.event(), HookEvent::PreToolUse);
        assert_eq!(HookInvocation::SessionStart { source: "startup" }.event(), HookEvent::SessionStart);
    }

    /// The payload used to be built by an inline `json!` at each call site, so
    /// its shape lived wherever someone typed it.
    #[test]
    fn the_payload_carries_the_documented_fields_for_its_event() {
        let input = json!({"command": "ls"});
        let i = HookInvocation::PreToolUse { tool: "bash", tool_call_id: "tc1", input: &input };
        let p: serde_json::Value = serde_json::from_str(&i.payload()).unwrap();
        assert_eq!(p["hook_event_name"], "PreToolUse");
        assert_eq!(p["tool_name"], "bash");
        assert_eq!(p["tool_use_id"], "tc1");
        assert_eq!(p["tool_input"]["command"], "ls");

        let s = HookInvocation::SessionStart { source: "resume" };
        let p: serde_json::Value = serde_json::from_str(&s.payload()).unwrap();
        assert_eq!(p["hook_event_name"], "SessionStart");
        assert_eq!(p["source"], "resume");
    }

    /// The loop guard the spec makes mandatory: a cooperative hook returns
    /// early when it sees this, which is the only reason to send it.
    #[test]
    fn stop_carries_stop_hook_active() {
        let i = HookInvocation::Stop { last_assistant_message: Some("done"), stop_hook_active: true };
        let p: serde_json::Value = serde_json::from_str(&i.payload()).unwrap();
        assert_eq!(p["stop_hook_active"], true);
        assert_eq!(p["last_assistant_message"], "done");
    }

    #[test]
    fn a_tool_invocations_matcher_subjects_include_the_claude_aliases() {
        let input = json!({});
        let i = HookInvocation::PreToolUse { tool: "write_file", tool_call_id: "t", input: &input };
        assert_eq!(i.matcher_subjects(), vec!["write_file", "Write"]);
        assert_eq!(HookInvocation::SessionStart { source: "fork" }.matcher_subjects(), vec!["fork"]);
    }

    #[test]
    fn a_clean_pre_tool_use_records_as_allowed_with_its_scope() {
        let input = json!({});
        let i = HookInvocation::PreToolUse { tool: "bash", tool_call_id: "tc1", input: &input };
        let rec = i.record("guard", 4, &allowed());
        assert_eq!(rec.plugin, "guard");
        assert_eq!(rec.duration_ms, 4);
        match rec.action {
            HookAction::PreToolUse(r) => {
                assert_eq!(r.call.tool, "bash");
                assert_eq!(r.call.tool_call_id, "tc1");
                assert!(matches!(r.outcome, PreToolUseOutcome::Allowed(_)));
            }
            other => panic!("expected a PreToolUse action, got {other:?}"),
        }
    }

    /// The contradiction on main: a hook that *failed* was recorded as
    /// `blocked`, against that field's own doc comment. Now they are different
    /// arms and cannot be confused.
    #[test]
    fn a_failure_and_a_denial_are_different_outcomes() {
        let input = json!({});
        let i = HookInvocation::PreToolUse { tool: "bash", tool_call_id: "tc1", input: &input };

        let failed = HookOutput { verdict: Verdict::Failed { reason: "spawn".into() }, ..Default::default() };
        match i.record("g", 0, &failed).action {
            HookAction::PreToolUse(r) => assert!(matches!(r.outcome, PreToolUseOutcome::Failed(_))),
            other => panic!("{other:?}"),
        }

        let denied = HookOutput {
            permission: Some(Permission::Deny { reason: Some("root".into()) }),
            ..Default::default()
        };
        match i.record("g", 0, &denied).action {
            HookAction::PreToolUse(r) => match r.outcome {
                PreToolUseOutcome::Denied(d) => assert_eq!(d.reason.as_deref(), Some("root")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// `SessionStart` cannot block, so a block-shaped output can only ever
    /// become a failure. The type makes the other reading unrepresentable.
    #[test]
    fn session_start_records_context_or_failure_and_nothing_else() {
        let i = HookInvocation::SessionStart { source: "startup" };
        let ran = HookOutput { additional_context: Some("conventions".into()), ..Default::default() };
        match i.record("g", 1, &ran).action {
            HookAction::SessionStart(r) => match r.outcome {
                SessionStartOutcome::Ran(c) => assert_eq!(c.additional_context.as_deref(), Some("conventions")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stop_records_a_block_as_a_block_not_a_failure() {
        let i = HookInvocation::Stop { last_assistant_message: None, stop_hook_active: false };
        let blocked = HookOutput {
            verdict: Verdict::Block { reason: Some("tests fail".into()) },
            ..Default::default()
        };
        match i.record("g", 1, &blocked).action {
            HookAction::Stop(r) => match r.outcome {
                StopOutcome::Blocked(b) => assert_eq!(b.reason.as_deref(), Some("tests fail")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// `systemMessage` reaches the record for every event that permits it —
    /// the field that has been parsed, stored and read by nobody since #140.
    #[test]
    fn a_system_message_is_carried_onto_the_record() {
        let i = HookInvocation::Stop { last_assistant_message: None, stop_hook_active: false };
        let out = HookOutput { system_message: Some("heads up".into()), ..Default::default() };
        match i.record("g", 1, &out).action {
            HookAction::Stop(r) => assert_eq!(r.system_message.as_deref(), Some("heads up")),
            other => panic!("{other:?}"),
        }
    }
}
```

- [ ] **Step 4: Run to verify it fails**

```bash
cargo test -p horsie-support --lib invoke
```
Expected: FAIL — `HookInvocation` undefined.

- [ ] **Step 5: Write `invoke.rs`**

```rust
//! What horsie is about to fire, with the facts that event needs.
//!
//! One arm per wired event, so a call site cannot fire `Stop` with a tool
//! payload or build a `SessionStartRecord` out of a tool call — the two shapes
//! never meet. Everything downstream is derived: the stdin payload, the
//! matcher's subject, and the record.
//!
//! Only wired events appear. Promoting one of the ten described-but-unwired
//! events means adding its arm here alongside its call site.

use super::events::{HookEvent, claude_aliases};
use super::process::{HookOutput, Permission, Verdict};
use horsie_models::hooks as rec;
use serde_json::{Value, json};

/// Cap on any before/after payload recorded for the UI. A hook that rewrites a
/// large file write must not bloat the journal.
const RECORD_CLAMP: usize = 8_000;

fn clamp(s: &str) -> String {
    s.chars().take(RECORD_CLAMP).collect()
}

#[derive(Debug, Clone, Copy)]
pub enum HookInvocation<'a> {
    PreToolUse {
        tool: &'a str,
        tool_call_id: &'a str,
        input: &'a Value,
    },
    PostToolUse {
        tool: &'a str,
        tool_call_id: &'a str,
        input: &'a Value,
        response: &'a str,
        is_error: bool,
    },
    SessionStart {
        source: &'a str,
    },
    UserPromptSubmit {
        prompt: &'a str,
    },
    Stop {
        last_assistant_message: Option<&'a str>,
        /// True when horsie is only still running because a previous `Stop`
        /// hook blocked. A cooperative hook returns early rather than looping.
        stop_hook_active: bool,
    },
}

impl HookInvocation<'_> {
    pub fn event(&self) -> HookEvent {
        match self {
            HookInvocation::PreToolUse { .. } => HookEvent::PreToolUse,
            HookInvocation::PostToolUse { .. } => HookEvent::PostToolUse,
            HookInvocation::SessionStart { .. } => HookEvent::SessionStart,
            HookInvocation::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            HookInvocation::Stop { .. } => HookEvent::Stop,
        }
    }

    /// The names this occurrence's `matcher` is tested against. A tool event
    /// offers the horsie tool name and every Claude name it answers to;
    /// `SessionStart` offers its `source`. An event with no matcher domain
    /// offers nothing, so only an absent matcher selects it.
    pub fn matcher_subjects(&self) -> Vec<&str> {
        match self {
            HookInvocation::PreToolUse { tool, .. } | HookInvocation::PostToolUse { tool, .. } => {
                let mut v = vec![*tool];
                v.extend_from_slice(claude_aliases(tool));
                v
            }
            HookInvocation::SessionStart { source } => vec![*source],
            HookInvocation::UserPromptSubmit { .. } | HookInvocation::Stop { .. } => Vec::new(),
        }
    }

    /// The JSON written to the hook's stdin.
    ///
    /// `session_id`, `transcript_path`, `cwd` and `permission_mode` are
    /// deliberately absent: horsie has no transcript file to name, no
    /// permission model, and the runtime's cwd is per-agent state a hook has no
    /// business acting on. Sending a placeholder would be worse than sending
    /// nothing — a hook that branched on it would branch on a lie.
    pub fn payload(&self) -> String {
        let v = match self {
            HookInvocation::PreToolUse { tool, tool_call_id, input } => json!({
                "hook_event_name": "PreToolUse",
                "tool_name": tool,
                "tool_use_id": tool_call_id,
                "tool_input": input,
            }),
            HookInvocation::PostToolUse { tool, tool_call_id, input, response, is_error } => json!({
                "hook_event_name": "PostToolUse",
                "tool_name": tool,
                "tool_use_id": tool_call_id,
                "tool_input": input,
                "tool_response": response,
                "is_error": is_error,
            }),
            HookInvocation::SessionStart { source } => json!({
                "hook_event_name": "SessionStart",
                "source": source,
            }),
            HookInvocation::UserPromptSubmit { prompt } => json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": prompt,
            }),
            HookInvocation::Stop { last_assistant_message, stop_hook_active } => json!({
                "hook_event_name": "Stop",
                "last_assistant_message": last_assistant_message,
                "stop_hook_active": stop_hook_active,
            }),
        };
        v.to_string()
    }

    /// Fold one hook's output into this event's record.
    ///
    /// The one place a `HookOutput` becomes a `HookRecord`, so every event's
    /// mapping is checked against its own outcome union. A `Block` on an event
    /// whose union has no `Blocked` arm is not a judgement call here — the type
    /// leaves nowhere to put it.
    pub fn record(&self, plugin: &str, duration_ms: u64, out: &HookOutput) -> rec::HookRecord {
        rec::HookRecord {
            plugin: plugin.to_string(),
            duration_ms,
            action: self.action(out),
        }
    }

    fn action(&self, out: &HookOutput) -> rec::HookAction {
        let sys = out.system_message.clone();
        let ctx = || rec::ContextInjected {
            additional_context: out.additional_context.as_deref().map(clamp),
        };
        let failed = |reason: &str| rec::HookFailed { reason: reason.to_string() };

        match self {
            HookInvocation::PreToolUse { tool, tool_call_id, input } => {
                let call = rec::ToolScope {
                    tool: (*tool).to_string(),
                    tool_call_id: (*tool_call_id).to_string(),
                };
                let outcome = match (&out.verdict, &out.permission) {
                    (Verdict::Failed { reason }, _) => {
                        rec::PreToolUseOutcome::Failed(failed(reason))
                    }
                    (Verdict::Block { reason }, _) => {
                        rec::PreToolUseOutcome::Denied(rec::HookDenied { reason: reason.clone() })
                    }
                    (Verdict::Proceed, Some(Permission::Deny { reason })) => {
                        rec::PreToolUseOutcome::Denied(rec::HookDenied { reason: reason.clone() })
                    }
                    (Verdict::Proceed, Some(Permission::Ask)) => rec::PreToolUseOutcome::Ask,
                    (Verdict::Proceed, Some(Permission::Defer)) => rec::PreToolUseOutcome::Defer,
                    (Verdict::Proceed, None) => {
                        rec::PreToolUseOutcome::Allowed(rec::PreToolUseAllowed {
                            input: out.updated_input.as_ref().map(|after| rec::HookRewrite {
                                before: clamp(&input.to_string()),
                                after: clamp(&after.to_string()),
                            }),
                        })
                    }
                };
                rec::HookAction::PreToolUse(rec::PreToolUseRecord {
                    call,
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::PostToolUse { tool, tool_call_id, response, .. } => {
                let call = rec::ToolScope {
                    tool: (*tool).to_string(),
                    tool_call_id: (*tool_call_id).to_string(),
                };
                let outcome = match &out.verdict {
                    Verdict::Failed { reason } => {
                        rec::PostToolUseOutcome::Failed(failed(reason))
                    }
                    Verdict::Block { reason } => {
                        rec::PostToolUseOutcome::Blocked(rec::HookBlocked { reason: reason.clone() })
                    }
                    Verdict::Proceed => rec::PostToolUseOutcome::Ran(rec::PostToolUseRan {
                        output: out.updated_tool_output.as_deref().map(|after| rec::HookRewrite {
                            before: clamp(response),
                            after: clamp(after),
                        }),
                        additional_context: out.additional_context.as_deref().map(clamp),
                    }),
                };
                rec::HookAction::PostToolUse(rec::PostToolUseRecord {
                    call,
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::SessionStart { source } => {
                // Cannot block, so a block-shaped reply can only be a failure.
                // The union leaves nowhere else for it to go.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::SessionStartOutcome::Ran(ctx()),
                    Verdict::Block { reason } => rec::SessionStartOutcome::Failed(failed(
                        reason.as_deref().unwrap_or("the hook tried to block, which SessionStart cannot"),
                    )),
                    Verdict::Failed { reason } => rec::SessionStartOutcome::Failed(failed(reason)),
                };
                rec::HookAction::SessionStart(rec::SessionStartRecord {
                    source: (*source).to_string(),
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::UserPromptSubmit { .. } => {
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::UserPromptSubmitOutcome::Ran(ctx()),
                    Verdict::Block { reason } => rec::UserPromptSubmitOutcome::Blocked(
                        rec::HookBlocked { reason: reason.clone() },
                    ),
                    Verdict::Failed { reason } => {
                        rec::UserPromptSubmitOutcome::Failed(failed(reason))
                    }
                };
                rec::HookAction::UserPromptSubmit(rec::UserPromptSubmitRecord {
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::Stop { .. } => {
                // `CapReached` is never produced here: only the call site knows
                // it has run out of continuations, so it rewrites the outcome.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::StopOutcome::Ran(ctx()),
                    Verdict::Block { reason } => {
                        rec::StopOutcome::Blocked(rec::HookBlocked { reason: reason.clone() })
                    }
                    Verdict::Failed { reason } => rec::StopOutcome::Failed(failed(reason)),
                };
                rec::HookAction::Stop(rec::StopRecord {
                    system_message: sys,
                    outcome,
                })
            }
        }
    }
}
```

Add `mod invoke;` and `pub use invoke::HookInvocation;` to `mod.rs`.

- [ ] **Step 6: Run to verify it passes**

```bash
cargo fmt --all && cargo test -p horsie-support --lib && cargo clippy -p horsie-support --all-targets -- -D warnings
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add models support clients/ts/package.json clients/web/package.json
git commit -m "hooks: per-event record model and typed invocations"
```

---

## Task 4: Reshape the runtime onto the library

The breaking sweep. `runtime.fl`'s flat `HookRecord` goes; the runtime stops parsing hook replies itself; the transcript entry id stops assuming a tool call.

**Files:**
- Modify: `models/fluorite/runtime.fl` (delete `HookRecord`; retype `ToolCallResponse.hooks`)
- Modify: `models/fluorite/agent.fl` (`use hooks.HookRecord`)
- Delete: `runtime/src/hooks.rs` → Create: `runtime/src/hooks/{mod,tool}.rs`
- Modify: `runtime/src/plugins.rs`, `runtime/src/lib.rs`
- Modify: `workflow/src/agent_actor.rs`, `server/src/sessions/events.rs`, `server/src/wire_redact.rs`
- Modify: `clients/web/src/**` (Task 6 finishes this; Task 4 only unblocks the build)

**Interfaces:**
- Consumes: `HookInvocation`, `process`, `HookReply` from Tasks 1–3.
- Produces:
  ```rust
  // runtime/src/hooks/mod.rs
  pub(crate) async fn run_one(plugin_root: &Path, plugin: &str, decl: &HookDecl,
      hook_path: &[PathBuf], invocation: HookInvocation<'_>) -> (HookOutput, HookRecord);
  pub(crate) fn matching(plugins_dir: &Path, event: HookEvent, subjects: &[&str])
      -> Vec<(PathBuf, String, HookDecl)>;
  // runtime/src/hooks/tool.rs
  pub async fn dispatch_with_hooks(registry: &WorkspaceRegistry, state: &RuntimeState,
      agent: &str, call_id: &str, call: ToolCall) -> (ToolResult, Vec<HookRecord>);
  // workflow
  pub fn hook_entry_id(seq: usize) -> String;   // "hook:{seq}"
  impl AgentState { pub fn hook_entry_count(&self) -> usize }
  ```

- [ ] **Step 1: Write the failing tests**

In `workflow/src/agent_actor.rs`, replace `hook_entry_ids_are_derived_and_stable_per_call` with:

```rust
/// The id counts hook entries in the transcript, not records against a call:
/// `hook:{tool_call_id}:{n}` cannot name a `SessionStart` record, which has no
/// tool call. The tool join goes through the record's own `ToolScope`.
#[test]
fn hook_entry_ids_count_the_transcript_not_the_call() {
    let mut state = AgentState::default();
    for (i, rec) in [
        hook_record_for_call("guard", "tc1"),
        hook_record_for_call("linter", "tc2"),
        session_start_record("bootstrap"),
    ]
    .into_iter()
    .enumerate()
    {
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::HookRan { record: rec, seq: i, at_ms: 1 },
        );
    }
    let ids: Vec<&str> = state
        .history
        .iter()
        .filter_map(|e| match e {
            HistoryEntry::Hook(h) => Some(h.id.as_str()),
            HistoryEntry::Llm(_) => None,
        })
        .collect();
    assert_eq!(ids, vec!["hook:0", "hook:1", "hook:2"]);
    assert_eq!(state.hook_entry_count(), 3);
}

/// A record with no tool call at all must reach the transcript — the locked
/// decision "every hook that runs is recorded" was already untrue for
/// `SessionStart`, which took a bespoke path returning a bare string.
#[test]
fn a_non_tool_record_is_a_transcript_entry_like_any_other() {
    let state = AgentActor::apply_event(
        AgentState::default(),
        AgentDomainEvent::HookRan { record: session_start_record("bootstrap"), seq: 0, at_ms: 7 },
    );
    assert_eq!(state.history.len(), 1);
    assert!(state.prompt_messages().is_empty(), "never shown to the model");
}
```

with these helpers in the same test module:

```rust
fn hook_record_for_call(plugin: &str, call: &str) -> horsie_models::hooks::HookRecord {
    use horsie_models::hooks::*;
    HookRecord {
        plugin: plugin.to_string(),
        duration_ms: 3,
        action: HookAction::PreToolUse(PreToolUseRecord {
            call: ToolScope { tool: "bash".into(), tool_call_id: call.into() },
            system_message: None,
            outcome: PreToolUseOutcome::Allowed(PreToolUseAllowed { input: None }),
        }),
    }
}

fn session_start_record(context: &str) -> horsie_models::hooks::HookRecord {
    use horsie_models::hooks::*;
    HookRecord {
        plugin: "boot".into(),
        duration_ms: 1,
        action: HookAction::SessionStart(SessionStartRecord {
            source: "startup".into(),
            system_message: None,
            outcome: SessionStartOutcome::Ran(ContextInjected {
                additional_context: Some(context.to_string()),
            }),
        }),
    }
}
```

In `runtime/src/hooks/tool.rs`, port every existing test from `runtime/src/hooks.rs` (they are all still valid behaviour) and rewrite their assertions against the new record. The four that change shape:

```rust
/// Matches the on-main test of the same name; only the assertion shape moves.
#[tokio::test]
async fn a_claude_named_matcher_selects_the_horsie_tool() {
    let plugins = TempDir::new().unwrap();
    plugin(plugins.path(), "p", "PreToolUse", "Bash", "echo denied 1>&2; exit 2");
    let e = env(plugins);
    let (result, hooks) = run(&e, echo()).await;
    match result {
        ToolResult::Err(ToolError { reason }) => {
            assert!(reason.contains("denied"), "{reason}");
            assert!(reason.contains("'p'"), "the plugin must be named: {reason}");
        }
        ToolResult::Ok(o) => panic!("expected a denial, got {o:?}"),
    }
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].plugin, "p");
    match &hooks[0].action {
        HookAction::PreToolUse(r) => {
            assert_eq!(r.call.tool, "bash");
            assert!(matches!(r.outcome, PreToolUseOutcome::Denied(_)));
        }
        other => panic!("{other:?}"),
    }
}

/// Fail closed, and recorded as an outage rather than a decision — the two are
/// different arms now, so the record cannot claim a hook decided anything.
#[tokio::test]
async fn a_failing_pre_hook_denies_and_is_recorded_as_failed() {
    let plugins = TempDir::new().unwrap();
    plugin(plugins.path(), "p", "PreToolUse", "", "exit 1");
    let e = env(plugins);
    let (result, hooks) = run(&e, echo()).await;
    match result {
        ToolResult::Err(ToolError { reason }) => assert!(reason.contains("could not be run"), "{reason}"),
        ToolResult::Ok(o) => panic!("a guard that could not run must deny, got {o:?}"),
    }
    match &hooks[0].action {
        HookAction::PreToolUse(r) => assert!(matches!(r.outcome, PreToolUseOutcome::Failed(_))),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_rewritten_input_changes_what_runs_and_is_recorded_as_a_diff() {
    let plugins = TempDir::new().unwrap();
    plugin(
        plugins.path(),
        "p",
        "PreToolUse",
        "",
        r#"printf '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"echo rewritten\"}}}'"#,
    );
    let e = env(plugins);
    let (result, hooks) = run(&e, echo()).await;
    match result {
        ToolResult::Ok(o) => assert!(o.stdout.contains("rewritten"), "{}", o.stdout),
        ToolResult::Err(e) => panic!("expected success, got {e:?}"),
    }
    match &hooks[0].action {
        HookAction::PreToolUse(r) => match &r.outcome {
            PreToolUseOutcome::Allowed(a) => {
                let rw = a.input.as_ref().expect("a rewrite");
                assert!(rw.before.contains("echo hello"));
                assert!(rw.after.contains("echo rewritten"));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// A hook cannot record a rewrite it did not make — the halves are one value.
#[tokio::test]
async fn a_no_op_hook_is_still_recorded_with_nothing_rewritten() {
    let plugins = TempDir::new().unwrap();
    plugin(plugins.path(), "p", "PostToolUse", "", "true");
    let e = env(plugins);
    let (_, hooks) = run(&e, echo()).await;
    assert_eq!(hooks.len(), 1);
    match &hooks[0].action {
        HookAction::PostToolUse(r) => match &r.outcome {
            PostToolUseOutcome::Ran(ran) => {
                assert!(ran.output.is_none() && ran.additional_context.is_none());
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The bug this reshape closes: `PreToolUse` has no `additionalContext`, so a
/// hook setting it changes nothing and the record does not pretend otherwise.
#[tokio::test]
async fn additional_context_on_pre_tool_use_is_ignored() {
    let plugins = TempDir::new().unwrap();
    plugin(
        plugins.path(),
        "p",
        "PreToolUse",
        "",
        r#"printf '{\"hookSpecificOutput\":{\"additionalContext\":\"nope\"}}'"#,
    );
    let e = env(plugins);
    let (result, hooks) = run(&e, echo()).await;
    match result {
        ToolResult::Ok(o) => assert!(!o.stdout.contains("nope"), "context must not leak: {}", o.stdout),
        ToolResult::Err(e) => panic!("expected success, got {e:?}"),
    }
    match &hooks[0].action {
        HookAction::PreToolUse(r) => assert!(matches!(r.outcome, PreToolUseOutcome::Allowed(_))),
        other => panic!("{other:?}"),
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p horsie-workflow --lib hook
```
Expected: FAIL — `hook_entry_count` undefined, `horsie_models::hooks` types not used by `AgentDomainEvent::HookRan` yet.

- [ ] **Step 3: Move `HookRecord` out of `runtime.fl`**

In `models/fluorite/runtime.fl`: delete the whole `struct HookRecord { … }` block, add `use hooks.HookRecord;` at the top next to the package declaration, and leave `ToolCallResponse.hooks: Vec<HookRecord>` as it stands — it now resolves to the imported type.

In `models/fluorite/agent.fl`: `use runtime.HookRecord;` → `use hooks.HookRecord;`.

```bash
cargo build -p horsie-models
```

- [ ] **Step 4: Split `runtime/src/hooks.rs` into a module**

`runtime/src/hooks/mod.rs`:

```rust
//! Running plugin hooks, which happens here and nowhere else.
//!
//! The runtime is the only process where the plugin files exist — every
//! vendor's job ends at materialising `plugins_dir` — so it runs both the tool
//! hooks that wrap a call it is already handling and the events the server
//! initiates over `RunHooks`. What each hook did rides back as a [`HookRecord`]
//! so the server can journal it and the user can see what a plugin changed.
//!
//! Nothing here parses a hook's reply: `horsie_support::plugin::hooks` owns
//! that, and this owns process execution and the plugin scan.

mod tool;

pub use tool::dispatch_with_hooks;

use horsie_models::hooks::HookRecord;
use horsie_support::plugin::hooks::{
    HookDecl, HookEvent, HookInvocation, HookOutput, HookReply, matcher_selects, process,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default per-hook budget when a declaration does not set one.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Every declaration for `event` whose matcher selects `subjects`, with the
/// plugin root and name it came from, in stable plugin order.
pub(crate) fn matching(
    plugins_dir: &Path,
    event: HookEvent,
    subjects: &[&str],
) -> Vec<(PathBuf, String, HookDecl)> {
    let mut out = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let Ok(hooks) = horsie_support::plugin::hooks::read(&plugin_root) else {
            continue;
        };
        let name = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for decl in hooks.decls {
            if decl.event == event && matcher_selects(decl.matcher.as_deref(), subjects) {
                out.push((plugin_root.clone(), name.clone(), decl));
            }
        }
    }
    out
}

/// Run one hook and fold its reply into an outcome and a record.
///
/// The reply is interpreted by the library, against the invocation's own event,
/// so this function has no per-event knowledge at all — which is what lets the
/// server-initiated path reuse it verbatim.
pub(crate) async fn run_one(
    plugin_root: &Path,
    plugin: &str,
    decl: &HookDecl,
    hook_path: &[PathBuf],
    invocation: HookInvocation<'_>,
) -> (HookOutput, HookRecord) {
    let command = decl
        .command
        .replace("${CLAUDE_PLUGIN_ROOT}", &plugin_root.to_string_lossy());
    let timeout = decl.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs);

    let started = Instant::now();
    let run = crate::plugins::run_hook_raw(
        plugin_root,
        &command,
        hook_path,
        &invocation.payload(),
        timeout,
    )
    .await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let out = process(
        invocation.event(),
        &HookReply { code: run.code, stdout: run.stdout, stderr: run.stderr },
    );
    if !out.ignored.is_empty() {
        tracing::info!(
            plugin,
            event = invocation.event().name(),
            fields = ?out.ignored,
            "hook set fields its event does not offer; ignored"
        );
    }
    let record = invocation.record(plugin, duration_ms, &out);
    (out, record)
}
```

`runtime/src/hooks/tool.rs` keeps `tool_name`, `call_input`, `with_input`, `deny_reason` and `dispatch_with_hooks` from the old file, rewritten to build invocations:

```rust
    // --- PreToolUse ---
    let mut call = call;
    for (root, plugin, decl) in matching(plugins_dir, HookEvent::PreToolUse, &subjects) {
        let input = call_input(&call);
        let invocation = HookInvocation::PreToolUse { tool: name, tool_call_id: call_id, input: &input };
        let (out, rec) = run_one(&root, &plugin, &decl, hook_path, invocation).await;
        let denied = matches!(out.verdict, Verdict::Block { .. } | Verdict::Failed { .. })
            || matches!(out.permission, Some(Permission::Deny { .. }));
        records.push(rec);
        if denied {
            // Fail closed: a guard that could not run is not a guard. A
            // deliberate divergence from Claude Code, and it applies to
            // `PreToolUse` alone — every other event runs after the fact.
            return (ToolResult::Err(ToolError { reason: deny_reason(&out, &plugin, name) }), records);
        }
        if let Some(updated) = &out.updated_input
            && let Some(rewritten) = with_input(&call, updated.clone())
        {
            call = rewritten;
        }
    }
```

Note the record is pushed *before* the denial return, so the ordering guarantee is unchanged and a denied call still carries its record. `deny_reason` takes `&HookOutput` and reads `out.verdict`/`out.permission` instead of the deleted `Outcome` struct. The `PostToolUse` loop mirrors it, keeping "a failure here is recorded but never fatal, and never rewrites a result the hook could not read".

`subjects` is built once per dispatch:

```rust
    let name = tool_name(&call);
    let mut subjects = vec![name];
    subjects.extend_from_slice(horsie_support::plugin::hooks::claude_aliases(name));
```

- [ ] **Step 5: Delete the runtime's duplicate hook parser**

From `runtime/src/plugins.rs` delete `session_start_commands`, `run_hook`, `extract_context`, `run_session_start`, `HOOK_OUTPUT_CLAMP`'s now-unused uses in them, and their tests. `run_hook_raw` and `HookRun` stay. This is the second `hooks.json` parser — it ignored classification entirely and is what let `SessionStart` run without producing a record.

Task 5 replaces its caller; until then `runtime/src/main.rs` will not compile. That is expected and fixed in Task 5's first step, so **do not commit between Task 4 and Task 5** — run `cargo check -p horsie-support -p horsie-models -p horsie-workflow` to gate progress instead.

- [ ] **Step 6: Retarget the transcript entry id**

In `workflow/src/agent_actor.rs`:

```rust
/// The cursor id of the `seq`-th hook entry in a transcript.
///
/// Counts entries rather than records-per-call, because not every record has a
/// call: `hook:{tool_call_id}:{n}` cannot name a `SessionStart`. The tool join
/// is unaffected — it goes through the record's own `ToolScope`, which is where
/// it belongs.
///
/// One function, two callers — the fold and the live broadcast — because the
/// stream and `/history` must name the same entry the same way.
#[must_use]
pub fn hook_entry_id(seq: usize) -> String {
    format!("hook:{seq}")
}
```

`hook_entry(record, seq, at_ms)` uses it. `hook_records_for(&self, tool_call_id)` becomes:

```rust
    /// How many hook entries this transcript already holds. The next one's `seq`.
    #[must_use]
    pub fn hook_entry_count(&self) -> usize {
        self.history
            .iter()
            .filter(|e| matches!(e, HistoryEntry::Hook(_)))
            .count()
    }
```

and the `AgentCommand::HooksRan` handler simplifies — no per-call map, just a running count:

```rust
            AgentCommand::HooksRan { records } => {
                let at_ms = now_ms();
                // Counted here, against the state as it stands, and carried on
                // the event: `agent_frame` sees only the event, so deriving the
                // id at fold time would give the live stream different cursors
                // than `/history`.
                let mut seq = state.hook_entry_count();
                let events = records
                    .into_iter()
                    .map(|record| {
                        let event = AgentDomainEvent::HookRan { record, seq, at_ms };
                        seq += 1;
                        event
                    })
                    .collect();
                CommandEffect::persist(events)
            }
```

Update `models/src/lib.rs`'s `HistoryEntry::id()` doc comment (`hook:{tool_call_id}:{n}` → `hook:{n}`) and `models/fluorite/agent.fl`'s `HookEntry.id` comment to match.

- [ ] **Step 7: Fix the remaining Rust call sites**

`server/src/sessions/events.rs` — the `agent_frame` test's fixture becomes a `hooks::HookRecord` and asserts `hook.id == "hook:1"`. `server/src/wire_redact.rs` and `server/src/sessions/session_actor.rs`'s `hook_record` test helper likewise. `runtime-client/src/client.rs`'s test sink fixture and `runtime-client/src/testkit.rs` likewise. Reuse the two helpers from Step 1 rather than re-typing the literals.

- [ ] **Step 8: Run the Rust workspace**

```bash
cargo fmt --all && cargo test --workspace
```
Expected: PASS except `runtime/src/main.rs` (Task 5). If `main.rs` blocks compilation of the whole crate, temporarily `todo!()` the `SessionStart` arm — Task 5 Step 1 replaces it.

---

## Task 5: The server-initiated path

`SessionStart`'s bespoke RPC becomes the general one, and `SessionStart` starts producing records like everything else.

**Files:**
- Modify: `models/fluorite/runtime.fl`
- Create: `runtime/src/hooks/server.rs`
- Modify: `runtime/src/main.rs`, `runtime-client/src/{client,transport,testkit}.rs`
- Modify: `server/src/sessions/session_actor.rs`, `workflow/src/workspace.rs`

**Interfaces:**
- Consumes: `run_one`, `matching`, `HookInvocation`.
- Produces:
  ```fluorite
  #[type_tag = "event"]
  union ServerHookEvent { SessionStart(SessionStartInput), UserPromptSubmit(UserPromptSubmitInput), Stop(StopInput) }
  struct SessionStartInput { source: String }
  struct UserPromptSubmitInput { prompt: String }
  struct StopInput { last_assistant_message: Option<String>, stop_hook_active: bool }
  struct RunHooksRequest  { call_id: String, event: ServerHookEvent }
  struct RunHooksResponse { call_id: String, records: Vec<HookRecord> }
  ```
  ```rust
  // runtime/src/hooks/server.rs
  pub async fn run_hooks(registry: &WorkspaceRegistry, event: &ServerHookEvent) -> Vec<HookRecord>;
  // runtime-client
  impl RuntimeClient { pub async fn run_hooks(&self, event: ServerHookEvent) -> Result<Vec<HookRecord>, RuntimeCallError> }
  pub fn injected_context(records: &[HookRecord]) -> Option<String>;
  ```

Only the three wired server-initiated events get arms. `ServerHookEvent` has no tool arm **by construction** — tool events run inline in the runtime, so asking for one out of band is unrepresentable rather than merely wrong.

- [ ] **Step 1: Write the failing tests**

`runtime/src/hooks/server.rs` tests:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The asymmetry this closes: `SessionStart` used to return a bare string
    /// and produce no record at all, so "every hook that runs is recorded" was
    /// already untrue for it.
    #[tokio::test]
    async fn a_session_start_hook_produces_a_record_and_its_context() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "boot", "SessionStart", "", "echo CONVENTIONS");
        let e = env(plugins);
        let records = run_hooks(
            &e.registry,
            &ServerHookEvent::SessionStart(SessionStartInput { source: "startup".into() }),
        )
        .await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin, "boot");
        match &records[0].action {
            HookAction::SessionStart(r) => {
                assert_eq!(r.source, "startup");
                match &r.outcome {
                    SessionStartOutcome::Ran(c) => {
                        assert_eq!(c.additional_context.as_deref(), Some("CONVENTIONS"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// A `SessionStart` matcher selects on `source`, not on a tool name.
    #[tokio::test]
    async fn a_source_matcher_selects_the_right_start() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "boot", "SessionStart", "resume", "echo ONLY_ON_RESUME");
        let e = env(plugins);
        let start = |source: &str| ServerHookEvent::SessionStart(SessionStartInput { source: source.into() });
        assert!(run_hooks(&e.registry, &start("startup")).await.is_empty());
        assert_eq!(run_hooks(&e.registry, &start("resume")).await.len(), 1);
    }

    /// A failing hook is recorded rather than dropped — the old path logged it
    /// and returned `None`, so nothing downstream could ever see it.
    #[tokio::test]
    async fn a_failing_session_start_hook_is_recorded_as_failed() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "boot", "SessionStart", "", "echo nope 1>&2; exit 1");
        let e = env(plugins);
        let records = run_hooks(
            &e.registry,
            &ServerHookEvent::SessionStart(SessionStartInput { source: "startup".into() }),
        )
        .await;
        match &records[0].action {
            HookAction::SessionStart(r) => assert!(matches!(r.outcome, SessionStartOutcome::Failed(_))),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn with_no_plugin_library_nothing_runs_and_nothing_is_recorded() {
        let work = TempDir::new().unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "main".into(),
            path: work.path().to_path_buf(),
        }]);
        let records = run_hooks(
            &registry,
            &ServerHookEvent::SessionStart(SessionStartInput { source: "startup".into() }),
        )
        .await;
        assert!(records.is_empty());
    }
}
```

`runtime-client/src/client.rs` tests:

```rust
/// Injected context is derived from the records rather than carried beside
/// them, which is what makes `SessionStart` recorded like everything else.
#[test]
fn injected_context_concatenates_what_the_records_carry() {
    let records = vec![
        session_start_record(Some("first")),
        session_start_record(None),
        session_start_record(Some("second")),
    ];
    assert_eq!(injected_context(&records).as_deref(), Some("first\n\nsecond"));
    assert!(injected_context(&[]).is_none());
    assert!(injected_context(&[session_start_record(None)]).is_none());
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p horsie-runtime --lib server_hooks
```
Expected: FAIL — `run_hooks` and `ServerHookEvent` undefined.

- [ ] **Step 3: Replace the RPC in `runtime.fl`**

```fluorite
// --- Server-initiated hooks ---

/// An event the server initiates, carrying that event's input.
///
/// Tool events are absent by construction: they run inline in the runtime with
/// the call they guard, so asking for one out of band is unrepresentable rather
/// than merely wrong. Only wired events have arms — promoting one of the ten
/// described-but-unwired events adds its arm here alongside its call site.
struct SessionStartInput { source: String }
struct UserPromptSubmitInput { prompt: String }
/// `stop_hook_active` is true when horsie is only still running because a
/// previous `Stop` hook blocked. A cooperative hook returns early rather than
/// looping; the hard cap exists for the hooks that do not.
struct StopInput { last_assistant_message: Option<String>, stop_hook_active: bool }

#[type_tag = "event"]
union ServerHookEvent {
    SessionStart(SessionStartInput),
    UserPromptSubmit(UserPromptSubmitInput),
    Stop(StopInput),
}

/// Run every matching hook for one server-initiated event inside the sandbox.
struct RunHooksRequest  { call_id: String, event: ServerHookEvent }
/// Every hook that ran, in execution order. Injected context is derived from
/// these rather than carried beside them, so no event is recorded specially.
struct RunHooksResponse { call_id: String, records: Vec<HookRecord> }
```

Delete `SessionStartRequest`/`SessionStartResponse`. In `RuntimeInboundMessage`, `SessionStart(SessionStartRequest)` → `RunHooks(RunHooksRequest)`. In `RuntimeOutboundMessage`, `SessionStartResult(SessionStartResponse)` → `HookRecords(RunHooksResponse)`.

- [ ] **Step 4: Write `runtime/src/hooks/server.rs`**

```rust
//! Hooks the server initiates, which the runtime runs because the runtime is
//! where the plugin files are.
//!
//! One function for every such event: the invocation carries what the event
//! needs, [`super::matching`] finds the declarations whose matcher selects it,
//! and [`super::run_one`] runs each. No per-event branching beyond building the
//! invocation — which is the whole reason `SessionStart`'s bespoke RPC was
//! worth replacing rather than duplicating.

use horsie_models::hooks::HookRecord;
use horsie_models::runtime::ServerHookEvent;
use horsie_support::plugin::hooks::HookInvocation;

use crate::workspace::WorkspaceRegistry;

pub async fn run_hooks(registry: &WorkspaceRegistry, event: &ServerHookEvent) -> Vec<HookRecord> {
    let Some(plugins_dir) = registry.plugins_dir() else {
        return Vec::new();
    };
    let invocation = match event {
        ServerHookEvent::SessionStart(i) => HookInvocation::SessionStart { source: &i.source },
        ServerHookEvent::UserPromptSubmit(i) => HookInvocation::UserPromptSubmit { prompt: &i.prompt },
        ServerHookEvent::Stop(i) => HookInvocation::Stop {
            last_assistant_message: i.last_assistant_message.as_deref(),
            stop_hook_active: i.stop_hook_active,
        },
    };
    let hook_path = registry.hook_path();
    let subjects = invocation.matcher_subjects();
    let mut records = Vec::new();
    for (root, plugin, decl) in super::matching(plugins_dir, invocation.event(), &subjects) {
        let (_, record) = super::run_one(&root, &plugin, &decl, hook_path, invocation).await;
        records.push(record);
    }
    records
}
```

Add `mod server; pub use server::run_hooks;` to `runtime/src/hooks/mod.rs`.

- [ ] **Step 5: Rewire `runtime/src/main.rs`**

Replace the `RuntimeInboundMessage::SessionStart(req)` arm with `RunHooks(req)`, spawning the same way and responding with `RuntimeOutboundMessage::HookRecords(RunHooksResponse { call_id, records })`. Keep the `in_flight` abort-handle registration verbatim — a slow hook must stay cancellable.

- [ ] **Step 6: Rewire the client**

In `runtime-client/src/transport.rs`, `run_session_start(call_id)` → `run_hooks(call_id, event: &ServerHookEvent) -> Result<Vec<HookRecord>, TransportError>`. In `client.rs`:

```rust
    /// Run every matching hook for a server-initiated event.
    ///
    /// Mints its own `call_id`: unlike a tool hook, this is not correlated to
    /// anything the model said, so there is no id to borrow.
    pub async fn run_hooks(
        &self,
        event: ServerHookEvent,
    ) -> Result<Vec<HookRecord>, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        let records = self
            .inner
            .run_hooks(&call_id, &event)
            .await
            .map_err(RuntimeCallError::Transport)?;
        // The same sink tool records take, so a server-initiated hook reaches
        // the transcript by the same route rather than a second one.
        if let Some(sink) = &self.hook_sink
            && !records.is_empty()
        {
            sink.record(records.clone()).await;
        }
        Ok(records)
    }

/// The context these records inject, concatenated in the order they ran.
///
/// Derived rather than carried: a record already says what its hook injected,
/// so a separate `context: String` beside them could only ever disagree.
#[must_use]
pub fn injected_context(records: &[HookRecord]) -> Option<String> {
    let sections: Vec<&str> = records
        .iter()
        .filter_map(|r| match &r.action {
            HookAction::SessionStart(s) => match &s.outcome {
                SessionStartOutcome::Ran(c) => c.additional_context.as_deref(),
                SessionStartOutcome::Failed(_) => None,
            },
            HookAction::UserPromptSubmit(u) => match &u.outcome {
                UserPromptSubmitOutcome::Ran(c) => c.additional_context.as_deref(),
                UserPromptSubmitOutcome::Blocked(_) | UserPromptSubmitOutcome::Failed(_) => None,
            },
            HookAction::Stop(s) => match &s.outcome {
                StopOutcome::Ran(c) => c.additional_context.as_deref(),
                StopOutcome::Blocked(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
            },
            // Listed rather than `_`, so promoting an event that injects
            // context cannot silently drop it here — which is the same
            // silent-widening bug the record reshape exists to close.
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::SessionEnd(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::SubagentStop(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            | HookAction::CwdChanged(_) => None,
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}
```

Update `runtime-client/src/testkit.rs`'s stub transport to match.

- [ ] **Step 7: Rewire the bootstrap call site**

`server/src/sessions/session_actor.rs` around line 1906:

```rust
        let shared = if use_plugins {
            // Records reach the transcript through the client's own hook sink,
            // exactly as a tool hook's do. All that is derived here is the
            // context, which the system prompt needs.
            let bootstrap = runtime_client
                .run_hooks(ServerHookEvent::SessionStart(SessionStartInput {
                    source: "startup".to_string(),
                }))
                .await
                .ok()
                .as_deref()
                .and_then(injected_context);
            Some(SharedContext { skills: Arc::new(shared_scan.skills), root: shared_scan.root, bootstrap })
        } else {
            None
        };
```

- [ ] **Step 8: Run the workspace**

```bash
cargo fmt --all && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --workspace
```
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "hooks: one server-initiated RPC, and SessionStart is recorded"
```

---

## Task 6: The web transcript

Two renderings from one union: a record carrying a `ToolScope` attaches to its tool-call card as it does now; every other record becomes its own row. `systemMessage` is surfaced, closing the field captured and shown to nobody since #140.

**Files:**
- Create: `clients/web/src/lib/hookSummary.ts`, `clients/web/src/lib/hookSummary.test.ts`
- Create: `clients/web/src/components/HookNoticeRow.tsx`
- Modify: `clients/web/src/hooks/useSessionStream.ts`, `clients/web/src/components/{Transcript,ToolCallCard}.tsx`, `clients/web/src/components/ToolCallCard.test.tsx`, `clients/web/src/lib/transcriptSegments.ts` and its test, `clients/web/src/pages/SessionView.tsx`

**Interfaces:**
- Consumes: generated `HookRecord`, `HookAction`, `HistoryEntry`.
- Produces:
  ```ts
  // lib/hookSummary.ts
  export function toolScope(r: HookRecord): { tool: string; toolCallId: string } | null;
  export function hookSummary(r: HookRecord): { text: string; intervened: boolean };
  // hooks/useSessionStream.ts
  export interface RenderedHookNotice { id: string; record: HookRecord; atMs: number }
  export type TranscriptItem =
    | { kind: "message"; value: RenderedMessage }
    | { kind: "notice"; value: RenderedHookNotice };
  // SessionStream.items: TranscriptItem[]   (replaces .messages)
  ```

- [ ] **Step 1: Write the failing tests**

`clients/web/src/lib/hookSummary.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { HookRecord } from "../api/types";
import { hookSummary, toolScope } from "./hookSummary";

function rec(action: HookRecord["action"]): HookRecord {
  return { plugin: "guard", durationMs: 4, action };
}

describe("toolScope", () => {
  it("names the call a tool record guarded", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: { tool: "bash", toolCallId: "tc1" },
        systemMessage: null,
        outcome: { outcome: "Allowed", value: { input: null } },
      },
    });
    expect(toolScope(r)).toEqual({ tool: "bash", toolCallId: "tc1" });
  });

  // The split the whole rendering hangs off: a record with no call cannot
  // attach to a card, so it gets a row.
  it("is null for a record with no tool call", () => {
    const r = rec({
      event: "SessionStart",
      value: {
        source: "startup",
        systemMessage: null,
        outcome: { outcome: "Ran", value: { additionalContext: "x" } },
      },
    });
    expect(toolScope(r)).toBeNull();
  });
});

describe("hookSummary", () => {
  it("reads a denial as an intervention", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: { tool: "bash", toolCallId: "tc1" },
        systemMessage: null,
        outcome: { outcome: "Denied", value: { reason: "writes are not allowed" } },
      },
    });
    expect(hookSummary(r)).toEqual({ text: "writes are not allowed", intervened: true });
  });

  // A hook that could not run denies the call, so it must read as an
  // intervention rather than as a hook that quietly passed.
  it("reads a failure as an intervention, distinctly from a denial", () => {
    const r = rec({
      event: "PreToolUse",
      value: {
        call: { tool: "bash", toolCallId: "tc1" },
        systemMessage: null,
        outcome: { outcome: "Failed", value: { reason: "spawn failed" } },
      },
    });
    const s = hookSummary(r);
    expect(s.intervened).toBe(true);
    expect(s.text).toContain("could not run");
  });

  it("says what a hook rewrote", () => {
    const r = rec({
      event: "PostToolUse",
      value: {
        call: { tool: "bash", toolCallId: "tc1" },
        systemMessage: null,
        outcome: {
          outcome: "Ran",
          value: { output: { before: "secret", after: "***" }, additionalContext: null },
        },
      },
    });
    expect(hookSummary(r).text).toContain("rewrote the output");
  });

  it("reads a no-op as allowed", () => {
    const r = rec({
      event: "PostToolUse",
      value: {
        call: { tool: "bash", toolCallId: "tc1" },
        systemMessage: null,
        outcome: { outcome: "Ran", value: { output: null, additionalContext: null } },
      },
    });
    expect(hookSummary(r)).toEqual({ text: "allowed", intervened: false });
  });

  // `Blocked` on Stop is the opposite of a refusal: the turn continues.
  it("reads a Stop block as a continuation, not a refusal", () => {
    const r = rec({
      event: "Stop",
      value: {
        systemMessage: null,
        outcome: { outcome: "Blocked", value: { reason: "tests still failing" } },
      },
    });
    const s = hookSummary(r);
    expect(s.intervened).toBe(true);
    expect(s.text).toContain("kept the turn going");
    expect(s.text).toContain("tests still failing");
  });

  it("says when the continuation cap ended the turn", () => {
    const r = rec({
      event: "Stop",
      value: {
        systemMessage: null,
        outcome: { outcome: "CapReached", value: { reason: "keep going" } },
      },
    });
    expect(hookSummary(r).text).toContain("continuation limit");
  });
});
```

Rewrite `ToolCallCard.test.tsx`'s fixtures onto the new shape, keeping all four existing cases (nothing rendered when no hooks; denial named on the collapsed row; every hook listed when expanded; a failure reads as an intervention) and adding:

```ts
  // The field that has been parsed, stored, put on the wire and read by nobody
  // since #140.
  it("shows a system message addressed to the user", () => {
    render(
      <ToolCallCard
        call={call({
          hooks: [
            hookRecord({
              event: "PostToolUse",
              value: {
                call: { tool: "bash", toolCallId: "tc1" },
                systemMessage: "this repo pins node 22",
                outcome: { outcome: "Ran", value: { output: null, additionalContext: null } },
              },
            }),
          ],
        })}
      />,
    );
    fireEvent.click(screen.getByTestId("tool-call-toggle"));
    expect(screen.getByTestId("tool-call-hook").textContent).toContain("this repo pins node 22");
  });
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd clients/web && bun run generate-types && bunx vitest run
```
Expected: FAIL — `./hookSummary` not found.

- [ ] **Step 3: Write `hookSummary.ts`**

```ts
import type { HookRecord } from "../api/types";

/** What a hook did, in a few words, plus whether it changed anything. The
 * record is the audit trail; this is the line a person reads without unpacking
 * it, and `intervened` is what the collapsed tool row and the notice row's
 * styling both key off. */
export interface HookSummary {
  text: string;
  intervened: boolean;
}

const ALLOWED: HookSummary = { text: "allowed", intervened: false };
const RAN: HookSummary = { text: "ran", intervened: false };

function failed(reason: string): HookSummary {
  return { text: `could not run — ${reason}`, intervened: true };
}

function rewrote(what: string): HookSummary {
  return { text: `rewrote the ${what}`, intervened: true };
}

/** The call a record guarded, or `null` when it guarded none. The split every
 * rendering decision hangs off: a record with no call cannot attach to a card,
 * so it becomes a transcript row of its own. */
export function toolScope(
  r: HookRecord,
): { tool: string; toolCallId: string } | null {
  const a = r.action;
  switch (a.event) {
    case "PreToolUse":
    case "PostToolUse":
    case "PostToolUseFailure":
      return a.value.call;
    // A batch names every call it covered, so no single one owns it.
    case "PostToolBatch":
    case "SessionStart":
    case "SessionEnd":
    case "UserPromptSubmit":
    case "Stop":
    case "StopFailure":
    case "SubagentStart":
    case "SubagentStop":
    case "TaskCreated":
    case "TaskCompleted":
    case "Notification":
    case "CwdChanged":
      return null;
  }
}

/** No `default` clause anywhere below, deliberately: adding a `HookAction` arm
 * must fail `tsc` rather than fall through to a generic sentence. That is the
 * TypeScript half of the illegal-states guarantee the Rust union gives. */
export function hookSummary(r: HookRecord): HookSummary {
  const a = r.action;
  switch (a.event) {
    case "PreToolUse":
      switch (a.value.outcome.outcome) {
        case "Allowed":
          return a.value.outcome.value.input ? rewrote("input") : ALLOWED;
        case "Denied":
          return {
            text: a.value.outcome.value.reason ?? "denied the call",
            intervened: true,
          };
        case "Ask":
        case "Defer":
          // horsie has no permission prompt, so there is nobody to ask.
          return { text: "asked for approval — allowed", intervened: false };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    // falls through only if a variant is added — which `tsc` rejects
    case "PostToolUse":
      switch (a.value.outcome.outcome) {
        case "Ran": {
          const ran = a.value.outcome.value;
          if (ran.output) return rewrote("output");
          if (ran.additionalContext)
            return { text: "added context to the result", intervened: true };
          return ALLOWED;
        }
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? "objected — the call had already run",
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PostToolUseFailure":
    case "PostToolBatch":
    case "SubagentStop":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "added context", intervened: true }
            : RAN;
        case "Blocked":
          return { text: a.value.outcome.value.reason ?? "objected", intervened: true };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "SessionStart":
    case "SubagentStart":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "added session context", intervened: true }
            : RAN;
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "UserPromptSubmit":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "added context to the prompt", intervened: true }
            : RAN;
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? "rejected the prompt",
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "Stop":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "left a note for the next turn", intervened: true }
            : RAN;
        // Blocked means blocked *from stopping*: the opposite of a refusal.
        case "Blocked":
          return {
            text: `kept the turn going — ${a.value.outcome.value.reason ?? "no reason given"}`,
            intervened: true,
          };
        case "CapReached":
          return {
            text: `hit the continuation limit — ${a.value.outcome.value.reason ?? "no reason given"}`,
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "TaskCreated":
    case "TaskCompleted":
    case "SessionEnd":
    case "StopFailure":
    case "Notification":
    case "CwdChanged":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return RAN;
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
  }
}
```

- [ ] **Step 4: Write `HookNoticeRow.tsx`**

```tsx
import { ShieldAlert, ShieldCheck } from "lucide-react";
import type { HookRecord } from "../api/types";
import { cn } from "../lib/cn";
import { hookSummary } from "../lib/hookSummary";

/** A hook record with no tool call of its own — a `SessionStart` bootstrap, a
 * `Stop` that kept the turn going. It has nowhere to attach, so it is a row.
 *
 * Deliberately quieter than a tool card: this is something a plugin did around
 * the conversation, not something the agent asked for. */
export function HookNoticeRow({ record }: { record: HookRecord }) {
  const { text, intervened } = hookSummary(record);
  return (
    <div
      data-testid="hook-notice"
      data-event={record.action.event}
      data-intervened={intervened ? "true" : "false"}
      className="flex items-start gap-2 py-1"
    >
      <span className="flex w-3.5 shrink-0 justify-center pt-0.5">
        {intervened ? (
          <ShieldAlert size={12} className="text-amber-ink" aria-hidden />
        ) : (
          <ShieldCheck size={12} className="text-faint" aria-hidden />
        )}
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="shrink-0 font-mono text-[0.6875rem] font-medium tracking-[0.02em] text-legend">
            {record.plugin}
          </span>
          <span className="legend shrink-0">{record.action.event}</span>
          <span
            className={cn(
              "min-w-0 flex-1 truncate font-mono text-[0.6875rem]",
              intervened ? "text-dim" : "text-faint",
            )}
          >
            {text}
          </span>
        </div>
        {/* `systemMessage` is addressed to the user, never the model. It has
            been captured and shown to nobody since #140; this is where it
            lands. */}
        {systemMessage(record) && (
          <p
            data-testid="hook-notice-system-message"
            className="mt-0.5 text-[0.6875rem] leading-relaxed text-amber-ink"
          >
            {systemMessage(record)}
          </p>
        )}
      </div>
    </div>
  );
}

/** The four side-effect-only events permit no JSON output at all, so they have
 * no `systemMessage` field to read — hence the narrowing rather than a cast. */
function systemMessage(r: HookRecord): string | null {
  const v = r.action.value as { systemMessage?: string | null };
  return v.systemMessage ?? null;
}
```

- [ ] **Step 5: Rework `useSessionStream`**

`state.hooks` splits in two:

```ts
  /** Hook records by the tool call they guarded. Keyed rather than ordered
   * because a record names its call: the server guarantees a record is
   * journaled before its tool result, but a client that reconnects mid-turn may
   * still see them in either order. */
  hooks: Record<string, HookRecord[]>;
  /** Hook records with no tool call — a `SessionStart` bootstrap, a `Stop` that
   * kept the turn going. These have nowhere to attach, so they are transcript
   * items of their own, ordered by their entry id. */
  notices: Record<string, RenderedHookNotice>;
  /** Entry ids in transcript order, messages and notices alike. */
  order: string[];
```

`withHooks` becomes `withHookEntries(state, entries)` and routes on `toolScope(record)`: non-null → the keyed map, deduped by `${toolCallId}:${plugin}:${event}` as today; null → `notices[entry.id]` plus `entry.id` spliced into `order`. Because a notice id is `hook:{n}` and messages are appended in the same journal order, appending it to `order` is already correct for the live stream; on a backfill (`prepend`), splice at the head with the batch's own order.

The `items` projection replaces `messages`:

```ts
    const items: TranscriptItem[] = state.order.map((id) => {
      const notice = state.notices[id];
      if (notice) return { kind: "notice", value: notice };
      const m = state.byId[id];
      return { kind: "message", value: { ...m, toolCalls: m.toolCalls.map(resolveTool) } };
    });
```

Queued and optimistic messages append as `{ kind: "message" }` items after it, unchanged.

- [ ] **Step 6: Thread `items` through the transcript**

In `Transcript.tsx`, `TurnGroup` gains a third kind and `groupTurns` takes items:

```ts
export type TurnGroup =
  | { kind: "user"; msg: RenderedMessage }
  | { kind: "assistant"; id: string; msgs: RenderedMessage[] }
  // A notice is never folded into an assistant turn: a plugin acting around the
  // conversation is not something the agent said.
  | { kind: "notice"; id: string; record: HookRecord };

export function groupTurns(items: TranscriptItem[]): TurnGroup[] {
  const groups: TurnGroup[] = [];
  let current: RenderedMessage[] | null = null;
  const intoAssistant = (m: RenderedMessage) => {
    if (current) {
      current.push(m);
      return;
    }
    current = [m];
    groups.push({ kind: "assistant", id: m.id, msgs: current });
  };
  for (const item of items) {
    if (item.kind === "notice") {
      current = null;
      groups.push({ kind: "notice", id: item.value.id, record: item.value.record });
      continue;
    }
    const m = item.value;
    if (m.role === "User") {
      current = null;
      groups.push({ kind: "user", msg: m });
    } else {
      intoAssistant(m);
    }
  }
  return groups;
}
```

(The `user`/`assistant` halves are the existing `groupTurns` body verbatim — only the loop's input type and the notice arm are new. Keep whatever the current body does for `queued`/`optimistic` flags.)

The renderer gains one arm:

```tsx
        {groups.map((g) =>
          g.kind === "notice" ? (
            <HookNoticeRow key={g.id} record={g.record} />
          ) : g.kind === "user" ? (
            <UserTurn key={g.msg.id} msg={g.msg} />
          ) : (
            <AssistantTurn key={g.id} msgs={g.msgs} />
          ),
        )}
```

`transcriptSegments.ts` takes `TranscriptItem[]` and skips notices before segmenting — a notice is not part of an assistant turn's segment run, so it must not split one. `SessionView.tsx` follows the rename at both reads: the scroll effect's dependency (`:199`, `stream.messages` → `stream.items`) and the empty check (`:357`, `stream.messages.length === 0` → `stream.items.length === 0`).

- [ ] **Step 7: Run to verify they pass**

```bash
cd clients/web && bun run generate-types && bunx vitest run && bun run build
```
Expected: PASS. Then from the repo root: `make ts-types && git diff --exit-code clients/ts/src/generated`.

- [ ] **Step 8: Commit**

```bash
git add clients
git commit -m "web: render hook records per event, with standalone rows"
```

---

## Task 7: `Stop` continues the turn

The event with the most real demand in the marketplace, and the only one whose two capabilities are both ways of *not* ending a turn. Recording it and ignoring both would fire the event and discard everything it said.

**Files:**
- Modify: `workflow/src/agent_actor.rs`, `workflow/src/context.rs`
- Modify: `server/src/sessions/session_actor.rs`
- Modify: `support/src/plugin/hooks/events.rs` (nothing — `Stop` is already wired from Task 1)

**Interfaces:**
- Consumes: `RuntimeClient::run_hooks`, `ServerHookEvent::Stop`, `StopOutcome`.
- Produces:
  ```rust
  // server/src/sessions/session_actor.rs
  struct StopHookParent { inner: Arc<dyn AgentOutcomeSink>, session: ActorRef<SessionCommand>,
      key: AgentKey, last_client: Arc<Mutex<Option<RuntimeClient>>>, continuations: Arc<AtomicUsize> }
  const MAX_STOP_CONTINUATIONS: usize = 3;
  // SessionCommand::ContinueAfterStop { key: AgentKey, reason: String }
  ```

**Where it runs, precisely.** `AgentOutcomeSink::deliver` is called from `AgentActor`'s own `RunFinished` handler (`agent_actor.rs:869-912`), not from the run task — verified, and it matters. A slow `Stop` hook therefore delays that *agent's* mailbox, never the session's command loop, so a cancel or a new message for a different agent is still served. The hook is bounded by its declared timeout (default 30s), and the agent is idle at that point anyway. This is #141's one durable idea, with its claim corrected.

- [ ] **Step 1: Write the failing tests**

In `server/src/sessions/session_actor.rs`'s test module:

```rust
/// A blocking `Stop` means *blocked from stopping*: the turn does not conclude,
/// and the reason becomes the input to another run. The opposite of a refusal.
#[tokio::test]
async fn a_blocking_stop_hook_starts_another_run_with_its_reason() {
    let h = harness_with_stop_hook("echo 'tests still failing' 1>&2; exit 2").await;
    h.send_user_message("do the thing").await;
    let inputs = h.await_agent_inputs(2).await;
    assert_eq!(inputs[0], "do the thing");
    assert!(inputs[1].contains("tests still failing"), "{}", inputs[1]);
}

/// Set on every continuation and absent on the first, so a cooperative hook can
/// return early rather than looping. Half the loop guard.
#[tokio::test]
async fn stop_hook_active_is_set_only_on_continuations() {
    let h = harness_recording_stop_payloads("exit 0").await;
    h.send_user_message("go").await;
    let payloads = h.await_stop_payloads(1).await;
    assert_eq!(payloads[0]["stop_hook_active"], false);
}

/// The other half, and the one that must not be optional: horsie runs
/// unattended sessions, so a hook that ignores `stop_hook_active` would spin
/// forever with nobody watching.
#[tokio::test]
async fn an_unconditionally_blocking_stop_hook_is_stopped_by_the_cap() {
    let h = harness_with_stop_hook("echo again 1>&2; exit 2").await;
    h.send_user_message("go").await;
    let inputs = h.await_agent_inputs_settled().await;
    assert_eq!(
        inputs.len(),
        1 + MAX_STOP_CONTINUATIONS,
        "the original turn plus exactly the cap"
    );
}

/// And the record says the cap ended it, rather than looking like a turn that
/// ended on its own.
#[tokio::test]
async fn the_capped_continuation_is_recorded_as_cap_reached() {
    let h = harness_with_stop_hook("echo again 1>&2; exit 2").await;
    h.send_user_message("go").await;
    h.await_agent_inputs_settled().await;
    let outcomes = h.stop_record_outcomes().await;
    assert!(
        matches!(outcomes.last(), Some(StopOutcome::CapReached(_))),
        "the last record must name the cap, got {outcomes:?}"
    );
}

/// Non-blocking feedback informs the model; it does not force a turn. Starting
/// a run on it would make every advisory `Stop` hook an infinite session.
#[tokio::test]
async fn non_blocking_additional_context_does_not_start_a_run() {
    let h = harness_with_stop_hook(
        r#"printf '{"hookSpecificOutput":{"additionalContext":"consider the tests"}}'"#,
    )
    .await;
    h.send_user_message("go").await;
    let inputs = h.await_agent_inputs_settled().await;
    assert_eq!(inputs.len(), 1, "informed, not forced");
}

/// `Stop` runs after the fact, so a guard that could not run cannot deny
/// anything. Only `PreToolUse` fails closed.
#[tokio::test]
async fn a_failing_stop_hook_concludes_the_turn_anyway() {
    let h = harness_with_stop_hook("exit 1").await;
    h.send_user_message("go").await;
    let inputs = h.await_agent_inputs_settled().await;
    assert_eq!(inputs.len(), 1);
    assert!(h.turn_concluded().await);
}

/// Every one of them is recorded, which is the point of running them at all.
#[tokio::test]
async fn every_stop_hook_run_reaches_the_transcript() {
    let h = harness_with_stop_hook("exit 0").await;
    h.send_user_message("go").await;
    h.await_agent_inputs_settled().await;
    assert_eq!(h.stop_record_outcomes().await.len(), 1);
}
```

**The harness.** `FakeRuntimeVendor` answers the protocol itself (`server/src/runtime_vendor/fake.rs:685`) rather than running a real runtime, so these tests script records instead of shell commands — real command execution is already covered at the runtime layer by Tasks 4 and 5. Extend the builder:

```rust
// server/src/runtime_vendor/fake.rs
/// Records this fake answers each `RunHooks` with, in order; the last entry
/// repeats once exhausted. A `Stop` continuation loop asks many times, and a
/// script that ran dry would look like a hook that stopped blocking.
#[must_use]
pub fn hook_records(mut self, records: Vec<Vec<HookRecord>>) -> Self {
    self.hook_records = records;
    self
}

// in the recorder, alongside `tool_agent_ids`:
/// Every `ServerHookEvent` this fake was asked to run, so a test can assert
/// what was on the wire — `stop_hook_active` above all.
pub server_hook_events: Arc<Mutex<Vec<ServerHookEvent>>>,
```

and the inbound arm:

```rust
                    RuntimeInboundMessage::RunHooks(req) => {
                        recorder
                            .server_hook_events
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(req.event.clone());
                        let n = {
                            let mut g = recorder
                                .hook_runs
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            *g += 1;
                            *g - 1
                        };
                        let records = hook_records
                            .get(n)
                            .or_else(|| hook_records.last())
                            .cloned()
                            .unwrap_or_default();
                        Some(RuntimeOutboundMessage::HookRecords(RunHooksResponse {
                            call_id: req.call_id,
                            records,
                        }))
                    }
```

The test-side helpers then read:

```rust
    /// A session whose every `Stop` hook returns `records`.
    async fn stop_harness(records: Vec<Vec<HookRecord>>) -> StopHarness { … }

    fn stop_blocked(reason: &str) -> Vec<HookRecord> {
        vec![HookRecord {
            plugin: "stopper".into(),
            duration_ms: 1,
            action: HookAction::Stop(StopRecord {
                system_message: None,
                outcome: StopOutcome::Blocked(HookBlocked { reason: Some(reason.into()) }),
            }),
        }]
    }
```

`await_agent_inputs_settled` waits for the session to report idle (the existing `SessionCommand::UsageStats` round-trip is enough of a barrier, since the mailbox is FIFO) rather than polling a fixed count, so the cap test observes a real stop rather than a timeout. `stop_record_outcomes` reads the journal for `AgentDomainEvent::HookRan` and projects `StopOutcome`.

The shell-command form of these tests belongs one layer down: add `a_blocking_stop_hook_records_a_block` to `runtime/src/hooks/server.rs` with `plugin(plugins.path(), "p", "Stop", "", "echo again 1>&2; exit 2")`, asserting the record only. Consequences are the server's; recording is the runtime's.

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p horsie-server --lib stop_hook
```
Expected: FAIL — `MAX_STOP_CONTINUATIONS` and the harness helpers undefined.

- [ ] **Step 3: Add the continuation command**

`SessionCommand` gains:

```rust
    /// A `Stop` hook blocked, so the turn continues with `reason` as its input.
    ///
    /// Routed through the session for the same reason `HooksRan` is: the sink
    /// is built before its `AgentActor` is spawned, so it holds a key rather
    /// than an `ActorRef`.
    ContinueAfterStop { key: AgentKey, reason: String },
```

whose handler forwards `AgentCommand::Resume { results: vec![], message: Some(reason), subagent_results: vec![] }` — the same path recovery uses at `agent_actor.rs:1474` to continue an interrupted task.

- [ ] **Step 4: Write the decorator**

```rust
/// Runs `Stop` hooks when a turn concludes, and honours what they say.
///
/// A decorator on the outcome sink rather than a branch in the session's
/// `AgentOutcome` handler: `deliver` is called from the *agent's* `RunFinished`
/// handler, so a slow hook delays that agent's own mailbox and never the
/// session's command loop. The session stays able to serve a cancel or another
/// agent while a 30-second `Stop` hook runs.
struct StopHookParent {
    inner: Arc<dyn AgentOutcomeSink>,
    session: ActorRef<SessionCommand>,
    key: AgentKey,
    /// The client this agent last acquired. `Stop` cannot acquire one of its
    /// own: a turn that concluded must not be able to fail on runtime
    /// provisioning, and there is nothing to guard if no runtime ever ran.
    last_client: Arc<Mutex<Option<RuntimeClient>>>,
    /// Consecutive continuations. Reset when a turn concludes without a block,
    /// so an interactive session that legitimately continues a few times does
    /// not accumulate toward the cap forever.
    continuations: Arc<AtomicUsize>,
}

/// How many times a `Stop` hook may hold a turn open before horsie ends it
/// regardless.
///
/// Not advisory. horsie runs unattended sessions, and `stop_hook_active` only
/// stops a hook that reads it — this is for the ones that do not.
const MAX_STOP_CONTINUATIONS: usize = 3;
```

`deliver` intercepts `AgentOutcome::Concluded` only. `Asked`, `Parked`, `Failed` and `UsageRecorded` pass straight through: `Stop` fires when a turn *ends*, and a park or an ask is a turn still in progress.

```rust
#[async_trait]
impl AgentOutcomeSink for StopHookParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let AgentOutcome::Concluded { .. } = &outcome else {
            return self.inner.deliver(outcome).await;
        };
        let Some(client) = self.last_client.lock().unwrap_or_else(PoisonError::into_inner).clone()
        else {
            return self.inner.deliver(outcome).await;
        };

        let used = self.continuations.load(Ordering::Relaxed);
        let records = client
            .run_hooks(ServerHookEvent::Stop(StopInput {
                last_assistant_message: last_text(&outcome),
                stop_hook_active: used > 0,
            }))
            .await
            .unwrap_or_default();

        match stop_verdict(&records) {
            // Blocked from stopping, and there is budget left: the turn does
            // not conclude. The parent never hears about it, so the session
            // never marks the turn done and never drains its queue early.
            Some(reason) if used < MAX_STOP_CONTINUATIONS => {
                self.continuations.fetch_add(1, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::ContinueAfterStop { key: self.key, reason })
                    .await;
            }
            // Blocked, but out of budget. The turn ends and the record says
            // why, so this does not read as a turn that stopped on its own.
            Some(_) => {
                self.continuations.store(0, Ordering::Relaxed);
                let _ = self
                    .session
                    .tell(SessionCommand::HooksRan {
                        key: self.key,
                        records: cap_reached(records),
                    })
                    .await;
                self.inner.deliver(outcome).await;
            }
            None => {
                self.continuations.store(0, Ordering::Relaxed);
                self.inner.deliver(outcome).await;
            }
        }
    }
}
```

`stop_verdict` returns the first `StopOutcome::Blocked` reason (defaulting to `"a Stop hook asked for another iteration"` when the hook gave none, since an empty input is not a turn). `cap_reached` rewrites that record's outcome to `StopOutcome::CapReached`, which is the only place that variant is produced — `HookInvocation::record` cannot know the budget.

`run_hooks` already sends its records through the hook sink, so the blocked and clean paths need no extra journaling; only the capped path re-sends a rewritten record, and it suppresses the original by rewriting in place rather than appending.

Wire it in all three `AgentRuntimeContext` constructions (`session_actor.rs:689`, `:772`, `:944`):

```rust
            parent: Arc::new(StopHookParent {
                inner: Arc::new(SessionParent { target: ctx.self_ref() }),
                session: ctx.self_ref(),
                key: AgentKey::Main,
                last_client: context_provider.last_client(),
                continuations: Arc::new(AtomicUsize::new(0)),
            }),
```

`SessionContextProvider` already holds `last_client` (`session_actor.rs:1888-1891`); add an accessor returning the `Arc<Mutex<Option<RuntimeClient>>>` clone.

- [ ] **Step 5: Run to verify they pass**

```bash
cargo fmt --all && cargo test -p horsie-server --lib stop_hook
```
Expected: PASS. Then the full workspace and clippy.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "hooks: Stop continues the turn, with a hard continuation cap"
```

---

## Task 8: `UserPromptSubmit`, and the install gates

The second half of the demand gap, plus the refusal that makes "described but unwired" honest.

**Files:**
- Modify: `server/src/sessions/session_actor.rs`
- Modify: `cli/src/plugins.rs`, `server/src/plugins/ingest.rs`

**Interfaces:**
- Consumes: `ServerHookEvent::UserPromptSubmit`, `injected_context`, `PluginHooks.unsupported`.
- Produces: no new public API.

- [ ] **Step 1: Write the failing tests**

```rust
/// The prompt reaches the model with the hook's context attached — the second
/// of the two routes context takes, and the reason `UserPromptSubmit` is worth
/// wiring at all.
#[tokio::test]
async fn user_prompt_submit_context_is_appended_to_the_prompt() {
    let h = harness_with_prompt_hook(r#"printf 'today is a Tuesday'"#).await;
    h.send_user_message("what day is it").await;
    let sent = h.await_agent_inputs(1).await;
    assert!(sent[0].contains("what day is it"));
    assert!(sent[0].contains("today is a Tuesday"), "{}", sent[0]);
}

/// A blocking hook rejects the prompt, which must not reach the model at all.
#[tokio::test]
async fn a_blocking_user_prompt_submit_hook_stops_the_turn() {
    let h = harness_with_prompt_hook("echo 'no secrets in prompts' 1>&2; exit 2").await;
    h.send_user_message("here is my api key").await;
    assert!(h.await_agent_inputs_settled().await.is_empty(), "the model must never see it");
    assert!(h.last_error().await.unwrap().contains("no secrets in prompts"));
}

/// A hook that could not run cannot reject a prompt: only `PreToolUse` fails
/// closed, and rejecting on an outage would make one broken plugin mute a
/// session.
#[tokio::test]
async fn a_failing_user_prompt_submit_hook_lets_the_prompt_through() {
    let h = harness_with_prompt_hook("exit 1").await;
    h.send_user_message("hello").await;
    assert_eq!(h.await_agent_inputs(1).await.len(), 1);
}
```

In `cli/src/plugins.rs`:

```rust
/// A plugin whose only hooks horsie cannot fire must be told so at install.
/// Installing to silence is the exact failure the classification exists to
/// prevent.
#[test]
fn installing_a_plugin_whose_hooks_are_all_unwired_names_the_events() {
    let (p, _src) = marketplace_with_plugin_hooks(
        "batcher",
        r#"{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
    );
    let err = install(&p, &InstallTarget::parse("batcher@acme"), None, None, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("PostToolBatch"), "must name the event: {err}");
    assert!(err.contains("not implemented"), "{err}");
}

/// A plugin with one runnable hook installs, and is still told about the rest —
/// impeccable's real shape: one `PostToolUse` horsie runs and one `CwdChanged`
/// it does not.
#[test]
fn a_partly_supported_plugin_installs_with_a_warning() {
    let (p, _src) = marketplace_with_plugin_hooks(
        "impeccable",
        r#"{"hooks":{
             "PostToolUse":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}],
             "CwdChanged":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}]}}"#,
    );
    let report = install(&p, &InstallTarget::parse("impeccable@acme"), None, None, false).unwrap();
    assert!(report.warnings.iter().any(|w| w.contains("CwdChanged")), "{:?}", report.warnings);
}

/// `Stop` is the most-declared event in the marketplace and is now wired, so it
/// must stop being refused. This is the test that fails if Task 7 regresses.
#[test]
fn stop_is_no_longer_refused_at_install() {
    let (p, _src) = marketplace_with_plugin_hooks(
        "stopper",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}]}}"#,
    );
    assert!(install(&p, &InstallTarget::parse("stopper@acme"), None, None, false).is_ok());
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p horsie-server --lib user_prompt && cargo test -p horsie-cli --lib hooks
```
Expected: FAIL.

- [ ] **Step 3: Fire `UserPromptSubmit`**

In the session actor's user-message handler, before the message reaches the agent and only when the session uses plugins:

```rust
        // Fires before the message reaches the agent, because a hook that
        // rejects a prompt must be able to stop it reaching the model — which
        // is only true if nothing has been journaled yet.
        if let Some(client) = self.last_runtime_client()
            && let Ok(records) = client
                .run_hooks(ServerHookEvent::UserPromptSubmit(UserPromptSubmitInput {
                    prompt: text.clone(),
                }))
                .await
        {
            if let Some(reason) = blocked_prompt(&records) {
                return self.reject_message(reason, ctx).await;
            }
            if let Some(ctx_text) = injected_context(&records) {
                text = format!("{text}\n\n{ctx_text}");
            }
        }
```

```rust
/// Why a hook rejected this prompt, if one did.
///
/// A `Failed` outcome is deliberately not a rejection: only `PreToolUse` fails
/// closed, and rejecting on an outage would let one broken plugin mute a
/// session with no way to tell that from a policy decision.
fn blocked_prompt(records: &[HookRecord]) -> Option<String> {
    records.iter().find_map(|r| match &r.action {
        HookAction::UserPromptSubmit(u) => match &u.outcome {
            UserPromptSubmitOutcome::Blocked(b) => Some(
                b.reason
                    .clone()
                    .unwrap_or_else(|| "a plugin hook rejected this prompt".to_string()),
            ),
            UserPromptSubmitOutcome::Ran(_) | UserPromptSubmitOutcome::Failed(_) => None,
        },
        _ => None,
    })
}
```

The two `Stop` helpers Task 7 referenced live next to it:

```rust
/// Why a `Stop` hook is holding this turn open, if one is.
fn stop_verdict(records: &[HookRecord]) -> Option<String> {
    records.iter().find_map(|r| match &r.action {
        HookAction::Stop(s) => match &s.outcome {
            StopOutcome::Blocked(b) => Some(
                b.reason
                    .clone()
                    // An empty input is not a turn, so a hook that blocked
                    // without saying why still has to say something.
                    .unwrap_or_else(|| "a Stop hook asked for another iteration".to_string()),
            ),
            StopOutcome::Ran(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
        },
        _ => None,
    })
}

/// Rewrite the blocking record's outcome to name the cap.
///
/// The only place `CapReached` is produced: `HookInvocation::record` sees one
/// hook's reply and cannot know the budget, so the outcome is narrowed here
/// rather than invented in the library.
fn cap_reached(mut records: Vec<HookRecord>) -> Vec<HookRecord> {
    for r in &mut records {
        if let HookAction::Stop(s) = &mut r.action
            && let StopOutcome::Blocked(b) = &s.outcome
        {
            s.outcome = StopOutcome::CapReached(b.clone());
        }
    }
    records
}
```

`Stop` is wired in Task 7 and `UserPromptSubmit` here, so `HookEvent::is_wired` already returns true for both from Task 1 — no change needed, and the `exactly_five_events_are_wired` test is what proves the two halves agree.

- [ ] **Step 4: Refuse unwired hooks at install**

In `cli/src/plugins.rs`'s `install`, after `PluginRoot::inspect`, read the plugin's hooks and split on `unsupported`:

```rust
    let hooks = horsie_support::plugin::hooks::read(&root_dir).map_err(CliError::Config)?;
    // A plugin whose *only* hooks are ones horsie cannot fire would install and
    // then do nothing, which is the failure the classification exists to
    // prevent. One it can fire is enough to install; the rest are warnings.
    if hooks.decls.is_empty() && !hooks.unsupported.is_empty() {
        gc_checkout(paths, &checkout.key);
        let reasons = hooks
            .unsupported
            .iter()
            .map(|(name, why)| why.explain(name))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CliError::Config(format!(
            "'{name}' declares no hook horsie can run: {reasons}"
        )));
    }
    warnings.extend(hooks.unsupported.iter().map(|(n, w)| w.explain(n)));
```

Mirror the warning collection in `server/src/plugins/ingest.rs`, which already inspects the tree — `has_hooks` becomes `has_hooks` plus `unsupported_hooks: Vec<String>` so the server surfaces the same sentence the CLI does.

- [ ] **Step 5: Run to verify they pass**

```bash
cargo fmt --all && cargo test --workspace && cargo clippy --locked --all-targets --all-features -- -D warnings
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "hooks: wire UserPromptSubmit and refuse unwired events at install"
```

---

## Task 9: End-to-end, and the PR

The requirement journaling exists for: a hook row is still there after a reload. #140 left this undone because the harness had no plugin fixture; Task 8's install path now gives it one.

**Files:**
- Modify: `clients/web/e2e/global-setup.ts` (the plugin library it already builds)
- Create: `clients/web/e2e/t-hook-records.spec.ts`

The harness already installs `e2e-plugin` with a `SessionStart` hook echoing `E2E_BOOTSTRAP_MARKER` (`global-setup.ts:84-104`), which group F asserts reaches the system prompt. One added event gives both renderings.

- [ ] **Step 1: Add a tool hook to the existing fixture**

```ts
  fs.writeFileSync(
    path.join(hooksDir, "hooks.json"),
    JSON.stringify({
      hooks: {
        SessionStart: [{ hooks: [{ type: "command", command: "echo E2E_BOOTSTRAP_MARKER" }] }],
        // A tool-scoped record alongside the standalone one, so both
        // transcript renderings come from one fixture. `systemMessage` is
        // addressed to the user, which is what group T asserts survives.
        PostToolUse: [
          {
            matcher: "Bash",
            hooks: [
              {
                type: "command",
                command: `printf '{"systemMessage":"E2E_HOOK_NOTE"}'`,
                timeout: 5,
              },
            ],
          },
        ],
      },
    }),
  );
```

Group F's `f-context-loading.spec.ts` asserts the bootstrap marker reaches the system prompt; that assertion is unaffected, but re-run group F after this change — the `SessionStart` path is now record-derived rather than string-derived.

- [ ] **Step 2: Write the failing spec**

```ts
import { expect, test } from "@playwright/test";
import { createSession, sendMessage } from "./helpers";
import { readRuntimeInfo } from "./harness";

// The requirement journaling exists for. An ephemeral frame would satisfy
// every other assertion in this file; only the reload separates them.
test("hook records survive a reload", async ({ page }) => {
  const { appBase } = readRuntimeInfo();
  await createSession(page, appBase);
  await sendMessage(page, "run `echo hi` please");

  const notice = page.getByTestId("hook-notice").filter({ hasText: "e2e-plugin" });
  await expect(notice).toBeVisible();
  await expect(notice).toContainText("session context");

  await page.getByTestId("tool-call-toggle").first().click();
  await expect(page.getByTestId("tool-call-hook").first()).toContainText("E2E_HOOK_NOTE");

  await page.reload();
  await expect(page.getByTestId("hook-notice").filter({ hasText: "e2e-plugin" })).toBeVisible();
  await page.getByTestId("tool-call-toggle").first().click();
  await expect(page.getByTestId("tool-call-hook").first()).toContainText("E2E_HOOK_NOTE");
});
```

The spec is named `t-` so it sorts after the existing groups; the harness runs them alphabetically and group T needs a session with plugins enabled, which the default config already provides.

- [ ] **Step 3: Run it**

```bash
cargo build --release -p horsie-server -p horsie-runtime -p horsie
cd clients/web && bun run build
TMPDIR=/tmp/he2e HORSIE_E2E_SKIP_BUILD=1 ./node_modules/.bin/playwright test t-hook-records f-context-loading
```
Expected: FAIL first (no `hook-notice` testid rendered for this session), then PASS — including group F, whose bootstrap assertion now runs through the record-derived path.

- [ ] **Step 4: Full green**

```bash
cargo fmt --all
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --workspace
make ts-types && git diff --exit-code clients/ts/src/generated
cd clients/web && bun install && bun run generate-types && bun run build && bunx vitest run
TMPDIR=/tmp/he2e HORSIE_E2E_SKIP_BUILD=1 ./node_modules/.bin/playwright test
```

- [ ] **Step 5: Commit and open the PR**

```bash
git add -A
git commit -m "e2e: a hook record survives a reload"
git push -u origin feat/hook-events-model
gh pr create --title "Hook events: one library, per-event records, and Stop that continues" --body "$(cat <<'EOF'
Replaces #141, which cannot be rebased — its lower commits are the discarded server-side #140.

Every per-event hook fact now lives in one spec-derived library. `HookRecord` becomes a union over fifteen per-event records, so the five illegal states that shipped with #140 are unrepresentable: `event: String`, `blocked` set on a hook that merely failed, `additionalContext` recorded on an event that never offered it, `input_before`/`input_after` as independent options, and a `SessionStart` hook that produced no record at all.

horsie wires five events — `PreToolUse`, `PostToolUse`, `SessionStart`, and now `Stop` and `UserPromptSubmit`, the two with real demand across the marketplace. The other ten are modelled but refused at install with a reason that names the event, so nothing can install believing a hook works and find it silently never fires.

`Stop` is honoured rather than recorded: a block means blocked *from stopping*, so the turn continues with the hook's reason as its input, capped at three continuations because horsie runs unattended sessions and `stop_hook_active` only stops a hook that reads it.

Breaking: hook records journaled since #140 no longer deserialize. Accepted — no backward compatibility, consistent with the `history` rename.

Design: `docs/superpowers/specs/2026-08-04-hook-events-library-design.md`
EOF
)"
```

---

## Follow-ups, deliberately not in this PR

- **The ten unwired events.** Each needs a call site: `SessionEnd` needs a session-end concept, `SubagentStart`/`SubagentStop` a spawn/finish seam, `TaskCreated`/`TaskCompleted` the task list, `Notification` a notion of notifying, `CwdChanged` the runtime's per-agent cwd, `PostToolUseFailure`/`PostToolBatch` a dispatch split, `StopFailure` a provider-error classifier. The library already describes all of them, so each is one arm in `HookInvocation` plus its call site.
- **`continue` / `stopReason`.** Universal in the spec, parsed by nothing here. Honouring `continue: false` is a turn-lifecycle change like `Stop`'s, and belongs with it rather than bolted on.
- **HTTP hooks.** The spec allows a hook to be an HTTP endpoint receiving the payload as a POST body. horsie runs commands only.
- **The 16 `NoConcept` events.** Each needs a horsie subsystem that does not exist.
