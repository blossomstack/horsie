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

1. Enter a **Git URL** for the bundle repository, and an optional **ref**
   (branch/tag/commit).
2. Install it. The server clones the repo and adds it to your bundle library.

Each installed bundle lists its name, version, skill count, a **hooks** badge if
it ships hooks, and a description.

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

These options appear only when the session's runtime supports provisioning
(velos). At session start, the runtime fetches the selected bundles and makes
their skills available to the agent.

## Skills on your own machine (host library)

If you run the **local** vendor (`horsie connect`), the runtime loads skills
from a plugin library on that machine instead of server bundles:

1. Install plugins with the CLI: `horsie plugin install <git-url>`
   (`horsie plugin list` / `update` / `remove` manage the library).
2. Start `horsie connect` as usual — it passes the library to the runtime
   automatically. The confirmation line shows `plugins: N installed from …`.

Every session on that runtime then sees the library's skills, and plugin
`SessionStart` hooks run on your machine when a session starts. Installs and
updates are picked up on the next session scan — no reconnect needed.

### Where horsie looks for skills

horsie reads the repository's own plugin packaging rather than guessing:

- `.claude-plugin/plugin.json` — its `skills` field (a path or a list of paths)
  says where the skills live; without it, the conventional `skills/` directory
  is used.
- `.claude-plugin/marketplace.json` — when a repository publishes its plugin
  from a subdirectory, this says which one.

A repository whose marketplace lists *several* plugins cannot be installed by
URL alone, because there is no way to tell which one you meant — add it as a
marketplace and install by name instead (see below). The error lists the
available names.

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

Removing a marketplace does **not** uninstall plugins you installed from it —
dropping a source is not dropping the software. Use `horsie plugin remove` for
that.

An index entry may point at a different repository than the marketplace itself
(most entries in the public marketplace do). horsie clones whatever the entry
names, at the ref it pins, and installs the subdirectory it declares.

Since `<plugin>@<marketplace>` and a git URL are both just text, horsie treats
an argument as a marketplace reference only when both halves are plain lowercase
names — so `git@github.com:you/your-plugin.git` is always read as a URL.

### Hooks

A plugin can ship hooks in `hooks/hooks.json` that run at points in the agent's
work. horsie runs:

- **`SessionStart`** — output is injected into the session's bootstrap context.
- **`PreToolUse`** — runs before a tool call and may block it, amend its
  arguments, or add context.
- **`PostToolUse`** — runs after a tool call and may replace or annotate what the
  agent sees.

Matchers written for Claude Code work unchanged: Claude's tool names are mapped
onto horsie's, so a matcher of `Edit|Write|MultiEdit` selects `write_file`,
`find_and_replace` and `replace_lines`.

Two behaviours worth knowing:

- **A `PreToolUse` hook that cannot run denies the call.** If it times out,
  fails to start, or exits with an unexpected code, horsie refuses the tool
  rather than proceeding unguarded. Hooks for other events are logged and
  skipped on failure, because the action has already happened.
- **A hook asking for approval is allowed.** horsie has no permission prompt and
  runs unattended sessions, so `permissionDecision: "ask"` proceeds and is
  logged.

Claude Code defines many more hook events than horsie runs. The rest are
recognised and reported, but do not fire.

### How the library is stored

Installed plugins are symlinks into a shared clone under `<data-dir>/sources`,
one clone per repository and ref. So `horsie plugin update` is a fast-forward
pull rather than a fresh clone, and two plugins published from one repository
share a single working copy. `horsie plugin remove` deletes the link and drops
the clone once nothing else points at it.

This is all-or-none: the whole library applies to every session on the runtime,
independently of the server's bundle library (the Skills page remains
velos-only).

> Hooks execute with the runtime's privileges on your machine — only install
> plugins you trust.

## Notes

- Bundles come from **git** — there's no upload; point the installer at a repo.
- The **local** runtime does not provision server bundles, so the Skills
  options are hidden for sessions using it — but it loads the CLI-installed
  host library (above). Use velos for per-session bundle selection.
