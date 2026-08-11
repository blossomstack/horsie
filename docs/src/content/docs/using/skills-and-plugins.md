---
title: Skills & plugins
description: Install skill and plugin bundles from git, select them per session, and know what horsie runs from them.
kind: how-to
sidebar:
  order: 5
---

A **bundle** packages skills, agents, slash commands, hooks and MCP servers
that an agent can use during a session. You install bundles from git
repositories once, then select which ones a session loads.

Every runtime loads them the same way, whichever vendor it runs on: it fetches
the bundles its session selected over its own outbound connection at startup.
That includes the local runtime.

:::caution
Hooks run with the runtime's privileges, and they are not only observers. A
`PreToolUse` hook can deny a tool call or rewrite its input; a `PostToolUse`
hook can rewrite the output before the agent reads it; a `UserPromptSubmit`
hook can refuse a prompt outright. Any hook at all can stop the agent
mid-task. Install bundles you trust.
:::

## Install one

**Settings → Skills.** Paste a **git URL** and an optional **ref**, then
install. The server clones it and works out what it found — you do not have to
know which shape you have before pasting:

- **A bundle repository**, or a marketplace publishing exactly one plugin,
  installs straight into your library.
- **A marketplace of several plugins** is added as a source instead, listed on
  the same page. Open it and install any of them with one click; the picker
  marks the ones you already have.

Each installed bundle shows its name, version, skill count, a **hooks** badge
if it ships hooks, the marketplace it came from, and a description.

## Manage bundles

Per bundle, on the Skills page:

- **Default for new sessions** — pre-select it in the session config.
- **Update** — re-clone at its ref to pull in changes.
- **Delete** — remove it from the library.

## Manage marketplaces

Registered sources appear on the same page, each with a filter box and two
buttons:

- **Refresh** — re-clone the source and re-read its index, picking up plugins
  published since you added it. There is no automatic refresh; the cached index
  is a snapshot until you ask for a new one.
- **Remove** — drop the source. **Bundles installed from it stay installed** —
  dropping a source is not dropping the software.

Entries an index declares but horsie could not parse are named on the row
rather than dropped, so a catalogue that has quietly lost a plugin is visible.

An index entry may point at a different repository than the marketplace itself,
and most public ones do. horsie clones whatever the entry names, at the ref it
pins, and installs the subdirectory it declares.

## Use them in a session

In the session config row, tick the **Skills** bundles to load. Bundles marked
*default* are already checked. The same picker sits on an agent preset.

At session start the runtime fetches the selection and makes it available to
the agent.

## What a bundle contributes

**Skills** (`skills/`) become things the agent can invoke, and answer to `/`
in the composer.

**Agents** (`agents/*.md`) become agent types the session's agent can delegate
to, and answer to `@`. Each file's frontmatter names it, says when to pick it,
and may narrow its tools or ask for a model. A declared tool list is written in
the upstream plugin specification's vocabulary and mapped onto horsie's own;
names with no equivalent
grant nothing. It can only ever *narrow* — it is intersected with what the
session already allows, so installing a plugin can never hand an agent a tool
you withheld.

**Commands** (`commands/*.md`) become slash commands. Send `/name args` and
horsie expands the template before the model sees anything: `$ARGUMENTS` is
everything after the name, `$1`, `$2` … the individual words. A command's name
is its filename. A `/name` horsie does not recognise is sent as you wrote it —
`/etc/hosts` has to survive being typed.

