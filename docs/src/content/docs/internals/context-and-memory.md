---
title: Context & memory
description: Why a long run degrades, what horsie does about it, and the difference between what an agent is holding and what it has written down.
kind: explanation
sidebar:
  order: 5
---

Everything the model knows on a given turn, it knows because the harness put it
in the context window. Deciding what goes in — and what comes out — is most of
what separates an agent that works for five minutes from one that works for
five hours.

## Why a long run degrades

A context window is a budget, and it is also a signal-to-noise problem. As a
window fills, quality falls before it runs out: the answer is somewhere in
there, but so are forty tool results the agent no longer needs, and the thing
that matters is competing with all of them for attention. Long runs get worse
before they get truncated.

The three levers against it are all in use here: keep less, look at less at
once, and write things down outside the window.

## Keep less: compaction

A session's log grows forever; the context does not.

When the last prompt reaches 80% of the model's context window, the agent
summarises everything before a recent-history cut into a **compaction
boundary** and prompts from there instead. What the model is then handed is the
summary, the exact state it must not forget, and the most recent messages
verbatim. The summary is written with the previous summary already in the
prompt, so summarising is a fold rather than a re-derivation: no span is ever
summarised twice, and the tenth compaction costs what the first one did.

**Nothing is deleted.** A boundary is appended to the log like any other entry;
it only moves where the prompt starts. The whole transcript stays readable, on
both sides of every boundary. The record is complete; the working set is not.

Two things are carried across verbatim rather than summarised: the task list
and armed timers, pending questions, and which subagents are still running.
These are durable state either way, but the model only knows about them through
the tool calls in the history — so a compaction that summarised them would
leave an agent holding three open tasks with no idea it had any.

A model whose card declares no context window never compacts automatically:
there is no share of an unknown number to trigger on, and guessing would either
compact a session that had room or fail to compact one that did not. Sessions
can opt out entirely at creation, or from an agent preset.

Type `/compact` to do it by hand, optionally with instructions —
`/compact keep the migration details` adds a focus without discarding the rest.
It runs at the next turn boundary, so a turn in flight finishes first.

`/summary-n-fork` is the other way out of a full context, and it does the
opposite thing with the summary: rather than rewriting this conversation, it
starts a [second one](/using/sessions/#branch-a-conversation) with the summary
as its whole history, and leaves this one exactly as it was.

Separately and confusingly similarly named, the *journal* is periodically
snapshotted and compacted: a snapshot is a serialized state, and replay starts
from the newest one rather than from the first event. That keeps recovery
cheap, and is why the state a session serializes is a contract rather than an
implementation detail — a change to it has to be able to read what older
snapshots wrote. It has nothing to do with what the model sees.

## Look at less: progressive disclosure

Skills are the clearest example of the idea. A bundle's skills do not sit in
the context taking up room on the chance they are needed. What the model sees is
their names and one-line descriptions; the body loads when a skill is actually
picked.

That is what makes it reasonable to install a bundle of forty skills. The cost
of a skill you never use is one line.

The same shape appears in slash commands, which expand to a template only when
you send one, and in subagent types, which the model chooses from by
description.

## Write it down: subagents

A subagent is usually described as parallelism. Its more important job is
**context isolation**.

When work involves twenty tool calls whose intermediate output the parent will
never need — reading a wide directory, searching a large repository, grinding
through a migration — doing it in the parent's context spends the parent's
budget on rubble. Handing it to a subagent spends the *subagent's* budget
instead, and what comes back to the parent is the result.

The result arrives as its own entry in the parent's transcript rather than
smuggled inside a tool result, so reading a transcript keeps telling you which
agent did what, and a subagent's internal steps never pollute what the parent
is working from.

## Memory: what survives the session

Two different things get called memory, and horsie keeps them apart.

**Short-term memory** is the transcript: what has been said and done in this
session. It is durable, it is compacted, and it dies with the session.

**Long-term memory** is a **memory space**: notes the agent writes and reads
across sessions — project conventions, things learned, decisions taken. A
session selects which spaces it may read and write, the same way it selects
skills and MCP servers.

The trade-off worth knowing: memory is not progressively disclosed. A selected
space is in the context, not fetched on demand. Keep memory small and factual,
and put anything long or procedural in a skill, where it costs a line until it
is needed.

Memory spaces are mutable and unversioned. An agent that writes a wrong fact has
written a wrong fact; there is no history to roll back to.

## Reasoning across a turn

Whether a model's own thinking carries from one turn to the next is a property
of the provider, not a choice horsie makes.

On the chat-completions wire, reasoning traces are displayed but never sent
back, because some backends reject them. On the Responses wire the model's
reasoning is replayed in encrypted form, which is what lets a reasoning model
keep one thread of thought across a long tool loop.

If a run involves many tool calls and the model is a reasoning model, that
difference is worth more than it sounds. See
[Models & providers](/operating/models-and-providers/).

## What horsie does not do

There is no tool-output clearing policy, and no retrieval layer that decides
which files belong in context. Compaction is on or off, with no threshold to
tune: the right share of a window is a property of the model rather than of a
session, so it stays a server constant that can be retuned centrally instead of
a number frozen into saved presets. Otherwise context is assembled from the
session, the selected memory spaces, and what the agent chose to read.

The lever you have is the outer harness: fewer tools per session, skills instead
of instructions, subagents for anything wide, and memory kept short.
