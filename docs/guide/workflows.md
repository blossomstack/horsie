# Workflows

A workflow is a graph of steps. Each step runs one of your agents against a
fixed instruction, and what it concludes decides which step runs next. Every
step in a run shares one workspace, so what one step writes to disk, the next
one reads.

Use a workflow when the order matters and you want to fix it yourself. If you
would rather let the model decide how to split the work, give one agent the
whole job and let it spawn subagents instead.

## Defining one

Open **Workflows** in the sidebar and press **New workflow**. Each step needs:

- **Agent** — one of your agent presets. It supplies the model, MCP servers,
  memory spaces and thinking effort.
- **Prompt** — the step's instruction. Whatever the step is handed (the run's
  input for the first step, the previous step's result for the rest) is
  appended below it.
- **Output fields** — what the step reports back. A condition can only read
  fields you declare here.
- **Goes to** — where to hand off, and on what condition.

The graph beside the list redraws as you type.

### Conditions

A condition is an expression over the step's output, which is bound to
`output`:

```
output.severity == "p0"
output.approved
output.attempts > 3
```

Transitions are tried in order and the first match wins. A row with no
condition is the catch-all — put it last. If no transition matches, the run
finishes and that step's output is the run's.

A step with **no output fields** ends its turn with plain text, which becomes
its output. That is fine for a final step, but nothing can branch on it.

### Loops

A transition may point back to an earlier step. The graph draws it as a dashed
curve. Loops are bounded by a run-wide budget of 100 steps; a run that hits it
fails rather than spinning forever.

## Running one

Press **Start run** on the workflow's page, or:

```console
$ horsie workflow run fix-bug --input "the build is red on main"
Started run 3f1a2b4c-…
```

A run needs two things the definition deliberately does not hold: which runtime
hosts it, and which repos are cloned into its workspace. Those belong to the
invocation, so a workflow can be run against different machines and checkouts
without editing it.

A run **is a session**. It appears in the sidebar alongside your other
sessions, annotated with the workflow it came from, and every session command
works on it:

```console
$ horsie workflow status 3f1a2b4c-…
fix-bug  running  1,204 tokens

#    STEP                 TRY  STATUS      AGENT
0    triage               1    concluded   6e3c20c8-…
1    fix                  1    running     8a91f0d2-…

Not reached: file
```

Opening the run in the browser shows its graph. Clicking a step opens that
step's own page — the transcript, tool calls and all — which is the same
session view scoped to one agent. To follow one from the terminal, take its
agent id from `workflow status`:

```console
$ horsie session tail 3f1a2b4c-… --output ./run.jsonl --agent 8a91f0d2-…
```

## When something goes wrong

**A step failed.** The run stops there. Read the step's page to see why, then
press **Retry** on that node.

**A step asked a question.** A run started from the UI or the API is attended,
so a step may ask. The question appears on the run's page; answering it resumes
the step.

**The server restarted mid-run.** The step that was running is marked
interrupted and the run is suspended rather than resumed. A retry is a
decision for you, because nobody knows how far the interrupted step got.

### Retrying is not a rollback

A retry re-runs the step against **whatever the previous attempt left on the
workspace**. Files it created are still there; commits it made are still made.
If a step is not safe to run twice, say so in its prompt, or have it check
before it acts.

Retries append. The earlier attempt stays readable, and the node shows a count.

## What a step can and cannot do

A step can read and write the shared workspace, call its preset's MCP servers,
use its memory spaces, and spawn subagents of its own.

A step cannot rename the session, and a run takes no messages — sending one
returns a 409. A workflow works from its definition; if you want to talk to it,
you want a session.

## Skills

A run provisions the union of the skill bundles named by every step's preset,
because all the steps share one runtime and its bundle set is fixed when it
starts. Two consequences:

- If any step's preset selects a bundle, that selection replaces the host
  plugin library for the whole run, including for steps whose presets selected
  nothing.
- Every step can reach every bundle in the union, not only its own preset's.

This is tracked as a known limitation.

## Commands

```console
horsie workflow list
horsie workflow get <name>
horsie workflow run <name> --input <text> [--vendor <v>] [--repo <url>]
horsie workflow status <session-id>
horsie session status <session-id>
horsie session tail <session-id> --output <file> [--agent <agent-id>]
```
