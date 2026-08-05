# Hooks Into The Conversation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `AgentState::prompt_messages()` translate turn-scoped hook records into conversation messages instead of dropping them, and fire `SessionStart` / `SubagentStart` / `UserPromptSubmit` at a new pre-run seam so the translation is correct on a session's very first turn.

**Architecture:** One pure function (`workflow/src/hook_translation.rs`) turns a `HookEntry` into an optional `Message`; `prompt_messages()` calls it in place of its filter. `AgentCommand::Resume` splits into a prepare step (fire due hooks, journal their records) and a start step (snapshot history, run), so records land before the snapshot. The `ContextProvider` trait grows the seam; `SessionContextProvider` implements it and loses its in-`provide()` `SessionStart` call.

**Tech Stack:** Rust (tokio, async-trait), fluorite codegen for `models/`, `cargo nextest`/`cargo test`, Playwright for the web client (untouched here).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-05-hooks-into-the-conversation-design.md`. Read it before Task 1.
- **Backward compatibility is not a constraint.** Break snapshot shapes, retype fields in place, delete rows. Do not add migrations to preserve old state.
- CI runs `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`. Run `cargo fmt` *before* clippy; a fmt failure masks clippy output.
- Iterate with `cargo test -p <crate> --lib`; run the full workspace suite **once** before pushing, never twice in one command.
- Any edit to `models/fluorite/*.fl` — including doc comments — must regenerate **both** `clients/ts` and `clients/web` type trees. CI only drift-checks `clients/ts`.
- Commit author must be `zxgshawn <zxgshawn@gmail.com>` (the repo default). Never pass `-c user.name` / `-c user.email`.
- Never enable auto-merge on the PR.

---

### Task 1: The translation function

**Files:**
- Create: `workflow/src/hook_translation.rs`
- Modify: `workflow/src/lib.rs` (add `mod hook_translation;` + re-exports)

**Interfaces:**
- Produces: `pub fn translate(entry: &HookEntry) -> Option<Message>` and `pub fn prompt_blocked(records: &[HookRecord]) -> Option<String>`, both re-exported from `horsie_workflow`.

- [ ] **Step 1: Write the failing tests**

Create `workflow/src/hook_translation.rs` with the test module only, then the impl below it.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use horsie_models::agent::{HookEntry, Role};
    use horsie_models::hooks::*;

    fn entry(action: HookAction) -> HookEntry {
        HookEntry {
            id: "hook:0".into(),
            created_at_ms: 1_000,
            record: HookRecord {
                plugin: "impeccable".into(),
                duration_ms: 3,
                action,
            },
        }
    }

    fn ctx(text: &str) -> ContextInjected {
        ContextInjected {
            additional_context: Some(text.into()),
        }
    }

    #[test]
    fn session_start_context_becomes_a_framed_user_message() {
        let m = translate(&entry(HookAction::SessionStart(SessionStartRecord {
            source: "startup".into(),
            system_message: None,
            outcome: SessionStartOutcome::Ran(ctx("this repo pins node 22")),
        })))
        .expect("translated");
        assert!(matches!(m.role, Role::User));
        assert_eq!(m.id, "hook-context:hook:0");
        assert_eq!(m.created_at_ms, 1_000);
        let text = super::tests::only_text(&m);
        assert!(text.starts_with("<hook-context plugin=\"impeccable\" event=\"SessionStart\">"));
        assert!(text.contains("this repo pins node 22"));
        assert!(text.ends_with("</hook-context>"));
    }

    fn only_text(m: &horsie_models::agent::Message) -> String {
        match m.parts.as_slice() {
            [horsie_models::agent::ContentPart::Text(t)] => t.text.clone(),
            other => panic!("expected exactly one text part, got {other:?}"),
        }
    }

    #[test]
    fn stop_context_translates() {
        assert!(
            translate(&entry(HookAction::Stop(StopRecord {
                system_message: None,
                outcome: StopOutcome::Ran(ctx("remember to run the linter")),
            })))
            .is_some()
        );
    }

    #[test]
    fn user_prompt_submit_context_translates() {
        assert!(
            translate(&entry(HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                system_message: None,
                outcome: UserPromptSubmitOutcome::Ran(ctx("current branch: main")),
            })))
            .is_some()
        );
    }

    #[test]
    fn subagent_start_and_stop_context_translate() {
        assert!(
            translate(&entry(HookAction::SubagentStart(SubagentStartRecord {
                agent_type: "reviewer".into(),
                system_message: None,
                outcome: SubagentStartOutcome::Ran(ctx("house rules")),
            })))
            .is_some()
        );
        assert!(
            translate(&entry(HookAction::SubagentStop(SubagentStopRecord {
                agent_type: "reviewer".into(),
                system_message: None,
                outcome: SubagentStopOutcome::Ran(ctx("summarise findings")),
            })))
            .is_some()
        );
    }

    #[test]
    fn post_tool_batch_context_translates() {
        assert!(
            translate(&entry(HookAction::PostToolBatch(PostToolBatchRecord {
                calls: vec![],
                system_message: None,
                outcome: PostToolBatchOutcome::Ran(ctx("two files changed")),
            })))
            .is_some()
        );
    }

    /// The central division of the design: a tool hook edits the tool's own
    /// output, so the tool result already represents it and there is nothing
    /// left to translate.
    #[test]
    fn tool_scoped_records_never_translate() {
        let call = ToolScope {
            tool: "bash".into(),
            tool_call_id: "tc1".into(),
        };
        assert!(
            translate(&entry(HookAction::PreToolUse(PreToolUseRecord {
                call: call.clone(),
                system_message: Some("noisy".into()),
                outcome: PreToolUseOutcome::Denied(HookDenied {
                    reason: Some("no".into())
                }),
            })))
            .is_none()
        );
        assert!(
            translate(&entry(HookAction::PostToolUse(PostToolUseRecord {
                call: call.clone(),
                system_message: None,
                outcome: PostToolUseOutcome::Ran(PostToolUseRan {
                    output: None,
                    additional_context: Some("appended by the runtime".into()),
                }),
            })))
            .is_none()
        );
        assert!(
            translate(&entry(HookAction::PostToolUseFailure(
                PostToolUseFailureRecord {
                    call,
                    system_message: None,
                    outcome: PostToolUseFailureOutcome::Ran(ctx("also appended")),
                }
            )))
            .is_none()
        );
    }

    /// A block reason is a turn *input* (`Resume { message }`), not context;
    /// a cap-reached turn is over; a failure is an outage, not content.
    #[test]
    fn stop_block_cap_and_failure_never_translate() {
        for outcome in [
            StopOutcome::Blocked(HookBlocked {
                reason: Some("keep going".into()),
            }),
            StopOutcome::CapReached(HookBlocked { reason: None }),
            StopOutcome::Failed(HookFailed {
                reason: "timeout".into(),
            }),
        ] {
            assert!(
                translate(&entry(HookAction::Stop(StopRecord {
                    system_message: Some("shown to the user only".into()),
                    outcome,
                })))
                .is_none()
            );
        }
    }

    #[test]
    fn side_effect_and_task_events_never_translate() {
        for action in [
            HookAction::SessionEnd(SessionEndRecord {
                reason: "clear".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::StopFailure(StopFailureRecord {
                error: "rate_limit".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::Notification(NotificationRecord {
                message: "hi".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::CwdChanged(CwdChangedRecord {
                cwd: "/tmp".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::TaskCreated(TaskCreatedRecord {
                task_id: "t1".into(),
                system_message: Some("user-facing".into()),
                outcome: TaskOutcome::Ran,
            }),
            HookAction::TaskCompleted(TaskCompletedRecord {
                task_id: "t1".into(),
                system_message: None,
                outcome: TaskOutcome::Ran,
            }),
        ] {
            assert!(translate(&entry(action)).is_none());
        }
    }

    #[test]
    fn an_empty_additional_context_translates_to_nothing() {
        assert!(
            translate(&entry(HookAction::SessionStart(SessionStartRecord {
                source: "startup".into(),
                system_message: None,
                outcome: SessionStartOutcome::Ran(ContextInjected {
                    additional_context: None,
                }),
            })))
            .is_none()
        );
    }

    #[test]
    fn a_blocking_user_prompt_submit_record_reports_its_reason() {
        let records = vec![HookRecord {
            plugin: "guard".into(),
            duration_ms: 1,
            action: HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                system_message: None,
                outcome: UserPromptSubmitOutcome::Blocked(HookBlocked {
                    reason: Some("secrets in the prompt".into()),
                }),
            }),
        }];
        assert_eq!(
            prompt_blocked(&records).as_deref(),
            Some("secrets in the prompt")
        );
        assert!(prompt_blocked(&[]).is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-workflow --lib hook_translation`
Expected: FAIL to compile — `translate` and `prompt_blocked` are not defined.

- [ ] **Step 3: Write the implementation**

Above the test module in `workflow/src/hook_translation.rs`:

```rust
//! Turning a hook record into the message it puts in front of the model.
//!
//! One rule decides every arm: **a hook record translates into the conversation
//! only when its effect has no other representation there.** A tool hook edits
//! the tool's own output, so the tool result already carries it. A `Stop` block
//! is a turn input and arrives as `AgentCommand::Resume { message }`. A
//! `system_message` is addressed to the user and never to the model. What is
//! left — context injected by a turn-scoped event — has nowhere else to live,
//! and becomes a message here.
//!
//! The match is exhaustive on purpose: wiring a new event cannot put text in
//! front of the model without someone deciding how.

use horsie_models::agent::{ContentPart, HookEntry, Message, Role, TextPart};
use horsie_models::hooks::{
    HookAction, PostToolBatchOutcome, SessionStartOutcome, StopOutcome, SubagentStartOutcome,
    SubagentStopOutcome, UserPromptSubmitOutcome,
};

/// The message a hook record contributes to the prompt, if any.
///
/// Derived at prompt assembly and never journaled, so the transcript keeps
/// showing a hook row where the model sees a message — which is what lets the
/// web client tell an injection from something the user wrote.
#[must_use]
pub fn translate(entry: &HookEntry) -> Option<Message> {
    let (event, context) = match &entry.record.action {
        // --- Translated: turn-scoped context with nowhere else to live ---
        HookAction::SessionStart(r) => (
            "SessionStart",
            match &r.outcome {
                SessionStartOutcome::Ran(c) => c.additional_context.as_deref(),
                SessionStartOutcome::Failed(_) => None,
            },
        ),
        HookAction::UserPromptSubmit(r) => (
            "UserPromptSubmit",
            match &r.outcome {
                UserPromptSubmitOutcome::Ran(c) => c.additional_context.as_deref(),
                // A blocked prompt never starts a run; there is no turn to
                // annotate. See `prompt_blocked`.
                UserPromptSubmitOutcome::Blocked(_) | UserPromptSubmitOutcome::Failed(_) => None,
            },
        ),
        HookAction::Stop(r) => (
            "Stop",
            match &r.outcome {
                StopOutcome::Ran(c) => c.additional_context.as_deref(),
                // `Blocked` is a turn input, delivered as `Resume { message }`;
                // `CapReached` ends the turn; `Failed` is an outage.
                StopOutcome::Blocked(_) | StopOutcome::CapReached(_) | StopOutcome::Failed(_) => {
                    None
                }
            },
        ),
        HookAction::SubagentStart(r) => (
            "SubagentStart",
            match &r.outcome {
                SubagentStartOutcome::Ran(c) => c.additional_context.as_deref(),
                SubagentStartOutcome::Failed(_) => None,
            },
        ),
        HookAction::SubagentStop(r) => (
            "SubagentStop",
            match &r.outcome {
                SubagentStopOutcome::Ran(c) => c.additional_context.as_deref(),
                SubagentStopOutcome::Blocked(_) | SubagentStopOutcome::Failed(_) => None,
            },
        ),
        // The one tool-*named* event that translates: it spans N calls, so no
        // single tool result can carry it, and it fires at a turn boundary.
        HookAction::PostToolBatch(r) => (
            "PostToolBatch",
            match &r.outcome {
                PostToolBatchOutcome::Ran(c) => c.additional_context.as_deref(),
                PostToolBatchOutcome::Blocked(_) | PostToolBatchOutcome::Failed(_) => None,
            },
        ),
        // --- Never translated: already represented in the tool result ---
        //
        // A tool hook edits the tool's own output. `PreToolUse` has no
        // `additionalContext` by spec; its denial is a `ToolResult::Err`. The
        // other two are appended to the output by the runtime.
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        // --- Never translated: no model-visible content at all ---
        | HookAction::SessionEnd(_)
        | HookAction::StopFailure(_)
        | HookAction::Notification(_)
        | HookAction::CwdChanged(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_) => return None,
    };
    // `system_message` is deliberately never read here, on any arm: it is
    // addressed to the user, and pinned as such by
    // `a_system_message_is_recorded_and_never_injected`.
    let context = context?;
    Some(Message {
        id: format!("hook-context:{}", entry.id),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            // Framed with its plugin, because a plugin is third-party and its
            // text must never read as horsie's own instruction.
            text: format!(
                "<hook-context plugin=\"{}\" event=\"{event}\">\n{context}\n</hook-context>",
                entry.record.plugin
            ),
        })],
        created_at_ms: entry.created_at_ms,
        started_at_ms: None,
    })
}

/// Why a `UserPromptSubmit` hook refused this prompt, if one did.
///
/// Not a translation: a blocked prompt is never journaled, so the run is
/// abandoned at the seam rather than filtered out of the fold.
#[must_use]
pub fn prompt_blocked(records: &[horsie_models::hooks::HookRecord]) -> Option<String> {
    records.iter().find_map(|r| match &r.action {
        HookAction::UserPromptSubmit(u) => match &u.outcome {
            UserPromptSubmitOutcome::Blocked(b) => Some(
                b.reason
                    .clone()
                    .unwrap_or_else(|| "a UserPromptSubmit hook blocked this prompt".to_string()),
            ),
            UserPromptSubmitOutcome::Ran(_) | UserPromptSubmitOutcome::Failed(_) => None,
        },
        _ => None,
    })
}
```

Note: the `_ => None` in `prompt_blocked` is acceptable because it answers a
single-event question; `translate` is the exhaustive one.

Wire it up in `workflow/src/lib.rs`, beside the existing module declarations:

```rust
mod hook_translation;
pub use hook_translation::{prompt_blocked, translate};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo fmt && cargo test -p horsie-workflow --lib hook_translation`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add workflow/src/hook_translation.rs workflow/src/lib.rs
git commit -m "feat: translate a hook record into the message it injects"
```

---

### Task 2: `prompt_messages()` translates instead of filtering

**Files:**
- Modify: `workflow/src/agent_actor.rs` (`AgentState::prompt_messages`, ~line 476-489)
- Modify: `models/fluorite/agent.fl:93-101` (the stale `HistoryEntry` doc comment)
- Regenerate: `clients/ts`, `clients/web`

**Interfaces:**
- Consumes: `crate::hook_translation::translate` from Task 1.
- Produces: `AgentState::prompt_messages()` — same signature, new behaviour.

- [ ] **Step 1: Write the failing test**

In `workflow/src/agent_actor.rs`'s existing `mod tests`:

```rust
#[test]
fn a_hook_entry_translates_in_place_between_the_messages_around_it() {
    use horsie_models::hooks::{
        ContextInjected, HookAction, HookRecord, StopOutcome, StopRecord,
    };
    let mut state = AgentState::default();
    state
        .history
        .push(HistoryEntry::Llm(Message::user("m1", "hello", 1)));
    state.history.push(HistoryEntry::Hook(crate::hook_entry(
        HookRecord {
            plugin: "nagger".into(),
            duration_ms: 1,
            action: HookAction::Stop(StopRecord {
                system_message: None,
                outcome: StopOutcome::Ran(ContextInjected {
                    additional_context: Some("check the tests".into()),
                }),
            }),
        },
        0,
        2,
    )));
    state
        .history
        .push(HistoryEntry::Llm(Message::user("m2", "carry on", 3)));

    let prompt = state.prompt_messages();
    assert_eq!(prompt.len(), 3, "the hook contributes one message");
    assert_eq!(prompt[0].id, "m1");
    assert_eq!(prompt[1].id, "hook-context:hook:0");
    assert_eq!(prompt[2].id, "m2");
}

#[test]
fn a_tool_scoped_hook_entry_contributes_nothing_to_the_prompt() {
    use horsie_models::hooks::{
        HookAction, HookRecord, PostToolUseOutcome, PostToolUseRan, PostToolUseRecord, ToolScope,
    };
    let mut state = AgentState::default();
    state.history.push(HistoryEntry::Hook(crate::hook_entry(
        HookRecord {
            plugin: "linter".into(),
            duration_ms: 1,
            action: HookAction::PostToolUse(PostToolUseRecord {
                call: ToolScope {
                    tool: "bash".into(),
                    tool_call_id: "tc1".into(),
                },
                system_message: None,
                outcome: PostToolUseOutcome::Ran(PostToolUseRan {
                    output: None,
                    additional_context: Some("appended to the tool output already".into()),
                }),
            }),
        },
        0,
        1,
    )));
    assert!(state.prompt_messages().is_empty());
}
```

- [ ] **Step 2: Run to verify the first test fails**

Run: `cargo test -p horsie-workflow --lib translates_in_place`
Expected: FAIL — `assert_eq!(prompt.len(), 3)` sees 2, because hooks are filtered.

- [ ] **Step 3: Replace the filter with the translation**

In `workflow/src/agent_actor.rs`, replace the body and doc comment of `prompt_messages`:

```rust
    /// What the model sees: the transcript, with every hook entry translated
    /// into the message it injects (most translate to nothing).
    ///
    /// The only way to obtain a `Vec<Message>` from state. `self.history` cannot
    /// be handed to a provider because the element types differ, so every new
    /// kind of entry must state what, if anything, it shows the model —
    /// `crate::hook_translation::translate` is where that is decided, in one
    /// exhaustive match.
    pub fn prompt_messages(&self) -> Vec<Message> {
        self.history
            .iter()
            .filter_map(|e| match e {
                HistoryEntry::Llm(m) => Some(m.clone()),
                HistoryEntry::Hook(h) => crate::hook_translation::translate(h),
            })
            .collect()
    }
```

- [ ] **Step 4: Update the stale model doc comment**

`models/fluorite/agent.fl`, replacing the `HistoryEntry` doc block (currently
"…`Hook` entries are recorded for the *user* and are filtered out on the way to
a provider…"):

```
/// One item in an agent's transcript.
///
/// A transcript is not a conversation: a `Hook` entry is what a plugin *did*,
/// and only some of those put text in front of the model. `AgentState::
/// prompt_messages` translates each entry on the way to a provider — most hook
/// entries translate to nothing. Because the union sits above `Message` rather
/// than inside `ContentPart`, no provider ever holds an arm for a value it must
/// interpret itself, and clients keep receiving hook entries verbatim so they
/// can render an intervention as an intervention.
```

While here, correct `HookEntry.id`'s doc: the format is `hook:{n}` (see
`hook_entry_id`), not `hook:{tool_call_id}:{n}`.

- [ ] **Step 5: Regenerate both type trees and run the tests**

Run:
```bash
cargo fmt
(cd clients/ts && bun run generate-types)
(cd clients/web && bun run generate-types)
cargo test -p horsie-workflow --lib
```
Expected: PASS. `git status` should show changes under both `clients/ts/src/generated` and `clients/web/src/generated`.

- [ ] **Step 6: Commit**

```bash
git add workflow/src/agent_actor.rs models/fluorite/agent.fl clients/ts clients/web
git commit -m "feat: prompt_messages translates hook entries instead of dropping them"
```

---

### Task 3: The pre-run seam in the agent actor

**Files:**
- Modify: `workflow/src/context.rs` (trait `ContextProvider`, new `StartTurn`)
- Modify: `workflow/src/agent_actor.rs` (`AgentCommand`, `AgentActor`, the `Resume` arm)

**Interfaces:**
- Consumes: `crate::hook_translation::prompt_blocked` from Task 1.
- Produces:
  - `pub struct StartTurn { pub start_source: Option<String>, pub prompt: Option<String> }`
  - `ContextProvider::has_start_hooks(&self) -> bool` (default `false`)
  - `ContextProvider::start_hooks(&self, turn: StartTurn) -> Result<Vec<HookRecord>, ContextError>` (default `Ok(vec![])`)
  - `AgentCommand::StartPrepared(Box<PreparedStart>)` (internal)

- [ ] **Step 1: Extend the `ContextProvider` trait**

In `workflow/src/context.rs`:

```rust
/// What a run's pre-start hooks need to know about the turn about to begin.
///
/// Built by the agent actor, which knows whether this load has already fired its
/// start hook and whether the turn begins on a user message; interpreted by the
/// provider, which knows whether this agent is a session or a subagent.
#[derive(Debug, Clone)]
pub struct StartTurn {
    /// `Some(source)` when this agent load has not yet fired its start hook.
    /// `"startup"` for a fresh agent, `"resume"` for one recovered from a
    /// journal — the only two lifecycle transitions horsie has.
    pub start_source: Option<String>,
    /// The user prompt this run starts on, when it has one.
    pub prompt: Option<String>,
}
```

and on the trait:

```rust
    /// Whether this provider has hooks to fire before a run starts. `false`
    /// skips the whole prepare round-trip, which is what keeps a session with no
    /// plugins exactly as fast as it is today.
    fn has_start_hooks(&self) -> bool {
        false
    }

    /// Fire the hooks that must run *before* the run snapshots its history —
    /// `SessionStart`/`SubagentStart` and `UserPromptSubmit`. Called on a
    /// spawned task, never a mailbox, exactly like `provide`.
    async fn start_hooks(
        &self,
        turn: StartTurn,
    ) -> Result<Vec<horsie_models::hooks::HookRecord>, ContextError> {
        let _ = turn;
        Ok(Vec::new())
    }
```

- [ ] **Step 2: Write the failing tests**

In `workflow/src/agent_actor.rs`'s `mod tests`, using the existing fake-provider
pattern there (see the `impl ContextProvider` blocks around lines 2051 / 2148 /
3522 and copy whichever fixture is closest):

```rust
/// The regression a naive translation would cause: `SessionStart` fires inside
/// `provide()` today, which runs *after* the run has snapshotted its history,
/// so its context would first appear on turn two.
#[tokio::test]
async fn session_start_context_reaches_the_very_first_prompt() { /* see below */ }

