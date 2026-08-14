# Workflow step contract — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the workflow step's raw JSON Schema, `eval` transitions and the
overloaded `conclude` tool with a declared result contract (`outcome` +
`description` + typed fields), transitions that filter on `outcome`, and a
`submit_result` / `ask_user` pair of tools that end a run by returning
`ToolOutcome::StopRun` from their own `execute`.

**Architecture:** A tool declares itself terminal by returning `StopRun` when
dispatched; agentcore returns `AgentResult::Stopped { calls }` and knows nothing
about what those tools mean. The agent actor interprets by tool name, and owns
the park-versus-stuck decision from its own state (timers, outstanding children,
queue). A step ends only on `submit_result`; every other turn ending is a park or
a nudge.

**Tech Stack:** Rust (workspace crates `agentcore`, `server`, `models`, `cli`,
`tests`), fluorite codegen (`make types`), React + Vitest + Playwright
(`clients/web`), sqlx migrations.

**Spec:** `docs/superpowers/specs/2026-08-14-workflow-step-contract-design.md`

## Global Constraints

- Branch `feat/workflow-step-contract`, worktree
  `.claude/worktrees/step-contract`. Four stacked PRs via `gh stack`.
- No backward compatibility anywhere: old workflow definitions and
  workflow-origin sessions are deleted by migration, not converted.
- `.fl` changes require `make types`; generated TypeScript is committed.
- The wire is camelCase; hand-written JSON in tests must match generated types.
- Verify with `cargo test -p <crate> --lib` while iterating; full
  `cargo test --workspace` once before pushing each PR.
- `cargo fmt` before `cargo clippy` — clippy reports formatting-sensitive lints.
- Never journal a tool result for a `StopRun` call.

---

## PR 1 — agentcore: a tool that ends the run

### Task 1.1: `ToolOutcome` and the `Toolbox` signature

**Files:**
- Modify: `crates/agentcore/src/tool.rs`
- Modify: `crates/agentcore/src/lib.rs` (re-export)
- Modify: all 23 `impl Toolbox for` sites (13 files, listed in Task 1.4)

**Produces:**
```rust
pub enum ToolOutcome { Result(Value), StopRun }
impl From<Value> for ToolOutcome { fn from(v: Value) -> Self { Self::Result(v) } }
async fn execute(&self, name: &str, input: Value, tool_call_id: &str)
    -> Result<ToolOutcome, ToolCallError>;
```
`Tool::execute` keeps returning `Result<Value, ToolCallError>` — individual
tools registered in `ToolboxImpl` are never terminal; `ToolboxImpl` wraps their
value in `ToolOutcome::Result`.

- [ ] **Step 1:** Write failing test in `tool.rs`:
  `toolbox_impl_wraps_a_tool_result` asserts
  `matches!(tb.execute("echo", json!({"x":1}), "tc1").await.unwrap(), ToolOutcome::Result(v) if v == json!({"x":1}))`.
- [ ] **Step 2:** `cargo test -p horsie-agentcore --lib tool::` — fails to compile.
- [ ] **Step 3:** Add `ToolOutcome`, `From<Value>`, change the trait, update
  `ToolboxImpl` and `EmptyToolbox`.
- [ ] **Step 4:** `cargo test -p horsie-agentcore --lib tool::` — passes.
- [ ] **Step 5:** Commit `feat(agentcore)!: a tool can end the run`.

### Task 1.2: the loop collects stopped calls

**Files:**
- Modify: `crates/agentcore/src/agent.rs` (`execute_tool_calls` ~751, the run
  loop ~545-660, `AgentResult`)
- Modify: `crates/agentcore/src/lib.rs`

**Produces:**
```rust
pub struct StoppedCall { pub tool: String, pub tool_call_id: String, pub input: Value }
// AgentResult::Handoff(HandoffOutput) -> AgentResult::Stopped { calls: Vec<StoppedCall> }
```
`execute_tool_calls` returns `(Vec<Message>, Vec<StoppedCall>)`. A `StopRun`
emits **no** `ToolComplete` event and produces **no** `Message`.

- [ ] **Step 1:** Tests in `agent.rs`:
  - `a_stopping_tool_ends_the_run_and_reports_its_call` — mock toolbox returns
    `StopRun` for `"finish"`; assert `AgentResult::Stopped` with tool, id, input.
  - `a_stopping_call_records_no_tool_result` — assert `history_for_test()` has no
    `ToolResult` part for that call id, and the sink saw no `ToolComplete`.
  - `siblings_execute_before_the_run_stops` — turn calls `echo` + `finish`;
    assert the `echo` result is in history and the run stopped.
  - `several_stopping_calls_are_all_returned` — two `ask` calls, both in `calls`.
  - `plain_text_still_completes_the_run` — no nudge, `AgentResult::Completed`.
  - `an_invalid_input_error_from_a_stopping_tool_is_an_ordinary_tool_error` —
    the model sees an error tool result and the run continues.
