# A complete hook-events library, and horsie's wiring onto it — design

Written 2026-08-04, after #140 merged. Supersedes the hook-record shape that
landed with it, and replaces PR #141 (which cannot be rebased — see below).

## Why

#140 shipped tool hooks: `PreToolUse` and `PostToolUse` run inside the runtime
and their records appear in the agent's transcript. Extending that to `Stop` —
PR #141's job — surfaced that the per-event knowledge is scattered across
`HookEvent::parse`, the runtime dispatcher, and each call site, and that the
record type had been shaped around tool hooks alone.

The symptoms are concrete and all present on `main` today:

- `system_message` is parsed, stored on the record, put on the wire, and **read
  by nothing** — no consumer in `server/`, none in the web client, none in the
  CLI. It is documented as "addressed to the user" and reaches no user.
- `additional_context` is recorded on `PreToolUse` records, where the spec does
  not offer it, and where nothing consumes it. It is acted on only in the
  `PostToolUse` arm.
- `blocked: bool` flattens two different spec mechanisms: `PreToolUse` refuses
  via `permissionDecision: "deny"`, every other event via top-level
  `decision: "block"`.
- `blocked` is also set when a hook *failed*, contradicting the field's own doc
  comment ("Distinct from `blocked`: one is a decision, the other an outage").
- `SessionStart` hooks produce **no records at all** — they take a bespoke
  `SessionStartRequest`/`SessionStartResponse` path that returns a bare string —
  so the locked decision "every hook that runs is recorded" is already untrue.

Adding twelve more events to that shape would multiply every one of these. So
the per-event knowledge moves into one spec-faithful library, and horsie wires
call sites onto it.

## Scope, and the two questions it separates

Two questions had been conflated:

- **What does the Claude Code hook protocol look like?** All of it. This is a
  published spec; modelling it completely costs only research.
- **Which events can horsie fire?** Far fewer, and for reasons that differ per
  event.

The library answers the first. horsie's wiring answers the second. Events that
are modelled but unwired stay classified `NotImplemented` and are still refused
at install, so no plugin can install believing a hook works and find it silently
never fires.

### What horsie can and cannot run

Measured from `HookEvent::parse` on `main` plus the published event list:

| | Count | Events |
| --- | --- | --- |
| Wired today | 3 | `PreToolUse`, `PostToolUse`, `SessionStart` |
| Has a seam, unwired | 12 | `Stop`, `UserPromptSubmit`, `SessionEnd`, `SubagentStart`, `SubagentStop`, `PostToolUseFailure`, `PostToolBatch`, `StopFailure`, `Notification`, `TaskCreated`, `TaskCompleted`, `CwdChanged` |
| No horsie concept | 16 | `PermissionRequest`, `PermissionDenied` (no permission model — horsie runs unattended by design), `PreCompact`, `PostCompact` (no context compaction), `WorktreeCreate`, `WorktreeRemove` (no worktrees), `FileChanged`, `DirectoryAdded` (no file watcher), `UserPromptExpansion`, `Setup`, `ConfigChange`, `InstructionsLoaded`, `TeammateIdle`, `MessageDisplay`, `Elicitation`, `ElicitationResult` |

So the ceiling is 15, not 31. Real demand is narrower still: across every plugin
in the official marketplace only six events are declared by anyone — `Stop` (3),
`SessionStart` (3), `UserPromptSubmit` (2), `PostToolUse` (2), `PreToolUse` (1),
`UserPromptExpansion` (1) — and the last has no horsie concept. Three of the
five already work. **The demand gap is `Stop` and `UserPromptSubmit`.**

## Three layers

```
support::plugin::hooks          the library — complete, spec-derived, no horsie
  ├── events        every documented event: input payload, matcher domain,
  │                 permitted output fields
  ├── process       (event, exit_code, stdout, stderr) → typed outcome
  └── classify      HookEvent::parse → Unsupported, as today

models/fluorite/hooks.fl        the record — one struct per event horsie runs
runtime + server                the wiring — call sites, dispatch, transcript
```

## The library

### Events are described once, in data

Each event carries three facts the rest of the system keeps re-deriving:

- **Its input payload** — the JSON horsie writes to the hook's stdin. Today the
  runtime builds these with inline `json!` macros at each call site, so the shape
  lives wherever someone typed it.