#[tokio::test]
async fn a_second_turn_does_not_fire_the_start_hook_again() { /* see below */ }

#[tokio::test]
async fn an_agent_with_recovered_history_reports_source_resume() { /* see below */ }

#[tokio::test]
async fn a_blocked_prompt_journals_no_input_and_starts_no_run() { /* see below */ }

#[tokio::test]
async fn a_provider_without_start_hooks_makes_no_prepare_round_trip() { /* see below */ }
```

Each test builds an agent whose `ContextProvider` records the `StartTurn`s it
received and returns scripted records, then asserts on (a) the prompt the fake
LLM was called with, (b) the recorded `StartTurn`s, and (c) `GetHistory`. The
fake LLM in this module already captures its prompt; reuse it rather than adding
another.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p horsie-workflow --lib start_hook`
Expected: FAIL — no seam exists; `StartTurn` is never constructed.

- [ ] **Step 4: Add the command and actor state**

In `workflow/src/agent_actor.rs`:

```rust
/// A turn whose pre-start hooks have run, on its way back to the actor.
pub struct PreparedStart {
    pub results: Vec<horsie_models::agent::ToolResultInput>,
    pub message: Option<String>,
    pub subagent_results: Vec<horsie_models::agent::SubAgentResultPart>,
    /// Records to journal before the run snapshots its history. Empty when no
    /// hook fired.
    pub records: Vec<horsie_models::hooks::HookRecord>,
    /// `Some` abandons the turn: a `UserPromptSubmit` hook refused it, or
    /// preparation failed outright.
    pub abandon: Option<AbandonedStart>,
}

/// Why a prepared turn never ran.
pub enum AbandonedStart {
    /// A `UserPromptSubmit` hook refused the prompt.
    Blocked(String),
    /// Preparation could not complete — no runtime, most likely.
    Failed(crate::ContextError),
}
```

