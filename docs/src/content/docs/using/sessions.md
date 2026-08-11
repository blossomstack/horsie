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
