# The hook control protocol, finished

Closes the three items #105's Phase 1 left open: `SubagentStop`, `continue: false` /
`stopReason`, and `type: "http"` hooks. One subsystem, one design — they share the
output-field table, the reply processor and the record envelope, and two of them are only
correct together.

Builds on `2026-08-04-hook-events-library-design.md` (the library) and
`2026-08-05-hooks-into-the-conversation-design.md` (the pre-run seam).

## 1. `SubagentStop` — and the `Stop` a subagent fires today

`StopHookParent` fires `ServerHookEvent::Stop` whatever agent it decorates, and it decorates
every agent a session hosts — main, step and subagent alike. So a subagent finishing its
turn fires **`Stop`**. This is the same conflation the previous PR removed at the *start*
seam, one seam later: `SessionStart` fired for subagents until it was gated on
`SessionAgentKind`, and `Stop` still does.

Two visible consequences today: a `Stop` hook written to mean "the session is done" fires
once per subagent, and a `SubagentStop` hook never fires at all — it is refused at install
as unwired.

**The fix is the same shape as the start seam's.** The sink picks the event from the
provider's kind:

| kind | start event | stop event |
| --- | --- | --- |
| `Main` | `SessionStart` | `Stop` |
| `Step(_)` | `SessionStart` | `Stop` |
| `Sub(_)` | `SubagentStart` | `SubagentStop` |

A step keeps `Stop` deliberately: it is the top of its own subagent tree and fires
`SessionStart`, so answering `SubagentStop` for it would contradict its own start.

**Blocking is symmetric with `Stop`.** `decision: "block"` means blocked *from stopping*, so
the subagent's turn continues through the same `ContinueAfterStop` path under the same
`MAX_STOP_CONTINUATIONS` budget. `stop_verdict` therefore reads `SubagentStopOutcome::Blocked`
alongside `StopOutcome::Blocked`, and `cap_reached` narrows both — which needs a `CapReached`
arm on `SubagentStopOutcome`, so that "an unattended run that hit the guard says so" stays
true for subagents.

**Matcher domain** is `agent_type`, the value `SubagentStart` already passes. Until #105's
Phase 2 lands that is the constant `"subagent"` for every subagent; a matcher on it selects
all or none, which is honest rather than a lie about a type horsie does not yet have.

**`agent_id` rides both subagent events too**, which the spec lists as a common field and
horsie did not send. It is not a matcher subject — selecting one run is not something the
spec offers — but while every subagent reports the same `agent_type` it is the only thing
telling two concurrent ones apart, or joining a subagent's start to its own stop.

`is_wired()` goes from six events to seven.

## 1a. `SessionStart`'s `source` is an enum

`source` is a matcher domain with a vocabulary the spec fixes — `startup`, `resume`, `clear`,
`compact`, `fork` — so it is a `SessionStartSource` union rather than a `String`. horsie
constructs two of the five: `Startup` for a fresh agent load, `Resume` for one folded from a
journal. The other three are arms nothing constructs, which is the same honesty `is_wired()`
gives an event horsie cannot fire, rather than three values that silently never appear.

The enum is horsie's vocabulary and `"startup"` is the spec's, so `as_wire()` maps between
them at the two places the foreign spelling is owed: the payload a hook reads, and the
subject its matcher selects on. `SessionStartRecord.source` stays a `String` — it holds the
spelling the hook was given, which is what the transcript shows.

Worth naming, because it is a real divergence: horsie reports `resume` on every *agent load*,
including the rehydration after an idle offload, which no user asked for. A `startup`-matched
hook is unaffected. One matching `resume` fires more often than the spec's wording suggests
— and legitimately so, since the context it exists to refresh really was lost. The guide says
this.

## 2. `continue: false` and `stopReason`

A *common* field in the spec — any hook on any event may set it, and it takes precedence
over `decision`. horsie does not parse it at all today.

### Where it lives in the model

**On `HookRecord`, not in the outcome unions.** `HookRecord` carries the facts that are true
of every hook that ran — which plugin declared it, how long it took. "It asked horsie to
stop" is exactly that kind of fact: orthogonal to what the event decided, available to all
of them. Adding a `Halted` arm to fifteen outcome unions would say the opposite — that
halting is a species of each event's own verdict — and would make a `PreToolUse` hook that
both allowed a call and halted the turn unrepresentable, which is a legal reply.

```
struct HookHalt { reason: Option<String> }        // `stopReason`

struct HookRecord {
    plugin: String,
    duration_ms: u64,
    halt: Option<HookHalt>,                        // new
    action: HookAction,
}
```

Journals written before this field read it as `None` — serde defaults an absent `Option`
field, verified rather than assumed.

### Where it is read

`OutputField::Halt`, permitted on every event that has any JSON output at all: the four
side-effect-only events (`SessionEnd`, `StopFailure`, `Notification`, `CwdChanged`) are
documented as producing none, and a `continue` on them would be a field the docs do not give
them. Everything else may halt. `process()` reads `"continue": false` plus `"stopReason"`
into `HookOutput.halt`, and `HookInvocation::record` copies it onto the envelope.

### What it does, per seam

Each seam reads the same field; the consequence differs because what is in flight differs.

