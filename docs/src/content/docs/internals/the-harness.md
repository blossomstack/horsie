---
title: The harness
description: What the orchestration loop owns, why the session and the sandbox are separate from it, and where the inner harness ends and yours begins.
kind: explanation
sidebar:
  order: 1
---

The harness is the part of an agent that is not the model: the loop that
assembles context, calls the model, decides whether a tool call may proceed,
dispatches it, records the result, and works out whether the job is done.

In horsie the harness is `horsie-server`. This page is what it owns, what it
deliberately does not, and where you take over.

## The loop

A turn is one pass of the same short loop:

1. **Assemble the context.** The transcript so far, the tool definitions the
   session is allowed, its instructions, its memory.
2. **Call the model.** Streaming, always — the browser is reading the reply as
   it is produced, and a backend that cannot stream is not supported.
3. **Take what came back.** Text is appended. Thinking is recorded and shown
   once the reply finishes. Tool calls go to step 4.
4. **Dispatch each tool call** into the sandbox, with a deadline, and journal
   its result — the raw result, not a summary.
5. **Go back to step 1** with the results appended, until the model stops
   asking for tools.

Everything in that loop is written to the session as it happens, which is why
the transcript shows tool calls arriving one at a time rather than appearing
once the turn is over.

### Progress during interactive work

An agent that can call `ask_user` is working in an interactive context: a person
may be watching while it uses tools. The horsie harness gives those agents a static
instruction to include a brief visible sentence before substantial tool work
and at meaningful phase changes. The sentence says what the agent is doing and
why; it is not raw chain-of-thought or a command-by-command log.

The instruction is part of the session context assembled alongside the
`ask_user` capability and remains unchanged between turns, preserving the
provider's system-prompt cache. Subagents, unattended sessions, and
non-interactive workflow steps remain quiet.

## The harness is the only part that decides

This is the load-bearing rule, and it is why permissions do not live in a
prompt.

Telling a model "do not touch the production database" is a request. A model
that complies is a model that happened to comply. The harness deciding a tool
call may not proceed is a fact about the system.

So the decisions live here: which tools a session has at all, which of them a
`PreToolUse` hook may refuse, what the deadline on a dispatch is, when a run
has exhausted its budget. The model proposes; the harness disposes.

The honest limit: horsie has **no permission prompt**. There is no "always ask"
tier that pauses a session before a dangerous tool call and waits for you. What
exists is a hook that can deny a call outright, and an agent that can choose to
ask a question. If a bundle's hook declares `permissionDecision: "ask"`, horsie
records that and treats it as allow, because there is nothing to ask with. See
[Plugins & hooks](/internals/plugins-and-hooks/).

## Three lifetimes

The harness, the session and the sandbox are separate on purpose.

**The session outlives the harness process.** It is an append-only log in the
database, not state in memory. A server that restarts replays what it has;
there is no separate recovery path, and therefore no recovery path that only
gets exercised after a crash. → [Sessions & durability](/internals/sessions-and-durability/)

**The sandbox outlives individual turns, and no longer.** It is where tool calls
execute and it is allowed to fail. It can hibernate when a session goes cold, be
rebuilt, or be swapped, without the run being lost — because nothing important
was in it. → [Runtimes & vendors](/internals/runtimes-and-vendors/)

**The harness owns neither.** It holds a session actor in memory while there is
work, offloads it when there is not, and acquires a sandbox when a turn needs
one. An idle session costs a row, not a process.

The practical consequence is that a run is a thing you can walk back into: open
it, read what happened, answer the question it parked on, retry one step against
the workspace as the last attempt left it. A run whose state lives in a process
can only be restarted.

## Inner harness, outer harness

The **inner harness** is what ships: the loop above, the file and shell tools,
the subagent machinery, context management, the journal.

The **outer harness** is what you assemble on top for your own work, and horsie
treats it as first-class rather than as configuration:

| You add | It becomes |
| --- | --- |
| [Skill and plugin bundles](/using/skills-and-plugins/) | skills, slash commands, subagent types, hooks |
| [MCP servers](/using/mcp-servers/) | more tools, per session |
| [Agent presets](/using/routines/#create-one) | a reusable agent: model, instructions, tools, effort |
| [Environments](/using/environments/) | a reusable place to run: sandbox, repositories, variables, setup |
| [Hooks](/internals/plugins-and-hooks/) | deterministic checks around every tool call and turn |
| [Workflows](/using/workflows/) | a fixed order over several agents |

The distinction matters because the two fail differently. An inner-harness bug
is ours. An outer-harness bug is a hook you wrote denying a call you needed —
which is why hook records are in the transcript rather than in a log file
somewhere.

## Guides and sensors

A useful way to think about what you put in the outer harness: two kinds of
control, doing opposite jobs.

**Guides** steer before the fact — instructions, skills that load when relevant,
a preset that fixes the model and effort, an environment whose setup steps
leave the workspace in a known state.

**Sensors** catch after the fact — a hook that runs a linter on every edit, a
test suite the agent is told to run, a workflow step whose only job is to check
the previous step's work.

Guides are cheaper and weaker; sensors are the ones that actually hold. A
convention written in a prompt is advice. The same convention enforced by a
`PostToolUse` hook is a constraint the run cannot talk its way past.

## What the harness does not do

It does not run tool calls. Those happen in the sandbox, which may be a machine
the server has no other access to.

It does not hold your credentials for you. Provider keys and integration
credentials are stored write-only and never returned by the API, but there is no
vault: an [environment's](/using/environments/) variables are plain values, and
the server refuses only its own reserved names.

It does not grade a run. A workflow step declares output fields and a condition
branches on them, but nothing independently decides whether the work was good.

It does not emit OpenTelemetry spans. The transcript and per-turn token usage
are the observability surface; `horsie session tail` streams them to a file.
