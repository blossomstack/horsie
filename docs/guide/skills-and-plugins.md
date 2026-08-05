# Skills & plugins

A **skill/plugin bundle** is a package of skills (and optional hooks) that an
agent can use during a session. You install bundles from git repositories once,
then select which ones a session loads.

Bundles are provisioned into the sandbox at session start, so they need a
**provisioning runtime** — the **velos** vendor. The local runtime doesn't
install server bundles, but it can load skills from a plugin library on its own
machine — see [Skills on your own machine](#skills-on-your-own-machine-host-library).
See [Runtime vendors](runtime-vendors.md).

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

## Skills on your own machine (host library)

If you run the **local** vendor (`horsie connect`), the runtime loads skills
from a plugin library on that machine instead of server bundles:

1. Install plugins with the CLI: `horsie plugin install <git-url>`
   (`horsie plugin list` / `update` / `remove` manage the library).
2. Start `horsie connect` as usual — it passes the library to the runtime
   automatically. The confirmation line shows `plugins: N installed from …`.

Every session on that runtime then sees the library's skills, and the plugin
hooks horsie supports run on your machine. Installs and updates are picked up on
the next session scan — no reconnect needed.

horsie runs seven hook events today: `SessionStart` once per agent load and
`SubagentStart` once per subagent, `UserPromptSubmit` on every prompt, `Stop`
when a turn ends and `SubagentStop` when a subagent's does, and
`PreToolUse`/`PostToolUse` around every tool call. A bundle declaring an event
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
URL alone, because there is no way to tell which one you meant. On the server,
pasting its URL registers it as a marketplace and shows you the list; from the
CLI, add it as a marketplace and install by name (see below) — the error lists
the available names.

### Marketplaces

Some repositories are *marketplaces*: they carry an index of plugins rather than
a plugin. Add one once, then install from it by name:

```
horsie marketplace add https://github.com/anthropics/claude-plugins-public
horsie marketplace show claude-plugins-public
horsie plugin install agent-sdk-dev@claude-plugins-public
```

`horsie marketplace list` shows what you have added and how many plugins each
offers; `update` pulls a fresh index; `remove` drops it.

The CLI's marketplaces and the server's are separate registries: one is
per-user on a machine, the other is shared server state. They share the same
resolution rules and the same semantics, not the same rows.

Removing a marketplace does **not** uninstall plugins you installed from it —
dropping a source is not dropping the software. Use `horsie plugin remove` for
that.

An index entry may point at a different repository than the marketplace itself
(most entries in the public marketplace do). horsie clones whatever the entry
names, at the ref it pins, and installs the subdirectory it declares.

Since `<plugin>@<marketplace>` and a git URL are both just text, horsie treats
an argument as a marketplace reference only when both halves are plain lowercase
names — so `git@github.com:you/your-plugin.git` is always read as a URL.

### How the library is stored

Installed plugins are symlinks into a shared clone under `<data-dir>/sources`,
one clone per repository and ref. So `horsie plugin update` is a fast-forward
pull rather than a fresh clone, and two plugins published from one repository
share a single working copy. `horsie plugin remove` deletes the link and drops
the clone once nothing else points at it.

This is all-or-none: the whole library applies to every session on the runtime.

**A session that selects server bundles gets exactly those, and the host library
does not apply to it.** Selecting nothing leaves the host library in place. So
the library is the default for sessions that express no preference, and an
explicit selection replaces it rather than adding to it.

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
