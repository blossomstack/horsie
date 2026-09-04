# Agent step-history redesign

Status: design approved 2026-09-03. This supersedes the agent-loop shape proposed in PR #506.

## Why

The agent loop should have one durable account of what happened and one place that decides what happens next. PR #506 moved `agentcore` toward the right boundary — one provider call through `run_step` — but rebuilt the server around queue, turn, provisioning, compaction, seed, usage, and read "components" that do not each own an independent state or lifecycle. The result still has parallel truths in component state and transient scratch, and queued input can accidentally join a provider loop already in progress.

This design makes the append-only agent history the source of truth. The actor holds one temporary projection of the newest open step, drives all foreground work from it, and keeps components only for domains that genuinely own state, such as timers and the task list.

## Decisions

| # | Decision |
|---|---|
| D1 | `agentcore` exposes `run_step`: one provider request in, one completed assistant message and usage result out. The agent actor owns the loop. |
| D2 | Durable history is one chronological append-only log. `StepStarted` markers seal the history prefix used by a foreground step. |
| D3 | The actor holds at most one transient `StepRun`, reconstructed from the newest history segment. It is never persisted. |
| D4 | A normal step may start only after the previous step stopped and every tool call in its assistant message has a durable result. |
| D5 | Incoming user, timer, subagent, and hook-continuation messages are appended immediately. Anything arriving after the current marker is pending and cannot change the request already in flight. |
| D6 | There is no second queue state. Unconsumed incoming history records are the queue. The next normal `StepStarted` consumes every eligible pending record in order. |
| D7 | There is no durable tool-start registry. Live `StepRun` dispatches one tool batch once. Recovery fails every call without a result and never re-executes it. |
| D8 | An unresolved `ask_user` call is the pending question. There is no separate ask registry. |
| D9 | Every provider-backed step persists its own usage with its result. Totals and current context size update immediately, never at whole-loop completion. |
| D10 | System prompts are immutable history records. They are composed once during first initialization and never regenerated from the workspace. |
| D11 | Initialization is one-time semantic work. Reload and recovery only reconnect disposable runtime and MCP clients; they never rescan the workspace or rebuild prompts. |
| D12 | A later workspace scan is an ordinary tool call and result. It adds current information to history without changing old system-prompt records. |
| D13 | Stop hooks, compaction, and seed summary use the same foreground-step lifecycle as normal provider work, but have distinct `StepKind` variants and result records. They are not components unless they gain independent durable state. |
| D14 | Timer and task-list support remain components: each owns state, commands, events, and tools. Timer sleeps do not occupy the foreground `StepRun`. |
| D15 | Cancellation ends the current run loop but preserves pending incoming messages. If any are present, they start the first step of a new run after the cancellation boundary is durable. |
| D16 | The sequence number of `StepStarted` is the step identity and callback fence. A callback not naming the open top marker is stale and is ignored. No separate generation counter is needed. |
| D17 | Exactly one durable `RunEnded` may exist for a run. Parent outcomes are delivered only after it is durable and are idempotent by run identity. |
| D18 | No backward compatibility is required for the old agent journal or snapshot shape. |
| D19 | `Transcript` is a pure read projection of history, not a field persisted in `AgentState`. Branches convert their visible prefix back into conversation-history records rather than carrying a second log. |

## History model

The history mixes records that render to a person, records offered to the model, and control records that make execution recoverable. Those are different projections of one order; they are not separate stores. `project_transcript(history)` selects visible records and assigns the dense cursor sequence used by the messages API. The projection is deterministic, non-serializable, and rebuilt when needed.

A minimal vocabulary is:

