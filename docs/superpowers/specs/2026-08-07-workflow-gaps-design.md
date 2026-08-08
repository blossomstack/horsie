# Closing the workflow gaps

A review of workflow support found six defects and seven gaps against what the
guide already promises. This is the design for fixing all of them.

Workflows shipped in PRs #183/#184/#188/#190 with the UI redesign in #202. The
shape is right — a run is a session, the driver is pure, the log is append-only
— and nothing here changes it. What follows is the work that was left undone,
plus one family of bugs that comes from code written before runs existed and
never taught about them.

## The root cause behind four of the six

`SessionActor::resolve_agent` answers "which agent does this request mean?" for
a session with a main agent. A run has none. Every path through it therefore
either resolves the wrong agent or none at all, and four separate user-visible
defects fall out of that one omission.

### Run-aware resolution

`resolve_agent` and `reach` gain the case they are missing: an agent id that the
run log names resolves to that step's agent, spawning its actor on demand
through the existing `spawn_step_agent` — the step name comes from
`WorkflowRunState::index_of_agent`. It returns `AgentKey::Step`, not
`AgentKey::Sub`, so the key describes what the agent is.

An *unaddressed* request on a run resolves to the step in flight
(`WorkflowRunState::current_agent`). Nothing else could be meant: a run has no
main agent and exactly one step runs at a time.

Two defects close directly:

**A cold step's transcript is unreadable.** `ReadLog`, `PageLog` and
`AgentState` all resolve through `resolve_agent`. A reloaded run holds an empty
`SessionAgents::workflow()` roster, and `reach` explicitly refused to spawn a
step, so every agent-scoped read answered empty once the session had unloaded —
for a finished run, permanently. The guide sends the reader to the step page for
the transcript, so this was the documented path failing.

**Answering a parked step does nothing.** The web client posts
`/sessions/:id/answers` with no `aid`, so the server resolved `main` and replied
`NothingPending` while the run sat in `AwaitingInput` forever. Both halves are
fixed: the client sends the agent id it is scoped to, and an unaddressed answer
resolves to the step in flight regardless.

### One rule for a step that stops without concluding

**The step is `Cancelled` and the run is `Suspended`.** That is already what a
retry does to the step it supersedes, and `Suspended` is defined as the state a
retry can move. Two paths never learned it:

**Interrupt.** `TurnCommand::Stop` cancelled the step agent — `cancel_in_flight`
is run-aware — and then journaled a bare `TurnStopped`, which sets the session
`Idle` and says nothing about the run. `run.status` stayed `Running` with its
step entry still `Running`, so `step_actions` returned nothing for ever after:
the run was wedged while its page claimed it was working. Stop on a run now
emits `StepCancelled` for the step in flight instead. Its gate also changes from
`status == Running` to "a step is in flight", so interrupting a parked run works
— which is what the run page's always-enabled Interrupt button implies.

**Recovery.** `Turns::on_load` fires `ReconcileInterrupted` for any session left
`Running`, which journals `TurnInterrupted` and, again, says nothing about the
run. `WorkflowRun::on_load` only advanced a run that was absent or `Pending`, so
a run interrupted mid-step had nothing repair it. A new
`RunCommand::ReconcileInterrupted` fires from `WorkflowRun::on_load` when
recovery finds a step still `Running`, and `Turns::on_load` stops firing for a
run — each component repairs itself, and for a run the step is the truth.