A few slash commands are horsie's own and need no bundle. Today that is
`/compact`, which summarises earlier history to free up context and takes
optional instructions — `/compact keep the migration details`. Built-ins are
offered whether or not any bundle is installed, and a bundle cannot take one
over by declaring the same name. See
[Context & memory](/internals/context-and-memory/#keep-less-compaction).

**MCP servers** (`.mcp.json`) run in the runtime, next to the workspace. Both
a local server (`"command": "npx", …`) and a remote one (`"type": "http"`)
work. They authenticate with whatever the declaration carries; one needing an
interactive login belongs in [Settings instead](/using/mcp-servers/).

**Hooks** (`hooks/hooks.json`) run around the agent's work. See below.

## Hooks

horsie runs eight of the events the plugin spec documents:

| | Events |
| --- | --- |
| **Run** (8) | `SessionStart`, `SubagentStart`, `UserPromptSubmit`, `UserPromptExpansion`, `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStop` |
| **Understood, not fired** (8) | `PostToolUseFailure`, `PostToolBatch`, `SessionEnd`, `StopFailure`, `TaskCreated`, `TaskCompleted`, `Notification`, `CwdChanged` |
| **No horsie equivalent** (15) | Permission prompts, context compaction, worktrees, file watching, agent teams, MCP elicitation, and the display layer |

A bundle declaring an event in the last two rows still installs — its skills
work — and the events it cannot run are named rather than silently ignored.

`SessionStart` fires with `source: "startup"` the first time a session runs and
`source: "resume"` whenever horsie reloads it, which it does on its own after a
session has been idle long enough to be unloaded. Match on `startup` for a hook
that must run once; leave the matcher off for one that should refresh context
whenever the session comes back. horsie never reports `clear`, `compact` or
`fork` — it has none of the three.

Both transports work. `type: "command"` runs a shell command with the plugin
root as its working directory. `type: "http"` POSTs the same payload to a URL
and reads the response body as the reply; headers may interpolate `$NAME` for
any variable the declaration lists in `allowedEnvVars`, which is how a webhook
gets its credential.

Two things differ over HTTP. There is no exit code, so an HTTP hook can only
refuse through `decision` or `permissionDecision` in its JSON body. And a hook
that does not answer at all — a non-2xx, an unreachable endpoint, a timeout —
counts as having failed, which for `PreToolUse` **denies the call it was
guarding**. horsie fails a guard closed on either transport, on the grounds
that a guard which could not run has not permitted anything.

Any hook may answer `{"continue": false, "stopReason": "…"}`. horsie stops the
agent that ran it and shows the reason: a tool hook's halt fails the turn,
fails a subagent for its parent, or fails a workflow step. On `Stop` and
`SubagentStop` the turn is already ending, so a halt there simply lets it end.

## Where the files go

The server stores each installed plugin as a content-addressed zip. When a
runtime starts, the server hands it the `{name, hash}` refs its session
selected plus a short-lived token; the runtime fetches each zip over its own
outbound connection, checks the hash, and unpacks it into a directory of its
own.

That directory belongs to one runtime. A session sees exactly the bundles it
selected and never another session's, and the whole thing is deleted when the
session is. On the local runtime the directory sits under the CLI's state
directory; a restart reuses what is there rather than downloading again, and
anything left by a crash is cleared the next time `horsie connect` starts.

Change a session's selection and the next runtime it gets materializes the new
set, replacing the old one.

## What is not supported

So you can tell before installing rather than after:

| Component | horsie | Notes |
| --- | --- | --- |
| Skills | yes | Manifest `skills` field honoured, string or array. |
| Agents | yes | `name`, `description`, `tools`, `model`. `color`, `effort` and `initialPrompt` are not read. |
| Commands | yes | `description`, `argument-hint`, `$ARGUMENTS`, `$1..$9`. Not `allowed-tools`, `@path`, `disable-model-invocation`, `hide-from-slash-command-tool`. |
| Hooks | 8 of 31 events | Both `command` and `http`. |
| MCP | yes | stdio and http, in the runtime. No OAuth. |
| `${CLAUDE_PLUGIN_ROOT}` | partly | Resolved in skill and agent bodies, hook commands and URLs, and MCP declarations — not in a command body, which the server catalogues without knowing where the runtime mounted the plugin. |
| Marketplaces | yes | Add a source, browse it, install by name. |

Two deliberate omissions. `` !`cmd` `` inside a command template is left as
written rather than executed — ask the agent to run it, since it has a shell.
And `allowed-tools` on a command is not read: a command narrowing your toolbox
mid-session would be a surprise rather than a convenience.

horsie also reads a repository's own packaging rather than guessing:
`.claude-plugin/plugin.json` says where the skills live, and
`.claude-plugin/marketplace.json` says which subdirectory a repository
publishes from.
