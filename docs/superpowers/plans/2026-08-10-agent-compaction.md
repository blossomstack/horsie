# Agent Compaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a session outlive its context window by summarizing older history into a log boundary, without ever removing anything a person can read.

**Architecture:** The append-only agent log gains a fourth body arm, `Compaction`. `AgentState::prompt_messages()` starts reading at the newest boundary instead of at zero. `Agent::run_inner` checks the budget at the top of each loop iteration — the one place a run's history is guaranteed balanced — and compacts through a `CompactionPolicy` trait the server implements. Nothing is deleted or renumbered.

**Tech Stack:** Rust (tokio actors, `horsie-actor` event sourcing, fluorite schema codegen), React + TypeScript + Tailwind (clients/web), Playwright e2e.

## Global Constraints

- **No backward compatibility work.** Break shapes and go to the right end state. No importers, no dual-read paths.
- **Snapshot contract:** `AgentState` is serialized. Every new field carries `#[serde(default)]`. **Add** union arms; never rename or repurpose one — renaming persisted variants took the supervisor down on 2026-08-02.
- **Wire is camelCase.** A snake_case key in a hand-written JSON body is silently ignored, never an error.
- **fluorite codegen:** after editing any `.fl`, run `make types`. Run `fluorite clean` first — generation never deletes, so orphaned files linger. Drift-check with `git status`, not `git diff`. Regenerate in **both** `clients/web/src/generated` and `clients/ts/src/generated`.
- **Verification cost:** iterate with `cargo test -p <crate> --lib`. Run the full workspace suite once before pushing, never twice in one command. `-p horsie-server` alone is a false green for API changes — `crates/tests` and the web e2e call the routes too.
- **Web installs with bun**, not npm: `bun install --frozen-lockfile`.
- **Playwright on macOS needs `TMPDIR=/tmp`** or global setup dies in `sun_path`.
- Thresholds are server constants: trigger at **80%** of `context_window`, retain approximately the trailing **20%**.

---

## PR 1 — The mechanism

Branch `feat/compaction`. Auto-compaction working end to end, no UI, no `/compact`, no hooks.

### Task 1: The boundary type

**Files:**
- Modify: `crates/models/fluorite/agent.fl`
- Modify: `crates/models/fluorite/events.fl`
- Test: `crates/server/src/agent_loop/agent_actor.rs` (existing test module)

**Interfaces:**
- Produces: `AgentLogBody::Compaction(CompactionEntry)`; `CompactionEntry { summary, carried_state, covers_through_seq, retained_from_seq, trigger, instructions, tokens_before, tokens_after }`; `CompactionTrigger::{Auto,Manual}`; `AgentEvent::Compacted(CompactedEvent { message_id, entry, at_ms })`.

- [ ] **Step 1: Add the schema types to `agent.fl`**, appended after `SessionFailedLifecycle`, with the `AgentLogBody` union gaining a fourth arm. Reuse the existing `EmptyOutcome` for the trigger arms.
- [ ] **Step 2: Add `CompactedEvent` to `events.fl`** and a `Compacted` arm on `AgentEvent`.
- [ ] **Step 3: Regenerate.** `fluorite clean && make types`, then `git status` to confirm no orphans and that both TS trees changed.
- [ ] **Step 4: Build.** `cargo build -p horsie-models` — expect failures in `horsie-server` from the now-non-exhaustive `AgentLogBody` matches. That list *is* the task list for Task 2.
- [ ] **Step 5: Commit** `feat(models): a compaction boundary in the agent log`.

### Task 2: `prompt_messages()` reads from the newest boundary

**Files:**
- Modify: `crates/server/src/agent_loop/agent_actor.rs` (`AgentState::prompt_messages`, ~line 686)
- Modify: `crates/server/src/agent_loop/agent_log.rs` (`LogPage::messages`)
- Test: same files' test modules