`docs/guide/workflows.md` already promises exactly this behaviour ("the step
that was running is marked interrupted and the run is suspended rather than
resumed"). The guide was written against the design, not the code.

## The remaining two defects

**Every tool call in a workflow step carries the call id `"tc1"`.**
`StepConcludeToolbox::execute` ignores its `tool_call_id` parameter and passes a
literal to the toolbox it wraps. That id is the runtime's correlation key
(`RuntimeClient::invoke` → `ToolCallRequest.call_id`) and the key of the
in-flight set that cancellation walks. Every step with an output schema — which
is every step a condition branches on — is wrapped by this toolbox, and the
agent loop executes a turn's tool calls concurrently. Two parallel calls
therefore share one id: replies can correlate to the wrong call, and
`cancel_in_flight` reaches only one of them. The fix is to forward the
parameter, guarded by a test that asserts the inner toolbox sees the real id.

**Editing a workflow in the browser wipes its per-step budgets.** `fromDraft`
hardcodes `maxIterations` and `maxRetries` to `undefined` and `toDraft` never
reads them, while `PUT` is a full replace — so any budget set through the API is
destroyed by the next save from the editor. `StepDraft` carries both verbatim,
the way `rawSchema` already survives a round-trip it cannot render. No new form
controls: the defect is data loss, not a missing knob.

## Gaps against the documented product

**Per-step token counts are always zero.** `step_run_view` hardcodes them.
Usage is journaled per agent as `UsageRecorded { agent_id, usage_total }` and a
step's `agent_id` is on its `StepRun`, so the data is already there and keyed
correctly. `SessionUsageStats` gains a per-agent map and `project_run` looks
each step up. Usage banks at turn end, so a step in flight reads zero until it
concludes — the run total already behaves that way, so this adds no new
surprise.

**A finished run never shows its output.** `WorkflowRunGraph.output` is
populated by the server and rendered by nobody: the run page shows only `error`,
and `horsie workflow status` shows neither. The run's actual result is reachable
only by opening its last step. Both surfaces now show it.

**A parked run offers no way to answer.** The run page shows the status
"Awaiting input" and nothing to act on. The run page stays graph-only — it is
deliberately built without a transcript, and a second transcript-shaped surface
does not belong on it. Instead it gains a banner naming the waiting step and
linking to it, and the guide's claim that "the question appears on the run's
page" is corrected to say where it actually appears. Answering there works once
the resolution fix above lands.

**A step's own page misreports its model and its status.** `get_agent` takes
`context_window` from `rec.spec.agent.model`, which for a run is the *first*
step's model, so a step on a different preset shows the wrong window. And
`status` defaults to `"running"` for anything absent from the subagent tree —
which is every step, for ever, including concluded ones. Both now come from the
run: the step spec's own model, and the step's `StepStatus`.

**The step budget is fixed at 100.** `WorkflowRunSpec.max_steps` is always the
constant and no API sets it. It moves onto the definition, where the issue
argued it belongs — the budget is a property of the graph's shape, and a
workflow that legitimately loops twenty times knows that about itself.
`max_steps: Option<u32>` on `WorkflowInput`/`WorkflowView`, a `0027` migration
in both dialects, validation that it is at least 1, a field in the definition
form, and `start_run` using it or falling back to the constant. Both generated
TypeScript trees are regenerated.

**The CLI cannot retry a step or manage a definition.** `horsie workflow` has
list/get/run/status; the API has retry and full CRUD. It gains `retry
<session-id> <index>`, and definition management as a JSON round-trip — `apply
-f <file>` (create or replace) and `delete <name>`, with `get --json` as the way
out. Deliberately not a new definition DSL: the API's own JSON is a format that
already exists and cannot drift.

**There is no regression coverage for any of this.** Each fix carries its guard
rather than deferring to a test-only change: actor tests that answer a parked
step, interrupt a running run, and recover an interrupted one; an HTTP
end-to-end test in `session_server_e2e.rs` that creates a workflow, runs it to
completion, retries a step and reads the graph — asserting against baselines,
because that suite is serial against one long-lived server; and the Playwright
run test extended to wait for `Finished` and assert the output renders.

## What is deliberately untouched

- **Parallel steps (#199).** A run is a walk through the graph, not a frontier.
  Filed as a deliberate omission and it stays one.
- **Environments on a run request (#198).** The issue itself argues this is
  better framed as "sessions can be created from an environment", with workflows
  as one caller. Not a workflow change.
- **Routines triggering workflows (#200).** Needs a decision about whose
  visibility rule wins; not planned.
- **The plugin union (#182).** An accepted consequence of one shared runtime,
  documented in the guide.
- **`WorkflowService::delete`'s stale comment** claims the caller refuses while
  a run is active. It does not, and it should not — a run snapshots its graph and
  survives the definition. The comment is corrected to say so.

## Shape of the work

Five PRs, merged as a stack:

1. Run-aware resolution and the interruption rule, with their actor tests.
2. The call-id passthrough, the editor's data loss, per-step tokens, and the
   step page's model and status.
3. The run's output on both surfaces, the parked-run banner, and the guide.
4. The per-definition step budget: wire, migration, form, both type trees.
5. The CLI's retry and definition management.

The HTTP end-to-end test lands with (1), where the behaviour it guards is.

## What building it turned up

Three things this design did not anticipate, recorded because the reasoning
matters more than the diffs.

**The browser had no answer box for a parked step at all.** This design treated
answering as one server-side defect plus a missing `aid` on the request, and
scoped the run page's job to *getting you to* the step. But `ToolCallCard`
dispatched to the answer card on `call.name === "ask_user"`, and a step never
gets `ask_user` — it asks through `conclude`. The question rendered as a
collapsed generic tool row with no input and no send button, so fixing the server
would have left a parked run unblockable from the browser and pointed a new
banner at a page that could not answer it. Matched by shape now, not by tool
name. The lesson generalises: **anything keyed on the ask tool's name is wrong
for a run**, because a run's terminal tool is a different one.

**Nothing ever cleared `AwaitingInput`.** Found while writing the test for the
answer fix: `apply_awaiting` set the status and no fold ever moved it off. So even
a delivered answer would have resumed the step and then stalled the run at the
step it had just finished — a third defect hiding behind the second, invisible
because answering had never worked at all. `apply_concluded` now reasserts
`Running`, which is what the run is between steps.

**CI gave every stacked PR but the bottom one zero checks.** `ci.yml` gated
`pull_request` on `branches: [main]`, and a stacked PR targets its parent — so
four of these five were unverifiable, and the widened trigger immediately caught
a real break (a `WorkflowView` field added in PR 4 whose fixtures were only fixed
in PR 5, so PR 4 alone did not compile). Fixed in PR 1 as `branches: ["**"]`.

The second one generalises: adding a field to a struct shared across crates
breaks fixtures in crates the change never touches, and `cargo clippy -p
horsie-server` cannot see them. The workspace-wide lint is the gate.
