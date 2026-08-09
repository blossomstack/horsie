---
title: Workflows
description: Chain agents into a graph of steps that share one workspace and branch on each step's result.
kind: how-to
sidebar:
  order: 7
---

A **workflow** is a graph of steps. Each step runs one of your agent presets
against a fixed instruction, and what it concludes decides which step runs
next. Every step in a run shares one workspace, so what one writes to disk the
next one reads.

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
- **Output fields** — what the step reports back. A condition can only read
  fields declared here.
- **Goes to** — where to hand off, and on what condition.

**Limits** are optional: how many turns the step may take before it fails, and
how many times a transient provider error is retried within it. Leave them
blank unless a step has earned an opinion.

**Visualize** swaps the panel for the graph, which redraws as you type;
choosing a node opens that step. Steps can be dragged into another order, which
changes how the list reads and nothing about how the run executes — the start
step and the transitions decide that.

### Conditions

A condition is an expression over the step's output, bound to `output`:

```text
output.severity == "p0"
output.approved
output.attempts > 3
```

Transitions are tried in order and the first match wins. A row with no
condition is the catch-all — put it last. If nothing matches, the run finishes
and that step's output is the run's.

A step with **no output fields** ends its turn with plain text, which becomes
its output. That is fine for a final step, but nothing can branch on it.

### Loops

A transition may point back to an earlier step; the graph draws it as a dashed
curve.

Loops are bounded by the workflow's **step budget** — the most steps one run
may execute, set on the definition, 100 if left blank. It is the only thing
stopping a loop whose condition never flips: a run that hits it fails with
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
step's output, which is the run's.

Opening a run in the browser shows its graph; clicking a step opens that step's
own page, which is the session view scoped to one agent. To follow one from a
terminal, take its agent id from `workflow status`:

```bash
horsie session tail 3f1a2b4c-… --output ./run.jsonl --agent 8a91f0d2-…
```

## When something goes wrong

**A step failed.** The run stops there. Read the step's page for why, then
press **Retry** on that node.

**A step asked a question.** A run started from the UI or the API is attended,
so a step may ask. The run's page says which step is waiting; **Answer it**
opens that step. Answering resumes it and the run carries on.

**You interrupted it, or the server restarted mid-run.** The step that was
running is marked cancelled and the run goes **suspended** rather than resuming
by itself — nobody knows how far that step got, so continuing is your decision.
The run's page says which step stopped and offers to retry it.

### Retrying is not a rollback

A retry re-runs the step against **whatever the previous attempt left on the
workspace**. Files it created are still there; commits it made are still made.
If a step is not safe to run twice, say so in its prompt, or have it check
before it acts.

Retries append. The earlier attempt stays readable, and the node shows a count.

## What a step can and cannot do

A step can read and write the shared workspace, call its preset's MCP servers,
use its memory spaces, and spawn subagents of its own.

A step cannot rename the session, and a run takes no messages — sending one is
answered `409`. A workflow works from its definition; if you want to talk to
it, you want a session.

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

`apply` creates the workflow or fully replaces it; the name comes from the
file. There is deliberately no separate definition format — the JSON the API
takes is the only one, so there is nothing to keep in step.

Deleting a workflow leaves its runs alone. Each run holds its own snapshot of
the graph, so finished runs stay readable.

Every workflow command is in the [CLI reference](/cli/reference/).