- **Its matcher domain** — matchers are *not* tool-only. `SessionStart` matches
  `startup`/`resume`/`clear`/`compact`/`fork`; `SessionEnd` matches
  `clear`/`resume`/`logout`/`prompt_input_exit`/…; `Notification` matches
  `permission_prompt`/`idle_prompt`/…; `StopFailure` matches its error class
  (`rate_limit`/`overloaded`/…); `CwdChanged` supports no matcher at all. Tool
  events match tool names, which is the only case horsie currently handles.

  So `matcher_applies(matcher, horsie_tool)` becomes `matches(event, matcher)`:
  the same regex semantics — absent or empty selects everything, a pattern that
  fails to compile selects nothing so a broken matcher cannot widen to "all" —
  but the *subject* comes from the event's description rather than being assumed
  to be a tool name. Tool events keep testing against the horsie tool name and
  each of its Claude aliases, which is what makes any published matcher fire at
  all.
- **Its permitted output fields** — which of `systemMessage`, `continue`,
  `stopReason`, `decision`/`reason`, `permissionDecision`, `additionalContext`,
  `updatedInput`, `updatedToolOutput` that event may set.

Common input fields are shared by every event: `session_id`, `transcript_path`,
`cwd`, `hook_event_name`, `permission_mode`, and — when inside a subagent —
`agent_id` and `agent_type`. Note `tool_use_id` is Claude's own name for the
tool call id, which is the same join key #140 threaded through
`Toolbox::execute`.

### Processing is generic

One code path turns a hook's reply into an outcome, driven by the table above:

- **Exit 0** — parse stdout as JSON if it is JSON. For `SessionStart`,
  `UserPromptSubmit` and `UserPromptExpansion`, *non-JSON* stdout is injected
  context; for every other event it is debug output and is discarded.
- **Exit 2** — a blocking error. stdout and any JSON in it are ignored; stderr
  is the reason. What blocking *means* is per-event: `PreToolUse` denies the
  call, `UserPromptSubmit` rejects the prompt, and the events that cannot block
  treat it as a plain failure.
- **Any other exit** — a non-blocking failure; the first line of stderr is the
  reason.
- **A field the event does not permit is ignored and noted**, rather than being
  recorded as though it had an effect. This is what stops the next
  `additional_context`-on-`PreToolUse`.

The library never decides *consequences* — what a verdict actually does is
horsie's call, made at the call site, because it depends on horsie's own
semantics. `PreToolUse` fails closed, so a hook that could not run denies the
call; `Stop` blocking means the opposite of a refusal and continues the turn (see
below); `Notification` cannot block at all, so the same exit code is merely
recorded. One parser, three consequences.

## What a hook can change, and where it lands

Two outputs reach two different audiences, and they must never share a path.

**`additionalContext` goes to the model.** Verified in the code, by two routes:

- `SessionStart` context becomes `SharedContext.bootstrap`, which
  `compose_system_prompt` prepends as a `# Session bootstrap` section of the
  agent's **system prompt** (`workflow/src/workspace.rs:285-299`).
- `PostToolUse` `additional_context` is appended to the tool's stdout
  (`runtime/src/hooks.rs:179`), landing in the **tool result** the model reads.

`UserPromptSubmit` and `SessionStart` can also inject context as *bare stdout*
rather than JSON, which the generic processor handles for those events and
discards for the rest.

**`systemMessage` goes to the user.** The spec calls it "warning text shown to
user". It is not injected into the model, which is why this design surfaces it in
the transcript rather than in a prompt. Today it reaches neither.

## The record

One struct per event horsie runs, with failure as a variant of that event's own
outcome. Duplication between arms is deliberate: each event's handling is checked
against its own type, and one event gaining a capability cannot silently widen
another.

### Shared payloads

```fluorite
/// What a tool hook guarded. The join key attaching a record to its call.
struct ToolScope { tool: String, tool_call_id: String }

/// A value a hook replaced. Both halves or neither — never a dangling "before".
struct HookRewrite { before: String, after: String }

/// The hook never ran to completion — spawn failure, timeout, or a bad exit.
struct HookFailed { reason: String }

/// A `PreToolUse` refusal, via `permissionDecision: "deny"`.
struct HookDenied { reason: Option<String> }

/// Every other event's refusal, via top-level `decision: "block"`.
struct HookBlocked { reason: Option<String> }

struct ContextInjected { additional_context: Option<String> }
```

