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

**Tools** — which of horsie's built-in tools the agent may call, grouped: files
and shell, workspace, planning, timers, delegation, workflows, session, and
horsie itself. Groups start collapsed, each showing how many of its tools are
chosen; the checkbox on a group row selects or clears the whole group without
opening it. Open one to pick individual tools.

Each tool is marked **read** or **write**, and the **All / Read / Write**
control filters what is *listed* — it never changes what is selected. Switching
to Read hides the write tools; anything chosen among them stays chosen and comes
back when you switch back. **Select all** and **Clear** act on whatever is
listed, so to reach a read-only agent, filter to Write and press Clear.

It starts on **Default** — every group except horsie. That is not the same as
ticking every box: it defers to the server, so a preset saved today follows a
later horsie's idea of a sensible default instead of freezing this one's list.
Touch anything and the selection becomes exactly what you chose.

The **horsie** group is how you let an agent manage this server — its agents,
workflows, routines, environments, models and runtimes. Selecting one of those
tools *is* the grant, which is why none of them is in the default set: changes
take effect immediately and are not confirmed with you first.

Skills, MCP servers and memory spaces are chosen by their own controls and are
never removed by narrowing this one.

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
  reaches, since a session is never over.
- **Tokens** — a running total for the session. This is cumulative usage, not
  how full the context window is; open it for the context meter and the
  per-turn breakdown.
- **Tasks** — when the agent is tracking a multi-step plan, a panel on the
  right shows it live. The key that opens it lights once there is a plan
  behind it.

## See its shape

A three-way switch beside the session name chooses what the pane shows. Its
middle setting is a **timeline**: the same session drawn along one axis instead
of down a page. Use it to find where the time went, or to see what a session
delegated without scrolling for it.

The message box belongs to the transcript, so neither picture has one under it
— press the switch's left setting to go back to the conversation and type.

The top lane is whichever run you are looking at — the session's main agent on
the session's own page, that subagent on a subagent's page — one bar per entry:
what you said, what it thought, each tool call, each answer, coloured by kind
and as wide as it took. Whatever that run spawned gets its own lane below, each
running from the moment it branched off to the last thing it did.

Names sit in a sidebar down the left, indented under whatever spawned them, and
they stay put while the lanes scroll. From there:

- **Hover a lane** — dashed lines drop from its start and end to the lane it
  came from, so you can see which part of the parent's work it covers, and a
  card gives its full name, what became of it and how long it took.
- **Click the name** — shows what that agent is in a panel on the right: its
  tokens, how full its context is, what it was asked to do and what it
  produced. The same panel the graph opens, because it is the same question.
- **Click the arrow beside the name** — leaves for that agent's own page.
- **Click its span** — draws that agent's own work along the same axis, without
  leaving the map.
- **Click the chevron** — folds away everything that agent spawned.

Click any bar on any lane to read that entry in the panel — its text, its
thinking and the calls it issued — with a key there to go and read it in place
in the transcript.

One thing about the axis is worth knowing: idle stretches longer than a minute
collapse to a narrow hatched gutter labelled with what they swallowed, so a
session you left overnight is still readable. Everything else is drawn to
scale, with the longest single step setting it — hover any bar for its exact
duration.

The view is in the address bar, so a link to it opens on the timeline.

## See what it spawned

The switch's third setting is a **graph**: every agent the session holds and
every sub session branched from it, drawn as the one tree they are. Where the
timeline answers *when*, this answers *what spawned what* — reach for it when a
session has branched or delegated deeply enough that the lineage is the thing
you are trying to follow. It is also how you reach a sub session: the rail
lists sessions, and this is where their shape lives.

The main agent sits on the left, and everything below it hangs off to the
right, generation by generation. Each box says what kind of thing it is — main
session, subagent, sub session, workflow step — then its title, then what it is
doing right now and the preset it runs; its colour is its status, the same lamp
colours the rest of the console uses. Hover one for its full name, how long it
took and when it started.