**Interfaces:**
- Produces: `AgentState::last_boundary(&self) -> Option<&CompactionEntry>`, `AgentState::boundary_seqs(&self) -> Vec<u64>`, `fn boundary_message(entry: &CompactionEntry, at_ms: u64) -> Message`.

- [ ] **Step 1: Write the failing tests** in `agent_actor.rs`'s test module:
  - `a_log_with_no_boundary_prompts_exactly_as_before`
  - `a_boundary_replaces_everything_it_covers_with_one_message`
  - `only_the_newest_of_two_boundaries_is_honoured`
  - `a_superseded_boundary_translates_to_nothing`
  - `entries_retained_across_a_boundary_are_sent_raw`
- [ ] **Step 2: Run** `cargo test -p horsie-server --lib prompt_messages` — expect compile failure (`last_boundary` undefined).
- [ ] **Step 3: Implement.** `prompt_messages()` finds the last `Compaction` by reverse scan, pushes `boundary_message(entry)` as a `Role::User` message whose body is the two labelled sections, then filters entries at `seq >= retained_from_seq`. The `Compaction` arm inside the filter yields `None`.
- [ ] **Step 4: Fix the other non-exhaustive matches** the Task 1 build surfaced — `LogPage::messages`, `has_run`, `hook_entry_count`, and any web-facing mapper. Each must state deliberately what a boundary means to it.
- [ ] **Step 5: Run** `cargo test -p horsie-server --lib` — all green.
- [ ] **Step 6: Commit** `feat(agent): prompt from the newest compaction boundary`.

### Task 3: The `CompactionPolicy` seam and the cut

**Files:**
- Create: `crates/agentcore/src/compaction.rs`
- Modify: `crates/agentcore/src/lib.rs`, `crates/agentcore/src/agent.rs`

**Interfaces:**
- Produces: `trait CompactionPolicy { async fn carried_state(&self) -> String; async fn before(&self, plan: &CompactionPlan) -> PreCompactDecision; async fn after(&self, result: &CompactionResult); }`; `struct CompactionPlan { covers_through: usize, retained_from: usize, tokens_before: u32, instructions: Option<String> }`; `enum PreCompactDecision { Proceed, Abandon(String) }`; `fn choose_cut(history: &[Message], retain_budget_tokens: u32) -> usize`.
- Consumes: `Message`, `ContentPart` from `horsie_models::agent`.

- [ ] **Step 1: Write the failing tests** for `choose_cut` — the pure function, so it gets isolated tests the way `agent_log.rs` does:
  - `the_cut_lands_on_a_user_message_boundary`
  - `the_cut_never_separates_an_assistant_message_from_its_tool_results`
  - `one_turn_larger_than_the_budget_retains_nothing`
  - `an_empty_history_cuts_at_zero`
- [ ] **Step 2: Run** `cargo test -p horsie-agentcore --lib choose_cut` — expect FAIL, undefined.
- [ ] **Step 3: Implement** `choose_cut`: walk backwards accumulating an approximate token count, stop once the budget is met, then walk further back to the nearest `Role::User` message that is not a tool result. Return 0 when no safe cut exists.
- [ ] **Step 4: Run** — green.
- [ ] **Step 5: Commit** `feat(agentcore): choose a safe compaction cut`.

### Task 4: `Agent::maybe_compact` and `Agent::compact`

**Files:**
- Modify: `crates/agentcore/src/agent.rs` (`Agent`, `AgentBuilder`, `AgentConfig`, `run_inner`)
- Modify: `crates/agentcore/src/compaction.rs`

**Interfaces:**
- Consumes: `choose_cut`, `CompactionPolicy` from Task 3.
- Produces: `AgentConfig.compaction: Option<CompactionBudget>` where `CompactionBudget { context_window: u32, trigger_at_percent: u32, retain_percent: u32 }`; `AgentBuilder::with_compaction(policy: Arc<dyn CompactionPolicy>)`; `Agent::compact(&mut self, instructions: Option<String>, events: &dyn EventSink) -> Result<(), AgentError>`; `Agent::seed_context_tokens(u32)`.

