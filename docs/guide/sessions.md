# Sessions

A **session** is one conversation with an agent. Sessions are durable: the whole
transcript is saved server-side and streams live to the browser, so you can close
the tab and reconnect without losing anything.

## The session rail

The left rail lists all sessions, each with a status lamp and the state it names
(idle, running, awaiting input, failed). Its header carries a lamp for the rail's
own connection to the server, so a dead feed is visible before you click
anything. From the rail you can:

- **Search** sessions by name.
- Press **New** to start a session.
- Click a session to open it.
- Reach **Agents**, **Settings**, and **Admin** from the footer.
- Toggle light/dark theme.

On a narrow screen the rail is a drawer — open it with the menu button in the
header.

## Creating a session

Press **New**. This does not create anything yet: you get a draft, and the row of
controls above the composer adapts to what you've configured. The session is
created when you send your first message.

- **Model** *(required)* — one of the models you added in Settings. If you have
  none, the control links you to Settings to add one.
- **Environment** *(required)* — where the session runs and what it runs against.
  One list with two sections: the environments you have saved under
  **Environments**, and the runtimes currently connected. Picking a runtime that
  provisions its own workspace (velos) reveals a repo checklist in the same
  popover — 0..N repos, each with an optional ref — provided GitHub is
  connected. Picking a saved environment shows its runtime and repos read-only:
  they are part of its definition, so you change them by editing it. See
  [Environments](environments.md), [Runtime vendors](runtime-vendors.md) and
  [GitHub](github.md).
- **Skills** — pick which skill bundles to load. Shown only for provisioning
  vendors; bundles marked as defaults are pre-checked. See
  [Skills & plugins](skills-and-plugins.md).
- **MCP servers** — enable any MCP servers you've marked enabled, for this
  session. See [MCP servers](mcp-servers.md).
- **Memory** — pick which memory spaces the agent can read and write.

Send your first message to create the session and open it.

## The chat view

- **Composer** — type a message and press the orange **Send** key (or Enter).
  The agent's reply text streams in live. Tool calls appear as collapsible rows
  you can expand to see the raw input and output; file edits show up there as
  tool calls, not as diffs — there is no file browser or diff view yet. Thinking
  is shown once the reply finishes, not streamed, and is hidden by default.
- **Stop** — the red key beside Send; interrupts the current run mid-turn.
- **Status** — a lamp and the state it names: idle, running, awaiting input,
  failed, or unrecoverable.
- **Tokens** — a running total of tokens used across the session. Note this is
  cumulative usage, not a measure of how full the context window is; open it for
  the context-window meter and the per-turn breakdown.
- **Header readouts** — the model, environment, skills, MCP servers,
  and memory spaces this session was launched with. These are fixed for the
  session's lifetime.
- **Delete** — remove the session.
- **Tasks panel** — when the agent tracks a multi-step plan, a collapsible panel
  on the right shows the task list live as it's created and updated. The key
  that opens it lights up once there is a plan behind it; hover it for the
  completed/total count.

### When the agent asks you a question

If the agent needs input, it can pause and ask. A question card appears in the
transcript and the status lamp turns to **awaiting input**. Pick one of the
offered choices or type your own answer to let the run continue. If several
questions are parked at once they are answered together, in one go.

## Reconnecting

Sessions survive disconnects and server restarts. Reopen a session and its most
recent messages load instantly; **scroll up to load older messages on demand**
(long transcripts aren't all fetched at once). Live updates resume on top. You
don't need to keep the tab open for work to continue — the run happens on the
server, and reopening an idle session to read it doesn't wake its runtime.

## Stopping vs. deleting

- **Stop** halts the current turn but keeps the session; you can send another
  message to continue.
- **Delete** removes the session entirely. With the velos vendor, this also tears
  down the session's ephemeral container. With the local vendor, the shared
  runtime daemon keeps running (it isn't owned by any one session).