### The envelope

```fluorite
struct HookRecord {
    plugin: String,
    duration_ms: u64,
    action: HookAction,
}
```

`plugin` and `duration_ms` are the only universally true facts: every hook that
ran was declared by a plugin and took time. Everything else is per-event.

The union is named `HookAction` rather than `HookEvent` because
`support::HookEvent` already exists as the *name classifier* that powers
install-time refusal. Two different jobs, two different names.

```fluorite
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
```

### Tool events

```fluorite
/// The only event that can deny a call before it runs, and the only one that can
/// rewrite its input. No `additionalContext`: the spec does not offer it here,
/// and there is no result yet to attach it to.
struct PreToolUseRecord { call: ToolScope, system_message: Option<String>, outcome: PreToolUseOutcome }
union PreToolUseOutcome {
    Allowed(PreToolUseAllowed),   // { input: Option<HookRewrite> } — only an allowed call is rewritten
    Denied(HookDenied),
    Ask,                          // logged and treated as allow: horsie has no permission prompt
    Defer,
    Failed(HookFailed),           // denies — PreToolUse fails closed
}

struct PostToolUseRecord { call: ToolScope, system_message: Option<String>, outcome: PostToolUseOutcome }
union PostToolUseOutcome {
    Ran(PostToolUseRan),          // { output: Option<HookRewrite>, additional_context: Option<String> }
    Blocked(HookBlocked),         // recorded; the call already ran
    Failed(HookFailed),           // recorded, never fatal
}

struct PostToolUseFailureRecord { call: ToolScope, system_message: Option<String>, outcome: PostToolUseFailureOutcome }
union PostToolUseFailureOutcome { Ran(ContextInjected), Blocked(HookBlocked), Failed(HookFailed) }

/// A whole batch of parallel calls, so it names every call rather than one.
struct PostToolBatchRecord { calls: Vec<ToolScope>, system_message: Option<String>, outcome: PostToolBatchOutcome }
union PostToolBatchOutcome { Ran(ContextInjected), Blocked(HookBlocked), Failed(HookFailed) }
```

### Turn and session events

```fluorite
/// `source` is the matcher domain: startup | resume | clear | compact | fork.
struct SessionStartRecord { source: String, system_message: Option<String>, outcome: SessionStartOutcome }
union SessionStartOutcome { Ran(ContextInjected), Failed(HookFailed) }   // cannot block: no decision field

/// Injects context via raw stdout as well as `additionalContext`.
struct UserPromptSubmitRecord { system_message: Option<String>, outcome: UserPromptSubmitOutcome }
union UserPromptSubmitOutcome { Ran(ContextInjected), Blocked(HookBlocked), Failed(HookFailed) }

/// `Blocked` means *blocked from stopping* — the turn continues. See "Stop
/// continues the turn" below; this is not a refusal like `PreToolUse`'s.
struct StopRecord { system_message: Option<String>, outcome: StopOutcome }
union StopOutcome { Ran(ContextInjected), Blocked(HookBlocked), Failed(HookFailed) }

struct SubagentStartRecord { agent_type: String, system_message: Option<String>, outcome: SubagentStartOutcome }
union SubagentStartOutcome { Ran(ContextInjected), Failed(HookFailed) }

struct SubagentStopRecord { agent_type: String, system_message: Option<String>, outcome: SubagentStopOutcome }
union SubagentStopOutcome { Ran(ContextInjected), Blocked(HookBlocked), Failed(HookFailed) }

struct TaskCreatedRecord { task_id: String, system_message: Option<String>, outcome: TaskCreatedOutcome }
union TaskCreatedOutcome { Ran, Failed(HookFailed) }

struct TaskCompletedRecord { task_id: String, system_message: Option<String>, outcome: TaskCompletedOutcome }
union TaskCompletedOutcome { Ran, Failed(HookFailed) }
```

### Side-effect-only events

These support no JSON output at all — not even `systemMessage` — and cannot
block: exit 2 has no special meaning for them. They can still *fail*, and their
stderr is still user-facing, which is the whole of what there is to record.

