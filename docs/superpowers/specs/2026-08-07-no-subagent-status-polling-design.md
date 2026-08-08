# Eliminate Subagent Status Polling — Design

Date: 2026-08-07
Status: Approved (design), pending implementation

## Goal

Stop agents from repeatedly calling `subagent_status` after `spawn_agent`.
A subagent's completed result or failure is already delivered automatically to
its parent as an injected message, so polling wastes turns and tool calls.

`subagent_status` remains available for exceptional inspection; this change
does not remove the tool or alter the session actor's completion delivery.

## Design

Align all model-facing delegation guidance at the three points an agent sees
it:

- `server/src/sessions/session_actor/system_prompt.md`, the general
  delegation policy;
- `server/src/sessions/session_actor/context.rs`, the subagent-specific
  prompt context; and
- `server/src/sessions/spawn_tool.rs`, the descriptions for `spawn_agent` and
  `subagent_status`.

The guidance will state that `spawn_agent` is asynchronous and that both
successful and failed terminal outcomes are delivered automatically to the
parent. After delegating, the parent should continue independent work and
incorporate the delivered result; if no independent work remains, it should
wait rather than poll.

The policy will prohibit repeated calls and status loops. It will explicitly
preserve `subagent_status` for either a progress report the user requested or
diagnosis of a suspected runtime/result-delivery problem.

## Scope

No change is made to:

- the `subagent_status` tool's availability, arguments, output, visibility,
  or authorization;
- `SessionActor` subagent lifecycle or persistence; or
- automatic terminal-result injection.

## Testing

Extend the focused context/toolbox unit tests to assert the generated prompt
and tool descriptions contain the no-poll policy and identify the exceptional
uses of `subagent_status`. This protects the behavior from prompt-text
regressions without adding actor-level tests for unchanged delivery behavior.

## Acceptance criteria

1. Main-agent and subagent prompt text no longer tells agents to check progress
   after spawning a subagent.
2. `spawn_agent` explains automatic delivery of success and failure outcomes.
3. `subagent_status` is described as an exceptional, non-polling tool for a
   user-requested update or diagnosis.
4. Focused tests validate the generated instructions and tool specifications.
