---
title: Agents, tools & subagents
description: What an agent is given, how a tool call is dispatched and bounded, and what happens when an agent delegates or stops to ask.
kind: explanation
sidebar:
  order: 4
---

An **agent** is a model, a set of tools, and a transcript. A session starts with
one and may end up with several.

The loop that drives it is [the harness](/internals/the-harness/); what the
agent can reach inside that loop is this page.

## The toolbox is fixed when the session starts

An agent gets:

- **Sandbox tools** — reading and writing files, running commands. The reason a
  sandbox exists.
- **Session state tools** — its working directory, its environment, what it may
  touch.
- **Memory tools**, for the spaces the session selected. →
  [Context & memory](/internals/context-and-memory/)
- **MCP tools**, from servers the session enabled, as
  `mcp__<server>__<tool>`. Servers you configured in Settings run in the server
  process; servers a bundle brought run in the sandbox.
- **Skills and subagent types**, from the bundles the session selected.

That set does not change mid-session. This is why the session header's readouts
are not editable: an agent whose capabilities changed halfway through would make
the transcript a record of two different agents, and every claim about
reproducing a run would stop being true.

## A tool call is bounded

Dispatch is not "run this and see what happens".

Each call carries a **deadline**, applied at dispatch — not somewhere above it,
where a hung call would take the whole turn with it. Output is **capped**, so a
command that prints a hundred megabytes truncates rather than poisoning the
context. A tool that fails returns an error the agent can read and react to;
failure is information, not an exception.

Two details that were bugs once and are now invariants: an error result bypasses
the output cap, because truncating the explanation of a failure is the worst
possible time to truncate; and end-of-output is not the same event as the child
process exiting, so a tool that closes its stream early does not get reported as
finished.

## Asking a question

An agent can stop and ask. The question becomes a card in the transcript and the
session's status turns to **awaiting input** — parked, not busy.

Three things about this are load-bearing.

**It is journaled like anything else**, so a parked session survives a restart
and is still parked afterwards, still holding the same question.

**Several questions raised at once are answered together**, in one go.
Answering them one at a time would mean resuming an agent still waiting on its
siblings, which is how you get a duplicated tool result and a wedged session.

**An unattended run is not offered the tool at all.** A [routine](/using/routines/)
run has nobody to answer, so a question would park it forever. The agent is told
up front that it cannot ask, and briefed to decide instead. This is the one
place where "human in the loop" is deliberately switched off, and it is why a
routine's prompt has to name the choice you want made when the obvious one is
ambiguous.

## Delegating to a subagent

An agent can spawn another with its own transcript, its own turn loop, and its
own tool results. The parent receives the subagent's result as its own entry in
the parent's transcript.

The reason to reach for one is usually context, not concurrency —
see [Context & memory](/internals/context-and-memory/#write-it-down-subagents).

Bundles can supply **subagent types**: named, described, with their own
instructions, which the model picks by description the way it picks a skill. A
type may declare a narrowed tool list, and it can only ever narrow — the list is
intersected with what the session already allows, so installing a bundle can
never hand an agent a tool you withheld.

## Where the model matters

Two provider differences change what an agent can do, rather than just how it
sounds.

**Reasoning across turns.** Whether the model's own thinking carries from one
turn to the next is a property of the wire, not a setting. It matters most
exactly where agents spend their time: long tool loops.

**Pinned tool choice.** Some models refuse a forced tool choice while thinking
is enabled. horsie handles that per model by turning thinking off for those
requests — which means an agent that pins a tool on every turn runs without
thinking throughout. Fine for a handoff step; a poor choice for reasoning work.

Both are covered in [Models & providers](/operating/models-and-providers/).

## Token usage is banked, not estimated

What a session reports is what providers said they charged, accumulated.

It is cumulative usage for the session, and explicitly **not** a measure of how
full the context window is. Conflating the two makes a long session look like it
is about to fail when it is not — the context meter is a separate readout,
because it answers a different question.