New `AgentCommand` variant, documented as internal like `RunFinished`:

```rust
    /// Internal: pre-start hooks finished; journal their records and start (or
    /// abandon) the turn. Boxed to keep the command enum small.
    StartPrepared(Box<PreparedStart>),
```

New field on `AgentActor`, initialised `false` in both constructors:

```rust
    /// A prepare step is in flight. Gates a second `Resume` exactly as `running`
    /// does — between `Resume` and `StartPrepared` no run exists yet, so
    /// `reject_if_running` alone would let two turns through.
    preparing: bool,
    /// Whether this agent load has fired its start hook. Deliberately not
    /// journaled: a rehydrated agent fires again, which is exactly what
    /// `source: "resume"` means.
    start_hook_fired: bool,
```

Extend `reject_if_running` to also refuse while `preparing`, renaming it
`reject_if_busy` and updating its three call sites (`Resume`, and wherever else
it is used).

- [ ] **Step 5: Split the `Resume` arm**

Replace the `AgentCommand::Resume` arm's body. Everything after the empty-input
guard moves into a new `fn start_prepared`, and `Resume` either calls it
directly or spawns the prepare task:

```rust
            AgentCommand::Resume {
                results,
                message,
                subagent_results,
            } => {
                if let Some(reason) = self.reject_if_busy("Resume") {
                    return reason;
                }
                if results.is_empty() && message.is_none() && subagent_results.is_empty() {
                    tracing::warn!("Resume with nothing to resume on; ignoring");
                    return CommandEffect::none();
                }
                let start_source = (!self.start_hook_fired).then(|| {
                    // A fresh agent has nothing in its transcript; anything else
                    // was folded from a journal. No framework flag needed.
                    if state.history.is_empty() {
                        "startup".to_string()
                    } else {
                        "resume".to_string()
                    }
                });
                let turn = crate::StartTurn {
                    start_source,
                    prompt: message.clone(),
                };
                let nothing_due = turn.start_source.is_none() && turn.prompt.is_none();
                if nothing_due || !self.ctx.context_provider.has_start_hooks() {
                    return self.start_prepared(
                        PreparedStart {
                            results,
                            message,
                            subagent_results,
                            records: Vec::new(),
                            abandon: None,
                        },
                        state,
                        ctx,
                    );
                }
                self.preparing = true;
                self.start_hook_fired = true;
                let provider = self.ctx.context_provider.clone();
                let self_ref = ctx.self_ref();
                tokio::spawn(async move {
                    let prepared = match provider.start_hooks(turn).await {
                        Ok(records) => PreparedStart {
                            abandon: crate::prompt_blocked(&records).map(AbandonedStart::Blocked),
                            records,
                            results,
                            message,
                            subagent_results,
                        },
                        Err(error) => PreparedStart {
                            results,
                            message,
                            subagent_results,
                            records: Vec::new(),
                            abandon: Some(AbandonedStart::Failed(error)),
                        },
                    };
                    let _ = self_ref
                        .tell(AgentCommand::StartPrepared(Box::new(prepared)))
                        .await;
                });
                CommandEffect::none()
            }
```

