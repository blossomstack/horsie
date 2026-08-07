# Skills & plugins

A **skill/plugin bundle** is a package of skills (and optional hooks) that an
agent can use during a session. You install bundles from git repositories once,
then select which ones a session loads.

Every runtime loads bundles the same way, whichever vendor it runs on: it
fetches the ones its session selected over its own outbound connection at
startup. That includes the local runtime — see
[Skills on your own machine](#skills-on-your-own-machine) for where the files
go there. See [Runtime vendors](runtime-vendors.md).

## Install a bundle

Open **Settings → Skills**:

1. Enter a **Git URL**, and an optional **ref** (branch/tag/commit).
2. Install it. The server clones the repo and works out what it found.

One box handles both shapes, because you should not have to know which you have
before pasting it:

- **A bundle repo** — or a marketplace publishing exactly one plugin — installs
  straight into your library.
- **A marketplace of several plugins** — the source is added to a
  **Marketplaces** section on the same page, opened, listing what it offers.
  Install any of them with one click; the picker marks the ones you already
  have.

Each installed bundle lists its name, version, skill count, a **hooks** badge if
it ships hooks, the marketplace it came from if it came from one, and a
description.

## Manage marketplaces

Registered sources appear on the Skills page, each with a filter box (the public
catalogue lists ~276 plugins) and two buttons:

- **Refresh** — re-clone the source and re-read its index, picking up plugins
  published since you added it. There is no automatic refresh; the cached index
  is a snapshot until you ask for a new one.
- **Remove** — drop the source. **Bundles installed from it stay installed** —
  dropping a source is not dropping the software. Delete them individually if
  that is what you want.

Entries an index declares but horsie could not parse are named on the row rather
than dropped, so a catalogue that has quietly lost a plugin is visible.

To add a marketplace, paste its URL into the install box. There is deliberately
no separate "add marketplace" form: one box, one place to paste.

## Manage bundles

On the Skills page, per bundle:

- **Default for new sessions** — toggle on to pre-select this bundle in the New
  Session dialog. Handy for bundles you almost always want.
- **Update** — re-clone the bundle at its ref to pull in changes.
- **Delete** — remove the bundle from the library.

## Use bundles in a session

In the New Session dialog, under **Advanced**:

1. Turn on **Enable plugins**.
2. Tick the **Skills** bundles to load. Bundles marked *Default for new sessions*
   are pre-checked.

At session start, the runtime fetches the selected bundles and makes their
skills available to the agent. Every runtime can do this, including the local
one — a runtime fetches bundles over its own outbound connection into its own
plugins directory, which does not require it to have built a workspace.

The same Skills picker sits on an agent preset, so every session that preset
starts loads the same bundles.

## Skills on your own machine

The **local** vendor (`horsie connect`) works the same way as any other: its
skills are the bundles selected for the session, fetched from the server. There
is no separate library to install into on that machine, and no CLI commands that
manage one.

What is different is where the files land. A runtime materializes its session's
bundles into a directory of its own under the CLI's state dir, once, when the
runtime is created. Restarting the runtime — which horsie does on its own, when
a session that was idle long enough comes back — reuses what is already there
rather than downloading it again. Deleting the session deletes the directory,
and anything left by a crash is cleared the next time `horsie connect` starts.

Change a session's Skills selection and the next runtime it gets materializes
the new set, replacing the old one. The plugin hooks horsie supports run on your
machine, under the same sandbox as the rest of the runtime.

horsie runs eight hook events today: `SessionStart` once per agent load and
`SubagentStart` once per subagent, `UserPromptSubmit` on every prompt and
`UserPromptExpansion` on every slash command, `Stop` when a turn ends and
`SubagentStop` when a subagent's does, and `PreToolUse`/`PostToolUse` around
every tool call. A bundle declaring an event
horsie cannot fire still installs — its skills work — and the events it cannot
run are named rather than silently ignored.

`SessionStart` fires with `source: "startup"` the first time a session runs and
`source: "resume"` whenever horsie reloads it — which it does on its own, after
a session has been idle long enough to be offloaded, not only when you resume
one yourself. Match on `startup` for a hook that must run once; leave the
matcher off, or match `startup|resume`, for one that refreshes context whenever
the session comes back. horsie never reports `clear`, `compact` or `fork`: it
has none of the three.

Both hook transports work: `type: "command"` runs a shell command with the
plugin root as its working directory, and `type: "http"` POSTs the same payload
to a URL and reads the response body as the reply. HTTP headers may interpolate
`$NAME` or `${NAME}` for any variable the declaration lists in `allowedEnvVars`,
which is how a webhook gets its credential; a variable not on that list is left
in the header as written rather than read out of the runtime's environment.

Two things differ over HTTP. There is no exit code, so an HTTP hook can only
refuse through `decision` / `permissionDecision` in its JSON body — exit 2 has
no equivalent. And a hook that does not answer at all — a non-2xx, an
unreachable endpoint, a timeout — is recorded as having failed, which for
`PreToolUse` **denies the call it was guarding**. horsie fails a guard closed
whichever transport it runs over, on the grounds that a guard which could not
run has not permitted anything; upstream Claude Code lets an HTTP failure
through. Point a `PreToolUse` webhook only at an endpoint you are willing to
have gate your tool calls.

A bundle's `agents/*.md` become **agent types** the session's agent can delegate
to. Each file's frontmatter names it (`name`), says when to pick it
(`description`), and may narrow its tools (`tools`) or ask for a model
(`model`); the body below the header is that agent's instructions. horsie offers
the list on its `spawn_agent` tool, so the model chooses one by description the
way it chooses a skill.

Three things to know. A declared `tools` list is written in Claude Code's
vocabulary (`Read`, `Grep`, `Edit`), and horsie maps it onto its own tools —
names with no horsie equivalent (`WebFetch`, `TodoWrite`) simply grant nothing.
It can only ever *narrow*: it is intersected with whatever the session already
allows, so installing a plugin can never hand an agent a tool you withheld, and
it covers the file, shell and MCP tools only — `spawn_agent`, `subagent_status`
and the memory tools sit outside it and stay available. A declared `model` is
honoured only when it names a model your horsie actually has; in practice agents
declare `inherit`, `sonnet` or `opus`, so they inherit the session's model rather
than silently switching your provider.

A bundle's `commands/*.md` become **slash commands**. Send `/name args` as your
message and horsie expands it into that file's template before the model sees
anything — `$ARGUMENTS` becomes everything you typed after the name, and `$1`,
`$2` … the individual words. A command's name is its filename, so
`commands/review.md` is `/review`.

Skills answer to `/` too, and a bundle's agents to `@`: `/brainstorming a new
API` and `@code-reviewer this diff` each expand into an instruction naming the
thing, which is what sends the agent to its skill or spawns the subagent. Start
a message with either character and horsie offers what your selected bundles
declare; the **Skills** settings page lists the same entries per bundle, so you
can see what installing one actually gave you.

Two things this deliberately does not do. `` !`cmd` `` — running a shell inside
a template — is left as written rather than executed; ask the agent to run the
command instead, since it has a shell. And `allowed-tools` on a command is not
read: a command narrowing your toolbox mid-session would be a surprise rather
than a convenience.

A `/name` horsie does not recognise is sent as you wrote it — a message may
legitimately start with a slash, and `/etc/hosts` has to survive being typed.

A bundle's `.mcp.json` brings **MCP servers** with it, and their tools appear
alongside horsie's own as `mcp__<server>__<tool>`. Both shapes work: a local one
(`"command": "npx", …`) and a remote one (`"type": "http", "url": …`).

These run **in the sandbox**, not on the server — a plugin's local server belongs
next to the workspace, and running it on the server host would be both wrong and
a way for a plugin to execute commands there. That is the difference from the MCP
servers you add in **Settings → MCP**, which run in the server process and can do
OAuth. A plugin server authenticates with whatever its declaration carries in
`env` or `headers`, sent exactly as written; if it needs an interactive login,
add it as a Settings server instead. A plugin server that will not start
contributes no tools and is logged — it never stops the session.

Two servers cannot share a name. If a plugin declares one your **Settings → MCP**
list already has, the one you configured wins and the plugin's is ignored; if two
plugins declare the same name, the first wins. Either way the loser is logged
rather than silently merged.

Any hook, on any event, may answer `{"continue": false, "stopReason": "…"}`.
horsie stops the agent that ran it and shows the reason: a tool hook's halt
fails the turn, fails a subagent for its parent, or fails a workflow step. On
`Stop` and `SubagentStop` the turn is already ending, so a halt there simply
lets it end — overriding a sibling hook's `decision: "block"`, which is where
`continue`'s precedence over `decision` is visible.

### Where horsie looks for skills

horsie reads the repository's own plugin packaging rather than guessing:

- `.claude-plugin/plugin.json` — its `skills` field (a path or a list of paths)
  says where the skills live; without it, the conventional `skills/` directory
  is used.
- `.claude-plugin/marketplace.json` — when a repository publishes its plugin
  from a subdirectory, this says which one.

A repository whose marketplace lists *several* plugins cannot be installed by
URL alone, because there is no way to tell which one you meant. Pasting its URL
registers it as a marketplace instead and shows you the list, so you can pick.

### Marketplaces

Some repositories are *marketplaces*: they carry an index of plugins rather than
a plugin. Add one in **Settings → Skills** by pasting its URL, and horsie
records it as a catalogue you can then install from by name. The row shows how
many plugins it offers, refreshes its index on demand, and can be removed.

Removing a marketplace does **not** uninstall plugins installed from it —
dropping a source is not dropping the software.

An index entry may point at a different repository than the marketplace itself
(most entries in the public marketplace do). horsie clones whatever the entry
names, at the ref it pins, and installs the subdirectory it declares.

### How bundles reach a session

The server stores each installed plugin as a content-addressed zip. When a
session's runtime starts, the server hands it the list of `{name, hash}` refs
that session selected plus a short-lived token, and the runtime fetches each zip
over its own outbound connection, checks the hash, and unpacks it into a
directory of its own.

That directory belongs to one runtime. A session sees exactly the bundles it
selected and never another session's, and the whole thing is deleted when the
session is.

> Hooks execute with the runtime's privileges on your machine, and they are not
> only observers: a `PreToolUse` hook can deny a tool call or rewrite its input
> before it runs, a `PostToolUse` hook can rewrite its output before the agent
> reads it, a `UserPromptSubmit` hook can refuse a prompt outright, and a `Stop`
> or `SubagentStop` hook can refuse to let a turn end and send the agent back for
> another round. Any hook at all can stop horsie mid-task with
> `continue: false`. `SessionStart`, `SubagentStart`, `UserPromptSubmit`, `Stop`
> and `SubagentStop` can also inject text straight into the model's context.
> Only install plugins you trust.

## Notes

- Bundles come from **git** — there's no upload; point the installer at a repo.
- Per-session bundle selection works on every runtime, the local one included.
  What the local runtime cannot do is check out **repos**, since it runs over a
  directory you own rather than one it built — so that picker stays hidden for
  it.