```rust
enum AgentHistoryRecord {
    SystemPromptRecorded {
        source: SystemPromptSource,
        content: String,
    },
    AgentInitialized,
    StepStarted {
        kind: StepKind,
    },
    IncomingMessage {
        message: Incoming,
    },
    AssistantCompleted {
        message: Message,
        usage: Usage,
    },
    AssistantAborted {
        message: Message,
    },
    ToolCompleted {
        tool_call_id: String,
        output: ToolOutput,
    },
    StopHookCompleted {
        outcome: StopOutcome,
    },
    CompactionCompleted {
        boundary: CompactionBoundary,
        usage: Usage,
    },
    SeedSummaryCompleted {
        request_id: SeedRequestId,
        summary: String,
        usage: Usage,
    },
    StepFailed {
        reason: StepFailure,
    },
    RunEnded {
        reason: RunEnd,
    },
    // Existing visible lifecycle, timer, task-list, hook, and branch records.
}

enum StepKind {
    Initialize,
    Connect,
    Agent,
    StopHook,
    Compaction,
    SeedSummary { request_id: SeedRequestId },
}
```

The final names may follow the existing event vocabulary, but the distinctions above are semantic. A provider response and its usage are one durable fact. A system prompt is not runtime context. A connection is not initialization.

### The marker is a cutoff

`StepStarted` does not carry copied inputs, a UUID, a run ID, or start/intermediate/end classification. Its journal sequence is its identity. It seals the history prefix the step reads.

For a normal provider step:

1. All eligible incoming messages and previous tool results are already durable.
2. `StepStarted { kind: Agent }` is appended.
3. `run_step` receives the model-visible history through that marker plus immutable system-prompt records.
4. Anything appended after the marker is pending for a later step.

A run identity is the sequence of the first normal `StepStarted` after the previous `RunEnded`. Start, intermediate, and end are derived positions. A one-step run is both its start and its end.

## Transient `StepRun`

`StepRun` is an `AgentActor` field, not part of event-sourced `AgentState`. It contains every process-local execution detail and does not duplicate pending input, provider iterations, repeated-tool fingerprints, or forced-tool decisions that history can derive.

```rust
struct StepRun {
    runtime_ready: bool,
    foreground: ForegroundStep,
    execution: Option<ExecutionContext>,
    reconnect_required: bool,
    start_hooks_ran: bool,
    streamed_text: Vec<String>,
}

enum ForegroundStep {
    Idle,
    Initializing { marker_seq, cancel },
    Connecting { marker_seq, cancel },
    StartingHooks { marker_seq, cancel },
    CallingProvider { marker_seq, attempt, cancel },
    RunningTools { marker_seq, cancel, calls, stopped },
    RunningStopHook { marker_seq, cancel },
    Compacting { marker_seq, cancel },
    SummarisingSeed { marker_seq, cancel },
}
```

The assistant message stays whole in durable history because thinking, text, and tool calls may be interleaved. Queries derive those parts without rearranging them.

There is no separate `Scratch`, `ActiveWork`, provider-flight, generation, or tool-call registry. Every callback carries `marker_seq` and can finish only its matching `ForegroundStep` variant while that sequence is still the open durable step.

The tagged variants make illegal combinations unrepresentable: idle state cannot retain a marker or cancellation token, provider retry state exists only during a provider call, and only tool execution can retain dispatched calls.

The exact Rust shape should make illegal combinations unrepresentable. For example, only a normal agent-step variant can be `CallingProvider`, and only a Stop-hook variant can hold a Stop-hook callback. The illustrative structs above do not require one wide struct full of optional fields.

## One next-step decision

After every durable append and every live transition that wrote nothing, the actor re-evaluates the top step:

1. If foreground work is running, wait.
2. If the agent has never been initialized, start initialization.
3. If execution requires live resources and they are disconnected, start connection work.
4. If recovery left a step incomplete, append its repairs first.
5. If a normal step has no assistant response, run `agentcore::run_step`.
6. If its assistant message has tool calls and any lack results, dispatch the whole unresolved batch once and wait.
7. If an unresolved `ask_user` remains, wait for its result.
8. If a boundary operation is due, start the appropriate special step.
9. If tool results or pending incoming messages provide continuation input, append the next normal `StepStarted`.
10. Otherwise append `StepStarted { kind: StopHook }` and run the Stop hook.
11. When the Stop hook returns, re-read history. New pending input wins over an allow-to-stop result.
12. Append exactly one `RunEnded` only when nothing can continue the run.

