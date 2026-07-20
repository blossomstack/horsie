# Sessions

A **session** is one conversation with an agent. Sessions are durable: the whole
transcript is saved server-side and streams live to the browser, so you can close
the tab and reconnect without losing anything.

## The sidebar

The left sidebar lists all sessions with a live status dot on each (updating,
idle, error). You can:

- **Search** sessions by name.
- Click **New** to open the New Session dialog.
- Click a session to open its chat view.
- Reach **Settings** and **Skills** from the footer.
- Toggle light/dark theme.

## Creating a session

Click **New**. The dialog adapts to what you've configured:

- **Name** *(optional)* — a label for the session. Auto-generated if left blank.
- **Model** *(required)* — one of the models you added in Settings. If you have
  none, the dialog links you to Settings to add one.
- **Runtime vendor** — shown only when more than one runtime is active. Otherwise
  the session uses the default vendor. See [Runtime vendors](runtime-vendors.md).
- **Repositories** — shown only when the chosen vendor supports provisioning
  (velos) **and** GitHub is connected. Pick 0..N repos, each with an optional
  ref. See [GitHub](github.md).
- **Advanced:**
  - **Enable plugins** + **Skills** — turn on plugin hooks and pick which skill
    bundles to load. Shown only for provisioning vendors; bundles marked as
    defaults are pre-checked. See [Skills & plugins](skills-and-plugins.md).
  - **MCP servers** — enable any MCP servers you've marked enabled, for this
    session. See [MCP servers](mcp-servers.md).

Click **Create** to open the session.

## The chat view

- **Composer** — type a message and send it. The agent's reply text streams in
  live. Tool calls appear as collapsible rows you can expand to see the raw
  input and output; file edits show up there as tool calls, not as diffs — there
  is no file browser or diff view yet. Thinking is shown once the reply
  finishes, not streamed.
- **Stop** — interrupt the current run mid-turn.
- **Status badge** — shows whether the session is idle, running, or errored.
- **Token usage** — a running total of tokens used across the session. Note this
  is cumulative usage, not a measure of how full the context window is.
- **Repo chips** — the repositories checked out for this session, if any.
- **Delete** — remove the session.

### When the agent asks you a question

If the agent needs input, it can pause and ask. A prompt appears in the
transcript; type your answer to let the run continue.

## Reconnecting

Sessions survive disconnects and server restarts. Reopen a session and its full
history replays instantly, then live updates resume. You don't need to keep the
tab open for work to continue — the run happens on the server.

## Stopping vs. deleting

- **Stop** halts the current turn but keeps the session; you can send another
  message to continue.
- **Delete** removes the session entirely. With the velos vendor, this also tears
  down the session's ephemeral container. With the local vendor, the shared
  runtime daemon keeps running (it isn't owned by any one session).
