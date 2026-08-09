---
title: What horsie is
description: An open-source managed agent harness — durable sessions, sandboxed execution and scheduled runs, on infrastructure you own.
kind: explanation
sidebar:
  order: 1
---

horsie is an **open-source managed agent harness**. You run it; it runs agents.

That sentence is doing a lot of work, so this page unpacks it. If you would
rather see it working first, the [quickstart](/start-here/quickstart/) takes
about ten minutes.

## Agent = model + harness

A useful agent is a model plus everything around the model. That everything has
a name now: the **harness**. It is the loop that assembles context, calls the
model, decides whether a tool call is allowed, dispatches it, writes down what
happened, and works out whether the job is done.

The model is the part you rent. The harness is the part that decides whether
renting it was worth anything — and it is the part horsie is.

horsie brings no model. You point it at providers you already pay for, with
keys you hold.

## The primitives, and which part of horsie owns each

A handful of nouns have settled as the way to describe systems like this. The
useful test is whether you can say which component owns each one; if you
cannot, what you have is a script rather than a runtime.

| Primitive | What it is | horsie's |
| --- | --- | --- |
| **Harness** | The loop that decides what happens next. The only part that makes decisions. | `horsie-server` |
| **Agent** | Reusable configuration: model, instructions, tools, MCP servers, skills. | an [agent preset](/using/routines/#create-one) |
| **Environment** | Where sessions run and what they run against. | an [environment](/using/environments/) |
| **Session** | One running agent, and the append-only record of everything it did. | a [session](/using/sessions/) |
| **Events** | The bidirectional stream — you send messages and answers, you receive turns, tool calls and status. | the SSE stream the browser and CLI read |
| **Sandbox** | The isolated place tool calls actually execute. Allowed to fail and be rebuilt. | a [runtime](/operating/cloud-vendors/) |
| **Checkpoint** | Resumable state, so recovery is not a replay from the beginning. | journal snapshots, and sandbox hibernation |
| **Trace** | The surface you review a run on afterwards. | the transcript |

Two of those are the same word horsie already used, which is not a coincidence
— the shape of this problem is converging, and horsie is built on the same cut.

## The cut that matters: three lifetimes, not one

The single design decision underneath everything else is that the **session**,
the **harness** and the **sandbox** are three separate things with three
separate lifetimes.

It is easiest to see by comparison with CI, because the resemblance to CI is
what most people reach for first — a job, a container, some steps, a result.

In CI, a run **is** a process. State lives in that process, so when it exits,
all that survives is a log. Isolation and execution are the same object: the
container *is* the run. That means you cannot pause without destroying the
workspace, you cannot resume without starting from the top, and an agent
running inside such a job has nowhere durable to keep its own state — its
transcript, a question it wanted to ask, a tool call that was half-finished.

In horsie, the session is a durable log rather than process memory. So:

- **A run can be paused and resumed.** Nothing important is in the process.
- **A run can be steered.** You can open a failed one, read exactly what
  happened, answer the question it parked on, and retry a single step against
  the workspace as the previous attempt left it.
- **The sandbox can be rebuilt underneath it.** It can hibernate when a session
  goes cold and come back when you return, without the run being lost.
- **The record outlives the machine.** The sandbox is disposable; the
  transcript is not.

CI hands you a log file and a red cross. This hands you a run you can walk back
into.

Scheduled and unattended work — [routines](/using/routines/) — inherits all of
that. A nightly job that fails at 3am is not a wall of log output the next
morning; it is a session you open, read, and continue.

## Managed, and owned

"Managed agent" describes a shape of product: you declare an agent and start a
session, and something else takes care of the loop, the sandbox, the durability,
the schedule and the record. You do not build a harness.

horsie is that shape with the ownership inverted. The harness runs on your
infrastructure. The sandbox is a machine you chose. The transcript is in your
database. The model keys are yours and nothing in the server's environment can
quietly lend a provider a credential it was not given.

Concretely, that gives you two things a service cannot:

**Your code never leaves.** The local runtime is a process you start on the
machine your work already lives on. It dials *out* to the server and holds the
connection open — no inbound port, no tunnel, no upload. The agent's tools then
run against your actual directories, on your actual machine.

**The harness is readable.** The interesting part of an agent is not the model,
it is the loop — how context is assembled, when a tool is refused, what gets
written down. In horsie all of that is source you can read, and change.

## What you get for the "managed" half

The features that make the difference between a script and something you leave
running:

- **Durable sessions** — journaled server-side, replayed on reconnect, resumed
  after a restart. → [Sessions & durability](/internals/sessions-and-durability/)
- **A sandbox per session** — your own machine, or a container or virtual
  machine the server builds and tears down. → [Runtimes & vendors](/internals/runtimes-and-vendors/)
- **Real repositories** — checked out with a short-lived, scoped token minted
  per session. → [GitHub repositories](/using/github-repositories/)
- **Reusable configuration** — agent presets and environments, so the same work
  runs the same way twice. → [Environments](/using/environments/)
- **Unattended runs** — routines on a schedule or an API call, reporting back a
  session rather than a log. → [Routines](/using/routines/)
- **Fixed-order orchestration** — workflows: a graph of steps sharing one
  workspace, branching on each step's result. → [Workflows](/using/workflows/)
- **An outer harness you build yourself** — skills, plugins, hooks and MCP
  servers, selected per session. → [Plugins & hooks](/internals/plugins-and-hooks/)
- **Human in the loop when it matters** — an agent can stop and ask, and the
  session waits, durably, until you answer. → [Agents, tools & subagents](/internals/agents-and-tools/)

## What it is not, and what it does not have

It is **not an IDE**. There is deliberately no file browser and no diff view —
you already have an editor open, and horsie's job ends where that begins. File
edits appear in the transcript as tool calls you can expand.

It does **not decide who your users are**. The server enforces that one account
cannot reach another's sessions, and leaves where accounts come from to the
deployment: one operator wants a password, another an identity-aware proxy in
front.

And there are things this category has that horsie does not have yet. They are
listed here rather than discovered later:

| Not yet | What exists instead |
| --- | --- |
| Approval tiers — marking a tool "always ask" and having the session pause | A `PreToolUse` hook can deny a call, and an agent can ask a question of its own accord. There is no built-in permission prompt. |
| A secret vault | Provider keys and integration credentials are stored write-only, but an environment's variables are plain and should be treated as readable. |
| Graded outcomes — a rubric and an independent grader deciding whether a run succeeded | A workflow step declares output fields and branches on them; nothing scores a run. |
| Versioned agents and memory | Presets and memory spaces are mutable; editing one changes what the next session gets. |
| OpenTelemetry traces | The transcript and per-turn token usage, read in the UI or streamed with `horsie session tail`. |

## Where to go next

- [Quickstart](/start-here/quickstart/) — from nothing to a running session.
- [The harness](/internals/the-harness/) — how the loop actually works.
- [Sessions](/using/sessions/) — what the chat view offers once you are in one.
