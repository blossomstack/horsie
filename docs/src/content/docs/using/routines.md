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
  https://horsie.example.com/api/routines/nightly-triage/run
```

**The timer**, if the routine has one and is not paused.

Pressing run does not disturb the schedule: the next firing stays where it was.

## Where the runs go

A routine's sessions are listed on **its own page**, newest first — never in
the session rail. A routine on a fifteen-minute timer would otherwise bury the
conversations you are actually having.

Each run is an ordinary session underneath, so opening one shows the full
transcript, tool calls and all.

Two consequences:

- **Deleting a routine deletes its runs.** Its page is the only place they are
  listed, so keeping them would leave sessions nothing can reach.
- **Runs are not prevented from overlapping.** A routine on a five-minute timer
  whose runs take ten minutes will have two in flight. Give the interval room.

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