- [ ] **Step 1: Write the failing tests** using the existing `testkit::script` scripted provider:
  - `a_run_under_the_threshold_never_compacts`
  - `crossing_the_threshold_compacts_at_the_top_of_the_next_iteration`
  - `the_history_after_a_compaction_is_balanced`
  - `a_failing_summarizer_leaves_the_run_untouched`
  - `a_compaction_emits_its_boundary_event_exactly_once`
  - `an_agent_with_no_policy_never_compacts`
- [ ] **Step 2: Run** `cargo test -p horsie-agentcore --lib compact` — expect FAIL.
- [ ] **Step 3: Implement.** `maybe_compact` returns early unless a policy, a budget and `context_tokens >= window * trigger / 100` are all present. `compact()` does the seven steps from the spec, summarizing via `self.provider.complete()` with an empty toolbox and a null `EventSink` so the call never reaches the transcript.
- [ ] **Step 4: Call it** from the top of `run_inner`'s loop, after the `max_iterations` check. Seed `spent.context_tokens` from `Agent::seed_context_tokens` at run start.
- [ ] **Step 5: Run** `cargo test -p horsie-agentcore --lib` — green.
- [ ] **Step 6: Commit** `feat(agentcore): compact when the context budget is crossed`.

### Task 5: Fold the event, render carried state

**Files:**
- Modify: `crates/server/src/agent_loop/agent_actor.rs` (`AgentDomainEvent`, `apply_event`, `coarse_event`, the run task)
- Modify: `crates/server/src/agent_loop/task_list.rs` (expose `render` if not already `pub`)
- Create: `crates/server/src/agent_loop/carried_state.rs`

**Interfaces:**
- Produces: `AgentDomainEvent::Compacted { entry, at_ms }`; `fn render_carried_state(state: &AgentState) -> String`; `struct ActorCompactionPolicy` implementing `CompactionPolicy`.

- [ ] **Step 1: Write the failing test** `carried_state_names_every_task_timer_and_ask_verbatim` in `carried_state.rs` — build an `AgentState` with three tasks, two timers, one pending ask and a working-directory override, then assert every id and the path appear literally in the rendered block.
- [ ] **Step 2: Run** `cargo test -p horsie-server --lib carried_state` — expect FAIL.
- [ ] **Step 3: Implement** `render_carried_state`, reusing `TaskListState::render()`.
- [ ] **Step 4: Add the fold.** `AgentDomainEvent::Compacted` → `state.push(at_ms, AgentLogBody::Compaction(entry))`. Map it in `coarse_event`. Add the arm to the exhaustive `AgentEvent` match there.
- [ ] **Step 5: Write the failing test** `a_compaction_folds_at_the_next_seq` and `a_snapshot_after_a_compaction_recovers_the_same_prompt`.
- [ ] **Step 6: Implement `ActorCompactionPolicy`** — `carried_state()` asks the actor; `before`/`after` are no-ops in this PR (hooks land in PR 2).
- [ ] **Step 7: Run** `cargo test -p horsie-server --lib` — green.
- [ ] **Step 8: Commit** `feat(server): fold a compaction boundary and carry exact state`.

### Task 6: Settings and wiring

**Files:**
- Modify: `crates/models/fluorite/session.fl` (`AgentSettings`), `crates/models/fluorite/agents.fl` (`AgentPresetInput`)
- Modify: `crates/server/src/agent_loop/context.rs` (`Contexts`)
- Modify: `crates/server/src/sessions/session_actor/context.rs` (`provide`)
- Modify: `crates/server/src/agent_loop/agent_actor.rs` (run task: build the budget)
- Modify: `crates/server/src/agents/service.rs`, `crates/server/src/sessions/*` (persist and thread the flag)

**Interfaces:**
- Produces: `AgentSettings.auto_compact: Option<bool>`, `AgentPresetInput.auto_compact: Option<bool>`, `Contexts.context_window: Option<u32>`.

