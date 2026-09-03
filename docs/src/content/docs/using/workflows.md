---
title: Workflows
description: Chain agents into a graph of steps that share one workspace and branch on each step's result.
kind: how-to
sidebar:
  order: 8
---

A **workflow** is a graph of steps. Each step runs one of your agent presets
against a fixed instruction, and the **outcome** it reports decides which step
runs next. Every step in a run shares one workspace, so what one writes to disk
the next one reads.

Use a workflow when the order matters and you want to fix it yourself. If you
would rather let the model decide how to split the work, give one agent the
whole job and let it spawn subagents instead.

## Define one

**Workflows → New workflow.** The editor lists the definition and one row per
step down the left, and shows whichever you select on the right.

Each step needs:

- **Agent** — one of your presets. It supplies the model, MCP servers, memory
  spaces and thinking effort.
- **Prompt** — the step's instruction. Whatever the step is handed — the run's
  input for the first step, the previous step's result for the rest — is
  appended below it.
- **Outcomes** — how the step can end. It picks one, and it is the only thing
  transitions read. Leave them alone and a step reports `success` or `failure`.
  Each value needs a description: that is what the model reads to choose
  between them.
- **Result fields** — anything else the step reports back, each with a type and
  a description. Optional; every result already carries its outcome and a
  written summary.
- **Can ask the person** — whether this step may stop and ask a question.
  Without it the step has no way to ask and must decide for itself.
- **Goes to** — where to hand off, and for which outcomes.

**Limits** are optional: how many turns the step may take before it fails, and
how many times a transient provider error is retried within it. Leave them
blank unless a step has earned an opinion.

**Visualize** swaps the panel for the graph, which redraws as you type;
choosing a node opens that step. Steps can be dragged into another order, which
changes how the list reads and nothing about how the run executes — the start
step and the transitions decide that.

### What a step returns

Every step finishes by submitting a result, and every result carries two things
whether you ask for them or not:

- **`outcome`** — one of the values the step declares. Transitions read this
  and nothing else.
- **`description`** — a written summary of what the step did. This is what the
  next step is handed, so it is worth being specific about in the prompt.

Anything else you declare as a result field comes along too, and shows up under
the description when the next step reads it.

### Transitions

A transition names the outcomes it is taken for — `outcome in [p0, p1]`, or
`outcome not in [wontfix]`. Transitions are tried in order and the first match
wins. A row that names no outcome is the catch-all — put it last. If nothing
matches, the run finishes and that step's result is the run's.

A transition can only name outcomes its step actually declares, and the editor
offers them as checkboxes for that reason. Saving one that names anything else
is refused: at run time a filter that matches nothing is indistinguishable from
a step that meant to end the graph, which is exactly how a typo used to pass
for success.

### Loops

A transition may point back to an earlier step; the graph draws it as a dashed
curve.

Loops are bounded by the workflow's **step budget** — the most steps one run
may execute, set on the definition, 1,000 if left blank. It is the only thing
stopping a loop whose outcome never changes: a run that hits it fails with
`step budget exhausted` rather than spinning forever. A run snapshots the
budget when it starts, so changing it never affects a run under way.

## Run one

Press **Run** on the workflow's page. That opens the new-session page with the
workflow selected, where you choose the environment and type the input the
first step is handed.

A run takes its model, skills, MCP servers and memory from each step's own
preset, so those controls are not offered while a workflow is selected. What
the definition deliberately does not hold is the **environment** — which
runtime hosts the run, and what is cloned into its workspace. That belongs to
the invocation, so one workflow can be run against different machines and
checkouts without editing it.

From the CLI:

```bash
horsie workflow run fix-bug --input "the build is red on main" --vendor velos
```

A run **is a session.** It appears in the rail alongside the others, annotated
with the workflow it came from, and every session command works on it.

```console
$ horsie workflow status 3f1a2b4c-…
fix-bug  running  1,204 tokens

#    STEP                 TRY  STATUS      AGENT
0    triage               1    concluded   6e3c20c8-…
1    fix                  1    running     8a91f0d2-…

Not reached: file
```

Once it finishes the same command prints what the run produced — the last
step's result, which is the run's.

A run is a session, and its page is the session page: the same header, the same
controls, the same three views, the same graph. It opens on the graph, where a
run draws as its steps in the order they ran. The transcript and timeline keys
are offered but switched off — a run has no transcript of its own, because it
*is* its steps.

Selecting a node opens the panel every agent gets: what it is, what it cost,
what it produced, and the keys to open its transcript or run it again. That
holds for the run itself, which is the leftmost node — its result reads there,
in the same place a step's does. Opening a step goes to its own page: the
ordinary session view scoped to one agent, where all three views work. To
follow one from a terminal, take its agent id from `workflow status`:

```bash
horsie session tail 3f1a2b4c-… --output ./run.jsonl --agent 8a91f0d2-…
```

## When something goes wrong

**A step failed.** The run stops there. Read the step's page for why, then
press **Retry** on that node.

**A step asked a question.** Only a step marked **can ask the person** has the
tool to do it, and only when someone is there to answer. The run's page says
which step is waiting; **Answer it** opens that step. Answering resumes it and
the run carries on — asking pauses a step, it does not end it.