Every box has a title: an agent names the session it is the main agent of, and
whoever spawns a subagent or branches a sub session titles it at that moment.

From there:

- **Click a box** — shows that agent in a panel on the right: its tokens, how
  full its context is, what it was asked to do, what it produced, and a key to
  delete it if it is a subagent's run or a sub session.
- **Click the arrow in its top-right corner** — leaves for that agent's own
  page.
- **Click the circle on its right edge** — folds away everything below it. The
  box then shows how many agents it is standing in for, so a folded branch
  still says how big it is. Click again to bring them back.

Folding is shared with the timeline: what you put away in one view is put away
in the other. The view is in the address bar, so a link to it opens on the
graph.

All three views are offered on every run's page, not just the session's own,
and each is drawn of the run you are on. Whichever view you pick is remembered
on this browser, so the next session you open lands on it rather than back on
the transcript. A link that names a view
still wins, and a session you have just started always opens on its transcript
— that is where the answer to the message you just sent appears.

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

## Branch a session

A session reaches a point where two directions are worth trying, or where
the context is full of settled work and the next thing is different. Type
`/fork` with what you want done next, and horsie starts a **sub session**
under it:

```
/fork try the same migration with a materialised view instead
```

You land in it straight away. It carries everything said up to that point, and
it shares the parent's workspace — the same checkout, the same uncommitted
edits — so it can pick up exactly where the first one was.

`/summary-n-fork` does the same, but seeds the sub session with a *summary*
of the parent rather than the whole history:

```
/summary-n-fork now write the migration guide
```

Use it when the detail is settled and only the conclusions matter. It starts
with a much smaller context, so it has more room to work in — at the cost of
being unable to scroll back into what it came from.

The parent is never changed by either. It gets a marker where the branch
happened, linking to the sub session that left.

A sub session is a session in every way that matters: it can ask you questions,
spawn its own subagents, and be branched again. The one thing it does not do is
name itself — it is titled at the moment it is branched, by whoever branched
it, so it is never a nameless session waiting for a model to get round to it.
A `/fork` takes its title from the first line of the brief you typed. It
appears on the session's [graph](#see-what-it-spawned) hanging off whatever it
branched from, with its own status lamp. Branching a sub session nests one
level deeper.

The agent can also start one on its own, without waiting to be asked. Its
`spawn_subsession` tool hands a *title* and a *task* to a sub session — no
copy, no summary. The agent already knows the context, so it writes the brief
itself, the same way it writes one for a subagent. When the work splits into a direction you
will want to steer separately, it can give that direction its own sub session
and carry on with the rest.

The sub session appears on the graph the moment it exists, shares the workspace
like any other, and starts on the brief and nothing else — so it is yours to
talk to from the first message. The agent hears nothing back from it.

Nothing ever removes a sub session or a subagent's run on its own. Delete
either with the bin key on its own page, or from the panel the
[graph](#see-what-it-spawned) opens; deleting a session removes everything it
hosts. Removing a subagent's run takes the work it delegated with it — the
subagents below it, and any workflow it invoked — but leaves any sub session
branched from it standing, because that is a session somebody is having rather
than work it was doing.

## Stop, or delete

**Stop** halts the current turn and keeps everything else.

**Delete** removes the session and its transcript. On a cloud vendor it also
tears down the session's container or machine. The local runtime is not owned
by any one session, so it keeps running.

## The session rail

The left rail lists every session with a status lamp — sessions only; a
session's sub sessions are its shape, and the graph draws that — and carries
its own lamp
for the rail's connection to the server — so a dead feed is visible before you
click anything. From it you can search by name, create a session, and reach
**Agents**, **Settings** and **Admin** from the footer. On a narrow screen it
becomes a drawer behind the menu button.

Runs started by a [routine](/using/routines/) are listed on that routine's own
page instead, so a job on a fifteen-minute timer cannot bury the sessions
you are actually having.
