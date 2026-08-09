---
title: Agents, tools & subagents
description: What an agent is given, how a turn runs, and what happens when an agent delegates or asks a question.
kind: explanation
sidebar:
  order: 3
---

An **agent** is a model, a set of tools, and a transcript. A session has one to
begin with, and may end up with several.

## What a turn is

You send a message. The agent gets the transcript so far, its tool
definitions, and its instructions, and produces a reply — which may include
tool calls. Each call is dispatched to the runtime, its result is journaled and
appended, and the model is asked again. The turn ends when the model stops
asking for tools.

Everything in that loop is journaled as it happens, which is why the transcript
shows tool calls arriving one at a time rather than appearing once the turn is
over. See [Sessions & durability](/internals/sessions-and-durability/).

Thinking is the exception the UI treats differently: it is shown once the reply
finishes rather than streamed, and hidden until asked for.

## What the agent is given

**Tools that reach the runtime** — reading and writing files, running commands.
These are the reason a runtime exists.

**Session state** the agent needs to behave sensibly: its working directory,
its environment, what it is allowed to touch.

**Memory spaces**, if the session selected any, which the agent can read and
write across sessions.

**MCP tools**, from servers the session enabled, appearing as
`mcp__<server>__<tool>`. Servers configured in Settings run in the server
process; servers a plugin brought run in the runtime. See
[MCP servers](/using/mcp-servers/).

**Skills and agent types**, from the bundles the session selected. See
[Plugins & hooks](/internals/plugins-and-hooks/).

The set is fixed when the session starts. That is why a session's header
readouts are not editable: changing what an agent can reach mid-transcript
would make the transcript a record of two different agents.

## Asking a question

An agent can stop and ask. The question becomes a card in the transcript and
the session's status turns to **awaiting input** — the agent is parked, not
busy.

Two things about this are load-bearing. A question is journaled like anything
else, so a parked session survives a restart and is still parked afterwards.
And several questions raised at once are answered **together**, in one go,
because answering them one at a time would mean resuming an agent that is still
waiting on its siblings.

A [routine](/using/routines/) run is not offered the tool at all. An unattended
run that asked a question would park forever, so the agent is told up front
that it cannot ask, and briefed to decide instead.

## Delegating to a subagent

An agent can spawn another with its own transcript, its own turn loop, and its
own results. What comes back to the parent is the subagent's result — as its
own entry in the parent's transcript, not smuggled into a tool result.

The distinction matters when you read a transcript: it stays obvious which
agent did what, and a subagent's internal steps do not pollute the parent's
context.

Bundles can supply **agent types** — named, described, with their own
instructions — which the model picks by description the way it picks a skill. A
declared tool list can only ever *narrow* what the session already allows, so
installing a plugin can never hand an agent a tool you withheld.

## Where the model differences show

Providers differ in what they will carry across a turn, and horsie does not
pretend otherwise.

Reasoning models on the chat-completions wire have their thinking displayed but
never replayed on the next turn, because some backends reject it. The Responses
kinds replay the model's own reasoning in encrypted form instead, which is what
lets a reasoning model keep its thread through a long tool loop.

A model that refuses a pinned tool choice while thinking is on gets thinking
turned off for exactly those requests, which is why a forced-handoff agent on
such a model runs without thinking throughout. See
[Models & providers](/operating/models-and-providers/).

## Token usage is banked, not estimated

The usage a session shows is what providers reported, accumulated. It is
cumulative usage for the session, not a measure of how full the context window
is — the two are different questions, and conflating them makes a long session
look like it is about to fail when it is not. The context meter is a separate
readout.