`start_hook_fired` is set when the prepare task is *spawned*, not when it
returns: a failed prepare must not re-fire `SessionStart` on the next turn, which
would double-inject after the first success.

- [ ] **Step 6: Handle `StartPrepared`**

```rust
            AgentCommand::StartPrepared(prepared) => {
                self.preparing = false;
                self.start_prepared(*prepared, state, ctx)
            }
```

And the method, which is `Resume`'s old tail with the records journaled first:

```rust
    /// Journal a prepared turn's hook records, then start it — or abandon it.
    ///
    /// The records are persisted *before* `prompt_messages()` reads state, which
    /// is why the fold sees them: `CommandEffect::persist` applies its events
    /// before the next command is handled, and `start_run` receives history
    /// computed here from `state` plus those events.
    fn start_prepared(
        &mut self,
        prepared: PreparedStart,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let at_ms = now_ms();
        let mut seq = state.hook_entry_count();
        let mut events: Vec<AgentDomainEvent> = Vec::new();
        // Fold the records into a local copy so this turn's own prompt sees
        // them, since `state` is the pre-command snapshot.
        let mut folded = state.clone();
        for record in prepared.records {
            let event = AgentDomainEvent::HookRan {
                record,
                seq,
                at_ms,
            };
            folded.apply_event(&event);
            events.push(event);
            seq += 1;
        }
        if let Some(abandon) = prepared.abandon {
            let (error, recoverable) = match abandon {
                AbandonedStart::Blocked(reason) => (reason, false),
                AbandonedStart::Failed(e) => (e.message, true),
            };
            self.ctx
                .parent
                .deliver(AgentOutcome::Failed {
                    session_id: self.ctx.session_id,
                    error,
                    recoverable,
                    terminal: false,
                })
                .await;
            // The records still reach the transcript: the user must be able to
            // see why nothing ran.
            return CommandEffect::persist(events);
        }
        /* …then exactly the body `Resume` has today, reading `folded` instead of
           `state` and pushing onto the existing `events` vec… */
    }
```