- **Tool hooks** (`PreToolUse` / `PostToolUse`, run in the runtime inline with the call). The
  record already travels to the session over the existing `HookSink` → `HooksRan { key,
  records }`. The session halts the agent named by `key` — a new
  `SessionCommand::HaltAgent { key, reason }`, sibling to `ContinueAfterStop`, which the same
  sink already reaches. It cancels the agent and then routes an `AgentOutcome::Failed`
  through the ordinary outcome path rather than branching per key itself — a halt *is* a
  failure with a reason, and what a failure means for a main agent (a failed turn), a
  subagent (`SubAgentFailed`, so the node does not sit `Running` forever) and a step
  (`StepFailed`) is already decided in one place.

  Only while that agent is still running. A halt races the turn it is halting — the records
  reach the session on the sink while the tool call that produced them is still returning —
  so a halt that arrives late finds a concluded turn and does nothing, exactly as
  `ContinueAfterStop` does.

  **Tool records only.** `RuntimeClient::run_hooks` puts a server-initiated event's records
  on this same sink *as well as* returning them, and every one of those seams reads the halt
  off its own return value. Acting on both routes halted the agent twice: a halting `Stop`
  hook journaled `TurnFailed` and then `TurnEnded` for one turn.
- **Start hooks** (`SessionStart` / `SubagentStart` / `UserPromptSubmit`, fired on the
  pre-run seam). The run has not begun, so halting is abandoning it: `AbandonedStart::Blocked
  (reason)`, the arm a `UserPromptSubmit` block already produces. `prompt_blocked` becomes
  `start_blocked` and reads a halt on *any* record as well as that block.
- **`Stop` / `SubagentStop`.** The turn is already ending, so a halt cannot end it harder —
  and must not be allowed to, which is what the tool-records-only rule above buys. What it
  does do is **override a sibling hook's block**: with a halt present the turn ends rather
  than continuing, which is the one place `continue`'s documented precedence over `decision`
  is observable. The records need no second `HooksRan`; the sink has already sent them.

Because the reason reaches the user and never the model, it rides the record — which the web
client already renders — rather than being injected as context.

## 3. HTTP hooks

`{"type": "http", "url": …, "headers": {…}, "timeout": N}`, alongside `type: "command"`.
Today `read()` skips every non-command hook.

`HookDecl` splits its command string into a transport:

```rust
pub enum HookTransport {
    Command(String),
    Http { url: String, headers: Vec<(String, String)> },
}
```

**Execution stays in the runtime**, one branch in `run_one`, for the same reason command
hooks do: it is the one place a `HookRecord` is made, and an HTTP hook whose endpoint sits on
the workspace's network needs the sandbox's network position rather than the server's.

- POST the same stdin payload as the JSON body, under the same per-hook timeout.
- `${CLAUDE_PLUGIN_ROOT}` is substituted in the URL and in header values, as it is in a
  command string. Header values additionally interpolate `$NAME` / `${NAME}` for the
  variables `allowedEnvVars` lists — an allowlist, because a header is where a plugin puts
  a credential and a hook free to name any variable could read every one the runtime holds.
  An unlisted or unset variable is left as written rather than blanked: an empty credential
  is indistinguishable from a wrong one at the far end.
- The response body becomes the reply's `stdout`, so the whole of `process()` applies
  unchanged. Read up to the clamp rather than buffered whole and truncated after, because
  the length is chosen by the far end rather than by a process horsie started.
- A non-2xx status is `Failed`, naming the status. A transport error is `code: None` — the
  existing outage shape, already distinguished from a decision.
- **There is no exit-2 analogue over HTTP.** An HTTP hook blocks only through `decision` /
  `permissionDecision` in its body. Stated in the guide, because the difference is invisible
  otherwise.
- **A failure to answer still fails closed**, which is a deliberate divergence: the spec has
  a non-2xx continue, `denies()` treats `Verdict::Failed` as denying, and a `PreToolUse`
  webhook that 500s therefore blocks the calls it guards. Consistent with a command hook
  that cannot be spawned, and the safer half of the choice, but invisible unless stated —
  so the guide states it.
- Redirects are refused. reqwest follows ten by default and a 302 downgrades the POST to a
  GET, so the endpoint would be sent no payload at all and its answer read as a reply.
- One process-wide client, not one per invocation: a `PreToolUse` webhook runs on every
  tool call, and rebuilding the client each time discards the connection pool and redoes
  the TLS setup. The per-hook budget rides the request.

`reqwest` is already a runtime dependency.

## Testing

- Library (`support`): the halt is parsed and permitted per event; a halt on a side-effect
  event is `ignored`; `SubagentStop`'s arm derives its payload, subjects and record; an HTTP
  declaration is read where it used to be skipped.
- Runtime: an HTTP hook against a local test server produces the same record a command hook
  would; a 500 is `Failed`; a connection refusal is an outage.
- Server: a subagent's conclusion fires `SubagentStop` and not `Stop`; a blocking
  `SubagentStop` continues that subagent under the cap; a halting tool hook stops the main
  turn, fails the subagent node, fails the step; a halt beats a sibling `Stop` block, and
  ends that turn without also failing it — read off the journal, since the `TurnEnded` that
  follows hides the spurious `TurnFailed` in the status.
- Runtime: an allowed env var interpolates in a header in either spelling; an unlisted or
  unset one is left as written; a redirect is refused.
- Web: a halted record renders its reason.

## Not in scope

The eight events that remain unwired, each of which needs a call site horsie does not have
(`PostToolUseFailure`, `PostToolBatch`, `SessionEnd`, `StopFailure`, `TaskCreated`,
`TaskCompleted`, `Notification`, `CwdChanged`). `UserPromptExpansion` stays `NoConcept` until
#105's Phase 3 gives horsie slash commands.
