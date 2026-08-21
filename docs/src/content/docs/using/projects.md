---
title: Projects
description: One per body of work — each with its own models, runtimes, skills, memory and sessions.
kind: explanation
sidebar:
  order: 0
---

A **project** is the scope everything else in horsie belongs to. Your models,
your runtimes, your skills, your MCP servers, your memory spaces, your agents,
your routines, your workflows and your sessions all live inside one — and none
of them is visible from any other.

You always have at least one. A new account starts with a project called
**Default**, and you can add more whenever a piece of work deserves its own
setup.

## What a project separates

Everything. That is the whole rule, and it is worth stating plainly because the
consequence surprises people: **a new project starts empty**. No models, no
providers, no connected repositories, no skills. Not "empty except for the
credentials" — the credentials too.

That is deliberate. A project is not a folder inside one big account; it is a
separate working world. Work for one client cannot read the API keys, the
memory or the transcripts of another, and a runtime you connect to one project
cannot be selected by a session in the next — even though both are yours.

## When to make a second one

Make one when the *setup* differs, not when the topic does. Two pieces of work
that use the same models, the same runtime and the same skills belong in one
project; tell them apart with session names and tags instead.

Reach for a second project when you want:

- **A different set of credentials.** Separate API keys, a different GitHub
  connection, a provider account that must not be shared.
- **A different machine.** A runtime connected for one project and nothing
  else, so nothing can accidentally run there.
- **A clean memory.** Memory spaces are per project, so an agent working in one
  never recalls the other's notes.

## Switching

The project you are in is named at the top of the sidebar. Click it to switch;
the page reloads into the project you chose, and nothing from the previous one
comes with it.

The project is in the address too — every page lives under `/p/<project>/…` —
so a link you paste to somebody opens in the project it was copied from.

## Managing them

**Settings → Projects** lists what you have, and is where you create, rename
and delete one.

Renaming is safe: a project is identified by an id that never changes, so links
keep working. Deleting is not — it takes the project's sessions, agents,
settings and memories with it, and destroys the runtimes it provisioned. The
default project cannot be deleted at all, so there is always somewhere to land.

## On the command line

Every command that talks to a server takes `--project`, by id or by name:

```bash
horsie session list --project client-work
horsie connect --project client-work --workspace ~/code/client
```

Leave it out and the command uses your default project.

`horsie connect` in particular publishes this machine as a runtime for **one**
project. Run it once per project you want to reach this machine from, with a
different `--name` for each if they should be selectable separately.
