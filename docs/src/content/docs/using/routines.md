---
title: Routines
description: Run an agent against a fixed prompt on a schedule, from the API, or on demand.
kind: how-to
sidebar:
  order: 6
---

A **routine** runs an agent against a fixed prompt — on a timer, from the API,
or whenever you press a button. Nobody has to be watching: a run works from the
prompt alone and reports what it did when it finishes.

Use one for the work you would otherwise remember to ask for. Triage the issue
queue every morning. Re-run the flaky suite hourly and summarise what broke.
Sweep a repository for stale dependencies once a week.

## It is not a CI job

The resemblance to CI is close enough to be worth naming, because the
difference is the reason to use this instead.

A CI run **is** a process. Its state lives in that process, so when it ends all
that survives is a log, and the only thing you can do with a failed one is read
the log and run it again from the top.

A routine run is a [session](/using/sessions/). It is an append-only record, so
a run that failed at 3am is something you open the next morning and *continue*:
read exactly which tool call went wrong, see the raw output it got, send a
message, or fix the prompt and re-run knowing what happened. A run that stopped
half way did not lose the half it did.

That is the whole argument. Everything below is mechanics.

## What one is made of

```text
routine = an agent preset  (model, skills, MCP, memory)
        + an environment   (a runtime and its repos, or a saved environment)
        + a prompt         (the whole instruction each run gets)
        + a trigger        (manually · repeatedly · once · daily · weekly · monthly · yearly)
```

The agent configuration lives in the **preset**, not the routine. A preset
already answers "how does this agent think?", so a routine only has to answer
"what should it do, where, and when?". Several routines can share one preset,
and editing the preset changes all of them at once.

The environment lives on the **routine**, because where work happens is a
property of the run rather than of the agent. It is required — a routine that
cannot say where it runs has nowhere to run.

## Create one

1. **Agents → New agent**, if you do not already have a preset. It holds the
   model, skills, MCP servers, memory spaces and thinking effort every run will
   use. It names no runtime and no repositories: those come from the routine.
2. **Routines → New routine.**

   - **Name** — a slug, like `nightly-triage`. It is the id, and cannot be
     changed later.
   - **Agent** — the preset above.
   - **Environment** — a connected runtime, with repositories if it builds its
     own workspace, or a saved [environment](/using/environments/). A run whose
     environment has gone fails and says so on the routine's page.
   - **Prompt** — everything the run is told. See below.
   - **Trigger** — *only when I run it*, *repeatedly* (every N minutes,
     minimum one), *once at a time you pick*, or a **daily / weekly / monthly /
     yearly** calendar trigger at a wall-clock time. Each calendar trigger
     carries its own IANA timezone, defaulting to your browser's.
   - **Timer active** — clear it to pause the schedule. Pausing never takes
     away the run button.

An agent preset a routine points at cannot be deleted while the routine
exists. The server refuses with a conflict rather than leaving a scheduled job
with nothing to run.

## Write the prompt

A routine run **cannot ask you a question.** The `ask_user` tool is not offered
to it at all, and the agent is told why — a question with nobody to answer it
would park the run forever.

So brief it the way you would brief someone who cannot reach you:

- Say what "done" looks like, and what to produce.
- Name the choice you want made when the obvious one is ambiguous: *if more
  than one release branch matches, take the newest*.
- Say what to do when there is nothing to do. *If the queue is empty, say so
  and stop* beats a run that invents work.

Everything else about the agent — its tools, its workspace, its skills — is
exactly as in an interactive session.

## Run one

**Run now** on the routine's page starts a run in the background and lists it
immediately.

**The API** takes the same credential as any other endpoint. A machine token
from **Settings → Account → Machine tokens** is how a script, a CI job, or a
webhook receiver triggers one:

```bash
curl -X POST -H "Authorization: Bearer $HORSIE_TOKEN" \
  https://horsie.example.com/api/p/<project>/routines/nightly-triage/run
```

**The timer**, if the routine has one and is not paused.

Pressing run does not disturb the schedule: the next firing stays where it was.

## Where the runs go

A routine's sessions are listed on **its own page**, newest first — never in
the session rail. A routine on a fifteen-minute timer would otherwise bury the
sessions you are actually having.

Each run is an ordinary session underneath, so opening one shows the full
transcript, tool calls and all.

Two consequences:

- **Deleting a routine deletes its runs.** Its page is the only place they are
  listed, so keeping them would leave sessions nothing can reach.
- **Runs are not prevented from overlapping.** A routine on a five-minute timer
  whose runs take ten minutes will have two in flight. Give the interval room.

## Let a routine improve an agent

A routine can read what an agent has been doing and edit the agent. Nothing
does this out of the box — you write the routine — but everything it needs is
there.

Tick **Let a tuning agent improve this preset** on any agent you want tuned.
Nothing happens until you point a routine at it, and an agent that has not been
ticked is invisible to one that looks: opting in is an act, because the thing
being granted is one agent rewriting another's instructions with nobody
watching.

Then give the tuning routine's own preset the horsie tool group, and it can:

- **Find the agents that opted in** — `horsie_agents` lists them, `tunable` and
  all.
- **Find their runs** — `horsie_agent-runs` answers every run of a named
  preset, across sessions, workflow steps and subagents alike. Filter by
  `status` for the ones that failed, or by `since_ms` for what has happened
  since it last looked.
- **Read one** — `horsie_sessions` takes the `sessionId` and `agentId` a run
  reports. Narrow the read or it will not fit: `kinds` picks which entries come
  back, `withoutThinking` drops the model's reasoning, and `search` finds where
  something was said without reading the transcript to get there.
- **Write the agent back** — `horsie_agents` replaces the preset,
  `horsie_memories` curates what it has remembered, and the authoring tools
  edit the skills it loads.

### Undoing a bad tune

Presets and memories keep every version. `horsie_agents` answers `revisions`
and `restore`, and a restore is recorded as a new version rather than a rewind
— so the change being undone stays in the history.

Reads carry a `revision`. Pass it back as `expectedRevision` when you write and
the write is refused if anything changed in between, which is what stops a
routine that read an agent an hour ago from silently reverting an edit you made
since. There is no merge for that case: the two writers disagree about what the
agent should say, so the later one is told to read again.

## How the schedule behaves

**Repeatedly** measures the next firing from the one that just happened, not
from a fixed origin. A server down for a day comes back and runs once, not a
day's worth of catch-up.

**Once** fires at its instant and never re-arms. An instant already in the past
never fires; edit the routine to move it forward.

**Calendar triggers** fire at their wall-clock time in the routine's own
timezone, and the next firing is the next occurrence after the previous one —
so a server that was down while one came due runs **once, late**, never a
backlog. A month without the day you picked is skipped, and 29 February recurs
only in leap years. Across a daylight-saving change the wall-clock time is
kept: a time that does not exist that day fires at the shifted time, and one
that occurs twice fires once.

**A run that fails to start** — an offline runtime, a model that was removed —
is recorded on the routine's page as its last error, and the schedule still
advances. A broken routine waits for its next interval rather than retrying
every few seconds.

The timer is checked every 15 seconds, so a routine fires within a few seconds
of when it was due.