A component may append its own event and tool result, but it does not decide whether another agent step starts. That remains the actor's one decision.

## Incoming messages and cancellation

An accepted incoming message is durable before its caller is acknowledged. While a foreground step is open, the message belongs to that step's `pending_messages`. It never enters an in-flight provider request.

Cancellation is one ordered actor command. It:

1. Cancels the current `StepRun` token.
2. Salvages any streamed assistant text worth retaining.
3. Appends interrupted results for every open tool call.
4. Appends `RunEnded { reason: Cancelled }`.
5. If pending incoming messages exist, appends a new normal `StepStarted` after the run boundary; this begins a new run.
6. Acknowledges cancellation after the batch is durable.

A message racing cancellation is ordered by the mailbox. If it lands first it is carried into the new run; if it lands afterwards it starts that run itself. Late callbacks name the old marker sequence and are ignored.

## Recovery

Recovery performs no provider, tool, hook, workspace-scan, compaction, or seed-summary side effect. It folds history, reconstructs the top `StepRun`, appends the minimum repair, reconstructs once more, and then uses the normal next-step decision.

Repairs are deterministic:

- Normal step with no completed provider response: append `StepFailed(Interrupted)` and end that run, unless pending input starts a new one.
- Assistant message with tool calls lacking results: append one interrupted error result per open call. The normal decision then starts the next step, allowing the model to decide whether to retry.
- Interrupted Stop hook: append a typed interrupted failure and end the run; never replay the hook.
- Interrupted compaction: append a skipped/interrupted result and continue with unchanged prompt history.
- Interrupted seed summary: append a failed result keyed by request ID so its requester can be answered idempotently.
- Settled step missing only its next marker or run end: run the ordinary decision; no repair-specific continuation exists.

## Usage

Each `agentcore::run_step` completion journals `AssistantCompleted { message, usage }`. Folding it immediately updates:

- cumulative usage;
- the newest provider step's usage;
- the newest provider call's input-token count, which is the current context size.

Compaction and seed-summary completions carry their own usage in the same way. A provider failure records usage only if the provider actually reports it. A cancelled stream generally has no final usage to record.

Session-level usage receives an idempotent update after each usage-bearing agent record becomes durable. Whole-run completion carries no unbanked usage and cannot lose the cost of earlier steps.

## Immutable system prompts

Initialization runs once, before the first normal agent step. It may acquire the runtime, materialize the agent, scan the workspace, resolve initial plugin and skill context, and compose prompt sections. It then appends every `SystemPromptRecorded` entry and `AgentInitialized` atomically.

After `AgentInitialized` exists, no load path may scan the workspace or regenerate those records. The prompt for every later provider step is reconstructed by concatenating the immutable system-prompt records in their original order.

Changing observations do not belong in a regenerated system prompt. A live workspace scan, current tool session state, a refreshed memory index, or newly discovered context is appended as an ordinary message or tool result. The old prompt remains an honest record of what the agent began with.

System-prompt records are pinned across compaction and count toward the context budget.

## Initialization versus connection

Initialization creates durable meaning once. Connection restores disposable machinery whenever required.

Connection may:

- resume or attach to the existing runtime;
- reconnect runtime transport;
- reconnect configured MCP servers and rebuild their live clients;
- rebuild process-local toolboxes from durable configuration;
- verify that required endpoints are reachable.

Connection must not:

- scan the workspace;
- recompute instructions;
- replace system-prompt records;
- rediscover context solely to build the prompt;
- silently create a new runtime when the existing one is gone.