```fluorite
/// `reason` is the matcher domain: clear | resume | logout | prompt_input_exit | …
struct SessionEndRecord   { reason: String, outcome: SideEffectOutcome }
/// `error` is the matcher domain: rate_limit | overloaded | … | unknown
struct StopFailureRecord  { error: String, outcome: SideEffectOutcome }
struct NotificationRecord { message: String, outcome: SideEffectOutcome }
struct CwdChangedRecord   { cwd: String, outcome: SideEffectOutcome }

union SideEffectOutcome { Ran, Failed(HookFailed) }
```

## Transport

Tool hooks stay where #140 put them: inside the runtime, inline with the call
they guard, reported on the tool response. Nothing about that changes.

`SessionStart`'s bespoke RPC becomes the general path for every event the runtime
cannot initiate:

```fluorite
/// Events the server initiates. Tool events are absent by construction — they
/// run inline in the runtime, so asking for one out of band is unrepresentable.
#[type_tag = "event"]
union ServerHookEvent {
    SessionStart(SessionStartInput),       // { source }
    SessionEnd(SessionEndInput),           // { reason }
    UserPromptSubmit(UserPromptSubmitInput), // { prompt }
    Stop(StopInput),                       // { last_assistant_message, stop_hook_active }
    SubagentStart(SubagentStartInput),
    SubagentStop(SubagentStopInput),
    // …one arm per server-initiated event, carrying that event's input
}

struct RunHooksRequest  { call_id: String, event: ServerHookEvent }
struct RunHooksResponse { call_id: String, records: Vec<HookRecord> }
```

This replaces `SessionStartRequest`/`SessionStartResponse`. Injected context
stops being a separate `context: String` and is derived by concatenating
`additional_context` off the records — so `SessionStart` becomes recorded like
everything else and the two-classes asymmetry disappears.

`run_hooks` fires the existing `HookSink`, so records reach the transcript by the
*same* route as tool records: `HookSink` → `SessionCommand::HooksRan { key,
records }` → `AgentCommand::HooksRan` → `AgentDomainEvent::HookRan`. No new
plumbing — that is the payoff of generalising rather than adding a third RPC.

## Stop continues the turn

`Stop` is not a notification that a turn ended — its only two capabilities are
ways of **not** ending it:

> Stop hooks support blocking—exit code 2 or `decision: "block"`—which prevents
> Claude from stopping and causes the agentic loop to continue. They also support
> non-blocking feedback through `additionalContext`… Return blocking feedback if
> you want to force another iteration.

So recording a `Stop` hook and ignoring both outputs would fire the event and
discard everything it said. That is the failure the phase's locked decision
exists to prevent — *no plugin installs believing a hook works and finds it
silently never fires* — one level down. It matters in practice: `Stop` is the
most-declared event in the whole official marketplace.

horsie therefore honours it, which makes `Stop` a **turn-lifecycle** change
rather than a hook-dispatch one:

- **`Blocked`** — the turn does not conclude. The reason is fed back as the input
  to another run, reusing the same `start_run(AgentInput::user_message(…))` path
  recovery already uses to continue an interrupted task.
- **`Ran` with `additional_context`** — non-blocking feedback. The context is
  recorded and carried into the *next* turn rather than forcing one now, which
  is the honest mapping of "inform Claude but let it decide" onto a server that
  has no idle agentic loop to re-enter.
- **`Failed`** — recorded, never fatal. `Stop` runs after the fact, so a guard
  that could not run cannot deny anything; only `PreToolUse` fails closed.

**The loop guard is mandatory, not optional.** horsie runs unattended sessions,
so a `Stop` hook that always blocks would spin forever with nobody watching.
Two mechanisms, both required:

- `stop_hook_active` is set on the input for every continuation-triggered run.
  The spec defines it as "true when Claude would normally stop but is being held
  in the loop by a blocking hook", so a cooperative hook returns early.
- A hard per-turn cap on continuations, after which the turn concludes regardless
  and the record says why. Cooperation cannot be relied on: a hook that ignores
  `stop_hook_active` is exactly the case the cap exists for.

Where it runs is unchanged from #141's one durable idea: a decorator on
`AgentOutcomeSink::deliver`, which is called on the agent's own run task. A slow
`Stop` hook delays that turn's completion without stalling the session's command
loop against a cancel or a new message.