Note `start_prepared` must be `async` (it awaits `deliver`); the `Resume` arm is
already in an async handler, so both call sites simply `.await`.

- [ ] **Step 7: Run the tests**

Run: `cargo fmt && cargo test -p horsie-workflow --lib`
Expected: PASS, including the five new tests and every pre-existing one.

- [ ] **Step 8: Commit**

```bash
git add workflow/src/context.rs workflow/src/agent_actor.rs
git commit -m "feat: fire a turn's start hooks before it snapshots its history"
```

---

### Task 4: `SessionContextProvider` implements the seam

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (`SessionContextProvider`, `provide`, `SharedContext`)
- Modify: `runtime-client/src/client.rs` (delete `injected_context`)
- Modify: `runtime-client/src/lib.rs` (drop the re-export)

**Interfaces:**
- Consumes: `StartTurn`, `ContextProvider::{has_start_hooks, start_hooks}` from Task 3.

- [ ] **Step 1: Write the failing test**

In `server/src/sessions/session_actor.rs`'s `mod tests`, beside the existing
`stop_harness` cases:

```rust
/// The bug this closes: `injected_context` extracted `Stop` context and had
/// exactly one caller, the `SessionStart` bootstrap — so a `Stop` hook's
/// `additionalContext` was recorded, rendered, and never shown to the model.
#[tokio::test]
async fn stop_hook_context_reaches_the_next_prompt() {
    let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
        horsie_models::hooks::ContextInjected {
            additional_context: Some("run the linter before you finish".into()),
        },
    ))]])
    .await;
    /* prompt the session, then assert the fake LLM's second prompt contains
       "run the linter before you finish" inside a <hook-context …> block */
}

#[tokio::test]
async fn a_subagent_fires_subagent_start_never_session_start() { /* … */ }

#[tokio::test]
async fn session_start_fires_once_across_two_turns() { /* … */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib stop_hook_context_reaches`
Expected: FAIL — the context never appears in the prompt.

