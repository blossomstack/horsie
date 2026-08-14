# The workflow step contract

A workflow step declares what it returns and where each answer leads. Today it
declares a raw JSON Schema and a set of `eval` expressions, finishes through a
tool called `conclude` whose payload means three different things depending on
three flags, and cannot wait for anything — not a person, not a timer, not a
subagent — without going through that same tool. This is the design for
replacing all of it.

Nothing here changes the shape workflows already have: a run is a session, the
driver is pure, the run log is append-only, the definition is snapshotted at run
creation. What changes is what a step promises, how the graph reads that
promise, and how a turn ends.

## What is wrong now

**A step's output is an unconstrained JSON Schema.** Nothing can require a
field to be documented, the editor is a JSON textarea, and the schema's only
consumer — a transition condition — has to be written as an expression over
whatever shape the author happened to choose.

**Transitions are `eval` expressions.** `eval` *panics* on some malformed input
(`!!!` unwinds inside its parser), so every evaluation is wrapped in
`catch_unwind` and every save re-parses the expression to catch it early. A typo
that parses — `output.severty` — still fails the run at the moment it is
evaluated, halfway through, rather than at save.

**One config slot for terminal tools, so one tool had to mean everything.**
`AgentConfig::handoff_tool` names a single tool whose call ends the run.
A step needs two turn-enders (finish, ask), so both were folded into `conclude`
as a `kind`-tagged union, and a third meaning (`park`) was added for timers.
Everything downstream inherits that: `classify_conclusion`'s
`(has_output_schema, allow_ask_user, allow_timers)` matrix, the trap where
allowing an ask silently re-nests the output under `kind`, and a web UI that has
to recognise a step's question by payload *shape* because it is not named
`ask_user`.

**Two sources of truth for which tool is terminal.** The toolbox decides which
tools exist; the config separately names the terminal one. Disagreement is a
build error in one direction (`HandoffToolNotRegistered`) and an infinite loop
in the other — the loop dispatches the tool, the toolbox answers "this tool is
terminal and is not executed", and the model retries until `max_iterations`.
`run.rs:299` carries a five-line comment warning a future reader not to set
`handoff_tool` on a step, because doing so breaks `conclude`.

**Forcing conflates two ideas.** `force_handoff_choice` sets
`tool_choice: Any`, which forbids ending a turn with text. That is the only
thing making a step call `conclude` — and it is also what makes a step unable to
wait on a subagent, because waiting *is* ending a turn without finishing.

## The step result contract

A step declares two things. horsie compiles them into the input schema of the
step's `submit_result` tool.

```
/// One value the step's `outcome` may take.
struct StepOutcome { value: String, description: String }

enum StepFieldType { String, Number, Boolean, StringList }

/// One extra field on the step's result.
struct StepField {
    name: String,
    type: StepFieldType,
    description: String,
    required: Option<bool>,
}

struct WorkflowStepDef {
    name: String,
    agent: String,
    prompt: String,
    /// Values `outcome` may take. Absent → success / failure.
    outcomes: Option<Vec<StepOutcome>>,
    /// Extra result fields, beyond `outcome` and `description`.
    fields: Option<Vec<StepField>>,
    /// Grants this step the `ask_user` tool. Default false.
    interactive: Option<bool>,
    transitions: Option<Vec<WorkflowTransition>>,
    max_iterations: Option<u32>,
    max_retries: Option<u32>,
}
```

Every step's result therefore has two fields it cannot opt out of:

- **`outcome`** — a string enum, the declared values, each value's own
  description folded into the property's documentation so the model knows what
  it is choosing between. This is the only thing transitions read.
- **`description`** — a required markdown summary of what the step did. Its
  schema documentation is horsie's, not the author's: this is the field the next
  step reads, so what it must contain is a property of the system rather than of
  one workflow.

Then whatever the author declares. A field carries its own description because
an undocumented field is one the model fills in by guessing.

Save-time validation rejects: an empty outcome list, duplicate outcome values, a
field named `outcome` or `description`, a field with a blank description, and a
transition naming an outcome the producing step does not declare.