## The transcript

The entry id becomes **`hook:{seq}`**, where `seq` counts hook entries already in
that transcript, journaled on the event as today so replay and the live stream
derive the same id. `hook:{tool_call_id}:{seq}` cannot name a `SessionStart`
record. The tool join is unaffected — it goes through the record's `ToolScope`,
which is where it belongs.

Rendering splits on the same fact: a record whose `action` arm carries a
`ToolScope` attaches to its tool-call card as it does now; every other record
renders as its own transcript row. `system_message` is surfaced on the row,
closing the field that has been captured and shown to nobody.

## Delivery

PR #141 is **rebuilt, not rebased**: `feat/plugin-hooks-events` is a separate
lineage whose lower commits are the discarded server-side #140, including
`hooked_toolbox.rs`. Only `ec4127e` is real PR2 content, and its `Stop` calls
`run_hook` and reads `HookDeclWire`, both deleted by #140. Its one durable idea —
running `Stop` from a decorator on `AgentOutcomeSink::deliver`, on the agent's own
task, so a slow hook cannot stall the session's command loop against a cancel —
carries forward.

1. **The library.** `support::plugin::hooks` gains the complete event
   description (inputs, matcher domains, permitted outputs) and the generic
   processor. No wiring changes; existing call sites move onto it. Behaviour
   holds except that off-spec fields stop being recorded.
2. **The record reshape.** `HookRecord` becomes the per-event model above;
   runtime dispatch, transcript and UI adapt. Breaking: hook records journaled
   since #140 merged stop deserializing. Accepted — no backward compatibility,
   consistent with the `history` rename.
3. **The server-initiated path.** `RunHooks` replaces the `SessionStart` RPC and
   `SessionStart` starts producing records. Behaviour-preserving apart from
   records appearing.
4. **`Stop` and `UserPromptSubmit`**, the two events with real demand, plus the
   unsupported-event gates from `ec4127e` (install refusal and session-start
   re-check). This is the largest and riskiest step: `Stop` continuation changes
   the turn lifecycle, and the loop guard has to hold for unattended sessions.
   Worth splitting again if it grows — the gates are independent of `Stop`.
5. **The remaining ten**, sequenced afterwards, each one a call site against a
   library that already knows the event.

## Testing

- **Library** — a table test per event asserting its permitted output fields;
  exit-code semantics (0 with JSON, 0 with bare stdout, 2, other); stdout-as-
  context for the three events that inject it and discarded for the rest; a
  field the event does not permit is ignored rather than recorded.
- **Record** — per-event fold tests; a mixed transcript surviving a snapshot
  round trip; entry ids derived and stable across replay.
- **Transport** — a server-initiated event's records reach the transcript by the
  same sink as tool records; `run_hooks` cannot be asked for a tool event.
- **Server** — a subagent's hooks land on the subagent's transcript; a `Stop`
  hook cannot stall the session command loop.
- **`Stop` continuation** — a blocking `Stop` starts another run with the reason
  as input; `stop_hook_active` is set on every continuation run and absent on the
  first; a hook that blocks unconditionally is stopped by the cap rather than
  looping, and the record says the cap ended it; non-blocking
  `additional_context` does *not* start a run.
- **Web** — the tool-attached and standalone renderings; `system_message` shown.
- **e2e** — the Playwright case #140 left undone: a hook row is present **after a
  reload**, which is the requirement journaling exists for. PR 4 provides the
  plugin fixture it needs.

## Out of scope

- **The 16 `NoConcept` events.** Each needs a horsie subsystem that does not
  exist (a permission model, context compaction, worktrees, a file watcher).
  They stay refused at install with a reason that names why.
- **`continue` / `stopReason`.** Universal in the spec; horsie parses neither,
  and honouring `continue: false` is a turn-lifecycle change rather than a
  hook-dispatch one. Worth its own issue.
- **HTTP hooks.** The spec allows a hook to be an HTTP endpoint receiving the
  payload as a POST body. horsie runs commands only.

## Sources

- [Claude Code hooks reference](https://code.claude.com/docs/en/hooks) — event
  list, per-event output-field table, exit-code semantics, input payloads.
- Prior specs: `2026-08-02-plugin-hooks-design.md`,
  `2026-08-04-hook-records-in-history-design.md`.