**A step is waiting on something.** A step that has spawned subagents, or armed
a timer, may end a turn without finishing; it stays running until whatever it
is waiting for wakes it. What it may not do is stop with nothing pending and no
result — that gets it one reminder, then one forced attempt, then the step
fails saying it never submitted.

**You interrupted it, or the server restarted mid-run.** The step that was
running is marked cancelled and the run goes back to **idle** rather than
resuming by itself — nobody knows how far that step got, so continuing is your
decision. The run's page says which step stopped and offers to retry it.

A run that ran to completion says **finished** instead, which is how a list of
past runs tells the two apart. Nothing but **unrecoverable** is a dead end: a
finished or failed run can still be moved by retrying a step.

### Retrying is not a rollback

A retry re-runs the step against **whatever the previous attempt left on the
workspace**. Files it created are still there; commits it made are still made.
If a step is not safe to run twice, say so in its prompt, or have it check
before it acts.

Retries append. The earlier attempt stays readable, and the node shows a count.

## What a step can and cannot do

A step can read and write the shared workspace, call its preset's MCP servers,
use its memory spaces, arm timers, spawn subagents of its own, and invoke
other workflows.

A step cannot rename the session, and a run takes no messages — sending one is
answered `409`. A workflow works from its definition; if you want to talk to
it, you want a session.

## Invoking a workflow from an agent

Every agent — a session's main agent, a subagent, a workflow step, a sub session —
carries an `invoke_workflow` tool whenever the server has workflows saved. The
tool lists them by name and description, which is how the model knows what
exists and when one fits; with none saved the tool is not offered at all.
Calling it starts a run of a saved workflow **inside the same session**,
sharing its workspace, and returns immediately with the run's id. When the run ends, its final result (or failure) is
delivered back into the invoking agent's session, exactly the way a
subagent's report is. The agent does not wait: it continues with other work,
or rests until the report wakes it.

This composes with everything else, because each edge is the same edge:

- The main agent can spawn subagents and invoke workflows.
- A subagent can spawn subagents and invoke workflows.
- A workflow's step can spawn subagents and invoke workflows — including the
  workflow it is itself a step of.

The rules that bound the tree:

- **Depth.** Every delegation edge counts — a spawn and an invocation alike —
  and the chain stops at four. This is also the recursion bound: a workflow
  that invokes itself gets four levels and the fifth is refused.
- **Live runs.** One session hosts at most eight runs that have not finished.
- **One step at a time, per run.** Sibling runs progress concurrently; within
  one run the graph's order holds, as always.
- **An invoked run never moves the session's status.** Its phase is its own;
  its invoker hears the result as a message and decides what it means. Only
  the session's *root* — the session, or the run the session was created
  as — is what the session list shows.
- **Stopping an agent stops its delegation.** Stop a subagent or retry a step
  and everything running under it — subagents, invoked runs, their steps —
  is cancelled with it, each reporting `stopped before it finished` to its
  own parent.
- **An agent is not done while its delegation runs.** A subagent whose turn
  ends while a child subagent or an invoked workflow is still running parks
  rather than concluding; its parent hears exactly one report, when the whole
  subtree is done.

The run resolves its steps' presets when it is invoked, snapshots them, and
never re-reads them — the same rule a run started from the Workflows page
follows. One caveat: a run invoked mid-session runs on the session's existing
runtime, so its steps can only use skill bundles that runtime already has.

## Skills across a run

A run provisions the union of the skill bundles named by every step's preset,
because all the steps share one runtime and its bundle set is fixed when it
starts. Two consequences: if any step's preset selects a bundle, that selection
replaces the host plugin library for the whole run, including for steps whose
presets selected nothing; and every step can reach every bundle in the union,
not only its own preset's.

This is a known limitation, tracked as one.

## Definitions as files

`get --json` prints the definition, and `apply` takes that same document back —
so a workflow can live in a repository, be reviewed as a diff, and be applied
to another server:

```bash
horsie workflow get fix-bug --json > fix-bug.json
horsie workflow apply -f fix-bug.json --server https://other.example
```

A definition reads as the editor shows it:

```json
{
  "name": "fix-bug",
  "start": "triage",
  "steps": [
    {
      "name": "triage",
      "agent": "bug-triager",
      "prompt": "Decide how urgent this is.",
      "outcomes": [
        { "value": "p0", "description": "Broken for everyone; fix it now." },
        { "value": "p2", "description": "Worth filing, not worth stopping for." }
      ],
      "fields": [
        { "name": "component", "kind": "String", "description": "Where it broke.", "required": true }
      ],
      "transitions": [
        { "to": "fix", "when": { "op": "In", "value": { "values": ["p0"] } } },
        { "to": "file" }
      ]
    },
    { "name": "fix", "agent": "coder", "prompt": "Fix it and open a PR." },
    { "name": "file", "agent": "writer", "prompt": "File an issue." }
  ]
}
```

`apply` creates the workflow or fully replaces it; the name comes from the
file. There is deliberately no separate definition format — the JSON the API
takes is the only one, so there is nothing to keep in step.

Deleting a workflow leaves its runs alone. Each run holds its own snapshot of
the graph, so finished runs stay readable.

Every workflow command is in the [CLI reference](/cli/reference/).