**The next step's input is rendered markdown**, not raw JSON: the outcome, the
description, then each field. `description` exists precisely to be read by
whoever comes next, and handing over `{"outcome":"success","description":"..."}`
buries it. The run's final output stays the last step's result object, since
that is an API payload rather than something a model reads.

## Transitions

```
union OutcomeFilter { In(OutcomeIn), NotIn(OutcomeNotIn) }
struct OutcomeIn { values: Vec<String> }
struct OutcomeNotIn { values: Vec<String> }

struct WorkflowTransition { to: String, when: Option<OutcomeFilter> }
```

`when: None` is the catch-all. Transitions are tried in order, first match wins,
no match ends the run carrying that step's result. Equality is a one-element
`in`, which is why there is no `eq`.

The `eval` dependency, `eval_condition`, its `catch_unwind` guard and the
save-time parse check all delete. In exchange, a whole class of failure moves
from run time to save time: a filter can only name outcomes its producing step
declares, so `output.severty` becomes unwritable rather than fatal-at-run.

The run log keeps recording which edge was taken (`via`), now as a rendered
filter — `outcome in [p0, p1]` — for display.

## A tool that ends the run

`Toolbox::execute` gains a return type:

```rust
pub enum ToolOutcome {
    /// Ordinary result: goes back to the model, the run continues.
    Result(Value),
    /// The run ends here. No tool result is recorded, so the call stays
    /// dangling — which is what lets an answer arrive later, or never.
    StopRun,
}

async fn execute(&self, name: &str, input: Value, id: &str)
    -> Result<ToolOutcome, ToolCallError>;
```

`ask_user` and `submit_result` return `StopRun`. The loop dispatches a turn's
calls as it does today, collects any that stopped, and — if there are any —
emits `RunComplete` and returns them:

```rust
pub struct StoppedCall { pub tool: String, pub tool_call_id: String, pub input: Value }
pub enum AgentResult { Completed(CompletedOutput), Stopped { calls: Vec<StoppedCall> }, ... }
```

agentcore never learns what those tools mean. It knows only that a tool ended
the run and which one; the meaning lives in the agent actor, which owns both.

**Why the run must end rather than block.** An `ask_user` that waited inside
`execute` would pin the run task, its cancellation token, the provider context
and the rehydrated sandbox for as long as the person takes to answer — and an
agent with a run in flight can never go idle, which is the precondition for
offload. Ending the run is what makes parking free.

**Siblings still run, and must.** A model may call `bash` or `spawn_agent` in
the same turn as `ask_user`, and the run resumes on the same history — so a
dispatched call whose result was never recorded would leave a `tool_use` with no
`tool_result`, which every provider rejects. Under `StopRun` this needs no
special branch: the whole batch is dispatched, ordinary calls journal their
results, and the run ends once the batch settles. Two consequences follow. A
slow sibling delays the question reaching the person, because the run cannot end
mid-batch. And `spawn_agent` alongside `ask_user` is legitimate: the agent parks
on the question holding outstanding children, and a child that reports meanwhile
waits for the answer, per the existing rule that a parked agent wakes only for a
person.

**Why this beats naming terminal tools in the config.** The object that
advertises a tool's spec is the object that decides the stop, so there is
nothing to keep in sync — no name list, no build-time cross-check, no
`ToolSpec` field, no parallel trait method for `FilteredToolbox` and
`CompositeToolbox` to forget to forward. It rides the dispatch path they already
forward correctly, and both directions of the desync bug become unrepresentable.

Deleted from agentcore: `handoff_tool`, `force_handoff_choice`,
`with_handoff_tool`, `with_handoff_tool_optional`, `HandoffToolNotRegistered`,
`InvalidHandoffSchema`, `handoff_validator`, `validate_handoff`, the forced
`tool_choice` branch, and the in-loop text-ending nudge.

Deleted from the server: `AgentParams::{has_output_schema, allow_ask_user,
allow_timers, optional_handoff_tool}`, `handoff_tool()`, `handoff_tools()`,
`AgentPlan::handoff_tool`, `conclude_tool_spec`, `CONCLUDE_TOOL`,
`classify_conclusion`, `Conclusion::Park` and `StepConcludeToolbox`, and the
"this tool is terminal and is not executed" error arms — being dispatched is the mechanism now, not the bug.