- [ ] **Step 3: Implement the seam on `SessionContextProvider`**

```rust
#[async_trait]
impl ContextProvider for SessionContextProvider {
    fn has_start_hooks(&self) -> bool {
        self.use_plugins()
    }

    async fn start_hooks(
        &self,
        turn: StartTurn,
    ) -> Result<Vec<HookRecord>, ContextError> {
        let client = self.runtime_client().await?;
        let mut records = Vec::new();
        if let Some(source) = turn.start_source {
            // A subagent's start is a `SubagentStart`, never a session's.
            let event = match self.kind {
                SessionAgentKind::Sub(_) => ServerHookEvent::SubagentStart(SubagentStartInput {
                    agent_type: self.settings.agent_type_name(),
                }),
                SessionAgentKind::Main | SessionAgentKind::Step(_) => {
                    ServerHookEvent::SessionStart(SessionStartInput { source })
                }
            };
            records.extend(client.run_hooks(event).await.unwrap_or_default());
        }
        if let Some(prompt) = turn.prompt {
            records.extend(
                client
                    .run_hooks(ServerHookEvent::UserPromptSubmit(UserPromptSubmitInput {
                        prompt,
                    }))
                    .await
                    .unwrap_or_default(),
            );
        }
        Ok(records)
    }

    async fn provide(&self) -> Result<Contexts, ContextError> { /* … */ }
}
```