A crash before `AgentInitialized` is durable may repeat initialization because no completed initialization exists. A crash after it is durable only reconnects.

An explicit `inspect_workspace` call is ordinary agent work: assistant tool call, tool result, then another normal step. Its result describes the current workspace without mutating the initial prompt.

## Special steps

### Stop hook

A normal settled step with no continuation input starts a Stop-hook step. The hook runs off-mailbox. Incoming messages during it become pending naturally.

- Allow + no pending input: end the run.
- Allow + pending input: start another normal step.
- Continue: append the hook's continuation message and start another normal step, including any other pending input.
- Fail: end the run as failed.
- Cancel: cancellation wins and the late hook result is stale.

The Stop-hook executor requires a timeout. A timeout is a typed failure, not an infinite mailbox block.

### Compaction

Compaction may start only at a settled boundary. It reads a stable history prefix, performs a tool-less provider call, and appends a compaction boundary plus usage. Incoming messages during it remain pending. Compaction appends; it never deletes history or rewrites system prompts.

### Seed summary

Seed summary is keyed by a durable request ID. It reads a stable history prefix, performs a tool-less provider call, and appends the summary plus usage. Its durable result is delivered idempotently to the requester. Initializing the target sub-session is a separate operation.

## Code organization

The module tree mirrors ownership rather than execution mechanics:

- `actor.rs`: persistence, recovery, observation, and actor lifecycle only.
- `run_loop/decision.rs`: the single ordered next-step decision.
- `run_loop/provider/`: one provider call plus interpretation of its ending.
- `run_loop/context_step.rs`, `compaction_step.rs`, and `seed_step.rs`: fenced special steps.
- `run_loop/incoming/`: pure pending-input projections and their stateless command handler.
- `step_run.rs`: all process-local foreground state.
- `state.rs` and `events.rs`: durable history, usage projections, and component state.
- `transcript.rs`: the pure history-to-transcript projection and branch-context conversion.
- `components/`: only stateful tools.

## Components

A component is reserved for a domain with its own state machine:

- `Timers`: armed records, fire/cancel commands, durable timer events, and timer tools.
- `TaskList`: task-list state, mutation commands, durable changes, and task-list tools.

Reads are actor queries. Usage is a history fold. Normal steps, initialization, connection, Stop hooks, compaction, and seed summary are actor orchestration. None becomes a component merely to make command routing look uniform.

## Required tests

1. Usage from every provider step is durable before the run ends.
2. A crash after several provider steps loses none of their usage.
3. The first agent step initializes once and persists immutable system-prompt records.
4. Twenty steps, several turns, offload/reload, and crash recovery perform one workspace scan total.
5. Reload reconnects runtime and MCP clients without changing prompt bytes.
6. `inspect_workspace` appears as an ordinary tool call/result and does not mutate system prompts.
7. A message arriving during a provider call is pending and first appears in the next provider request.
8. A message arriving during a Stop hook prevents an allow result from ending the run.
9. Cancellation preserves pending messages and starts them in a new run.
10. Recovery appends interrupted results for every open tool call and executes none of them.
11. A callback from an old marker sequence cannot append after cancellation or a later step.
12. Compaction and seed summary buffer incoming messages and bank usage independently.
13. Exactly one `RunEnded` is durable under Stop-hook, cancellation, and late-callback races.
14. Reads and warm actor load perform no runtime, MCP, or workspace operation.
15. Serializing `AgentState` stores each message once in history and contains no transcript field.
16. Repeated transcript projection produces identical entries and cursor sequences.
17. A branch carries the visible transcript prefix as inert history without inheriting pending input, run state, initialization, or prompt identity.

## Out of scope

- Backward-compatible loading of pre-redesign agent journals and snapshots.
- Automatically changing an existing session's immutable system prompt when files, plugins, skills, memories, or settings change.
- Retrying possibly side-effecting tool calls after process failure.
