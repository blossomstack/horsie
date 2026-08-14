---
title: Sessions
description: Create a session, configure it, read the transcript, and pick it up again after a disconnect.
kind: how-to
sidebar:
  order: 1
---

A **session** is one running agent and the append-only record of everything it
did. It is saved server-side and streamed to the browser, so a session is
something you come back to rather than something you have open.

## Create one

Press **New** in the left rail. Nothing is created yet — you get a draft, and
the row of controls above the composer is where you configure it. The session
comes into being when you send the first message.

**Model** *(required)* — one of the models added under Settings. With none
configured the control links you there instead.

**Environment** *(required)* — which sandbox the session runs in and what it
runs against. One list with two sections: environments you have saved, and the
runtimes connected right now. Picking a runtime that builds its own workspace
reveals a repository checklist in the same popover, if GitHub is connected.
Picking a saved environment shows its runtime and repositories read-only —
they are part of its definition. See [Environments](/using/environments/).

**Skills** — which skill bundles to load. Bundles marked as defaults are
pre-checked. See [Skills & plugins](/using/skills-and-plugins/).

**MCP servers** — which of your configured MCP servers this session may use.
See [MCP servers](/using/mcp-servers/).

**Memory** — which memory spaces the agent may read and write.

Everything above is fixed for the session's lifetime — an agent whose
capabilities changed halfway through would make the transcript a record of two
different agents. The header shows what it was launched with.

## Read the transcript

The agent's reply text streams in as it is generated. Tool calls appear as
collapsible rows; expand one for the exact input the agent sent and the raw
output it got back. File edits are tool calls like any other — there is no
diff view and no file browser, by design.

Thinking is shown once a reply finishes rather than streamed, and is hidden
until you ask for it.

Beside the composer:

- **Send** — the orange key, or Enter.
- **Stop** — the red key. Interrupts the current run mid-turn; the session
  stays, and you can send another message to carry on.
- **Status** — a lamp and the word for it: provisioning, idle, running,
  awaiting input, finished, failed, or unrecoverable. A workflow run is a
  session, so it uses these same words; **finished** is the one only a run
  reaches, since a conversation is never over.
- **Tokens** — a running total for the session. This is cumulative usage, not
  how full the context window is; open it for the context meter and the
  per-turn breakdown.
- **Tasks** — when the agent is tracking a multi-step plan, a panel on the
  right shows it live. The key that opens it lights once there is a plan
  behind it.

## See its shape

The key beside the session name swaps the transcript for a **timeline**: the
same session drawn along one axis instead of down a page. Use it to find where
the time went, or to see what a session delegated without scrolling for it.

The top lane is the main agent, one bar per entry — what you said, what it
thought, each tool call, each answer — coloured by kind and as wide as it took.
Subagents and forked conversations get their own lanes below, each starting at
the moment it branched off. Click a bar to jump back to that entry in the
transcript; click a lane to open that agent.

One thing about the axis is worth knowing: idle stretches longer than a minute
collapse to a narrow hatched gutter labelled with what they swallowed, so a
session you left overnight is still readable. Everything else is drawn to
scale, with the longest single step setting it — hover any bar for its exact
duration.

The view is in the address bar, so a link to it opens on the timeline.

## Answer a question

An agent can pause and ask you something. A question card appears in the
transcript and the status turns to **awaiting input**. Pick one of the offered
choices or type your own; the run continues from there. Several questions
parked at once are answered together, in one go.

## Come back to it

Sessions survive a closed tab, a lost connection, and a server restart. Reopen
one and the most recent messages load immediately; scroll up to pull older
ones on demand, so a long transcript is not fetched all at once. Live updates
resume on top.

You do not need the tab open for work to continue — the run happens on the
server. Opening an idle session to read it does not wake its runtime.

## Branch a conversation

A conversation reaches a point where two directions are worth trying, or where
the context is full of settled work and the next thing is different. Type
`/fork` with what you want done next, and horsie starts a second conversation
inside the same session:

```
/fork try the same migration with a materialised view instead
```

You land in it straight away. It carries everything said up to that point, and
it shares the session's workspace — the same checkout, the same uncommitted
edits — so it can pick up exactly where the first one was.

`/summary-n-fork` does the same, but seeds the new conversation with a *summary*
of the old one rather than the whole history:

```
/summary-n-fork now write the migration guide
```

Use it when the detail is settled and only the conclusions matter. It starts
with a much smaller context, so it has more room to work in — at the cost of
being unable to scroll back into what it came from.

The original conversation is never changed by either. It gets a marker where the
branch happened, linking to the conversation that left.

A fork is a full conversation: it can ask you questions, spawn its own
subagents, and be forked again. It names itself once the direction is clear, and
appears in the rail nested under the session it belongs to, with its own status
lamp. Forking a fork nests one level deeper.

Nothing ever removes a fork on its own. Delete one from its menu in the rail
when you are done with it; deleting the session removes its forks too.

## Stop, or delete

**Stop** halts the current turn and keeps everything else.

**Delete** removes the session and its transcript. On a cloud vendor it also
tears down the session's container or machine. The local runtime is not owned
by any one session, so it keeps running.

## The session rail

The left rail lists every session with a status lamp, and carries its own lamp
for the rail's connection to the server — so a dead feed is visible before you
click anything. From it you can search by name, create a session, and reach
**Agents**, **Settings** and **Admin** from the footer. On a narrow screen it
becomes a drawer behind the menu button.

Runs started by a [routine](/using/routines/) are listed on that routine's own
page instead, so a job on a fifteen-minute timer cannot bury the conversations
you are actually having.