The client here must be **sink-less**: the agent journals these records itself,
via `StartPrepared`, and letting `SessionHookSink` also route them (agent →
session → agent) would both duplicate them and race the `StartPrepared`. Factor
the runtime acquisition `provide()` already does into a `runtime_client()`
helper and attach the hook sink only on `provide()`'s path.

If `ServerHookEvent` has no `SubagentStart` / `UserPromptSubmit` arm yet, add
them in `models/fluorite/runtime.fl` alongside the existing `SessionStart` /
`Stop` inputs, map them in `runtime/src/hooks/server.rs:26`, and regenerate both
type trees.

- [ ] **Step 4: Remove the old `SessionStart` path**

In `provide()`: delete the `run_hooks(ServerHookEvent::SessionStart(…))` call and
the `bootstrap` binding; drop `bootstrap` from `SharedContext` and from wherever
the system prompt renders it. Delete `injected_context` and its tests from
`runtime-client/src/client.rs`, and its re-export from `runtime-client/src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo fmt && cargo test -p horsie-server --lib && cargo test -p horsie-runtime-client`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server runtime-client runtime models clients
git commit -m "feat: session start hooks fire once per load, at the run seam"
```

---

### Task 5: Full verification and PR

- [ ] **Step 1: Workspace build, lint, test — once**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all clean. Fix `large_enum_variant` on `AgentCommand::StartPrepared`
by keeping it boxed (already specified); if clippy still complains, follow the
existing `#[allow]`-with-a-comment precedent in `models/src/lib.rs`.

