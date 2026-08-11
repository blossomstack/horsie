---
title: Plugins & hooks
description: How you build an outer harness on top of horsie's, where bundle content lives, and why a hook that could not run denies the call it guarded.
kind: explanation
sidebar:
  order: 6
---

The **inner harness** is what ships: the loop, the sandbox tools, the subagent
machinery, the journal. The **outer harness** is what you assemble on top for
your own work — and bundles are the main way you assemble it.

A bundle can bring skills, subagent types, slash commands, hooks and MCP
servers. Between them they cover both kinds of control worth having: guides that
steer before the fact, and sensors that catch after it. See
[The harness](/internals/the-harness/#guides-and-sensors).

## The server owns the content, the sandbox runs it

Installing a bundle clones it once and stores it as a content-addressed zip. Its
catalogue — which skills, commands and subagent types it declares — is read at
install time and kept in the database.

Nothing scans a sandbox to find out what a bundle offers. The catalogue is a
database read, so the UI can list a bundle's contents with no session running,
and two sessions selecting the same bundle provably get the same bytes.

When a sandbox starts, the harness hands it the `{name, hash}` refs its session
selected plus a short-lived token. The sandbox fetches each zip over its own
outbound connection, checks the hash, and unpacks it into a directory of its
own — deleted with the session.

Hooks, bundle MCP servers, and anything a skill runs execute **there**, next to
the workspace, under the same confinement as the rest of the sandbox. Running a
bundle's local MCP server in the server process would be both wrong and a way
for a bundle to execute commands where the harness lives.

That is the difference from the MCP servers you add in Settings, which do run in
the server process and can therefore do interactive OAuth.

## Hooks are participants

A hook can change what happens. `PreToolUse` can deny a call or rewrite its
input; `PostToolUse` can rewrite the output before the agent reads it;
`UserPromptSubmit` can refuse a prompt; `Stop` can refuse to let a turn end and
send the agent back round. Any hook can halt the agent outright with
`continue: false`.

Because they participate, they are journaled. Hook records live in the agent's
transcript as their own kind of entry rather than folded into the tool call they
wrapped — so reading a transcript tells you a hook intervened, and which one.
An outer-harness bug should be as visible as an inner-harness one.

This is also the closest horsie has to an approval tier. There is no permission
prompt that pauses a session and waits for a human before a dangerous call; a
hook decides, deterministically, with no one to ask. A bundle declaring
`permissionDecision: "ask"` has that recorded and treated as allow.

## A guard that could not run has not permitted anything

An HTTP hook that does not answer — a non-2xx, an unreachable endpoint, a
timeout — is recorded as having failed. For `PreToolUse`, that **denies** the
call it was guarding.

This is a deliberate divergence from the upstream specification, which lets an
HTTP hook failure through. The argument for failing closed is that a
`PreToolUse` hook exists to withhold permission, and a hook that never ran has
withheld it. The argument against is that a flaky webhook now breaks your
session.

Both are real, which is why the guidance is narrow rather than general: point a
`PreToolUse` webhook only at an endpoint you are willing to have gate your tool
calls. Command hooks have an exit code and do not have this ambiguity.

## Why only ten events

horsie runs ten of the thirty-one events the specification documents. Eight more
are understood and not yet fired. The remaining thirteen have no horsie
equivalent — permission prompts, worktrees, file watching, agent teams, MCP
elicitation, the display layer. Each would need a subsystem, not a hook.

`PreCompact` and `PostCompact` joined the first list when compaction landed.
`PreCompact` is the last chance to write something down that the summary would
otherwise become the only record of, and a hook that blocks abandons the
compaction — the turn then runs uncompacted, which is worse than compacting but
better than compacting past a guard that was about to save something.

A bundle declaring an event horsie cannot fire still installs, and the events it
cannot run are **named** rather than silently ignored. Silently ignoring them
would make a bundle whose guard never fires look exactly like one whose guard
passes — which is the worst failure mode a sensor can have.

## Trust is not partitioned

Installing a bundle is an account-level act, and the software it brings runs
with the sandbox's privileges. There is no boundary *within* the sandbox that
would make an untrusted bundle safe to select for one session and not another.

"Who installed this bundle" was never a privilege question, and treating it as
one would suggest a boundary that does not exist. Install bundles you trust.
