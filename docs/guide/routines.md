# Routines

A **routine** runs an agent against a fixed prompt — on a timer, from the API,
or whenever you press a button. Nobody has to be watching: a routine's runs work
from the prompt alone and report what they did when they finish.

Use one for the work you would otherwise remember to ask for. Triage the issue
queue every morning. Re-run the flaky suite hourly and summarise what broke.
Sweep a repo for stale dependencies once a week.

## What a routine is made of

```
routine = an agent preset  (runtime, model, repos, skills, MCP, memory)
        + a prompt         (the whole instruction each run gets)
        + a trigger        (manually · repeatedly · once, at a time)
```

The configuration lives in the **agent preset**, not in the routine. That is
deliberate: a preset already answers "how does this agent run?", and a routine
only has to answer "what should it do, and when?". Several routines can share
one preset, and editing the preset changes all of them at once.

## Creating one

1. **Agents → New agent** — if you don't already have a preset, make one. It
   holds the runtime vendor, model, repositories, skills, MCP servers, memory
   spaces and thinking effort every run will use.
2. **Routines → New routine.**
   - **Name** — a slug (`nightly-triage`). It is the id: routines are addressed
     by name in the API, and the name cannot be changed later.
   - **Agent** — the preset above.
   - **Prompt** — everything the run is told. Write it as a complete brief; see
     [Writing the prompt](#writing-the-prompt).
   - **Trigger** — one of:
     - *Only when I run it* — no timer; the run button and the API still work.
     - *Repeatedly* — every N minutes. The minimum is one minute.
     - *Once, at a time* — a single firing at an instant you pick.
   - **Timer active** — clear this to pause the schedule. Pausing never takes
     away the run button.

An agent preset a routine points at cannot be deleted while the routine exists —
the server refuses with a conflict rather than leaving a scheduled job with
nothing to run.

## Writing the prompt

A routine run **cannot ask you a question**. The `ask_user` tool is not offered
to it at all, and the agent is told why, because a question with nobody to
answer it would park the run forever.

So write the prompt the way you would brief someone who cannot reach you:

- Say what "done" looks like, and what to produce.
- Name the choice you want made when the obvious one is ambiguous — "if more
  than one release branch matches, take the newest".
- Say what to do when there is nothing to do. "If the queue is empty, say so and
  stop" beats a run that invents work.

Everything else about the agent — its tools, its workspace, its skills — is
exactly the same as an interactive session.

## Running one

- **Run now** on the routine's page. The run starts in the background; the page
  lists it immediately.
- **The API** — `POST /api/routines/<name>/run`, with the same credential as any
  other endpoint. A machine token (**Settings → Account →
  Machine tokens**) is how a script, a CI job, or a webhook receiver triggers a
  routine:

  ```bash
  curl -X POST -H "Authorization: Bearer $HORSIE_TOKEN" \
    https://horsie.example.com/api/routines/nightly-triage/run
  ```

- **The timer**, if the routine has one and is not paused.

Pressing run does not disturb the schedule: the next scheduled firing stays
where it was.

## Where the runs go

A routine's sessions are listed on **its own page**, newest first — never in the
sidebar's session list. A routine on a fifteen-minute timer would otherwise bury
the conversations you are actually having.

Each run is an ordinary session underneath: click it to read the full
transcript, tool calls and all.

Two consequences worth knowing:

- **Deleting a routine deletes its runs.** Its page is the only place they are
  listed, so keeping them would leave sessions nothing can reach. The confirm
  dialog says so.
- **Runs are not prevented from overlapping.** A routine on a five-minute timer
  whose runs take ten minutes will have two in flight. Give the interval room to
  finish.

## How the schedule behaves

- **Repeatedly** — the next firing is measured from the one that just happened,
  not from a fixed origin. A server that was down for a day comes back and runs
  once, not a day's worth of catch-up.
- **Once** — fires at its instant and never re-arms. An instant already in the
  past never fires; edit the routine to move it forward.
- **A run that fails to start** — an offline runtime, a model that was removed —
  is recorded on the routine's page as its last error, and the schedule still
  advances. A broken routine waits for its next interval rather than retrying
  every few seconds.

The timer is checked every 15 seconds, so a routine fires within a few seconds
of when it was due.
