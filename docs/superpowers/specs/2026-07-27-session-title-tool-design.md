# Session title tool for interactive agents

## Context

Interactive sessions currently receive a display title by deriving the first
line of the first user message in `SessionActor::on_user_message`. That gives
the sidebar an immediate label, but the label is often just the raw opening
sentence rather than a concise description of the conversation's purpose.

Requested: expose a `set_session_title` tool to the session agent, instruct it
in the system prompt to set a concise title after receiving the first user
message, and update the live session list when the title changes. The existing
first-message derivation remains as the immediate fallback; the model may
replace it with a better title on any turn.

## Existing architecture

- `server/src/sessions/session_actor.rs` owns the interactive session's
  lifecycle and derives a fallback title from the first user message when
  `spec.name` is `None`.
- `SessionActor` reports that fallback title to
  `SessionSupervisorCommand::SessionNamed`; the supervisor persists
  `SessionSupervisorEvent::SessionNamed` and folds it into `SessionRecord.spec`.
- `SessionContextProvider` composes each run's toolbox by wrapping the
  runtime/MCP toolbox in `AskUserToolbox`.
- `server/src/sessions/system_prompt.md` is the baseline system prompt for
  every session agent.
- `models/fluorite/session.fl` defines the global session-list SSE wire type;
  `clients/web/src/hooks/useSessions.ts` consumes it to keep the sidebar and
  open session detail live.

## Decision

### Server-owned tool

Add a session-server-owned `SessionTitleToolbox` in
`server/src/sessions/title_tool.rs`, and compose it with the existing session
toolbox in `SessionContextProvider`:

```text
SessionTitleToolbox
  -> AskUserToolbox
    -> runtime tools + MCP tools
```

The wrapper advertises `set_session_title` and executes that call itself. All
other names and specs delegate unchanged to the inner toolbox. This keeps
session metadata ownership in the server and requires no sandbox/runtime tool
protocol changes.

`SessionTitleToolbox` holds an `ActorRef<SessionCommand>` for the owning
session. Executing `set_session_title` sends a new
`SessionCommand::SetSessionTitle { title, reply }` and waits for the result.
The command handler validates and normalizes the title, then asks the
supervisor to persist the rename. `SessionActor.spec.name` changes only after
the supervisor acknowledges a durable write.

The supervisor rename command gains post-persist acknowledgement using the
actor runtime's existing `CommandEffect::and_ack` mechanism, so the tool
reports success only after `SessionSupervisorEvent::SessionNamed` has been
durably journaled. A journal failure is returned as a tool error rather than
reported as a successful rename, and it leaves both the supervisor registry
and the session actor's local title unchanged.

### Title mutation semantics

Any session title may be changed at any time. A name supplied at session
creation is only the initial title; it does not lock the session. The latest
successful `set_session_title` call wins, whether it replaces an auto-derived
title, an earlier model-set title, or a caller-supplied creation name.

No `title_locked`, provenance, or migration state is added. Recovery continues
to replay the existing `SessionCreated` and `SessionNamed` supervisor events.

### Tool contract

Tool name: `set_session_title`.

Description requirements:

- State that the tool renames the session at any point.
- State that the latest successful call wins.
- Ask for a concise, specific, single-line title.

Input schema:

```json
{
  "type": "object",
  "required": ["title"],
  "properties": {
    "title": {
      "type": "string",
      "minLength": 1,
      "maxLength": 60,
      "description": "A concise single-line session title, at most 60 characters. The latest successful call renames the session."
    }
  }
}
```

Server-side validation is authoritative:

- Trim surrounding whitespace.
- Reject an empty title after trimming.
- Reject `\n` and `\r` so the title remains a single-line label.
- Reject more than 60 Unicode characters, matching the existing fallback-title
  limit.
- Return a short success string containing the accepted title.
- Return validation or journal failures as ordinary tool errors so the model
  can correct its call or report the failure.

### System prompt

Add a short `## Session title` section to
`server/src/sessions/system_prompt.md` instructing the agent to call
`set_session_title` on the first turn with a concise, specific title. The
instruction should explain that the server may already have set a fallback
title from the first user message, and that the tool should improve it when a
clearer title is apparent. It should also permit calling the tool on a later
turn when the conversation's purpose changes.

### Dedicated live title event

Keep title changes separate from status changes by changing the global session
feed from a struct to a tagged union:

```text
struct GlobalSessionStatusEvent {
    session_id: String,
    status: SessionStatusKind,
    reason: Option<String>,
}

struct GlobalSessionTitleEvent {
    session_id: String,
    name: String,
}

#[type_tag = "type"]
union GlobalSessionEvent {
    StatusChanged(GlobalSessionStatusEvent),
    TitleChanged(GlobalSessionTitleEvent),
}
```

Existing status updates publish `StatusChanged`. Handling the supervisor's
rename command persists the existing `SessionSupervisorEvent::SessionNamed`
journal event and acknowledges the durable write. After receiving that
acknowledgement, the session actor sends a separate internal
`PublishSessionTitle { id, name }` command; the supervisor publishes the
dedicated `TitleChanged` frame in response. This ordering prevents a failed
journal write from broadcasting a title that was never durably accepted.

Regenerate the web client's fluorite TypeScript types. Update
`applyGlobalEvent` in `clients/web/src/hooks/useSessions.ts` to:

- Apply `StatusChanged` exactly as it handles today's global event.
- Apply `TitleChanged` to both the session-list cache and the open
  session-detail cache.

Keep the existing optimistic first-message title in the web UI. It preserves
immediate feedback before the model responds; the dedicated `TitleChanged`
frame later replaces it with the model's improved title.

## Error handling and races

- Tool input validation happens in the session actor's command handler, not
  only in the JSON schema, so all callers receive the same rules.
- If two title calls race, actor mailbox ordering determines the durable
  order; the later processed `SessionNamed` event is the final title.
- A failed supervisor journal write leaves registry state unchanged and is
  surfaced to the tool caller.
- The global `TitleChanged` frame is live-only. A client that misses it can
  recover from the normal session list/detail endpoints, which read the
  journaled supervisor state.

## Testing

Co-located Rust unit tests cover:

- `SessionTitleToolbox` advertises the new tool and delegates unrelated specs
  and calls unchanged.
- Valid titles update the session and return a success message.
- Empty, multiline, and over-60-character titles are rejected without a
  mutation.
- A model-set title can replace an auto-derived first-message title, an
  earlier model-set title, and a name supplied at session creation.
- `SessionNamed` still folds into supervisor state and is restored from the
  journal.
- A successful rename publishes the dedicated global `TitleChanged` event.
- The supervisor rename acknowledgement waits for the journal write.
- The session system prompt includes the title instruction.

Frontend verification:

- Regenerate fluorite TypeScript types.
- Typecheck the web client.
- Do not add a new frontend unit-test harness for this change; the web client
  currently has no unit-test script. Rely on exhaustive generated types,
  typechecking, and the existing e2e infrastructure.

Run the standard pre-PR gate:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```