- [ ] **Step 2:** Run them — fail.
- [ ] **Step 3:** Implement: `execute_tool_calls` splits outcomes; the loop
  returns `Stopped` when the batch produced any.
- [ ] **Step 4:** Run — pass.
- [ ] **Step 5:** Commit `feat(agentcore)!: return the calls that ended a run`.

### Task 1.3: delete the handoff machinery

**Files:**
- Modify: `crates/agentcore/src/agent.rs`, `crates/agentcore/src/error.rs`

Delete: `handoff_tool`, `force_handoff_choice`, `with_handoff_tool`,
`with_handoff_tool_optional`, `handoff_validator`, `validate_handoff`,
`HandoffCall`, `HandoffOutput`, `AgentBuildError::{HandoffToolNotRegistered,
InvalidHandoffSchema}`, `AgentError::HandoffValidationFailed`, the forced
`tool_choice` branch (always `Auto` unless the caller overrides), and the
text-ending nudge. Keep `handoff_max_retries` renamed to
`tool_error_max_retries` only if it still has a consumer; otherwise delete.

**Produces:** `AgentBuilder::with_tool_choice(ToolChoice)` — the per-run
override the actor uses for the escalated nudge. Default `Auto`.

- [ ] **Step 1:** Test `a_forced_tool_choice_is_sent_to_the_provider`:
  build with `with_tool_choice(ToolChoice::Required("submit_result".into()))`,
  assert the mock provider saw it.