- [ ] **Step 2: Confirm the generated trees are not drifting**

```bash
git status --porcelain clients/ts clients/web
```
Expected: empty — the regenerations were committed in Tasks 2 and 4.

- [ ] **Step 3: Open the PR**

```bash
git push -u origin feat/hooks-into-conversation
gh pr create --title "Hook records translate into the conversation" --body "$(cat <<'EOF'
…
EOF
)"
```

Body: one long line per paragraph or bullet, no hard wrapping, no test-by-test
narration, no CI status, no diff restatement. Link the spec and note that it
closes #208. **Do not enable auto-merge.**

## Self-Review

**Spec coverage.** The rule and its three "never translated" categories → Task 1's
match and its four negative tests. The translation table → Task 1. Message shape
→ Task 1. Where it lives → Task 1. The seam → Task 3. Fire-once and `source` →
Task 3 (actor state) + Task 4 (event selection). `SubagentStart` → Task 4. Both
failure paths → Task 3's `AbandonedStart`. Deletions → Task 4 Step 4. Testing →
Tasks 1–4, one test per bullet in the spec's list. What is *not* covered by a
task, matching the spec's "Out of scope": wiring `SubagentStop` and
`PostToolBatch` (their translation arms exist and are tested; no call site),
`continue: false`, HTTP hooks.

**Placeholders.** Task 3 Step 2 and Task 4 Step 1 give test names and assertions
in prose rather than full bodies, because both depend on which existing fixture
in those modules is closest and copying the wrong one is worse than adapting the
right one. Every other step carries its actual code. Task 3 Step 6's
`/* …then exactly the body `Resume` has today… */` is a deliberate move-this,
not a write-this.

**Type consistency.** `translate(&HookEntry) -> Option<Message>` and
`prompt_blocked(&[HookRecord]) -> Option<String>` are used with those exact
signatures in Tasks 2 and 3. `StartTurn { start_source, prompt }` is constructed
in Task 3 and destructured in Task 4 under the same field names.
`PreparedStart`/`AbandonedStart` appear only within Task 3.