- [ ] **Step 1: Add the fields** to both `.fl` files; `fluorite clean && make types`.
- [ ] **Step 2: Add a DB migration** for the preset column, per-dialect (SQLite rebuild + Postgres `ALTER`), following the existing migration pattern.
- [ ] **Step 3: Write the failing test** `a_session_created_with_auto_compact_off_never_compacts` and `a_model_card_without_a_context_window_disables_auto_compaction`.
- [ ] **Step 4: Resolve `context_window`** in `SessionContextProvider::provide` from the config store — the same lookup `handlers.rs:350` does — and carry it on `Contexts`.
- [ ] **Step 5: Build the budget** in the run task: `Some(CompactionBudget { .. })` only when `auto_compact != Some(false)` **and** a `context_window` is known. Attach `ActorCompactionPolicy` via `with_compaction`.
- [ ] **Step 6: Run** `cargo test -p horsie-server --lib` then the full workspace suite once.
- [ ] **Step 7: Commit** `feat(sessions): an auto_compact setting on sessions and presets`.

### Task 7: PR 1 integration test and docs

**Files:**
- Create: `crates/tests/tests/compaction_e2e.rs`
- Modify: `docs/src/content/docs/internals/context-and-memory.md`

- [ ] **Step 1: Write the e2e** — a session against the mock LLM with a tiny model-card window: run turns until it compacts, assert `/history` still serves the pre-compaction entries, and assert the boundary appears exactly once.
- [ ] **Step 2: Run** `TMPDIR=/tmp cargo test -p horsie-tests compaction`.
- [ ] **Step 3: Rewrite** the doc's "Keep less: compaction" section — it currently describes only journal snapshotting — and delete the "no automatic mid-run summarisation" claim from "What horsie does not do".
- [ ] **Step 4: Full workspace suite once**, then push and open PR 1.

---

## PR 2 — `/compact` and the hooks

Branch `feat/compaction-command`, based on `feat/compaction`.

### Task 8: The builtin command registry

**Files:**
- Create: `crates/support/src/plugin/builtins.rs`
- Modify: `crates/support/src/plugin/mod.rs`, `crates/server/src/sessions/session_actor/context.rs` (`expand_invocation`)
- Modify: `crates/server/src/http/handlers.rs` (catalogue endpoint)

**Interfaces:**
- Produces: `struct Builtin { name: &'static str, description: &'static str }`, `const BUILTINS: &[Builtin]`, `fn builtin(name: &str) -> Option<&'static Builtin>`, `fn catalogue_entries() -> Vec<CatalogEntryView>`.

- [ ] **Step 1: Write the failing tests** — `a_builtin_is_offered_when_no_plugins_are_selected`, `a_bundle_cannot_shadow_a_builtin`, `an_unknown_slash_word_is_still_sent_verbatim`.
- [ ] **Step 2: Run** — expect FAIL.
- [ ] **Step 3: Implement** the registry; merge into the catalogue endpoint unconditionally (not gated on `use_plugins`); consult it ahead of the plugin catalogue in `expand_invocation`.
- [ ] **Step 4: Run** — green. **Commit** `feat(plugins): a registry of builtin slash commands`.

### Task 9: `Incoming::Compact`

**Files:**
- Modify: `crates/server/src/agent_loop/inbox.rs`, `crates/server/src/agent_loop/agent_actor.rs`

- [ ] **Step 1: Write the failing tests** — `a_compact_command_queues_rather_than_prompting`, `a_turn_in_flight_finishes_before_a_queued_compaction`, `compact_instructions_reach_the_summarizer`, `a_compact_on_an_empty_log_is_a_no_op`.
- [ ] **Step 2: Run** — expect FAIL.
- [ ] **Step 3: Implement** the `Incoming::Compact { id, instructions }` variant, its `id()` arm, and the turn-boundary branch that builds an `Agent` and calls `compact()` instead of `run()`.
- [ ] **Step 4: Run** — green. **Commit** `feat(sessions): a queued /compact command`.