`park_or_resume` is kept but re-keyed: it no longer answers a `park`
conclusion, it answers a turn that ended with text while timers were armed. Its
"parked with no active timers" failure branch goes away — that case is now the
stuck turn, and it nudges rather than killing the agent.

**Validation moves into the tool.** `submit_result::execute` checks its own
input — required fields present, `outcome` within the declared enum — and
returns `ToolCallError::InvalidInput` when it is not. That is the ordinary
tool-error path: the model sees the error as a tool result and re-issues,
bounded by the existing retry budget. A privileged validator compiled into
agentcore for one tool is no longer needed, and validation lives with the schema
that defines it.

## How a turn ends

A step ends when it calls `submit_result`. **A turn ending is not a step
ending** — this is the correction that everything else rests on. Since a turn
may legitimately end while the step waits for something, `submit_result` cannot
be a forced `tool_choice`, and "did it finish correctly?" is decided after the
fact from what will wake the agent.

| Turn ended with | Timers | Outstanding children | Result |
|---|---|---|---|
| `submit_result` | — | — | step concludes; driver transitions on `outcome` |
| `ask_user` ×1..n | — | — | parked on questions; step stays Running |
| `submit_result` + `ask_user`, or two `submit_result` | — | — | contradictory → corrective turn |
| `submit_result` + ordinary tools | — | — | valid; siblings ran, then the step concludes |
| text | armed | — | parked on timers |
| text | none | yes | waiting on children |
| text | none | none | stuck → nudge, escalate, then fail |
| provider error / max_iterations | — | — | step fails, run fails |
| process death mid-step | — | — | step Cancelled, run Suspended |

Ordering: the queue drains first. If a subagent report was already waiting when
the turn ended, the next turn starts and nothing is classified at all — the
stuck check only runs on a genuinely idle agent.

**A step that submits while timers are armed** cancels them. The agent said it
is done, so its timers are moot; treating it as an error would need a second
nudge path for a contradiction the agent cannot be told about at the tool
boundary.

### The nudge, escalating

- **First bare ending with nothing pending:** inject "you ended your turn
  without calling `submit_result`" and start a turn with `tool_choice: Auto`.
  Auto deliberately — the model may realise it is not finished and go do more
  work, or arm a timer, which a forcing would forbid.
- **Second:** start the turn with `ToolChoice::Required("submit_result")`. The
  variant exists and every provider implements it (`anthropic.rs:544`,
  `responses.rs:174`, `openai.rs:183`); the loop has simply never constructed
  one. Now the only thing the model can emit is the result.
- **Then:** fail the step. Only a repeatedly invalid payload can reach here.

`Required` cannot be set from the first iteration: `tool_choice` applies to
every call in the loop, so a step would submit an empty result having done no
work. It is safe on a retry precisely because the model has already declared
itself finished.

### The agent actor decides all of it

The agent actor folds `outstanding_children: BTreeSet<Uuid>` from events it
already journals — `spawn_agent`'s tool result carries the child id, the child's
report arrives as an `Incoming`. This is the agent's own view of who owes it a
report, not a copy of the session's forest; the session keeps owning the tree,
the concurrency cap and the UI projection.

So park-versus-stuck is entirely local: timers, outstanding children and the
queue are all in `AgentState`. The session hears only the outcomes it already
handles — `Concluded`, `Asked`, `Parked`, `Failed` — and needs no new
`TurnEnd` variant, no `StepNudged` event, and no classification of its own.

The thing to get right: every way a child can end must reach the parent as an
event it folds — completion, failure, cancellation, and the recovery path that
re-delivers owed results. Miss one and the parent waits for ever on a child that
is gone.

### Recovery

`missing_tool_results` currently uses the terminal tool *names* to tell a
legitimately parked agent (a dangling `ask_user` call awaiting an answer) from
the wreckage of an interrupted turn. It re-keys onto `state.asks`, which already
holds the exact `tool_call_id` of every question the agent is parked on. Stricter
as well as simpler: it exempts the calls actually parked on, rather than any call
that happens to name a terminal tool.

## The step agent

Toolbox: the ordinary runtime tools, plus `submit_result`, plus `ask_user` when
`interactive`. `tool_choice` is `Auto`. The prompt suffix tells it that its
result decides where the run goes next, that ending a turn without
`submit_result` is only valid while it waits for something, and — when not
interactive — that it cannot ask.