- [ ] **Step 2:** Run — fails.
- [ ] **Step 3:** Delete the machinery; add `with_tool_choice`; delete the tests
  that only exercised forcing (`two_calls_to_a_forced_handoff_tool_are_rejected`,
  `optional_handoff_tool_*`, `build_fails_when_handoff_tool_missing`,
  `handoff_tool_returns_handoff_result` — replaced by Task 1.2's tests).
- [ ] **Step 4:** `cargo test -p horsie-agentcore` — passes.
- [ ] **Step 5:** Commit `refactor(agentcore)!: delete the handoff tool config`.

### Task 1.4: update every `Toolbox` impl

**Files (13):** `crates/agentcore/src/{tool.rs,agent.rs,testkit/mod.rs}`,
`crates/server/src/memory/toolbox.rs`,
`crates/server/src/agent_loop/{mcp_toolbox.rs,agent_actor.rs,context.rs}`,
`crates/server/src/sessions/{title_tool.rs,spawn_tool.rs,ask_tool.rs,workflow/toolbox.rs}`,
`crates/tests/tests/{agent_recovery_e2e.rs,agent_e2e.rs}`

- [ ] **Step 1:** Change each signature; wrap returns with
  `ToolOutcome::Result` (or `.into()`); forwarding wrappers pass the outcome
  through untouched.
- [ ] **Step 2:** `cargo build --workspace --tests` — clean.
- [ ] **Step 3:** `cargo test --workspace` — green.
- [ ] **Step 4:** `cargo fmt && cargo clippy --workspace --all-targets` — clean.
- [ ] **Step 5:** Commit `refactor: thread ToolOutcome through every toolbox`.
- [ ] **Step 6:** Push, open PR 1.

---

## PR 2 — the step result contract

### Task 2.1: wire types

**Files:**
- Modify: `crates/models/fluorite/workflow.fl`
- Run: `make types`

Add `StepOutcome`, `StepFieldType`, `StepField`; on `WorkflowStepDef` replace
`output_schema` with `outcomes`, `fields`, `interactive`. (Transitions change in
PR 3 — leave `condition` alone here.)

- [ ] **Step 1:** Edit `.fl`, run `make types`, commit generated output with the
      source in one commit (`feat(models)!: a step declares its result`).

### Task 2.2: compile the result schema

**Files:**
- Create: `crates/server/src/sessions/workflow/result_schema.rs`
- Modify: `crates/server/src/sessions/workflow/mod.rs`

**Produces:**
```rust
pub const SUBMIT_RESULT_TOOL: &str = "submit_result";
pub fn result_schema(outcomes: &[StepOutcome], fields: &[StepField]) -> Value;
pub fn validate_result(value: &Value, outcomes: &[StepOutcome], fields: &[StepField])
    -> Result<(), String>;
pub fn default_outcomes() -> Vec<StepOutcome>;  // success / failure
pub fn render_result(value: &Value) -> String;  // markdown for the next step
```

- [ ] **Step 1:** Tests: `outcome_is_a_required_enum_carrying_each_values_meaning`,
  `description_is_required_and_documented_by_horsie`,
  `a_declared_field_keeps_its_type_and_description`,
  `an_optional_field_is_not_required`,
  `validate_rejects_an_outcome_outside_the_enum`,
  `validate_rejects_a_missing_required_field`,
  `validate_accepts_an_absent_optional_field`,
  `render_puts_the_description_first_then_the_fields`.
- [ ] **Step 2:** Run — fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run — pass. Commit `feat(workflows): compile a step's result schema`.

### Task 2.3: the `submit_result` tool

**Files:**
- Rewrite: `crates/server/src/sessions/workflow/toolbox.rs`
  (`StepConcludeToolbox` → `StepResultToolbox`)

`specs()` appends `submit_result` built from `result_schema`. `execute` for
`submit_result` validates via `validate_result` and returns
`Ok(ToolOutcome::StopRun)`, or `Err(ToolCallError::InvalidInput(reason))`.
Everything else forwards with the real `tool_call_id`.

- [ ] **Step 1:** Tests: `a_step_advertises_submit_result`,
  `submit_result_stops_the_run`, `an_invalid_outcome_is_an_input_error`,
  `the_wrapped_toolbox_sees_the_real_call_id`.
- [ ] **Step 2-4:** Red, implement, green.
- [ ] **Step 5:** Commit `feat(workflows): the submit_result tool`.

### Task 2.4: `ask_user` returns `StopRun`; delete `conclude`

**Files:**
- Modify: `crates/server/src/sessions/ask_tool.rs`
- Modify: `crates/server/src/agent_loop/context.rs` (delete `CONCLUDE_TOOL`,
  `conclude_tool_spec`, `ask_schema`, `both_schema`, `timers_kind_schema`,
  `AgentToolbox.conclude`, `AgentRunDef::{output_schema, allow_ask_user,
  allow_timers}`)

- [ ] **Step 1:** Test `ask_user_stops_the_run` replaces
  `ask_user_is_not_executable`.
- [ ] **Step 2-4:** Red, implement, green.
- [ ] **Step 5:** Commit `refactor!: delete the conclude tool`.

### Task 2.5: timers for every agent

**Files:**
- Modify: `crates/server/src/agent_loop/agent_actor.rs` (`TimerToolbox` layered
  unconditionally, `AgentParams::allow_timers` deleted)

- [ ] **Step 1:** Test `every_agent_gets_the_timer_tools`.
- [ ] **Step 2-4:** Red, implement, green. Commit `feat(agents): timers for every agent`.

### Task 2.6: interpret by name; park; nudge

**Files:**
- Modify: `crates/server/src/agent_loop/agent_actor.rs`
  (`interpret`, `handle_finished`, `AgentState`, `park_or_resume`)

**Produces:** `AgentState.outstanding_children: BTreeSet<Uuid>`, folded from the
`spawn_agent` tool result and the child's report; `AgentState.nudges: u32`.

Rules:
- `Stopped` with only `ask_user` calls → `Conclusion::Ask` (all of them).
- `Stopped` with exactly one `submit_result` and nothing else →
  `Conclusion::Output(input)`; cancel any armed timers.
- `Stopped` mixed → corrective turn: inject error tool results for those call
  ids, count a nudge.
- `Completed { text }` → if the drain started a turn, nothing; else if timers
  armed or `outstanding_children` non-empty → `park_or_resume`; else nudge:
  first with `ToolChoice::Auto`, second with
  `ToolChoice::Required("submit_result")`, third → `AgentOutcome::Failed`.

- [ ] **Step 1:** Tests (one per row of the spec's decision table, names as in
  the spec's test matrix).
- [ ] **Step 2-4:** Red, implement, green.
- [ ] **Step 5:** Commit `feat(agents): a turn ending is not a step ending`.

### Task 2.7: recovery keyed on pending asks

**Files:**
- Modify: `crates/server/src/agent_loop/agent_actor.rs:1817`,
  `crates/agentcore/src/...` wherever `missing_tool_results` lives

- [ ] **Step 1:** Test `a_dangling_ask_is_exempt_but_a_dangling_bash_is_repaired`.
- [ ] **Step 2-4:** Red, implement, green. Commit `fix(agents): exempt the calls actually parked on`.

### Task 2.8: step spawn, prompt, migration

**Files:**
- Modify: `crates/server/src/sessions/session_actor/run.rs` (`spawn_step_agent`),
  `crates/server/src/sessions/session_actor/context.rs` (`STEP_PROMPT_SUFFIX`,
  `SessionContextProvider.step_output_schema` → the step's outcomes/fields),
  `crates/server/src/sessions/workflow/spec.rs` (`WorkflowStepSpec`)
- Create: `crates/server/migrations/<timestamp>_workflow_step_contract.sql`
  (delete every workflow row and every workflow-origin session)

- [ ] **Step 1:** Update the spec types and spawn path; write the migration.
- [ ] **Step 2:** `cargo test -p horsie-server` — green.
- [ ] **Step 3:** `cargo test --workspace`, `cargo fmt`, `cargo clippy`.
- [ ] **Step 4:** Commit, push, open PR 2.

---

## PR 3 — transitions on `outcome`

### Task 3.1: wire + storage types

**Files:** `crates/models/fluorite/workflow.fl` (+ `make types`),
`crates/server/src/sessions/workflow/spec.rs`

Replace `WorkflowTransition.condition` with `when: Option<OutcomeFilter>`;
`TransitionSpec` likewise.

- [ ] Commit `feat(models)!: transitions filter on outcome`.

### Task 3.2: the driver

**Files:** `crates/server/src/sessions/workflow/driver.rs`,
`crates/server/Cargo.toml` (drop `eval`), `Cargo.toml` (drop the workspace dep)

**Produces:** `pub fn matches(filter: &OutcomeFilter, outcome: &str) -> bool`,
`pub fn next_transition(&[TransitionSpec], outcome: &str) -> Option<(String, Option<String>)>`,
`pub fn render_filter(&OutcomeFilter) -> String` (the `via` label).

Delete `eval_condition` and the `catch_unwind` guard.

- [ ] **Step 1:** Tests: `in_matches`, `in_misses_and_falls_through`,
  `not_in_matches`, `not_in_misses`, `first_match_wins_when_filters_overlap`,
  `a_catch_all_matches_from_any_position`,
  `no_match_finishes_the_run_with_that_steps_result`,
  `a_loop_is_stopped_by_the_step_budget` (kept), plus rewriting the existing
  condition tests.
- [ ] **Step 2-4:** Red, implement, green. Commit `feat(workflows)!: route on the step's outcome`.

### Task 3.3: save-time validation

**Files:** `crates/server/src/workflows/service.rs`, `store.rs` (seed rows)

- [ ] **Step 1:** Tests: `a_filter_naming_an_undeclared_outcome_is_refused`,
  `an_empty_outcome_list_is_refused`, `duplicate_outcome_values_are_refused`,
  `a_field_named_outcome_is_refused`, `a_field_without_a_description_is_refused`,
  `a_catch_all_before_a_filter_is_refused`,
  `a_transition_to_an_unknown_step_is_refused` (kept).
- [ ] **Step 2-4:** Red, implement, green.
- [ ] **Step 5:** `cargo test --workspace`, fmt, clippy. Push, open PR 3.

---

## PR 4 — surfaces

### Task 4.1: web editor

**Files:** `clients/web/src/pages/workflows/{StepForm.tsx,stepDraft.ts,
WorkflowEditPage.tsx}` + their `.test.ts`

Outcome rows (value + description), field rows (name, type, required,
description), an `interactive` toggle, and a transition row that is a target
plus `in`/`not in` plus a multi-select over the producing step's outcomes. The
JSON-schema textarea deletes.

### Task 4.2: run view and ask card

**Files:** `clients/web/src/pages/workflows/WorkflowRunView.tsx`,
`clients/web/src/components/ToolCallCard.tsx`,
`clients/web/src/lib/askUser.ts` (delete `isAskCall` shape sniffing — a step's
question is now an `ask_user` call), `clients/web/src/lib/transcriptSegments.ts`

### Task 4.3: CLI and guide

**Files:** `crates/cli/src/workflow.rs`,
`docs/src/content/docs/using/workflows.md`

### Task 4.4: e2e

**Files:** `clients/web/e2e/t-workflows.spec.ts`,
`crates/tests/tests/session_server_e2e.rs`

- A run whose step asks and is answered through the API.
- A run whose step ends bare, is nudged, and then submits.
- A two-branch graph taking each branch on a different `outcome`.

- [ ] `bun install --frozen-lockfile`, `bun run test`, `TMPDIR=/tmp bunx playwright test`,
      `cargo test --workspace`, fmt, clippy. Push, open PR 4.