### Task 10: PreCompact and PostCompact

**Files:**
- Modify: `crates/models/fluorite/runtime.fl`, `crates/support/src/plugin/hooks/events.rs`, `crates/server/src/agent_loop/carried_state.rs` (`ActorCompactionPolicy`), `crates/server/src/agent_loop/hook_translation.rs`

- [ ] **Step 1: Write the failing tests** — `a_precompact_hook_that_blocks_abandons_the_compaction`, `a_postcompact_hook_sees_the_boundary`, `hook_records_reach_the_transcript`.
- [ ] **Step 2: Run** — expect FAIL.
- [ ] **Step 3: Implement** — add `PreCompactInput`/`PostCompactInput`, move both off `NoConcept` in `events.rs`, fire them from `ActorCompactionPolicy::before`/`after`, and add their arms to `hook_translation`'s exhaustive match. Update the `SessionStartSource::Compact` comment to say why it stays unconstructed.
- [ ] **Step 4: Run** the full workspace suite once. **Commit** `feat(hooks): fire PreCompact and PostCompact`, push, open PR 2.

---

## PR 3 — The UI

Branch `feat/compaction-ui`, based on `feat/compaction-command`.

### Task 11: The transcript divider

**Files:**
- Modify: `clients/web/src/lib/transcriptSegments.ts`, `clients/web/src/components/Transcript.tsx`
- Create: `clients/web/src/components/CompactionDivider.tsx`
- Test: `clients/web/src/lib/transcriptSegments.test.ts`

- [ ] **Step 1: Write the failing test** — `a compaction entry becomes its own segment`, `messages on both sides of a boundary still render`.
- [ ] **Step 2: Run** `bun run test transcriptSegments` — expect FAIL.
- [ ] **Step 3: Implement** the segment kind and the divider component (rule, centred label, expandable summary + carried state).
- [ ] **Step 4: Run** — green. **Commit** `feat(web): render a compaction boundary in the transcript`.

### Task 12: The spine

**Files:**
- Create: `clients/web/src/components/TranscriptSpine.tsx`, `clients/web/src/components/TranscriptSpine.test.tsx`
- Modify: `clients/web/src/pages/SessionPage.tsx` (or wherever the transcript is mounted)

- [ ] **Step 1: Write the failing test** — `renders two caps and no ticks with no compactions`, `renders one tick per boundary`, `clicking a tick scrolls to that boundary`.
- [ ] **Step 2: Run** — expect FAIL.
- [ ] **Step 3: Implement** the spine: caps, proportional ticks, brighter current span, hover label.
- [ ] **Step 4: Run** — green. **Commit** `feat(web): a spine for seeking across compactions`.

### Task 13: Gauge tick and the forms

**Files:**
- Modify: `clients/web/src/components/ContextGauge.tsx`, the session-create form, `clients/web/src/pages/...` agent-preset form

- [ ] **Step 1: Write the failing tests** — `the gauge marks the compaction threshold`, `the auto-compact checkbox is disabled without a context window`.
- [ ] **Step 2: Run** — expect FAIL.
- [ ] **Step 3: Implement** the threshold tick and the checkbox in both forms.
- [ ] **Step 4: Run** `bun run test && bunx tsc --noEmit -p tsconfig.json` (note: a bare `tsc --noEmit` is a no-op here).
- [ ] **Step 5: Commit** `feat(web): expose the auto-compact setting and threshold`.

### Task 14: e2e and stack

**Files:**
- Create/modify: `clients/web/e2e/compaction.spec.ts`

- [ ] **Step 1: Write the e2e** — drive a session to a compaction against the mock LLM, assert the divider renders, the pre-compaction messages are still reachable, and the spine shows one tick.
- [ ] **Step 2: Run** `TMPDIR=/tmp bunx playwright test compaction`.
- [ ] **Step 3: Full workspace suite once**, push, open PR 3, and link the three with `gh stack` — a squash-only stack merged one PR at a time breaks every downstream PR.