Timers become universal in the same change: `set_timer`, `list_timers` and
`cancel_timer` are layered on for every agent the way `task_list` already is,
and `allow_timers` disappears. The tools exist today but nothing in the API can
switch them on, so this is new capability, not a re-enablement.

## Data

One migration deletes every workflow definition and every workflow-origin
session. An old snapshot cannot be expressed in the new spec, and a converted
`eval` condition would be a guess.

## Tests

The last round shipped four bugs that survived because nothing ended a step
unusually. The matrix below exists to make that class impossible.

**agentcore** — `StopRun` mechanics: a stopping call ends the run and reports
its name, id and input; **no tool result is journaled for it** (see Traps);
sibling tools in the same turn execute and their results are recorded first;
several stopping calls are all returned; plain text still ends the run as
`Completed` with no nudge; an `InvalidInput` from a stopping tool is an ordinary
tool error the model can retry.

**agent actor** — interpretation and lifecycle: `ask_user` → parked, one
question and several; `submit_result` → concluded with the payload verbatim;
mixed stoppers → corrective turn; text + timers → parked; text + outstanding
children → parked; text + queued report → next turn starts, no nudge; text +
nothing → nudge with `Auto`, then `Required`, then fail; `outstanding_children`
closes on completion, failure and cancellation of a child; submitting with
timers armed cancels them.

**session / run** — the step lifecycle: ask → park → answer → `submit_result` →
transition; park on a timer → fire → resume → conclude; stop while parked (on a
question, and on a timer) → `StepCancelled`, run Suspended; recovery with a step
parked → suspended and retryable.

**driver** — the graph: `in` matches and misses; `not_in` matches and misses;
first match wins when filters overlap; a catch-all matches regardless of
position; no match finishes the run with that step's result; the step budget
still bounds a self-loop.

**save-time validation** — a filter naming an undeclared outcome; an empty
outcome list; duplicate outcome values; a field named `outcome` or
`description`; a blank field description; a transition to an unknown step; a
catch-all placed before a filter, which makes the filter unreachable.

**HTTP e2e** — a run whose step asks and is answered through the API; a run
whose step ends bare and is nudged; a two-branch graph taking each branch on a
different `outcome`. The last proves the wire, the schema compilation and the
driver agree end to end.

## Traps

- **A `StopRun` call must journal no tool result** — no `ToolComplete`, no
  synthetic success. If it did, `ask_user`'s dangling `tool_use` would already
  have a result and the answer arriving later would append a second one, which
  is issue #62's duplicate `tool_result` re-created from the other end. The
  dangling call *is* the parked state, in the journal and on the wire.
- **The wire is camelCase.** A hand-written body with a snake_case key is
  silently ignored; `outcomes`, `fields` and `when` all arrive through generated
  types, and tests that build JSON by hand must match them.
- **`.fl` edits need `make types`**, and a codegen bump racing a PR reddens
  main.
- **The web UI can stop matching asks by shape.** `isAskCall` exists because a
  step's question was a `conclude` payload rather than an `ask_user` call. Once
  a step asks with `ask_user`, name matching is correct again everywhere, and
  the shape sniffing deletes.
- **Forward the real `tool_call_id`.** A wrapper that substitutes a literal
  breaks the runtime's correlation key and the in-flight set a cancel walks;
  `StepConcludeToolbox` shipped with `"tc1"` hardcoded.

## Shape of the work

Four stacked PRs (`gh stack`):

1. **agentcore**: `ToolOutcome`, `AgentResult::Stopped`, and the deletion of the
   handoff machinery. Mechanical across 23 `Toolbox` impls in 13 files; a
   `From<Value>` keeps most bodies unchanged.
2. **The result contract**: wire types, schema compilation, `submit_result`,
   `ask_user` for interactive steps, `conclude` deleted, universal timers,
   `outstanding_children`, the escalating nudge, and the migration.
3. **Transitions**: outcome filters, `eval` removed, save-time validation, the
   driver, and `via` rendering.
4. **Surfaces**: web editor and run view, CLI, the guide, and the e2e suites.
